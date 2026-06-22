//! Introspection — machine-readable schema descriptors for the future config TUI.
//!
//! Two layers: per-field metadata derived from a type's [`FieldSpec`]s (so the TUI can render
//! a form for any user type), and static descriptor tables for the theme/config/overrides
//! forms. Keeping this in `core` means the daemon and the TUI share one source of truth.

use serde::Serialize;

use crate::template::{FieldKind, FieldSpec, TypeTemplate};

/// A widget hint for one field, with the constraints the TUI needs to render an input.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "widget", rename_all = "snake_case")]
pub enum FieldWidget {
    Text { default: Option<String> },
    Float { min: Option<f64>, max: Option<f64>, default: Option<f64> },
    Enum { values: Vec<String>, default: Option<String> },
    Bool { default: Option<bool> },
    Image { default: Option<String> },
    Icon { default: Option<String> },
}

/// Full metadata for one schema field.
#[derive(Clone, Debug, Serialize)]
pub struct FieldMeta {
    pub name: String,
    pub required: bool,
    pub doc: Option<String>,
    pub widget: FieldWidget,
}

impl FieldSpec {
    pub fn meta(&self, name: &str) -> FieldMeta {
        let default_text = self.default.as_ref().map(|v| v.as_text());
        let default_float = self.default.as_ref().map(|v| v.as_float());
        let default_bool = self.default.as_ref().map(|v| v.as_float() != 0.0);
        let widget = match self.kind {
            FieldKind::String => FieldWidget::Text { default: default_text },
            FieldKind::Float => FieldWidget::Float {
                min: self.min,
                max: self.max,
                default: default_float,
            },
            FieldKind::Enum => FieldWidget::Enum {
                values: self.values.clone(),
                default: default_text,
            },
            FieldKind::Bool => FieldWidget::Bool { default: default_bool },
            FieldKind::Image => FieldWidget::Image { default: default_text },
            FieldKind::Icon => FieldWidget::Icon { default: default_text },
        };
        FieldMeta { name: name.to_string(), required: self.required, doc: self.doc.clone(), widget }
    }
}

impl TypeTemplate {
    /// Field metadata in declaration order (preserved via `IndexMap`).
    pub fn field_metas(&self) -> Vec<FieldMeta> {
        self.fields.iter().map(|(name, spec)| spec.meta(name)).collect()
    }
}

/// The kind of a settings field, for the theme/config/overrides forms.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SettingKind {
    Bool,
    Int { min: Option<i64>, max: Option<i64> },
    Float { min: Option<f64>, max: Option<f64> },
    Color,
    Font,
    Enum { values: Vec<&'static str> },
    String,
}

/// One descriptor row for a settings form.
#[derive(Clone, Debug, Serialize)]
pub struct SettingField {
    /// Dotted path into the struct, e.g. `island.corner_radius`.
    pub path: &'static str,
    pub kind: SettingKind,
    pub default: serde_json::Value,
    pub doc: &'static str,
}

/// Descriptors for `theme.toml` form rendering.
pub fn theme_schema() -> Vec<SettingField> {
    use serde_json::json;
    vec![
        SettingField { path: "island.anchor", kind: SettingKind::Enum { values: vec!["top"] }, default: json!("top"), doc: "Island anchor edge (top = centered)." },
        SettingField { path: "island.margin_top", kind: SettingKind::Int { min: Some(0), max: Some(400) }, default: json!(12), doc: "Gap below the screen edge, px." },
        SettingField { path: "island.min_width", kind: SettingKind::Int { min: Some(100), max: Some(2000) }, default: json!(360), doc: "Minimum island width, px." },
        SettingField { path: "island.max_width", kind: SettingKind::Int { min: Some(100), max: Some(2000) }, default: json!(520), doc: "Maximum island width, px." },
        SettingField { path: "island.height", kind: SettingKind::Int { min: Some(24), max: Some(400) }, default: json!(64), doc: "Island height, px." },
        SettingField { path: "island.corner_radius", kind: SettingKind::Float { min: Some(0.0), max: Some(200.0) }, default: json!(28.0), doc: "Corner radius, px." },
        SettingField { path: "island.background", kind: SettingKind::Color, default: json!("#0a0a0bf2"), doc: "Background fill (RRGGBBAA — alpha = transparency)." },
        SettingField { path: "island.blur", kind: SettingKind::Bool, default: json!(false), doc: "Backdrop blur behind the island." },
        SettingField { path: "fonts.ui", kind: SettingKind::Font, default: json!("Noto Sans"), doc: "UI font family." },
        SettingField { path: "fonts.title_px", kind: SettingKind::Float { min: Some(6.0), max: Some(72.0) }, default: json!(15.0), doc: "Title font size, px." },
        SettingField { path: "fonts.subtitle_px", kind: SettingKind::Float { min: Some(6.0), max: Some(72.0) }, default: json!(12.0), doc: "Subtitle font size, px." },
        SettingField { path: "colors.title", kind: SettingKind::Color, default: json!("#ffffffff"), doc: "Title text color." },
        SettingField { path: "colors.subtitle", kind: SettingKind::Color, default: json!("#b8b8beff"), doc: "Subtitle text color." },
        SettingField { path: "colors.accent", kind: SettingKind::Color, default: json!("#ffffffff"), doc: "Accent (progress bar fill)." },
        SettingField { path: "colors.icon", kind: SettingKind::Color, default: json!("#e6e6eaff"), doc: "Icon tint." },
    ]
}

/// Descriptors for `config.toml` form rendering.
pub fn config_schema() -> Vec<SettingField> {
    use serde_json::json;
    vec![
        SettingField { path: "monitor", kind: SettingKind::String, default: json!("auto"), doc: "Output to show on (\"auto\" or a connector like DP-1)." },
        SettingField { path: "log_level", kind: SettingKind::Enum { values: vec!["error", "warn", "info", "debug", "trace"] }, default: json!("info"), doc: "Log verbosity." },
        SettingField { path: "queue.policy", kind: SettingKind::Enum { values: vec!["priority-preempt", "fifo"] }, default: json!("priority-preempt"), doc: "How competing notifications share the island." },
        SettingField { path: "queue.max_visible", kind: SettingKind::Int { min: Some(1), max: Some(1) }, default: json!(1), doc: "Concurrent surfaces (1 = single island)." },
        SettingField { path: "queue.coalesce_replace", kind: SettingKind::Bool, default: json!(true), doc: "Collapse same-replace_key updates onto one surface." },
        SettingField { path: "sources.mpris.debounce_ms", kind: SettingKind::Int { min: Some(0), max: Some(5000) }, default: json!(250), doc: "Debounce MPRIS position spam, ms." },
    ]
}

/// Descriptors for the fields a type's `[overrides]` may set (a subset of theme + behavior).
pub fn overrides_schema() -> Vec<SettingField> {
    use serde_json::json;
    vec![
        SettingField { path: "min_width", kind: SettingKind::Int { min: Some(100), max: Some(2000) }, default: json!(null), doc: "Override min width, px." },
        SettingField { path: "max_width", kind: SettingKind::Int { min: Some(100), max: Some(2000) }, default: json!(null), doc: "Override max width, px." },
        SettingField { path: "height", kind: SettingKind::Int { min: Some(24), max: Some(400) }, default: json!(null), doc: "Override height, px." },
        SettingField { path: "corner_radius", kind: SettingKind::Float { min: Some(0.0), max: Some(200.0) }, default: json!(null), doc: "Override corner radius, px." },
        SettingField { path: "background", kind: SettingKind::Color, default: json!(null), doc: "Override background (RRGGBBAA — transparency)." },
        SettingField { path: "blur", kind: SettingKind::Bool, default: json!(null), doc: "Override backdrop blur." },
        SettingField { path: "timeout_ms", kind: SettingKind::Int { min: Some(0), max: Some(120_000) }, default: json!(null), doc: "Override timeout (0 = sticky), ms." },
        SettingField { path: "priority", kind: SettingKind::Int { min: Some(0), max: Some(100) }, default: json!(null), doc: "Override priority." },
        SettingField { path: "anim_profile", kind: SettingKind::String, default: json!(null), doc: "Override anim profile name." },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inline type covering ordered fields + an enum widget, independent of any shipped type.
    const SCHEMA: &str = r#"
[type]
name = "schema"
priority = 0
timeout_ms = 0
anim_profile = "island_soft"

[fields]
title = { type = "string", required = true }
status = { type = "enum", values = ["playing", "paused"], default = "playing" }

[layout]
kind = "leaf"
primitive = "text"
binding = "{title}"
"#;

    #[test]
    fn field_metas_preserve_order_and_kind() {
        let t = TypeTemplate::from_toml(SCHEMA).unwrap();
        let metas = t.field_metas();
        // title is declared first.
        assert_eq!(metas[0].name, "title");
        assert!(metas[0].required);
        // status is the enum field.
        let status = metas.iter().find(|m| m.name == "status").unwrap();
        assert!(matches!(status.widget, FieldWidget::Enum { .. }));
    }

    #[test]
    fn schemas_are_nonempty_and_serializable() {
        assert!(!theme_schema().is_empty());
        assert!(!config_schema().is_empty());
        assert!(!overrides_schema().is_empty());
        // Must serialize for the TUI to consume over its own channel.
        serde_json::to_string(&theme_schema()).unwrap();
    }
}
