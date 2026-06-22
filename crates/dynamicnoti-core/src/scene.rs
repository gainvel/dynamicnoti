//! The data-driven render model — the heart of dynamicnoti's modularity.
//!
//! A notification TYPE (a TOML file in `~/.config/dynamicnoti/types/`) declares a tree of
//! these primitives with [`Binding`]s. `bind()` validates a source's fields against the
//! type's schema; [`build`] resolves every binding into an immutable [`Scene`] of concrete
//! values. The renderer consumes ONLY [`Scene`] — it never sees TOML, field names, or
//! bindings. Keep the primitive set CLOSED (these six): adding render capability is a
//! deliberate cross-crate change; adding a *notification type* is just a new TOML file.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;

use crate::bind::BoundNotification;
use crate::style::ResolvedStyle;
use crate::template::{LayoutNode, LeafSpec, PrimitiveKind};

/// A dynamically-typed field value carried from a source into a notification.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Text(String),
    Float(f64),
    /// Resolved image handle: a filesystem path or cache key the renderer can load.
    Image(String),
    Bool(bool),
}

impl Value {
    pub fn as_text(&self) -> String {
        match self {
            Value::Text(s) | Value::Image(s) => s.clone(),
            Value::Float(f) => f.to_string(),
            Value::Bool(b) => b.to_string(),
        }
    }
    pub fn as_float(&self) -> f64 {
        match self {
            Value::Float(f) => *f,
            Value::Bool(b) => *b as i32 as f64,
            Value::Text(s) => s.parse().unwrap_or(0.0),
            Value::Image(_) => 0.0,
        }
    }
}

/// How a primitive sources its content. The TOML form is a bare string (or scalar) parsed by
/// a small grammar:
/// - contains `{` → [`Binding::Format`]   e.g. `"{artist} — {title}"`
/// - a bare identifier → [`Binding::FieldRef`]   e.g. `art`, `status`, `position`
/// - a leading `=` (or a number/bool scalar) → [`Binding::Literal`]   e.g. `"=Sale ends"`, `0.5`
///
/// The `=` sigil disambiguates a constant string from a field reference. `\=` escapes a
/// literal that genuinely starts with `=`.
#[derive(Clone, Debug, PartialEq)]
pub enum Binding {
    /// `"{artist} — {title}"` — interpolates `{field}` tokens from the field map.
    Format(String),
    /// A bare field name — resolves to that field's value (text, image path, or enum).
    FieldRef(String),
    /// A constant baked into the type definition.
    Literal(Value),
}

impl Binding {
    /// Apply the grammar to a bare string from TOML.
    fn parse_str(s: &str) -> Binding {
        if let Some(rest) = s.strip_prefix('=') {
            Binding::Literal(Value::Text(rest.to_string()))
        } else if let Some(rest) = s.strip_prefix("\\=") {
            Binding::Literal(Value::Text(format!("={rest}")))
        } else if s.contains('{') {
            Binding::Format(s.to_string())
        } else if is_identifier(s) {
            Binding::FieldRef(s.to_string())
        } else {
            // A non-identifier with no braces and no sigil: treat as a constant string.
            Binding::Literal(Value::Text(s.to_string()))
        }
    }

    /// Resolve to display text. Missing fields resolve to the empty string.
    pub fn resolve_text(&self, values: &HashMap<String, Value>) -> String {
        match self {
            Binding::Literal(v) => v.as_text(),
            Binding::Format(fmt) => interpolate(fmt, values),
            Binding::FieldRef(name) => values.get(name).map(Value::as_text).unwrap_or_default(),
        }
    }

    /// Resolve to a scalar (for [`Primitive::Progress`]). Missing/unparseable → 0.0.
    pub fn resolve_float(&self, values: &HashMap<String, Value>) -> f64 {
        match self {
            Binding::Literal(v) => v.as_float(),
            Binding::FieldRef(name) => values.get(name).map(Value::as_float).unwrap_or(0.0),
            Binding::Format(fmt) => interpolate(fmt, values).parse().unwrap_or(0.0),
        }
    }

    /// Resolve to an image handle. Returns `None` when the referenced field is absent so the
    /// renderer can skip the image leaf entirely (e.g. a notification with no album art).
    pub fn resolve_image(&self, values: &HashMap<String, Value>) -> Option<String> {
        match self {
            Binding::Literal(v) => Some(v.as_text()),
            Binding::FieldRef(name) => values.get(name).map(Value::as_text),
            Binding::Format(fmt) => {
                let s = interpolate(fmt, values);
                (!s.is_empty()).then_some(s)
            }
        }
    }

    /// Borrow the referenced field value, if any (used by `bind`/`build` for enum lookups).
    pub fn resolve_value<'a>(&'a self, values: &'a HashMap<String, Value>) -> Option<Cow<'a, Value>> {
        match self {
            Binding::Literal(v) => Some(Cow::Borrowed(v)),
            Binding::FieldRef(name) => values.get(name).map(Cow::Borrowed),
            Binding::Format(fmt) => Some(Cow::Owned(Value::Text(interpolate(fmt, values)))),
        }
    }
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn interpolate(fmt: &str, values: &HashMap<String, Value>) -> String {
    let mut out = String::with_capacity(fmt.len());
    let mut rest = fmt;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        if let Some(close) = rest[open..].find('}') {
            let key = &rest[open + 1..open + close];
            if let Some(v) = values.get(key) {
                out.push_str(&v.as_text());
            }
            rest = &rest[open + close + 1..];
        } else {
            out.push_str(&rest[open..]);
            return out;
        }
    }
    out.push_str(rest);
    out
}

// `Binding` (de)serializes as a bare TOML scalar so type files read naturally.
impl<'de> Deserialize<'de> for Binding {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = Binding;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a binding string, number, or bool")
            }
            fn visit_str<E>(self, s: &str) -> Result<Binding, E> {
                Ok(Binding::parse_str(s))
            }
            fn visit_f64<E>(self, v: f64) -> Result<Binding, E> {
                Ok(Binding::Literal(Value::Float(v)))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Binding, E> {
                Ok(Binding::Literal(Value::Float(v as f64)))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Binding, E> {
                Ok(Binding::Literal(Value::Float(v as f64)))
            }
            fn visit_bool<E>(self, v: bool) -> Result<Binding, E> {
                Ok(Binding::Literal(Value::Bool(v)))
            }
        }
        d.deserialize_any(V)
    }
}

impl Serialize for Binding {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Binding::Format(f) => s.serialize_str(f),
            Binding::FieldRef(name) => s.serialize_str(name),
            Binding::Literal(Value::Float(f)) => s.serialize_f64(*f),
            Binding::Literal(Value::Bool(b)) => s.serialize_bool(*b),
            // Constant strings are re-emitted with the `=` sigil so they round-trip.
            Binding::Literal(v) => s.serialize_str(&format!("={}", v.as_text())),
        }
    }
}

/// How children are aligned along a container's cross axis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Align {
    Start,
    #[default]
    Center,
    End,
}

/// Concrete container geometry, resolved from a type's optional layout attributes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayoutAttrs {
    /// `[vertical, horizontal]` padding in px.
    pub padding: [f32; 2],
    pub align: Align,
    pub weight: f32,
}

impl Default for LayoutAttrs {
    fn default() -> Self {
        LayoutAttrs { padding: [0.0, 0.0], align: Align::Center, weight: 1.0 }
    }
}

/// What a [`Primitive::Progress`] bar measures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProgressMode {
    /// A static fraction bound from a field (e.g. download progress).
    #[default]
    Value,
    /// The notification's remaining lifetime — the renderer counts it down 1→0 over the
    /// notification's `timeout_ms`, independent of any field. Selected by `value = "lifetime"`.
    Lifetime,
}

/// The CLOSED set of render primitives. Do not extend casually.
#[derive(Clone, Debug, PartialEq)]
pub enum Primitive {
    Text { content: String, style: String },
    Marquee { content: String, style: String, speed_px_s: f32 },
    Image { handle: String, radius: f32 },
    Icon { name: String, style: String },
    Progress { value: f32, mode: ProgressMode, style: String },
    Spacer { size: f32 },
}

/// Layout container kinds. A `Scene` is a tree of these with `Leaf` primitives. Containers
/// carry resolved [`LayoutAttrs`] so the renderer has real geometry (the headless renderer
/// ignores them).
#[derive(Clone, Debug)]
pub enum Scene {
    Row { attrs: LayoutAttrs, children: Vec<Scene> },
    Column { attrs: LayoutAttrs, children: Vec<Scene> },
    Stack { attrs: LayoutAttrs, children: Vec<Scene> },
    Leaf(Primitive),
}

impl Scene {
    /// True when `self` and `other` have identical structure and content, differing *at most* in
    /// a [`Primitive::Progress`] bar's scalar `value`. The renderer uses this to update a live
    /// notification IN PLACE instead of crossfade-morphing it: a media card emits a fresh scene
    /// on every position tick (a few times a second), but only the elapsed bar moved — so slide
    /// the bar, don't refade the whole card. Any other change (title, artist, album art, icon,
    /// marquee text, layout) is a real content change and returns false → a proper morph.
    pub fn same_shape(&self, other: &Scene) -> bool {
        match (self, other) {
            (Scene::Row { attrs: a, children: ca }, Scene::Row { attrs: b, children: cb })
            | (Scene::Column { attrs: a, children: ca }, Scene::Column { attrs: b, children: cb })
            | (Scene::Stack { attrs: a, children: ca }, Scene::Stack { attrs: b, children: cb }) => {
                a == b && ca.len() == cb.len() && ca.iter().zip(cb).all(|(x, y)| x.same_shape(y))
            }
            (Scene::Leaf(a), Scene::Leaf(b)) => primitive_same_shape(a, b),
            _ => false,
        }
    }
}

/// Helper for [`Scene::same_shape`]: two primitives match when only a `Progress` value differs.
fn primitive_same_shape(a: &Primitive, b: &Primitive) -> bool {
    match (a, b) {
        (
            Primitive::Progress { style: sa, mode: ma, .. },
            Primitive::Progress { style: sb, mode: mb, .. },
        ) => sa == sb && ma == mb,
        _ => a == b,
    }
}

const DEFAULT_MARQUEE_SPEED: f32 = 30.0;

/// Walk a bound notification's layout tree, resolving every binding into concrete
/// [`Primitive`]s. Panic-free by construction — a missing field renders empty, a missing
/// optional image leaf is dropped. `style` is currently threaded through for future
/// per-primitive styling; the headless renderer ignores it.
pub fn build(bound: &BoundNotification, _style: &ResolvedStyle) -> Scene {
    build_node(&bound.template.layout, &bound.fields)
        .unwrap_or(Scene::Row { attrs: LayoutAttrs::default(), children: Vec::new() })
}

fn build_node(node: &LayoutNode, fields: &HashMap<String, Value>) -> Option<Scene> {
    match node {
        LayoutNode::Row { common, children } => Some(Scene::Row {
            attrs: common.resolve(),
            children: build_children(children, fields),
        }),
        LayoutNode::Column { common, children } => Some(Scene::Column {
            attrs: common.resolve(),
            children: build_children(children, fields),
        }),
        LayoutNode::Stack { common, children } => Some(Scene::Stack {
            attrs: common.resolve(),
            children: build_children(children, fields),
        }),
        LayoutNode::Leaf { leaf } => build_leaf(leaf, fields).map(Scene::Leaf),
    }
}

fn build_children(children: &[LayoutNode], fields: &HashMap<String, Value>) -> Vec<Scene> {
    children.iter().filter_map(|c| build_node(c, fields)).collect()
}

fn build_leaf(leaf: &LeafSpec, fields: &HashMap<String, Value>) -> Option<Primitive> {
    let style = leaf.style.clone().unwrap_or_default();
    Some(match leaf.primitive {
        PrimitiveKind::Text => {
            let content = leaf.binding.as_ref().map(|b| b.resolve_text(fields)).unwrap_or_default();
            Primitive::Text { content, style }
        }
        PrimitiveKind::Marquee => {
            let content = leaf.binding.as_ref().map(|b| b.resolve_text(fields)).unwrap_or_default();
            let speed_px_s = leaf.speed_px_s.map(|f| f.0).unwrap_or(DEFAULT_MARQUEE_SPEED);
            Primitive::Marquee { content, style, speed_px_s }
        }
        PrimitiveKind::Image => {
            // Drop the leaf entirely when the bound notification has no image for it.
            let handle = leaf.binding.as_ref().and_then(|b| b.resolve_image(fields))?;
            let radius = leaf.radius.map(|f| f.0).unwrap_or(0.0);
            Primitive::Image { handle, radius }
        }
        PrimitiveKind::Icon => {
            let raw = leaf.binding.as_ref().map(|b| b.resolve_text(fields)).unwrap_or_default();
            // Map a known enum value (e.g. play/pause status) to a glyph; otherwise pass the
            // name through as a freedesktop icon name.
            let name = enum_icon(&raw).unwrap_or(&raw).to_string();
            Primitive::Icon { name, style }
        }
        PrimitiveKind::Progress => {
            let src = leaf.value.as_ref().or(leaf.binding.as_ref());
            // `value = "lifetime"` is a reserved source: the bar tracks the notification's
            // remaining lifetime (filled by the renderer from the clock), not a field.
            if matches!(src, Some(Binding::FieldRef(n)) if n == "lifetime") {
                Primitive::Progress { value: 1.0, mode: ProgressMode::Lifetime, style }
            } else {
                let value =
                    src.map(|b| b.resolve_float(fields)).unwrap_or(0.0).clamp(0.0, 1.0) as f32;
                Primitive::Progress { value, mode: ProgressMode::Value, style }
            }
        }
        PrimitiveKind::Spacer => Primitive::Spacer { size: leaf.size.map(|f| f.0).unwrap_or(0.0) },
    })
}

/// Map a known status/enum value to a glyph. Returns `None` for unknown values so the caller
/// falls back to treating the binding as an icon name.
fn enum_icon(value: &str) -> Option<&'static str> {
    match value {
        "playing" => Some("\u{f04b}"), // play
        "paused" => Some("\u{f04c}"),  // pause
        "stopped" => Some("\u{f04d}"), // stop
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_binding_interpolates_fields() {
        let mut values = HashMap::new();
        values.insert("artist".into(), Value::Text("Boards of Canada".into()));
        values.insert("title".into(), Value::Text("Roygbiv".into()));
        let b = Binding::Format("{artist} — {title}".into());
        assert_eq!(b.resolve_text(&values), "Boards of Canada — Roygbiv");
    }

    #[test]
    fn missing_field_resolves_empty() {
        let values = HashMap::new();
        let b = Binding::Format("{nope}!".into());
        assert_eq!(b.resolve_text(&values), "!");
    }

    #[test]
    fn binding_grammar_classifies() {
        assert_eq!(Binding::parse_str("{a} {b}"), Binding::Format("{a} {b}".into()));
        assert_eq!(Binding::parse_str("art"), Binding::FieldRef("art".into()));
        assert_eq!(Binding::parse_str("=Sale"), Binding::Literal(Value::Text("Sale".into())));
        assert_eq!(
            Binding::parse_str("two words"),
            Binding::Literal(Value::Text("two words".into()))
        );
    }

    fn card(title: &str, progress: f32) -> Scene {
        Scene::Row {
            attrs: LayoutAttrs::default(),
            children: vec![
                Scene::Leaf(Primitive::Text { content: title.into(), style: "title".into() }),
                Scene::Leaf(Primitive::Progress {
                    value: progress,
                    mode: ProgressMode::Value,
                    style: "bar".into(),
                }),
            ],
        }
    }

    #[test]
    fn same_shape_ignores_progress_value_only() {
        // Only the progress value moved → same shape (update in place, no morph).
        assert!(card("Roygbiv", 0.10).same_shape(&card("Roygbiv", 0.42)));
        // Title changed → different shape (a real morph).
        assert!(!card("Roygbiv", 0.10).same_shape(&card("Telephasic", 0.10)));
        // Structural difference (child count) → different shape.
        let bare = Scene::Row { attrs: LayoutAttrs::default(), children: vec![] };
        assert!(!card("Roygbiv", 0.1).same_shape(&bare));
    }

    #[test]
    fn lifetime_value_binding_sets_mode() {
        use crate::template::{LeafSpec, PrimitiveKind};
        let progress_leaf = |value: Option<Binding>| LeafSpec {
            primitive: PrimitiveKind::Progress,
            binding: None,
            value,
            style: Some("bar".into()),
            speed_px_s: None,
            radius: None,
            fit: None,
            size: None,
        };
        let fields = HashMap::new();
        assert!(matches!(
            build_leaf(&progress_leaf(Some(Binding::FieldRef("lifetime".into()))), &fields),
            Some(Primitive::Progress { mode: ProgressMode::Lifetime, .. })
        ));
        // A normal float source stays a Value bar.
        match build_leaf(&progress_leaf(Some(Binding::Literal(Value::Float(0.5)))), &fields) {
            Some(Primitive::Progress { mode: ProgressMode::Value, value, .. }) => {
                assert!((value - 0.5).abs() < 1e-6)
            }
            other => panic!("expected a Value progress, got {other:?}"),
        }
    }

    #[test]
    fn same_shape_distinguishes_progress_mode() {
        let value_bar = Scene::Leaf(Primitive::Progress {
            value: 1.0,
            mode: ProgressMode::Value,
            style: "bar".into(),
        });
        let lifetime_bar = Scene::Leaf(Primitive::Progress {
            value: 1.0,
            mode: ProgressMode::Lifetime,
            style: "bar".into(),
        });
        assert!(!value_bar.same_shape(&lifetime_bar));
    }

    #[test]
    fn fieldref_resolves_image_or_none() {
        let mut values = HashMap::new();
        let b = Binding::FieldRef("art".into());
        assert_eq!(b.resolve_image(&values), None);
        values.insert("art".into(), Value::Image("/tmp/cover.png".into()));
        assert_eq!(b.resolve_image(&values), Some("/tmp/cover.png".into()));
    }
}
