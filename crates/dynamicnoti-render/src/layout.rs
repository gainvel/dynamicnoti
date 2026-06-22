//! Pure two-pass layout over a [`Scene`] tree → a flat list of positioned [`Item`]s in
//! island-local pixels. No GPU, no Wayland: text measurement is injected via [`TextMeasure`]
//! so this is fully unit-tested (the real measurer wraps glyphon; tests use a fake metric).
//!
//! Pass 1 (`measure`) computes intrinsic sizes bottom-up. The island width is the root's
//! intrinsic width clamped to `[min_width, max_width]`; its height is `max(intrinsic, height)`.
//! Pass 2 (`place`) distributes that box top-down, honoring container weight (flex) on the main
//! axis and centering unweighted content.

use dynamicnoti_core::scene::{Primitive, Scene};
use dynamicnoti_core::style::ResolvedStyle;
use dynamicnoti_core::theme::Color;

/// Injected text metrics so layout stays GPU-free and testable.
pub trait TextMeasure {
    /// Natural (unwrapped) width and line height of `text` at `px` in font `family`.
    fn measure(&mut self, text: &str, px: f32, family: &str) -> (f32, f32);
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Geometric media-control glyphs we draw via the SDF pipeline (no icon-font dependency).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconShape {
    Play,
    Pause,
    Stop,
    Dot,
}

#[derive(Clone, Debug)]
pub enum ItemKind {
    Text {
        text: String,
        px: f32,
        family: String,
        color: Color,
        /// True when the natural text is wider than its box (must scroll).
        marquee: bool,
        natural_w: f32,
        /// Marquee scroll speed in px/s (0 for static text).
        speed: f32,
    },
    Image {
        handle: String,
        radius: f32,
    },
    Progress {
        value: f32,
        track: Color,
        fill: Color,
    },
    Icon {
        shape: IconShape,
        color: Color,
    },
}

#[derive(Clone, Debug)]
pub struct Item {
    pub rect: Rect,
    pub kind: ItemKind,
}

/// The placed scene plus the (unclamped-then-clamped) island content box.
pub struct Layout {
    pub items: Vec<Item>,
    pub content_w: f32,
    pub content_h: f32,
}

impl Layout {
    /// Any visible marquee that overflows its box → the loop must keep animating.
    pub fn marquee_active(&self) -> bool {
        self.items
            .iter()
            .any(|i| matches!(&i.kind, ItemKind::Text { marquee: true, .. }))
    }
}

/// Progress-bar thickness in px. Deliberately thin (pill-shaped, drawn fully rounded in
/// `app.rs::paint`) for the clean modern "now playing" look.
const PROGRESS_H: f32 = 3.0;

fn art_size(st: &ResolvedStyle) -> f32 {
    (st.height as f32 - 16.0).max(36.0)
}
fn icon_size(st: &ResolvedStyle) -> f32 {
    st.title_px * 1.15
}

fn text_role(role: &str, st: &ResolvedStyle) -> (f32, Color) {
    match role {
        "title" => (st.title_px, st.title_color),
        "subtitle" => (st.subtitle_px, st.subtitle_color),
        _ => (st.subtitle_px, st.title_color),
    }
}

fn with_alpha(c: Color, a: u8) -> Color {
    Color { a, ..c }
}

fn icon_shape(name: &str) -> IconShape {
    match name {
        "\u{f04b}" | "playing" => IconShape::Play,
        "\u{f04c}" | "paused" => IconShape::Pause,
        "\u{f04d}" | "stopped" => IconShape::Stop,
        _ => IconShape::Dot,
    }
}

#[derive(Clone, Copy)]
struct Size {
    w: f32,
    h: f32,
}

/// Flex weight along a container's main axis: containers carry it in `LayoutAttrs`; leaves are fixed.
fn child_weight(node: &Scene) -> f32 {
    match node {
        Scene::Row { attrs, .. } | Scene::Column { attrs, .. } | Scene::Stack { attrs, .. } => {
            attrs.weight
        }
        Scene::Leaf(_) => 0.0,
    }
}

/// Public entry: measure then place into the clamped island box.
pub fn compute(scene: &Scene, st: &ResolvedStyle, m: &mut dyn TextMeasure) -> Layout {
    let intrinsic = measure(scene, st, m);
    let content_w = intrinsic.w.clamp(st.min_width as f32, st.max_width as f32);
    let content_h = intrinsic.h.max(st.height as f32);
    let mut items = Vec::new();
    place(scene, st, m, Rect { x: 0.0, y: 0.0, w: content_w, h: content_h }, &mut items);
    Layout { items, content_w, content_h }
}

fn measure(node: &Scene, st: &ResolvedStyle, m: &mut dyn TextMeasure) -> Size {
    match node {
        Scene::Leaf(p) => measure_leaf(p, st, m),
        Scene::Row { attrs, children } => {
            let mut w = 0.0;
            let mut h: f32 = 0.0;
            for c in children {
                let s = measure(c, st, m);
                w += s.w;
                h = h.max(s.h);
            }
            Size { w: w + attrs.padding[1] * 2.0, h: h + attrs.padding[0] * 2.0 }
        }
        Scene::Column { attrs, children } => {
            let mut w: f32 = 0.0;
            let mut h = 0.0;
            for c in children {
                let s = measure(c, st, m);
                w = w.max(s.w);
                h += s.h;
            }
            Size { w: w + attrs.padding[1] * 2.0, h: h + attrs.padding[0] * 2.0 }
        }
        Scene::Stack { attrs, children } => {
            let mut w: f32 = 0.0;
            let mut h: f32 = 0.0;
            for c in children {
                let s = measure(c, st, m);
                w = w.max(s.w);
                h = h.max(s.h);
            }
            Size { w: w + attrs.padding[1] * 2.0, h: h + attrs.padding[0] * 2.0 }
        }
    }
}

fn measure_leaf(p: &Primitive, st: &ResolvedStyle, m: &mut dyn TextMeasure) -> Size {
    match p {
        Primitive::Text { content, style } => {
            let (px, _) = text_role(style, st);
            let (w, h) = m.measure(content, px, &st.font_ui);
            Size { w, h }
        }
        Primitive::Marquee { content, style, .. } => {
            let (px, _) = text_role(style, st);
            let (w, h) = m.measure(content, px, &st.font_ui);
            Size { w, h }
        }
        Primitive::Image { .. } => {
            let s = art_size(st);
            Size { w: s, h: s }
        }
        Primitive::Icon { name, .. } if name.is_empty() => Size { w: 0.0, h: 0.0 },
        Primitive::Icon { .. } => {
            let s = icon_size(st);
            Size { w: s, h: s }
        }
        Primitive::Progress { .. } => Size { w: 80.0, h: PROGRESS_H },
        Primitive::Spacer { size } => Size { w: *size, h: *size },
    }
}

fn place(node: &Scene, st: &ResolvedStyle, m: &mut dyn TextMeasure, rect: Rect, out: &mut Vec<Item>) {
    match node {
        Scene::Leaf(p) => place_leaf(p, st, m, rect, out),
        Scene::Row { attrs, children } => {
            let pad = attrs.padding;
            let inner = Rect {
                x: rect.x + pad[1],
                y: rect.y + pad[0],
                w: rect.w - 2.0 * pad[1],
                h: rect.h - 2.0 * pad[0],
            };
            let sizes: Vec<Size> = children.iter().map(|c| measure(c, st, m)).collect();
            let total_w: f32 = sizes.iter().map(|s| s.w).sum();
            let total_weight: f32 = children.iter().map(child_weight).sum();
            let extra = inner.w - total_w;
            let mut cx = inner.x + if total_weight == 0.0 && extra > 0.0 { extra / 2.0 } else { 0.0 };
            for (c, s) in children.iter().zip(&sizes) {
                let wgt = child_weight(c);
                let cw = (s.w + if total_weight > 0.0 { extra * wgt / total_weight } else { 0.0 })
                    .max(0.0);
                place(c, st, m, Rect { x: cx, y: inner.y, w: cw, h: inner.h }, out);
                cx += cw;
            }
        }
        Scene::Column { attrs, children } => {
            let pad = attrs.padding;
            let inner = Rect {
                x: rect.x + pad[1],
                y: rect.y + pad[0],
                w: rect.w - 2.0 * pad[1],
                h: rect.h - 2.0 * pad[0],
            };
            let sizes: Vec<Size> = children.iter().map(|c| measure(c, st, m)).collect();
            let total_h: f32 = sizes.iter().map(|s| s.h).sum();
            let total_weight: f32 = children.iter().map(child_weight).sum();
            let extra = inner.h - total_h;
            let mut cy = inner.y + if total_weight == 0.0 && extra > 0.0 { extra / 2.0 } else { 0.0 };
            for (c, s) in children.iter().zip(&sizes) {
                let wgt = child_weight(c);
                let ch = (s.h + if total_weight > 0.0 { extra * wgt / total_weight } else { 0.0 })
                    .max(0.0);
                place(c, st, m, Rect { x: inner.x, y: cy, w: inner.w, h: ch }, out);
                cy += ch;
            }
        }
        Scene::Stack { attrs, children } => {
            let pad = attrs.padding;
            let inner = Rect {
                x: rect.x + pad[1],
                y: rect.y + pad[0],
                w: rect.w - 2.0 * pad[1],
                h: rect.h - 2.0 * pad[0],
            };
            for c in children {
                place(c, st, m, inner, out);
            }
        }
    }
}

fn place_leaf(p: &Primitive, st: &ResolvedStyle, m: &mut dyn TextMeasure, b: Rect, out: &mut Vec<Item>) {
    match p {
        Primitive::Text { content, style } => {
            let (px, color) = text_role(style, st);
            let (nw, th) = m.measure(content, px, &st.font_ui);
            let ty = b.y + (b.h - th) / 2.0;
            out.push(Item {
                rect: Rect { x: b.x, y: ty, w: nw.min(b.w), h: th },
                kind: ItemKind::Text {
                    text: content.clone(),
                    px,
                    family: st.font_ui.clone(),
                    color,
                    marquee: false,
                    natural_w: nw,
                    speed: 0.0,
                },
            });
        }
        Primitive::Marquee { content, style, speed_px_s } => {
            let (px, color) = text_role(style, st);
            let (nw, th) = m.measure(content, px, &st.font_ui);
            let ty = b.y + (b.h - th) / 2.0;
            out.push(Item {
                rect: Rect { x: b.x, y: ty, w: b.w, h: th },
                kind: ItemKind::Text {
                    text: content.clone(),
                    px,
                    family: st.font_ui.clone(),
                    color,
                    marquee: nw > b.w + 0.5,
                    natural_w: nw,
                    speed: *speed_px_s,
                },
            });
        }
        Primitive::Image { handle, radius } => {
            let s = art_size(st).min(b.w).min(b.h);
            out.push(Item {
                rect: Rect { x: b.x + (b.w - s) / 2.0, y: b.y + (b.h - s) / 2.0, w: s, h: s },
                kind: ItemKind::Image { handle: handle.clone(), radius: *radius },
            });
        }
        Primitive::Icon { name, .. } => {
            if name.is_empty() {
                return;
            }
            let s = icon_size(st).min(b.w).min(b.h);
            out.push(Item {
                rect: Rect { x: b.x + (b.w - s) / 2.0, y: b.y + (b.h - s) / 2.0, w: s, h: s },
                kind: ItemKind::Icon { shape: icon_shape(name), color: st.icon_color },
            });
        }
        Primitive::Progress { value, .. } => {
            let h = PROGRESS_H.min(b.h);
            out.push(Item {
                rect: Rect { x: b.x, y: b.y + (b.h - h) / 2.0, w: b.w, h },
                kind: ItemKind::Progress {
                    value: *value,
                    track: with_alpha(st.subtitle_color, 64),
                    fill: st.accent,
                },
            });
        }
        Primitive::Spacer { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dynamicnoti_core::scene::{Align, LayoutAttrs, Primitive, Scene};
    use dynamicnoti_core::theme::Theme;

    /// Deterministic monospace-ish metric: width ∝ chars, height ∝ px.
    struct Fake;
    impl TextMeasure for Fake {
        fn measure(&mut self, text: &str, px: f32, _family: &str) -> (f32, f32) {
            (text.chars().count() as f32 * px * 0.5, px * 1.25)
        }
    }

    fn style() -> ResolvedStyle {
        Theme::default().resolve_style(None)
    }

    fn text(s: &str, role: &str) -> Scene {
        Scene::Leaf(Primitive::Text { content: s.into(), style: role.into() })
    }

    fn row(children: Vec<Scene>) -> Scene {
        Scene::Row { attrs: LayoutAttrs { padding: [4.0, 8.0], align: Align::Center, weight: 1.0 }, children }
    }

    #[test]
    fn short_content_clamps_to_min_width() {
        let st = style();
        let scene = row(vec![text("hi", "title")]);
        let l = compute(&scene, &st, &mut Fake);
        assert_eq!(l.content_w, st.min_width as f32, "narrow content clamps up to min_width");
        assert_eq!(l.content_h, st.height as f32);
        assert_eq!(l.items.len(), 1);
    }

    #[test]
    fn long_title_clamps_to_max_width_and_marquees() {
        let st = style();
        // A column with weight so the row gives it the (insufficient) remaining width.
        let col = Scene::Column {
            attrs: LayoutAttrs { padding: [0.0, 0.0], align: Align::Start, weight: 1.0 },
            children: vec![Scene::Leaf(Primitive::Marquee {
                // Long enough that the fake metric (chars * px * 0.5) far exceeds max_width.
                content: "a very long song title that absolutely cannot fit in the island \
                          no matter how hard the compositor tries to squeeze it in there"
                    .into(),
                style: "title".into(),
                speed_px_s: 30.0,
            })],
        };
        let scene = row(vec![col]);
        let l = compute(&scene, &st, &mut Fake);
        assert_eq!(l.content_w, st.max_width as f32, "overflowing content clamps to max_width");
        assert!(l.marquee_active(), "overflowing marquee must report active");
    }

    #[test]
    fn row_lays_children_left_to_right_within_padding() {
        let st = style();
        let scene = row(vec![text("aa", "title"), text("bb", "subtitle")]);
        let l = compute(&scene, &st, &mut Fake);
        let xs: Vec<f32> = l.items.iter().map(|i| i.rect.x).collect();
        assert!(xs[0] >= 8.0, "first child past the horizontal padding");
        assert!(xs[1] > xs[0], "second child is to the right of the first");
    }

    #[test]
    fn image_leaf_produces_image_item_and_empty_icon_is_dropped() {
        let st = style();
        let scene = row(vec![
            Scene::Leaf(Primitive::Image { handle: "/tmp/a.png".into(), radius: 8.0 }),
            Scene::Leaf(Primitive::Icon { name: String::new(), style: "icon".into() }),
        ]);
        let l = compute(&scene, &st, &mut Fake);
        assert_eq!(l.items.len(), 1, "image kept, empty-name icon dropped");
        assert!(matches!(l.items[0].kind, ItemKind::Image { .. }));
    }

    #[test]
    fn progress_fills_its_box_width() {
        let st = style();
        let col = Scene::Column {
            attrs: LayoutAttrs { padding: [0.0, 0.0], align: Align::Center, weight: 1.0 },
            children: vec![Scene::Leaf(Primitive::Progress { value: 0.5, style: "bar".into() })],
        };
        let scene = row(vec![col]);
        let l = compute(&scene, &st, &mut Fake);
        let p = l.items.iter().find(|i| matches!(i.kind, ItemKind::Progress { .. })).unwrap();
        assert!(p.rect.w > 200.0, "progress bar stretches across the column");
    }
}
