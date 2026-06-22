//! Theme — visual styling and animation presets loaded from `theme.toml`.
//!
//! A [`Theme`] holds fully-concrete values. A type's optional [`crate::style::StyleOverrides`]
//! layer over it (see [`crate::style`]) to produce the per-notification resolved style.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// An 8-bit RGBA color. Parses `#RRGGBB` (opaque) and `#RRGGBBAA`; serializes to lowercase
/// `#rrggbbaa` so the config TUI round-trips losslessly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Color { r, g, b, a }
    }

    pub fn from_hex(s: &str) -> Result<Color, String> {
        let h = s.strip_prefix('#').unwrap_or(s);
        let byte = |i: usize| -> Result<u8, String> {
            u8::from_str_radix(&h[i..i + 2], 16).map_err(|e| format!("bad hex in '{s}': {e}"))
        };
        match h.len() {
            6 => Ok(Color { r: byte(0)?, g: byte(2)?, b: byte(4)?, a: 255 }),
            8 => Ok(Color { r: byte(0)?, g: byte(2)?, b: byte(4)?, a: byte(6)? }),
            _ => Err(format!("color '{s}' must be #RRGGBB or #RRGGBBAA")),
        }
    }

    pub fn to_hex(&self) -> String {
        format!("#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
    }
}

impl Serialize for Color {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Color::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// The surface "finish" — a subtle depth treatment painted over the flat fill. Theme-driven so
/// the look can be swapped without recompiling (the renderer reads it as plain data).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SurfaceFinish {
    /// Flat fill, no sheen.
    None,
    /// A soft top→bottom brightness ramp across the whole height.
    Gradient,
    /// A glossy specular band concentrated near the top edge (the default Dynamic-Island look).
    #[default]
    Glossy,
}

/// Where the island sits and how big/round/translucent it is.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IslandTheme {
    #[serde(default = "default_anchor")]
    pub anchor: String,
    #[serde(default = "d12")]
    pub margin_top: u32,
    #[serde(default = "d360")]
    pub min_width: u32,
    #[serde(default = "d520")]
    pub max_width: u32,
    #[serde(default = "d64")]
    pub height: u32,
    #[serde(default = "d28f")]
    pub corner_radius: f32,
    #[serde(default = "default_bg")]
    pub background: Color,
    #[serde(default)]
    pub blur: bool,
    /// Drop-shadow color (incl. alpha). `None` disables the shadow entirely.
    #[serde(default)]
    pub shadow: Option<Color>,
    /// Soft-shadow blur falloff in px (how far the shadow feathers out beyond the pill).
    #[serde(default = "d24f")]
    pub shadow_radius: f32,
    /// Downward bias of the shadow in px (a real drop shadow sits slightly below).
    #[serde(default = "d8f")]
    pub shadow_offset_y: f32,
    /// Extra solid size added around the pill before the blur falloff begins.
    #[serde(default = "d2f")]
    pub shadow_spread: f32,
    /// Which depth treatment to paint over the fill.
    #[serde(default)]
    pub finish: SurfaceFinish,
    /// Sheen strength, 0 (off) … 255 (max).
    #[serde(default = "d22u8")]
    pub finish_intensity: u8,
    /// Sheen tint (usually white).
    #[serde(default = "white")]
    pub finish_color: Color,
}

impl Default for IslandTheme {
    fn default() -> Self {
        IslandTheme {
            anchor: default_anchor(),
            margin_top: 12,
            min_width: 360,
            max_width: 520,
            height: 64,
            corner_radius: 28.0,
            background: default_bg(),
            blur: false,
            shadow: None,
            shadow_radius: 24.0,
            shadow_offset_y: 8.0,
            shadow_spread: 2.0,
            finish: SurfaceFinish::default(),
            finish_intensity: 22,
            finish_color: white(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fonts {
    #[serde(default = "default_font")]
    pub ui: String,
    #[serde(default = "d15f")]
    pub title_px: f32,
    #[serde(default = "d12f")]
    pub subtitle_px: f32,
}

impl Default for Fonts {
    fn default() -> Self {
        Fonts { ui: default_font(), title_px: 15.0, subtitle_px: 12.0 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Colors {
    #[serde(default = "white")]
    pub title: Color,
    #[serde(default = "grey")]
    pub subtitle: Color,
    #[serde(default = "blue")]
    pub accent: Color,
    #[serde(default = "white")]
    pub icon: Color,
}

impl Default for Colors {
    fn default() -> Self {
        Colors { title: white(), subtitle: grey(), accent: blue(), icon: white() }
    }
}

/// One named spring preset. Mirrors `dynamicnoti_anim::SpringParams` but lives in core so the
/// theme stays pure; the daemon converts it to the anim type when handing springs to render.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpringPreset {
    pub stiffness: f32,
    pub damping: f32,
    #[serde(default = "one_f32")]
    pub mass: f32,
    #[serde(default = "default_rest_eps")]
    pub rest_eps: f32,
}

/// An anim profile: which named spring preset drives each animated property.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnimProfileRef {
    #[serde(default = "island_soft_name")]
    pub geometry: String,
    #[serde(default = "island_soft_name")]
    pub opacity: String,
    #[serde(default = "island_soft_name")]
    pub scale: String,
    #[serde(default = "island_soft_name")]
    pub crossfade: String,
    /// Drives the slide-from-top Enter/Exit travel (the springy island drop). Defaults to the
    /// bouncy `island_slide` preset; older themes without it degrade to `island_soft`.
    #[serde(default = "island_slide_name")]
    pub translate_y: String,
}

/// The full theme.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Theme {
    #[serde(default)]
    pub island: IslandTheme,
    #[serde(default)]
    pub fonts: Fonts,
    #[serde(default)]
    pub colors: Colors,
    #[serde(default)]
    pub springs: HashMap<String, SpringPreset>,
    #[serde(default)]
    pub anim_profiles: HashMap<String, AnimProfileRef>,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            island: IslandTheme::default(),
            fonts: Fonts::default(),
            colors: Colors::default(),
            springs: default_springs(),
            anim_profiles: default_anim_profiles(),
        }
    }
}

impl Theme {
    pub fn from_toml(s: &str) -> Result<Theme, toml::de::Error> {
        toml::from_str(s)
    }

    /// Look up a spring preset by name, falling back to `island_soft`'s parameters.
    pub fn spring(&self, name: &str) -> SpringPreset {
        self.springs
            .get(name)
            .copied()
            .unwrap_or(SpringPreset { stiffness: 170.0, damping: 26.0, mass: 1.0, rest_eps: 0.01 })
    }
}

fn default_springs() -> HashMap<String, SpringPreset> {
    let mut m = HashMap::new();
    m.insert(
        "island_soft".into(),
        SpringPreset { stiffness: 170.0, damping: 26.0, mass: 1.0, rest_eps: 0.01 },
    );
    m.insert(
        "snappy".into(),
        SpringPreset { stiffness: 320.0, damping: 30.0, mass: 1.0, rest_eps: 0.01 },
    );
    m.insert(
        "gentle".into(),
        SpringPreset { stiffness: 120.0, damping: 20.0, mass: 1.0, rest_eps: 0.01 },
    );
    // Under-damped (damping well below critical ~2*sqrt(210)≈29) → a springy, overshooting slide.
    m.insert(
        "island_slide".into(),
        SpringPreset { stiffness: 210.0, damping: 18.0, mass: 1.0, rest_eps: 0.01 },
    );
    m
}

fn default_anim_profiles() -> HashMap<String, AnimProfileRef> {
    let mut m = HashMap::new();
    m.insert(
        "island_soft".into(),
        AnimProfileRef {
            geometry: "island_soft".into(),
            opacity: "gentle".into(),
            scale: "island_soft".into(),
            crossfade: "snappy".into(),
            translate_y: "island_slide".into(),
        },
    );
    m
}

fn default_anchor() -> String {
    "top".into()
}
fn default_font() -> String {
    "Noto Sans".into()
}
fn island_soft_name() -> String {
    "island_soft".into()
}
fn island_slide_name() -> String {
    "island_slide".into()
}
fn default_bg() -> Color {
    Color::rgba(0x0A, 0x0A, 0x0B, 0xF2)
}
fn white() -> Color {
    Color::rgba(0xFF, 0xFF, 0xFF, 0xFF)
}
fn grey() -> Color {
    Color::rgba(0xB8, 0xB8, 0xBE, 0xFF)
}
fn blue() -> Color {
    Color::rgba(0x5E, 0x9E, 0xFF, 0xFF)
}
fn one_f32() -> f32 {
    1.0
}
fn default_rest_eps() -> f32 {
    0.01
}
fn d12() -> u32 {
    12
}
fn d360() -> u32 {
    360
}
fn d520() -> u32 {
    520
}
fn d64() -> u32 {
    64
}
fn d28f() -> f32 {
    28.0
}
fn d24f() -> f32 {
    24.0
}
fn d8f() -> f32 {
    8.0
}
fn d2f() -> f32 {
    2.0
}
fn d22u8() -> u8 {
    22
}
fn d15f() -> f32 {
    15.0
}
fn d12f() -> f32 {
    12.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const THEME: &str = include_str!("../../../config.example/theme.toml");

    #[test]
    fn example_theme_parses() {
        let t = Theme::from_toml(THEME).expect("theme.toml parses");
        assert_eq!(t.island.corner_radius, 12.0);
        assert_eq!(t.island.background, Color::rgba(0x0A, 0x0A, 0x0B, 0xF2));
        assert!(t.springs.contains_key("snappy"));
        assert!(t.springs.contains_key("island_slide"));
        assert!(t.anim_profiles.contains_key("alert"));
        assert_eq!(t.island.finish, SurfaceFinish::Glossy);
        assert_eq!(t.anim_profiles["island_soft"].translate_y, "island_slide");
    }

    #[test]
    fn surface_finish_parses() {
        let t: Theme = toml::from_str("[island]\nfinish = \"gradient\"\n").unwrap();
        assert_eq!(t.island.finish, SurfaceFinish::Gradient);
        // Omitted finish defaults to Glossy.
        let d: Theme = toml::from_str("").unwrap();
        assert_eq!(d.island.finish, SurfaceFinish::Glossy);
    }

    #[test]
    fn color_roundtrips() {
        let c = Color::from_hex("#0A0A0Bf2").unwrap();
        assert_eq!(c.to_hex(), "#0a0a0bf2");
        // 6-digit form gets full alpha.
        assert_eq!(Color::from_hex("#5E9EFF").unwrap().a, 255);
        assert!(Color::from_hex("#xyz").is_err());
    }

    #[test]
    fn default_theme_is_complete() {
        let t = Theme::default();
        assert!(t.springs.contains_key("island_soft"));
    }
}
