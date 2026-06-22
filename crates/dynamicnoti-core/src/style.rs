//! Per-type style overrides and the merge that layers them over the [`Theme`].
//!
//! Every field in [`StyleOverrides`] is optional: `None` means "inherit from the theme".
//! [`Theme::resolve_style`] and [`Theme::resolve_anim`] collapse the theme + a type's
//! overrides into the fully-concrete [`ResolvedStyle`] / [`ResolvedAnimProfile`] handed to the
//! renderer. This is what makes a type able to customize *anything* — colors, sizes,
//! transparency, timing — while omitting whatever it wants to inherit.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::theme::{Color, SpringPreset, SurfaceFinish, Theme};

/// A type's optional `[overrides]` table. Anything set here wins over the theme.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StyleOverrides {
    // island geometry / appearance
    #[serde(default)]
    pub min_width: Option<u32>,
    #[serde(default)]
    pub max_width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub corner_radius: Option<f32>,
    #[serde(default)]
    pub background: Option<Color>,
    #[serde(default)]
    pub blur: Option<bool>,
    #[serde(default)]
    pub shadow: Option<Color>,
    #[serde(default)]
    pub shadow_radius: Option<f32>,
    #[serde(default)]
    pub shadow_offset_y: Option<f32>,
    #[serde(default)]
    pub shadow_spread: Option<f32>,
    #[serde(default)]
    pub finish: Option<SurfaceFinish>,
    #[serde(default)]
    pub finish_intensity: Option<u8>,
    #[serde(default)]
    pub finish_color: Option<Color>,
    #[serde(default)]
    pub margin_top: Option<u32>,
    // colors
    #[serde(default)]
    pub title_color: Option<Color>,
    #[serde(default)]
    pub subtitle_color: Option<Color>,
    #[serde(default)]
    pub accent: Option<Color>,
    #[serde(default)]
    pub icon_color: Option<Color>,
    // fonts
    #[serde(default)]
    pub font_ui: Option<String>,
    #[serde(default)]
    pub title_px: Option<f32>,
    #[serde(default)]
    pub subtitle_px: Option<f32>,
    // behavior
    #[serde(default)]
    pub timeout_ms: Option<u32>,
    #[serde(default)]
    pub priority: Option<i32>,
    // animation
    /// Override the type's `anim_profile` name entirely.
    #[serde(default)]
    pub anim_profile: Option<String>,
    /// Inline spring tuning, consulted before the theme's named presets.
    #[serde(default)]
    pub springs: Option<HashMap<String, SpringPreset>>,
}

/// Fully-concrete visual style for one notification.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedStyle {
    pub min_width: u32,
    pub max_width: u32,
    pub height: u32,
    pub corner_radius: f32,
    pub background: Color,
    pub blur: bool,
    pub shadow: Option<Color>,
    pub shadow_radius: f32,
    pub shadow_offset_y: f32,
    pub shadow_spread: f32,
    pub finish: SurfaceFinish,
    pub finish_intensity: u8,
    pub finish_color: Color,
    pub margin_top: u32,
    pub title_color: Color,
    pub subtitle_color: Color,
    pub accent: Color,
    pub icon_color: Color,
    pub font_ui: String,
    pub title_px: f32,
    pub subtitle_px: f32,
}

/// Concrete spring parameters per animated property. Plain data; the daemon maps these onto
/// `dynamicnoti_anim::SpringParams` when handing springs to the renderer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedAnimProfile {
    pub geometry: SpringPreset,
    pub opacity: SpringPreset,
    pub scale: SpringPreset,
    pub crossfade: SpringPreset,
    pub translate_y: SpringPreset,
}

impl Theme {
    /// Layer a type's overrides over the theme into a concrete [`ResolvedStyle`].
    pub fn resolve_style(&self, ov: Option<&StyleOverrides>) -> ResolvedStyle {
        macro_rules! pick {
            ($field:ident, $default:expr) => {
                ov.and_then(|o| o.$field.clone()).unwrap_or($default)
            };
        }
        ResolvedStyle {
            min_width: pick!(min_width, self.island.min_width),
            max_width: pick!(max_width, self.island.max_width),
            height: pick!(height, self.island.height),
            corner_radius: pick!(corner_radius, self.island.corner_radius),
            background: pick!(background, self.island.background),
            blur: pick!(blur, self.island.blur),
            shadow: ov.and_then(|o| o.shadow).or(self.island.shadow),
            shadow_radius: pick!(shadow_radius, self.island.shadow_radius),
            shadow_offset_y: pick!(shadow_offset_y, self.island.shadow_offset_y),
            shadow_spread: pick!(shadow_spread, self.island.shadow_spread),
            finish: pick!(finish, self.island.finish),
            finish_intensity: pick!(finish_intensity, self.island.finish_intensity),
            finish_color: pick!(finish_color, self.island.finish_color),
            margin_top: pick!(margin_top, self.island.margin_top),
            title_color: pick!(title_color, self.colors.title),
            subtitle_color: pick!(subtitle_color, self.colors.subtitle),
            accent: pick!(accent, self.colors.accent),
            icon_color: pick!(icon_color, self.colors.icon),
            font_ui: pick!(font_ui, self.fonts.ui.clone()),
            title_px: pick!(title_px, self.fonts.title_px),
            subtitle_px: pick!(subtitle_px, self.fonts.subtitle_px),
        }
    }

    /// Resolve the named anim profile to concrete spring parameters. An `anim_profile`
    /// override replaces `profile_name`; a type's inline `springs` win over theme presets.
    pub fn resolve_anim(&self, profile_name: &str, ov: Option<&StyleOverrides>) -> ResolvedAnimProfile {
        let name = ov
            .and_then(|o| o.anim_profile.as_deref())
            .unwrap_or(profile_name);
        let profile = self.anim_profiles.get(name);
        let spring = |role: fn(&crate::theme::AnimProfileRef) -> &str, fallback: &str| {
            let preset_name = profile.map(role).unwrap_or(fallback);
            ov.and_then(|o| o.springs.as_ref())
                .and_then(|s| s.get(preset_name))
                .copied()
                .unwrap_or_else(|| self.spring(preset_name))
        };
        ResolvedAnimProfile {
            geometry: spring(|p| &p.geometry, "island_soft"),
            opacity: spring(|p| &p.opacity, "gentle"),
            scale: spring(|p| &p.scale, "island_soft"),
            crossfade: spring(|p| &p.crossfade, "snappy"),
            translate_y: spring(|p| &p.translate_y, "island_slide"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_means_inherit() {
        let theme = Theme::default();
        let resolved = theme.resolve_style(None);
        assert_eq!(resolved.corner_radius, theme.island.corner_radius);
        assert_eq!(resolved.title_color, theme.colors.title);
    }

    #[test]
    fn overrides_win_field_by_field() {
        let theme = Theme::default();
        let ov = StyleOverrides {
            corner_radius: Some(4.0),
            background: Some(Color::rgba(1, 2, 3, 4)),
            ..Default::default()
        };
        let resolved = theme.resolve_style(Some(&ov));
        assert_eq!(resolved.corner_radius, 4.0); // overridden
        assert_eq!(resolved.background, Color::rgba(1, 2, 3, 4)); // overridden
        assert_eq!(resolved.height, theme.island.height); // inherited
    }

    #[test]
    fn anim_profile_override_and_inline_springs() {
        let theme = Theme::default();
        // Default profile "island_soft" → geometry uses the "island_soft" preset.
        let base = theme.resolve_anim("island_soft", None);
        assert_eq!(base.geometry, theme.spring("island_soft"));

        // Inline spring tuning replaces the named preset for that role.
        let tuned = SpringPreset { stiffness: 999.0, damping: 1.0, mass: 1.0, rest_eps: 0.01 };
        let mut springs = HashMap::new();
        springs.insert("island_soft".to_string(), tuned);
        let ov = StyleOverrides { springs: Some(springs), ..Default::default() };
        let resolved = theme.resolve_anim("island_soft", Some(&ov));
        assert_eq!(resolved.geometry, tuned);
    }
}
