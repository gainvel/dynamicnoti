//! glyphon text stage: owns the font system, glyph atlas, and text renderer. Builds
//! `cosmic_text` buffers from the layout's text items, prepares them, and records the text
//! pass. Also provides the [`crate::layout::TextMeasure`] impl so layout can size text without
//! a second font path.

use glyphon::{
    Attrs, Buffer, Cache, Color as GColor, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use wgpu::{Device, MultisampleState, Queue, RenderPass, TextureFormat};

use crate::layout::TextMeasure;

/// One text run to draw, in surface-local pixels. `clip` bounds (l,t,r,b) clip overflow
/// (marquee). `color` alpha is already pre-multiplied by the surface's animated opacity.
pub struct TextDraw {
    pub text: String,
    pub px: f32,
    pub family: String,
    pub left: f32,
    pub top: f32,
    pub box_w: f32,
    pub box_h: f32,
    pub scale: f32,
    pub clip: [f32; 4],
    pub color: [u8; 4],
}

pub struct TextStage {
    font_system: FontSystem,
    swash: SwashCache,
    atlas: TextAtlas,
    viewport: Viewport,
    renderer: TextRenderer,
}

const LINE: f32 = 1.3;

impl TextStage {
    pub fn new(device: &Device, queue: &Queue, format: TextureFormat) -> TextStage {
        let cache = Cache::new(device);
        let mut atlas = TextAtlas::new(device, queue, &cache, format);
        let viewport = Viewport::new(device, &cache);
        let renderer = TextRenderer::new(&mut atlas, device, MultisampleState::default(), None);
        TextStage {
            font_system: FontSystem::new(),
            swash: SwashCache::new(),
            atlas,
            viewport,
            renderer,
        }
    }

    /// Build buffers for every draw and stage them for rendering. Buffers are local — glyphon
    /// rasterizes into its atlas/vertex buffer during `prepare`, so they can be dropped after.
    pub fn prepare(&mut self, device: &Device, queue: &Queue, screen: (u32, u32), draws: &[TextDraw]) {
        self.viewport.update(queue, Resolution { width: screen.0, height: screen.1 });

        let mut buffers = Vec::with_capacity(draws.len());
        for d in draws {
            let mut buf = Buffer::new(&mut self.font_system, Metrics::new(d.px, d.px * LINE));
            buf.set_size(&mut self.font_system, Some(d.box_w.max(1.0)), Some(d.box_h.max(1.0)));
            buf.set_text(
                &mut self.font_system,
                &d.text,
                Attrs::new().family(Family::Name(&d.family)),
                Shaping::Advanced,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            buffers.push(buf);
        }

        let areas = draws.iter().zip(&buffers).map(|(d, buf)| TextArea {
            buffer: buf,
            left: d.left,
            top: d.top,
            scale: d.scale,
            bounds: TextBounds {
                left: d.clip[0] as i32,
                top: d.clip[1] as i32,
                right: d.clip[2] as i32,
                bottom: d.clip[3] as i32,
            },
            default_color: GColor::rgba(d.color[0], d.color[1], d.color[2], d.color[3]),
            custom_glyphs: &[],
        });

        if let Err(e) = self.renderer.prepare(
            device,
            queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            areas,
            &mut self.swash,
        ) {
            tracing::warn!(target: "render", "glyphon prepare failed: {e:?}");
        }
    }

    pub fn render<'a>(&'a self, pass: &mut RenderPass<'a>) {
        if let Err(e) = self.renderer.render(&self.atlas, &self.viewport, pass) {
            tracing::warn!(target: "render", "glyphon render failed: {e:?}");
        }
    }

    /// Release atlas space for glyphs not used this frame.
    pub fn trim(&mut self) {
        self.atlas.trim();
    }
}

impl TextMeasure for TextStage {
    fn measure(&mut self, text: &str, px: f32, family: &str) -> (f32, f32) {
        let mut buf = Buffer::new(&mut self.font_system, Metrics::new(px, px * LINE));
        buf.set_text(
            &mut self.font_system,
            text,
            Attrs::new().family(Family::Name(family)),
            Shaping::Advanced,
        );
        buf.shape_until_scroll(&mut self.font_system, false);
        let mut w = 0.0_f32;
        let mut lines = 0.0_f32;
        for run in buf.layout_runs() {
            w = w.max(run.line_w);
            lines += 1.0;
        }
        (w, lines.max(1.0) * px * LINE)
    }
}
