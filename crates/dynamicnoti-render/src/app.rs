//! The main-thread render loop: SCTK wlr-layer-shell + wgpu, bridged into a calloop event loop
//! alongside the tokio→main `calloop::channel`. Owns the single live [`Island`] and drives its
//! springs off Wayland frame callbacks (0% GPU at idle). Everything here is `!Send`.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::NonNull;
use std::time::Duration;

use calloop::channel::{Channel, Event as ChannelEvent};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_surface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur::OrgKdeKwinBlur;
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur_manager::OrgKdeKwinBlurManager;

use dynamicnoti_core::scene::Scene;
use dynamicnoti_core::style::ResolvedStyle;

use crate::blur::BlurManager;
use crate::gpu::{instance_buffer, premul_linear, Gpu, ImageInstance, Pipelines, RectInstance};
use crate::image_cache::ImageCache;
use crate::layout::{self, IconShape, ItemKind, Layout};
use crate::phase::{compute_dt, SurfaceAnim};
use crate::text::{TextDraw, TextStage};
use crate::{NotificationEvent, OutboundEvent};

/// Lazily-built (once the first surface reveals the format) GPU draw resources.
struct Render {
    pipes: Pipelines,
    text: TextStage,
}

/// One on-screen island surface plus its animation state.
struct Island {
    id: u64,
    layer: LayerSurface,
    surface: wgpu::Surface<'static>,
    alpha_mode: wgpu::CompositeAlphaMode,
    format: wgpu::TextureFormat,
    configured: bool,
    surf_w: u32,
    surf_h: u32,
    style: ResolvedStyle,
    layout: Layout,
    prev_layout: Option<Layout>,
    anim: SurfaceAnim,
    prev_time: Option<u32>,
    marquee_t: f32,
    // Kept alive so the compositor's copy-on-commit sees them.
    _blur: Option<(OrgKdeKwinBlur, Region)>,
    _input_region: Option<Region>,
}

pub struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    blur_mgr: BlurManager,
    conn: Connection,
    qh: QueueHandle<App>,

    gpu: Gpu,
    render: Option<Render>,
    images: ImageCache,

    island: Option<Island>,
    #[allow(dead_code)]
    outbound: flume::Sender<OutboundEvent>,
    shutting_down: bool,
    exit: bool,
    frame_count: u64,
}

const MARQUEE_GAP: f32 = 48.0;

/// Entry point: build the Wayland + GPU context and run the loop until shutdown.
pub fn run(rx: Channel<NotificationEvent>, outbound: flume::Sender<OutboundEvent>) -> anyhow::Result<()> {
    let conn = Connection::connect_to_env()
        .map_err(|e| anyhow::anyhow!("cannot connect to Wayland: {e}"))?;
    let (globals, event_queue) = registry_queue_init::<App>(&conn)
        .map_err(|e| anyhow::anyhow!("registry init failed: {e}"))?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("wl_compositor unavailable: {e}"))?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("wlr-layer-shell unavailable (is this KWin/wlroots?): {e}"))?;
    let blur_mgr = BlurManager::bind(&globals, &qh);

    let gpu = Gpu::new()?;

    let mut event_loop =
        calloop::EventLoop::<App>::try_new().map_err(|e| anyhow::anyhow!("calloop: {e}"))?;
    let lh = event_loop.handle();

    calloop_wayland_source::WaylandSource::new(conn.clone(), event_queue)
        .insert(lh.clone())
        .map_err(|e| anyhow::anyhow!("insert wayland source: {}", e.error))?;

    lh.insert_source(rx, |event, _, app: &mut App| match event {
        ChannelEvent::Msg(ev) => app.on_event(ev),
        ChannelEvent::Closed => {
            app.shutting_down = true;
            app.begin_exit();
        }
    })
    .map_err(|e| anyhow::anyhow!("insert channel source: {e}"))?;

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        blur_mgr,
        conn,
        qh,
        gpu,
        render: None,
        images: ImageCache::new(),
        island: None,
        outbound,
        shutting_down: false,
        exit: false,
        frame_count: 0,
    };

    tracing::info!(target: "render", "render loop running");
    while !app.exit {
        // A long timeout is just a safety net; real wakeups come from frame callbacks and the
        // tokio→main channel. At idle (no springs, no marquee) we request no frames → 0% GPU.
        event_loop
            .dispatch(Some(Duration::from_millis(1000)), &mut app)
            .map_err(|e| anyhow::anyhow!("dispatch: {e}"))?;
    }
    tracing::info!(target: "render", "render loop stopped");
    Ok(())
}

impl App {
    fn on_event(&mut self, ev: NotificationEvent) {
        match ev {
            NotificationEvent::Show { id, scene, style, anim } => {
                if let Err(e) = self.show(id, scene, &style, anim) {
                    tracing::error!(target: "render", "show #{id} failed: {e}");
                }
            }
            NotificationEvent::Morph { id, scene, style, anim } => {
                self.morph(id, scene, &style, anim);
            }
            NotificationEvent::Close { id } => self.close(id),
            NotificationEvent::ImageReady { key, image } => self.image_ready(&key, &image),
            NotificationEvent::ConfigReloaded => {}
            NotificationEvent::Shutdown => {
                self.shutting_down = true;
                self.begin_exit();
            }
        }
    }

    /// Spawn a fresh island. Creates the layer surface + wgpu surface, builds render resources on
    /// first use, measures the scene, sizes the surface, attaches blur, and waits for configure.
    fn show(
        &mut self,
        id: u64,
        scene: Scene,
        style: &ResolvedStyle,
        anim: dynamicnoti_core::style::ResolvedAnimProfile,
    ) -> anyhow::Result<()> {
        // Drop any existing island (the queue normally Morphs/Closes first, but be safe).
        self.island = None;

        let wl_surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            wl_surface,
            Layer::Overlay,
            Some("dynamicnoti"),
            None,
        );
        layer.set_anchor(Anchor::TOP);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_exclusive_zone(-1);
        layer.set_margin(style.margin_top as i32, 0, 0, 0);

        let surface = self.create_wgpu_surface(&layer)?;
        let (format, alpha_mode) = self.gpu.pick_config(&surface);

        if self.render.is_none() {
            self.render = Some(Render {
                pipes: Pipelines::new(&self.gpu.device, format),
                text: TextStage::new(&self.gpu.device, &self.gpu.queue, format),
            });
        }
        let render = self.render.as_mut().unwrap();

        let lay = layout::compute(&scene, style, &mut render.text);
        // Width is fixed at max_width so width-changing morphs never need a Wayland resize; the
        // drawn island animates inside. Height tracks content. Empty input region → clicks pass
        // through the transparent margins.
        let surf_w = style.max_width.max(lay.content_w.ceil() as u32);
        let surf_h = lay.content_h.ceil() as u32;
        layer.set_size(surf_w, surf_h);

        let input_region = Region::new(&self.compositor).ok();
        if let Some(r) = &input_region {
            layer.wl_surface().set_input_region(Some(r.wl_region()));
        }

        let blur = if style.blur && self.blur_mgr.available() {
            self.blur_mgr.apply(&self.compositor, &self.qh, layer.wl_surface(), surf_w as i32, surf_h as i32)
        } else {
            None
        };

        layer.commit();
        // Push the initial (buffer-less) commit to KWin now so it sends the first configure
        // promptly, rather than waiting for the loop's next before-sleep flush.
        let _ = self.conn.flush();

        let anim_state =
            SurfaceAnim::enter(lay.content_w, lay.content_h, style.corner_radius, &anim);

        self.island = Some(Island {
            id,
            layer,
            surface,
            alpha_mode,
            format,
            configured: false,
            surf_w,
            surf_h,
            style: style.clone(),
            layout: lay,
            prev_layout: None,
            anim: anim_state,
            prev_time: None,
            marquee_t: 0.0,
            _blur: blur,
            _input_region: input_region,
        });
        tracing::debug!(target: "render", "show #{id} ({surf_w}x{surf_h})");
        Ok(())
    }

    /// Replace the live island's content in place — the signature morph (resize + crossfade).
    fn morph(
        &mut self,
        id: u64,
        scene: Scene,
        style: &ResolvedStyle,
        anim: dynamicnoti_core::style::ResolvedAnimProfile,
    ) {
        let Some(render) = self.render.as_mut() else { return };
        let Some(island) = self.island.as_mut() else { return };
        if island.id != id {
            return;
        }
        let lay = layout::compute(&scene, style, &mut render.text);
        island.anim.morph(lay.content_w, lay.content_h, style.corner_radius, &anim);
        island.prev_layout = Some(std::mem::replace(&mut island.layout, lay));
        island.style = style.clone();
        // We never shrink the Wayland surface mid-life; grow height if the new content is taller.
        let new_h = island.layout.content_h.ceil() as u32;
        if new_h > island.surf_h {
            island.surf_h = new_h;
            island.layer.set_size(island.surf_w, island.surf_h);
            island.layer.commit();
            // configure will reconfigure wgpu + repaint.
        } else {
            self.render_frame(None);
        }
    }

    fn close(&mut self, id: u64) {
        if let Some(island) = self.island.as_mut() {
            if island.id == id {
                island.anim.exit();
                self.render_frame(None);
            }
        }
    }

    /// Cache decoded art (a source fetched it off-thread) and repaint so a live island that
    /// references this handle reveals it. Requires the GPU pipelines, which exist once the first
    /// surface has revealed the format; before then there's no island to show it on anyway.
    fn image_ready(&mut self, key: &str, image: &dynamicnoti_core::ImageData) {
        let Some(render) = self.render.as_ref() else {
            tracing::debug!(target: "render", "image ready before GPU init, dropping: {key}");
            return;
        };
        self.images.insert_decoded(&self.gpu.device, &self.gpu.queue, &render.pipes, key, image);
        if self.island.is_some() {
            self.render_frame(None);
        }
    }

    /// Begin exit on the live island (graceful shutdown). If none, exit immediately.
    fn begin_exit(&mut self) {
        match self.island.as_mut() {
            Some(island) => {
                island.anim.exit();
                self.render_frame(None);
            }
            None => self.exit = true,
        }
    }

    fn create_wgpu_surface(&self, layer: &LayerSurface) -> anyhow::Result<wgpu::Surface<'static>> {
        let display = self.conn.backend().display_ptr() as *mut std::ffi::c_void;
        let rdh = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(display).ok_or_else(|| anyhow::anyhow!("null wayland display"))?,
        ));
        let surf_ptr = layer.wl_surface().id().as_ptr() as *mut std::ffi::c_void;
        let rwh = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(surf_ptr).ok_or_else(|| anyhow::anyhow!("null wl_surface"))?,
        ));
        let surface = unsafe {
            self.gpu.instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: rdh,
                raw_window_handle: rwh,
            })?
        };
        Ok(surface)
    }

    fn configure_surface(&mut self, w: u32, h: u32) {
        let Some(island) = self.island.as_mut() else { return };
        let cfg = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: island.format,
            width: w.max(1),
            height: h.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: island.alpha_mode,
            view_formats: vec![island.format],
            desired_maximum_frame_latency: 2,
        };
        island.surface.configure(&self.gpu.device, &cfg);
        island.surf_w = w;
        island.surf_h = h;
        island.configured = true;
    }

    /// Advance animation (when `time` is a real frame callback) and repaint. Destroys the island
    /// when its exit animation completes.
    fn render_frame(&mut self, time: Option<u32>) {
        let qh = self.qh.clone();

        let mut destroy = false;
        if let Some(island) = self.island.as_mut() {
            if let Some(t) = time {
                let dt = compute_dt(island.prev_time, t);
                island.prev_time = Some(t);
                island.marquee_t += dt;
                island.anim.tick(dt);
                if island.anim.past_morph_midpoint() {
                    island.prev_layout = None;
                }
                if island.anim.exit_done() {
                    destroy = true;
                }
            }
        }
        if destroy {
            self.island = None;
            if self.shutting_down {
                self.exit = true;
            }
            return;
        }

        let (Some(island), Some(render)) = (self.island.as_ref(), self.render.as_mut()) else {
            return;
        };
        if !island.configured {
            return;
        }
        let images = &mut self.images;
        let gpu = &self.gpu;
        self.frame_count += 1;
        if self.frame_count <= 2 || self.frame_count.is_multiple_of(60) {
            tracing::debug!(
                target: "render",
                "paint #{} phase={:?} op={:.2} w={:.0}",
                self.frame_count,
                island.anim.phase,
                island.anim.opacity.value,
                island.anim.width.value
            );
        }
        // Fault fence #3: one bad frame must not take down the loop.
        let result = catch_unwind(AssertUnwindSafe(|| paint(island, render, images, gpu, &qh)));
        if result.is_err() {
            tracing::error!(target: "render", "paint panicked — dropping island");
            self.island = None;
        }
    }
}

/// Map an item rect (island-local px) into surface px, applying island scale about the centre.
fn scaled(r: layout::Rect, s: f32, cx: f32, cy: f32) -> [f32; 4] {
    [cx + (r.x - cx) * s, cy + (r.y - cy) * s, r.w * s, r.h * s]
}

fn apply_alpha(mut c: [f32; 4], a: f32) -> [f32; 4] {
    for v in &mut c {
        *v *= a;
    }
    c
}

/// Build all draw instances and record the frame. Re-requests the next frame callback iff the
/// animation is still moving (or a marquee is scrolling) — otherwise the loop goes idle.
fn paint(island: &Island, render: &mut Render, images: &mut ImageCache, gpu: &Gpu, qh: &QueueHandle<App>) {
    let (sw, sh) = (island.surf_w as f32, island.surf_h as f32);
    render.pipes.set_screen(&gpu.queue, sw, sh);

    let s = island.anim.scale.value;
    let op = island.anim.opacity.value.clamp(0.0, 1.0);
    let (cx, cy) = (sw / 2.0, sh / 2.0);
    let iw = island.anim.width.value;
    let ih = island.anim.height.value;
    let radius = island.anim.corner_radius.value;
    let island_x = cx - iw / 2.0;
    let island_y = cy - ih / 2.0;

    let mut rects: Vec<RectInstance> = Vec::new();
    let mut text_draws: Vec<TextDraw> = Vec::new();
    let mut image_specs: Vec<(ImageInstance, String)> = Vec::new();

    // Drop shadow (a softer, larger rounded rect behind the pill).
    if let Some(shadow) = island.style.shadow {
        let pad = 7.0;
        let r = scaled(
            layout::Rect { x: island_x - pad, y: island_y - pad + 4.0, w: iw + 2.0 * pad, h: ih + 2.0 * pad },
            s,
            cx,
            cy,
        );
        let c = apply_alpha(premul_linear(shadow.r, shadow.g, shadow.b, shadow.a), op);
        rects.push(RectInstance { rect: r, color: c, meta: [(radius + pad) * s, 0.0, 0.0, 0.0] });
    }

    // Island background pill.
    {
        let bg = island.style.background;
        let r = scaled(layout::Rect { x: island_x, y: island_y, w: iw, h: ih }, s, cx, cy);
        let c = apply_alpha(premul_linear(bg.r, bg.g, bg.b, bg.a), op);
        rects.push(RectInstance { rect: r, color: c, meta: [radius * s, 0.0, 0.0, 0.0] });
    }

    // Content: before the morph midpoint show the old layout fading out; after, the new one
    // fading in. Outside Morph, crossfade is pinned at 1 → full content alpha.
    let cross = island.anim.crossfade.value.clamp(0.0, 1.0);
    let morphing = island.prev_layout.is_some();
    let (draw_layout, content_alpha) = if morphing && cross < 0.5 {
        (island.prev_layout.as_ref().unwrap(), op * (1.0 - cross * 2.0))
    } else if morphing {
        (&island.layout, op * (cross * 2.0 - 1.0))
    } else {
        (&island.layout, op)
    };

    // Centre the drawn layout's content box within the animated island box.
    let off_x = island_x + (iw - draw_layout.content_w) / 2.0;
    let off_y = island_y + (ih - draw_layout.content_h) / 2.0;

    for item in &draw_layout.items {
        let r = layout::Rect {
            x: off_x + item.rect.x,
            y: off_y + item.rect.y,
            w: item.rect.w,
            h: item.rect.h,
        };
        match &item.kind {
            ItemKind::Text { text, px, family, color, marquee, natural_w, speed } => {
                let sr = scaled(r, s, cx, cy);
                let col =
                    [color.r, color.g, color.b, (color.a as f32 * content_alpha) as u8];
                let clip = [sr[0], sr[1], sr[0] + sr[2], sr[1] + sr[3]];
                if *marquee && *natural_w > r.w {
                    let span = natural_w + MARQUEE_GAP;
                    let offset = (island.marquee_t * speed.max(1.0)) % span;
                    for k in 0..2 {
                        let lx = sr[0] - (offset - k as f32 * span) * s;
                        text_draws.push(TextDraw {
                            text: text.clone(),
                            px: *px,
                            family: family.clone(),
                            left: lx,
                            top: sr[1],
                            box_w: *natural_w,
                            box_h: r.h,
                            scale: s,
                            clip,
                            color: col,
                        });
                    }
                } else {
                    text_draws.push(TextDraw {
                        text: text.clone(),
                        px: *px,
                        family: family.clone(),
                        left: sr[0],
                        top: sr[1],
                        box_w: r.w.max(*natural_w),
                        box_h: r.h,
                        scale: s,
                        clip,
                        color: col,
                    });
                }
            }
            ItemKind::Image { handle, radius: rad } => {
                let sr = scaled(r, s, cx, cy);
                images.get(&gpu.device, &gpu.queue, &render.pipes, handle);
                image_specs.push((
                    ImageInstance { rect: sr, meta: [rad * s, content_alpha, 0.0, 0.0] },
                    handle.clone(),
                ));
            }
            ItemKind::Progress { value, track, fill } => {
                let sr = scaled(r, s, cx, cy);
                let h = sr[3];
                let tc = apply_alpha(premul_linear(track.r, track.g, track.b, track.a), content_alpha);
                rects.push(RectInstance { rect: sr, color: tc, meta: [h * 0.5, 0.0, 0.0, 0.0] });
                let fw = (sr[2] * value.clamp(0.0, 1.0)).max(h);
                let fc = apply_alpha(premul_linear(fill.r, fill.g, fill.b, fill.a), content_alpha);
                rects.push(RectInstance {
                    rect: [sr[0], sr[1], fw, h],
                    color: fc,
                    meta: [h * 0.5, 0.0, 0.0, 0.0],
                });
            }
            ItemKind::Icon { shape, color } => {
                push_icon(&mut rects, *shape, r, *color, content_alpha, s, cx, cy);
            }
        }
    }

    render.text.prepare(&gpu.device, &gpu.queue, (island.surf_w, island.surf_h), &text_draws);

    let rect_buf = instance_buffer(&gpu.device, &rects);
    // Build image buffers + bind-group refs before the pass so they outlive it.
    let image_bufs: Vec<(wgpu::Buffer, &wgpu::BindGroup)> = image_specs
        .iter()
        .filter_map(|(inst, handle)| {
            let bg = images.peek(handle)?;
            Some((instance_buffer(&gpu.device, std::slice::from_ref(inst)), bg))
        })
        .collect();

    let frame = match island.surface.get_current_texture() {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(target: "render", "acquire swapchain texture failed: {e}");
            return;
        }
    };
    let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("island-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        render.pipes.draw_rects(&mut pass, &rect_buf, rects.len() as u32);
        for (buf, bg) in &image_bufs {
            render.pipes.draw_image(&mut pass, buf, bg);
        }
        render.text.render(&mut pass);
    }

    // Re-arm the next frame callback BEFORE present (present commits the surface).
    if island.anim.needs_frame(island.layout.marquee_active()) {
        island.layer.wl_surface().frame(qh, island.layer.wl_surface().clone());
    }

    gpu.queue.submit(Some(encoder.finish()));
    frame.present();
    render.text.trim();
}

/// Emit the SDF rect instances for a media-control glyph inside box `r`.
#[allow(clippy::too_many_arguments)]
fn push_icon(
    rects: &mut Vec<RectInstance>,
    shape: IconShape,
    r: layout::Rect,
    color: dynamicnoti_core::theme::Color,
    alpha: f32,
    s: f32,
    cx: f32,
    cy: f32,
) {
    let c = apply_alpha(premul_linear(color.r, color.g, color.b, color.a), alpha);
    match shape {
        IconShape::Play => {
            let sr = scaled(r, s, cx, cy);
            rects.push(RectInstance { rect: sr, color: c, meta: [0.0, 1.0, 0.0, 0.0] });
        }
        IconShape::Stop => {
            let sr = scaled(r, s, cx, cy);
            rects.push(RectInstance { rect: sr, color: c, meta: [r.w * 0.18 * s, 0.0, 0.0, 0.0] });
        }
        IconShape::Pause => {
            let bw = r.w * 0.32;
            let gap = r.w * 0.18;
            let left = layout::Rect { x: r.x + (r.w / 2.0 - gap / 2.0 - bw), y: r.y, w: bw, h: r.h };
            let right = layout::Rect { x: r.x + r.w / 2.0 + gap / 2.0, y: r.y, w: bw, h: r.h };
            for b in [left, right] {
                let sr = scaled(b, s, cx, cy);
                rects.push(RectInstance { rect: sr, color: c, meta: [bw * 0.4 * s, 0.0, 0.0, 0.0] });
            }
        }
        IconShape::Dot => {
            let d = r.w.min(r.h) * 0.55;
            let dot = layout::Rect { x: r.x + (r.w - d) / 2.0, y: r.y + (r.h - d) / 2.0, w: d, h: d };
            let sr = scaled(dot, s, cx, cy);
            rects.push(RectInstance { rect: sr, color: c, meta: [d * 0.5, 0.0, 0.0, 0.0] });
        }
    }
}

// ── SCTK handlers ─────────────────────────────────────────────────────────────────────────

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
        // Integer scale: reconfigure at the new buffer size on the next configure. (Fractional
        // scaling is a documented follow-up.)
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        time: u32,
    ) {
        self.render_frame(Some(time));
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        if let Some(island) = self.island.as_ref() {
            if island.layer.wl_surface() == layer.wl_surface() {
                self.island = None;
            }
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (mut w, mut h) = configure.new_size;
        if let Some(island) = self.island.as_ref() {
            if w == 0 {
                w = island.surf_w;
            }
            if h == 0 {
                h = island.surf_h;
            }
        }
        tracing::debug!(target: "render", "configure {w}x{h} (requested {:?})", configure.new_size);
        self.configure_surface(w, h);
        // First paint (and re-arm the frame loop). dt is established on the first real callback.
        self.render_frame(None);
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

// The blur protocol is request-only (no events) — trivial Dispatch impls satisfy the queue.
impl Dispatch<OrgKdeKwinBlurManager, ()> for App {
    fn event(
        _: &mut Self,
        _: &OrgKdeKwinBlurManager,
        _: <OrgKdeKwinBlurManager as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<OrgKdeKwinBlur, ()> for App {
    fn event(
        _: &mut Self,
        _: &OrgKdeKwinBlur,
        _: <OrgKdeKwinBlur as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_layer!(App);
delegate_registry!(App);
