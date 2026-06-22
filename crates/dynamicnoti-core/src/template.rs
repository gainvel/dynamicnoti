//! Notification type templates — the schema loaded from `types/<name>.toml`.
//!
//! A template declares its `[type]` metadata, a `[fields]` schema (validated by
//! [`crate::bind`]), a `[layout]` primitive tree, and optional `[overrides]` that layer over
//! the theme. Adding a type is just dropping a TOML here — no recompile.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::scene::{Align, Binding, LayoutAttrs, Value};
use crate::style::StyleOverrides;

/// A number field that tolerates both TOML integers and floats (e.g. `padding = [10, 14]`
/// and `radius = 8.0`). TOML does not coerce int↔float, so we accept both explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct F32(pub f32);

impl<'de> Deserialize<'de> for F32 {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = F32;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a number")
            }
            fn visit_f64<E>(self, v: f64) -> Result<F32, E> {
                Ok(F32(v as f32))
            }
            fn visit_i64<E>(self, v: i64) -> Result<F32, E> {
                Ok(F32(v as f32))
            }
            fn visit_u64<E>(self, v: u64) -> Result<F32, E> {
                Ok(F32(v as f32))
            }
        }
        d.deserialize_any(V)
    }
}

fn default_anim_profile() -> String {
    "island_soft".to_string()
}

/// The `[type]` header.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TypeMeta {
    pub name: String,
    #[serde(default)]
    pub priority: i32,
    /// 0 = sticky (closed explicitly, e.g. a song while playing).
    #[serde(default)]
    pub timeout_ms: u32,
    #[serde(default = "default_anim_profile")]
    pub anim_profile: String,
    /// Collapses updates onto one live surface (e.g. all MPRIS updates share one key).
    #[serde(default)]
    pub replace_key: Option<String>,
}

/// The kind of a schema field. Drives coercion/validation in [`crate::bind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    String,
    Float,
    Image,
    Icon,
    Enum,
    Bool,
}

/// One declared field in a type's `[fields]` schema.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FieldSpec {
    #[serde(rename = "type")]
    pub kind: FieldKind,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<Value>,
    /// Allowed set for `enum` fields.
    #[serde(default)]
    pub values: Vec<String>,
    /// Inclusive clamp bounds for `float` fields.
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    /// Human description, surfaced to the config TUI.
    #[serde(default)]
    pub doc: Option<String>,
}

/// One of the six render primitives a leaf can be.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrimitiveKind {
    Text,
    Marquee,
    Image,
    Icon,
    Progress,
    Spacer,
}

/// How an image fills its box.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFit {
    Cover,
    Contain,
}

/// Attributes shared by every container node. All optional → inherits a sensible default.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ContainerCommon {
    #[serde(default)]
    pub padding: Option<[F32; 2]>,
    #[serde(default)]
    pub align: Option<Align>,
    #[serde(default)]
    pub weight: Option<F32>,
}

impl ContainerCommon {
    /// Resolve to concrete [`LayoutAttrs`], filling unset fields with defaults.
    pub fn resolve(&self) -> LayoutAttrs {
        let d = LayoutAttrs::default();
        LayoutAttrs {
            padding: self.padding.map(|[v, h]| [v.0, h.0]).unwrap_or(d.padding),
            align: self.align.unwrap_or(d.align),
            weight: self.weight.map(|w| w.0).unwrap_or(d.weight),
        }
    }
}

/// A single leaf: one primitive plus how it sources its content and its visual extras.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LeafSpec {
    pub primitive: PrimitiveKind,
    /// Content binding (text/marquee/image/icon). Progress reads [`LeafSpec::value`] instead.
    #[serde(default)]
    pub binding: Option<Binding>,
    /// Scalar binding for [`PrimitiveKind::Progress`].
    #[serde(default)]
    pub value: Option<Binding>,
    /// Named style (color/font role) resolved by the renderer.
    #[serde(default)]
    pub style: Option<String>,
    #[serde(default)]
    pub speed_px_s: Option<F32>,
    #[serde(default)]
    pub radius: Option<F32>,
    #[serde(default)]
    pub fit: Option<ImageFit>,
    #[serde(default)]
    pub size: Option<F32>,
}

/// The recursive layout tree. Internally tagged on `kind`; container nodes flatten their
/// shared attributes alongside their `children`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LayoutNode {
    Row {
        #[serde(flatten)]
        common: ContainerCommon,
        #[serde(default)]
        children: Vec<LayoutNode>,
    },
    Column {
        #[serde(flatten)]
        common: ContainerCommon,
        #[serde(default)]
        children: Vec<LayoutNode>,
    },
    Stack {
        #[serde(flatten)]
        common: ContainerCommon,
        #[serde(default)]
        children: Vec<LayoutNode>,
    },
    Leaf {
        #[serde(flatten)]
        leaf: LeafSpec,
    },
}

/// A full notification type, parsed from one `types/<name>.toml`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TypeTemplate {
    #[serde(rename = "type")]
    pub meta: TypeMeta,
    /// Declaration order preserved (via `IndexMap`) so the config TUI lays out fields as authored.
    #[serde(default)]
    pub fields: IndexMap<String, FieldSpec>,
    pub layout: LayoutNode,
    /// Optional per-type theme overrides (`[overrides]`).
    #[serde(default)]
    pub overrides: Option<StyleOverrides>,
}

impl TypeTemplate {
    /// Parse a template from a TOML string.
    pub fn from_toml(s: &str) -> Result<TypeTemplate, toml::de::Error> {
        toml::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GENERIC: &str = include_str!("../../../config.example/types/generic.toml");
    const SONG: &str = include_str!("../../../config.example/types/song.toml");
    const DEAL: &str = include_str!("../../../config.example/types/deal.toml");

    #[test]
    fn generic_template_parses() {
        let t = TypeTemplate::from_toml(GENERIC).expect("generic.toml parses");
        assert_eq!(t.meta.name, "generic");
        assert_eq!(t.meta.priority, 20);
        assert!(t.fields.contains_key("title"));
        assert!(t.fields["title"].required);
    }

    #[test]
    fn song_template_parses_with_bindings() {
        let t = TypeTemplate::from_toml(SONG).expect("song.toml parses");
        assert_eq!(t.meta.name, "song");
        assert_eq!(t.meta.timeout_ms, 4000); // auto-dismisses; the lifetime bar counts it down
        assert_eq!(t.meta.replace_key.as_deref(), Some("mpris:single"));
        assert!(t.fields.contains_key("title"));
    }

    #[test]
    fn deal_template_parses() {
        let t = TypeTemplate::from_toml(DEAL).expect("deal.toml parses");
        assert_eq!(t.meta.name, "deal");
        assert_eq!(t.meta.anim_profile, "alert");
    }

    #[test]
    fn song_binding_forms_are_distinct() {
        let t = TypeTemplate::from_toml(SONG).unwrap();
        // Walk to find the image leaf (binding = "art" → FieldRef) and a Format marquee.
        let mut saw_fieldref = false;
        let mut saw_format = false;
        let mut saw_value = false;
        walk(&t.layout, &mut |leaf| {
            if let Some(Binding::FieldRef(n)) = &leaf.binding {
                if n == "art" {
                    saw_fieldref = true;
                }
            }
            if matches!(&leaf.binding, Some(Binding::Format(_))) {
                saw_format = true;
            }
            if matches!(&leaf.value, Some(Binding::FieldRef(n)) if n == "lifetime") {
                saw_value = true;
            }
        });
        assert!(saw_fieldref, "expected `binding = \"art\"` → FieldRef");
        assert!(saw_format, "expected a Format marquee/text binding");
        assert!(saw_value, "expected `value = \"lifetime\"` → FieldRef");
    }

    fn walk(node: &LayoutNode, f: &mut impl FnMut(&LeafSpec)) {
        match node {
            LayoutNode::Leaf { leaf } => f(leaf),
            LayoutNode::Row { children, .. }
            | LayoutNode::Column { children, .. }
            | LayoutNode::Stack { children, .. } => {
                for c in children {
                    walk(c, f);
                }
            }
        }
    }
}
