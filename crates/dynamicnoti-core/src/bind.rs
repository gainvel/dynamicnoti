//! `bind()` — validate a raw notification's fields against its type schema, apply defaults,
//! coerce kinds, and clamp ranges. This is fault-isolation boundary #2: it must NEVER panic
//! (the daemon also wraps it in `catch_unwind`, but it is panic-free by construction).

use std::collections::HashMap;
use std::sync::Arc;

use crate::scene::Value;
use crate::template::{FieldKind, FieldSpec, TypeTemplate};
use crate::{Behavior, CoreError, RawNotification};

/// A validated, ready-to-render notification: every schema field coerced/defaulted, behavior
/// and replace_key resolved, and the template carried along for [`crate::scene::build`].
#[derive(Clone, Debug)]
pub struct BoundNotification {
    pub type_name: String,
    pub behavior: Behavior,
    pub replace_key: Option<String>,
    pub fields: HashMap<String, Value>,
    pub template: Arc<TypeTemplate>,
}

/// Validate and coerce `raw` against `template`. Returns an error only for a missing required
/// field or an unknown type; everything else degrades gracefully (defaults, clamps, drops).
pub fn bind(template: Arc<TypeTemplate>, raw: RawNotification) -> Result<BoundNotification, CoreError> {
    let ty = &template.meta.name;
    let mut fields: HashMap<String, Value> = HashMap::new();

    for (name, spec) in &template.fields {
        match raw.fields.get(name) {
            Some(v) => {
                if let Some(coerced) = coerce(v, spec) {
                    fields.insert(name.clone(), coerced);
                } else if let Some(def) = spec.default.as_ref().and_then(|d| coerce(d, spec)) {
                    fields.insert(name.clone(), def);
                } else if spec.required {
                    return Err(CoreError::MissingField { ty: ty.clone(), field: name.clone() });
                }
                // else: optional field with an invalid value and no default → drop it.
            }
            None => {
                if let Some(def) = spec.default.as_ref().and_then(|d| coerce(d, spec)) {
                    fields.insert(name.clone(), def);
                } else if spec.required {
                    return Err(CoreError::MissingField { ty: ty.clone(), field: name.clone() });
                }
                // else: optional field, absent → leave out (image leaves get skipped in build).
            }
        }
    }

    // Keep fields the source supplied that the schema didn't declare, so Format bindings can
    // reference them (e.g. `{app_name}`). They are passed through as text.
    for (name, v) in &raw.fields {
        fields.entry(name.clone()).or_insert_with(|| v.clone());
    }

    let replace_key = raw.replace_key.or_else(|| template.meta.replace_key.clone());
    let behavior = Behavior { priority: template.meta.priority, timeout_ms: template.meta.timeout_ms };

    Ok(BoundNotification { type_name: ty.clone(), behavior, replace_key, fields, template })
}

/// Coerce a value to the field's declared kind, returning `None` when the value is invalid for
/// that kind (an out-of-set enum, say). Floats are clamped to `[min, max]`.
fn coerce(v: &Value, spec: &FieldSpec) -> Option<Value> {
    match spec.kind {
        FieldKind::String => Some(Value::Text(v.as_text())),
        FieldKind::Icon => Some(Value::Text(v.as_text())),
        FieldKind::Image => Some(Value::Image(v.as_text())),
        FieldKind::Bool => Some(match v {
            Value::Bool(b) => Value::Bool(*b),
            other => Value::Bool(matches!(other.as_text().as_str(), "true" | "1")),
        }),
        FieldKind::Float => {
            let mut f = v.as_float();
            if let Some(min) = spec.min {
                f = f.max(min);
            }
            if let Some(max) = spec.max {
                f = f.min(max);
            }
            Some(Value::Float(f))
        }
        FieldKind::Enum => {
            let text = v.as_text();
            if spec.values.is_empty() || spec.values.contains(&text) {
                Some(Value::Text(text))
            } else {
                None // invalid enum value → caller falls back to default
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceKind;

    fn raw(fields: &[(&str, Value)]) -> RawNotification {
        RawNotification {
            source: SourceKind::Ipc,
            app_name: "test".into(),
            requested_type: None,
            replace_key: None,
            fields: fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
        }
    }

    const GENERIC: &str = include_str!("../../../config.example/types/generic.toml");
    const SONG: &str = include_str!("../../../config.example/types/song.toml");

    /// An inline type exercising the float-clamp and enum-fallback paths (kept independent of any
    /// shipped type's evolving schema).
    const SCHEMA: &str = r#"
[type]
name = "schema"
priority = 0
timeout_ms = 0
anim_profile = "island_soft"

[fields]
title = { type = "string", required = true }
amount = { type = "float", default = 0.0, min = 0.0, max = 1.0 }
status = { type = "enum", values = ["playing", "paused"], default = "playing" }

[layout]
kind = "leaf"
primitive = "text"
binding = "{title}"
"#;

    fn tmpl(s: &str) -> Arc<TypeTemplate> {
        Arc::new(TypeTemplate::from_toml(s).unwrap())
    }

    #[test]
    fn missing_required_field_errors() {
        let t = tmpl(GENERIC);
        let err = bind(t, raw(&[])).unwrap_err();
        assert!(matches!(err, CoreError::MissingField { .. }));
    }

    #[test]
    fn default_applied_for_absent_optional() {
        let t = tmpl(GENERIC);
        let b = bind(t, raw(&[("title", Value::Text("Hi".into()))])).unwrap();
        // body has default "" → present and empty.
        assert_eq!(b.fields.get("body"), Some(&Value::Text(String::new())));
    }

    #[test]
    fn float_is_clamped() {
        let t = tmpl(SCHEMA);
        let b = bind(
            t,
            raw(&[("title", Value::Text("x".into())), ("amount", Value::Float(5.0))]),
        )
        .unwrap();
        // amount declares min/max 0..1, so 5.0 clamps down to 1.0.
        assert_eq!(b.fields.get("amount"), Some(&Value::Float(1.0)));
    }

    #[test]
    fn invalid_enum_falls_back_to_default() {
        let t = tmpl(SCHEMA);
        let b = bind(
            t,
            raw(&[("title", Value::Text("x".into())), ("status", Value::Text("nope".into()))]),
        )
        .unwrap();
        // "nope" not in [playing, paused] → default "playing".
        assert_eq!(b.fields.get("status"), Some(&Value::Text("playing".into())));
    }

    #[test]
    fn image_field_coerces_to_image_value() {
        let t = tmpl(SONG);
        let b = bind(
            t,
            raw(&[("title", Value::Text("x".into())), ("art", Value::Text("/a.png".into()))]),
        )
        .unwrap();
        assert_eq!(b.fields.get("art"), Some(&Value::Image("/a.png".into())));
    }

    #[test]
    fn replace_key_defaults_from_template() {
        let t = tmpl(SONG);
        let b = bind(t, raw(&[("title", Value::Text("x".into()))])).unwrap();
        assert_eq!(b.replace_key.as_deref(), Some("mpris:single"));
    }
}
