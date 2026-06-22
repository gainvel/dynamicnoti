//! The main-thread render loop: SCTK wlr-layer-shell + wgpu, bridged into a calloop event loop
//! alongside the tokio→main `calloop::channel`. Owns the live [`Island`] surfaces (one per target
//! monitor — see [`MonitorSelect`]) and drives each one's springs off its own Wayland frame
//! callbacks (0% GPU at idle). Everything here is `!Send`.

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

use dynamicnoti_core::scene::{ProgressMode, Scene};
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

/// Which monitor(s) the island appears on, parsed once from `config.monitor`.
enum MonitorSelect {
    /// Mirror onto every connected output (one surface each). Follows hotplug.
    All,
    /// A single surface; the compositor picks the output (usually primary/active).
    Auto,
    /// A specific connector name (e.g. `"DP-1"`), matched against `OutputInfo::name`. Follows
    /// hotplug: the surface appears when that monitor connects.
    Named(String),
}

impl MonitorSelect {
    fn parse(s: &str) -> Self {
        match s.trim() {
            "all" => MonitorSelect::All,
            "auto" | "" => MonitorSelect::Auto,
            name => MonitorSelect::Named(name.to_string()),
        }
    }
}

/// The currently-displayed notification's spawn inputs, retained so a monitor hotplugged mid-life
/// can spawn a matching surface. Cleared on close / exit.
struct ActiveNotif {
    id: u64,
    timeout_ms: u32,
    scene: Scene,
    style: ResolvedStyle,
    anim: dynamicnoti_core::style::ResolvedAnimProfile,
}

/// One on-screen island surface plus its animation state.
struct Island {
    id: u64,
    /// The output this surface targets (`None` = compositor's choice, the `Auto` case). Used to
    /// match the surface on output-removed/hotplug.
    output: Option<wl_output::WlOutput>,
    layer: LayerSurface,
    surface: wgpu::Surface<'static>,
    alpha_mode: wgpu::CompositeAlphaMode,
    format: wgpu::TextureFormat,
    configured: bool,
    surf_w: u32,
    surf_h: u32,
    style: ResolvedStyle,
    /// The scene currently displayed. Kept so an incoming morph can be compared against it: a
    /// value-only change (e.g. a media position tick) updates in place instead of crossfading.
    scene: Scene,
    layout: Layout,
    prev_layout: Option<Layout>,
    anim: SurfaceAnim,
    prev_time: Option<u32>,
    marquee_t: f32,
    /// Notification lifetime (ms); 0 = sticky. Drives the lifetime countdown bar.
    timeout_ms: u32,
    /// Seconds elapsed since this content was shown — counts the lifetime bar down 1→0.
    lifetime_t: f32,
    /// Top padding (px) reserved inside the surface above the island's rest position (for the
    /// soft shadow and slide overshoot). The island rests `pad_top` px below the surface top.
    pad_top: f32,
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

    /// One live surface per target monitor (see [`MonitorSelect`]). All islands of a notification
    /// share the same `id`.
    islands: Vec<Island>,
    /// Which output(s) to mirror onto. Fixed for the process (read from config at startup).
    monitor: MonitorSelect,
    /// The notification currently on screen, kept so a hotplugged monitor can join mid-life.
    active: Option<ActiveNotif>,
    #[allow(dead_code)]
    outbound: flume::Sender<OutboundEvent>,
    shutting_down: bool,
    exit: bool,
    frame_count: u64,
}

const MARQUEE_GAP: f32 = 48.0;

/// Entry point: build the Wayland + GPU context and run the loop until shutdown.
pub fn run(
    rx: Channel<NotificationEvent>,
    outbound: flume::Sender<OutboundEvent>,
    monitor: String,
) -> anyhow::Result<()> {
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
        islands: Vec::new(),
        monitor: MonitorSelect::parse(&monitor),
        active: None,
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
            NotificationEvent::Show { id, timeout_ms, scene, style, anim } => {
                if let Err(e) = self.show(id, timeout_ms, scene, &style, anim) {
                    tracing::error!(target: "render", "show #{id} failed: {e}");
                }
            }
            NotificationEvent::Morph { id, timeout_ms, scene, style, anim } => {
                self.morph(id, timeout_ms, scene, &style, anim);
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

    /// Show a notification: spawn one island per target monitor (see [`MonitorSelect`]). Retains
    /// the spawn inputs in `self.active` so a hotplugged monitor can join mid-life.
    fn show(
        &mut self,
        id: u64,
        timeout_ms: u32,
        scene: Scene,
        style: &ResolvedStyle,
        anim: dynamicnoti_core::style::ResolvedAnimProfile,
    ) -> anyhow::Result<()> {
        // Drop any existing islands (the queue normally Morphs/Closes first, but be safe).
        self.islands.clear();

        let active = ActiveNotif {
            id,
            timeout_ms,
            scene,
            style: style.clone(),
            anim,
        };

        let targets = self.compute_targets();
        for target in targets {
            match self.spawn_island(target, &active) {
                Ok(island) => self.islands.push(island),
                Err(e) => tracing::error!(target: "render", "spawn island for #{id} failed: {e}"),
            }
        }
        tracing::debug!(target: "render", "show #{id} on {} surface(s)", self.islands.len());
        self.active = Some(active);
        Ok(())
    }

    /// The list of output targets to mirror onto, per the configured [`MonitorSelect`].
    /// `Some(output)` pins a surface to that output; `None` lets the compositor choose.
    fn compute_targets(&self) -> Vec<Option<wl_output::WlOutput>> {
        match &self.monitor {
            MonitorSelect::Auto => vec![None],
            MonitorSelect::All => {
                let outs: Vec<_> = self.output_state.outputs().map(Some).collect();
                // Cold-start fallback: if no outputs are advertised yet, let the compositor place
                // one surface; hotplug fills in the rest.
                if outs.is_empty() {
                    vec![None]
                } else {
                    outs
                }
            }
            MonitorSelect::Named(name) => match self.find_output(name) {
                Some(o) => vec![Some(o)],
                // Not connected yet — `new_output` will spawn it when that monitor appears.
                None => vec![],
            },
        }
    }

    /// Find a connected output by its connector name (e.g. `"DP-1"`).
    fn find_output(&self, name: &str) -> Option<wl_output::WlOutput> {
        self.output_state
            .outputs()
            .find(|o| self.output_state.info(o).and_then(|i| i.name).as_deref() == Some(name))
    }

    /// Create one island surface targeting `target` (`None` = compositor's choice). Builds the
    /// layer + wgpu surface, lazily inits render resources, measures the scene, sizes the surface,
    /// attaches blur, commits, and returns the island in its Enter phase (awaiting configure).
    fn spawn_island(
        &mut self,
        target: Option<wl_output::WlOutput>,
        active: &ActiveNotif,
    ) -> anyhow::Result<Island> {
        let style = &active.style;
        let wl_surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            wl_surface,
            Layer::Overlay,
            Some("dynamicnoti"),
            target.as_ref(),
        );
        layer.set_anchor(Anchor::TOP);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_exclusive_zone(-1);

        let surface = self.create_wgpu_surface(&layer)?;
        let (format, alpha_mode) = self.gpu.pick_config(&surface);

        if self.render.is_none() {
            self.render = Some(Render {
                pipes: Pipelines::new(&self.gpu.device, format),
                text: TextStage::new(&self.gpu.device, &self.gpu.queue, format),
            });
        }
        let render = self.render.as_mut().unwrap();

        let lay = layout::compute(&active.scene, style, &mut render.text);
        // The surface is a padded "canvas" larger than the island so the soft shadow and the
        // slide-from-top overshoot are never clipped. The island rests `pad_top` below the surface
        // top and is centered horizontally. Width is fixed at max_width (+ side pad) so
        // width-changing morphs never need a Wayland resize; the drawn island animates inside.
        let (pad_x, pad_top, pad_bottom) = surface_padding(style);
        let surf_w = style.max_width.max(lay.content_w.ceil() as u32) + 2 * pad_x as u32;
        let surf_h = lay.content_h.ceil() as u32 + (pad_top + pad_bottom).ceil() as u32;
        layer.set_size(surf_w, surf_h);
        // Pull the surface up by `pad_top` so the island still rests `margin_top` below the screen
        // edge (the headroom above it extends toward/over the edge for the slide-in).
        layer.set_margin(style.margin_top as i32 - pad_top as i32, 0, 0, 0);

        let input_region = Region::new(&self.compositor).ok();
        if let Some(r) = &input_region {
            layer.wl_surface().set_input_region(Some(r.wl_region()));
        }

        // Blur only the island's rest rect — not the padded/transparent canvas — so the
        // compositor doesn't blur a halo around the shadow.
        let blur = if style.blur && self.blur_mgr.available() {
            let bw = (style.max_width.max(lay.content_w.ceil() as u32)) as i32;
            self.blur_mgr.apply(
                &self.compositor,
                &self.qh,
                layer.wl_surface(),
                pad_x as i32,
                pad_top as i32,
                bw,
                lay.content_h.ceil() as i32,
            )
        } else {
            None
        };

        layer.commit();
        // Push the initial (buffer-less) commit to KWin now so it sends the first configure
        // promptly, rather than waiting for the loop's next before-sleep flush.
        let _ = self.conn.flush();

        // Start the slide fully above the island's rest position (off the surface top).
        let slide_offset = lay.content_h + pad_top + style.margin_top as f32;
        let anim_state = SurfaceAnim::enter(
            lay.content_w,
            lay.content_h,
            style.corner_radius,
            slide_offset,
            &active.anim,
        );

        Ok(Island {
            id: active.id,
            output: target,
            layer,
            surface,
            alpha_mode,
            format,
            configured: false,
            surf_w,
            surf_h,
            style: style.clone(),
            scene: active.scene.clone(),
            layout: lay,
            prev_layout: None,
            anim: anim_state,
            prev_time: None,
            marquee_t: 0.0,
            timeout_ms: active.timeout_ms,
            lifetime_t: 0.0,
            pad_top,
            _blur: blur,
            _input_region: input_region,
        })
    }

    /// Replace every matching island's content in place — the signature morph (resize + crossfade)
    /// applied to each mirrored surface.
    fn morph(
        &mut self,
        id: u64,
        timeout_ms: u32,
        scene: Scene,
        style: &ResolvedStyle,
        anim: dynamicnoti_core::style::ResolvedAnimProfile,
    ) {
        let Some(render) = self.render.as_mut() else { return };
        // Measure once; all mirrored surfaces share the same content.
        let lay = layout::compute(&scene, style, &mut render.text);

        // Keep `active` current so a monitor hotplugged after this morph shows the new content.
        if let Some(a) = self.active.as_mut() {
            if a.id == id {
                a.timeout_ms = timeout_ms;
                a.scene = scene.clone();
                a.style = style.clone();
                a.anim = anim;
            }
        }

        let (_, pad_top, pad_bottom) = surface_padding(style);
        let new_h = lay.content_h.ceil() as u32 + (pad_top + pad_bottom).ceil() as u32;

        // Islands that resize repaint on their configure; the rest repaint synchronously here.
        let mut to_paint: Vec<usize> = Vec::new();
        for (idx, island) in self.islands.iter_mut().enumerate() {
            if island.id != id {
                continue;
            }
            // A value-only change (e.g. a static progress tick on the same replace_key) must NOT
            // crossfade the whole card — that churns the GPU and never lets the loop idle. Swap the
            // content in place; the spring phase is untouched.
            if island.scene.same_shape(&scene) {
                island.layout = lay.clone();
                island.scene = scene.clone();
                island.style = style.clone();
                island.timeout_ms = timeout_ms;
                to_paint.push(idx);
                continue;
            }

            island.anim.morph(lay.content_w, lay.content_h, style.corner_radius, &anim);
            island.prev_layout = Some(std::mem::replace(&mut island.layout, lay.clone()));
            island.scene = scene.clone();
            island.style = style.clone();
            // A real content change (e.g. a new track) restarts the lifetime countdown.
            island.timeout_ms = timeout_ms;
            island.lifetime_t = 0.0;
            // We never shrink the Wayland surface mid-life; grow height if the new (padded) content
            // is taller. Side/top/bottom padding is unchanged (driven by style, not content).
            if new_h > island.surf_h {
                island.surf_h = new_h;
                island.layer.set_size(island.surf_w, island.surf_h);
                island.layer.commit();
                // configure will reconfigure wgpu + repaint this surface.
            } else {
                to_paint.push(idx);
            }
        }
        for idx in to_paint {
            self.paint_idx(idx);
        }
    }

    fn close(&mut self, id: u64) {
        if self.active.as_ref().is_some_and(|a| a.id == id) {
            self.active = None;
        }
        let mut to_paint: Vec<usize> = Vec::new();
        for (idx, island) in self.islands.iter_mut().enumerate() {
            if island.id == id {
                island.anim.exit();
                to_paint.push(idx);
            }
        }
        for idx in to_paint {
            self.paint_idx(idx);
        }
    }

    /// Cache decoded art (a source fetched it off-thread) and repaint every live island that
    /// references this handle. Requires the GPU pipelines, which exist once the first surface has
    /// revealed the format; before then there's no island to show it on anyway.
    fn image_ready(&mut self, key: &str, image: &dynamicnoti_core::ImageData) {
        let Some(render) = self.render.as_ref() else {
            tracing::debug!(target: "render", "image ready before GPU init, dropping: {key}");
            return;
        };
        self.images.insert_decoded(&self.gpu.device, &self.gpu.queue, &render.pipes, key, image);
        self.repaint_all();
    }

    /// Begin exit on every live island (graceful shutdown). If none, exit immediately.
    fn begin_exit(&mut self) {
        self.active = None;
        if self.islands.is_empty() {
            self.exit = true;
            return;
        }
        for island in self.islands.iter_mut() {
            island.anim.exit();
        }
        self.repaint_all();
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

    fn configure_surface(&mut self, idx: usize, w: u32, h: u32) {
        let Some(island) = self.islands.get_mut(idx) else { return };
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

    /// Index of the island owning `surface`, if any.
    fn island_at(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.islands.iter().position(|i| i.layer.wl_surface() == surface)
    }

    /// A real frame callback for one surface: advance only that island's springs/marquee/lifetime
    /// (each island keeps its own clock) and repaint it — or destroy it when its exit completes.
    fn advance_island(&mut self, surface: &wl_surface::WlSurface, time: u32) {
        let Some(idx) = self.island_at(surface) else { return };
        let mut destroy = false;
        {
            let island = &mut self.islands[idx];
            let dt = compute_dt(island.prev_time, time);
            island.prev_time = Some(time);
            island.marquee_t += dt;
            island.lifetime_t += dt;
            island.anim.tick(dt);
            if island.anim.past_morph_midpoint() {
                island.prev_layout = None;
            }
            if island.anim.exit_done() {
                destroy = true;
            }
        }
        if destroy {
            self.islands.remove(idx);
            if self.islands.is_empty() && self.shutting_down {
                self.exit = true;
            }
            return;
        }
        self.paint_idx(idx);
    }

    /// Repaint every configured island (used for synchronous content/state updates). Iterates in
    /// reverse so a fault-removal of one island doesn't shift the indices still to paint.
    fn repaint_all(&mut self) {
        for idx in (0..self.islands.len()).rev() {
            self.paint_idx(idx);
        }
    }

    /// Build and present one island's frame. Fault fence #3: a panicking draw drops only that
    /// island, not the loop. Re-arms its own frame callback iff still animating (see `paint`).
    fn paint_idx(&mut self, idx: usize) {
        let qh = self.qh.clone();
        let Some(render) = self.render.as_mut() else { return };
        let Some(island) = self.islands.get(idx) else { return };
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
        let result = catch_unwind(AssertUnwindSafe(|| paint(island, render, images, gpu, &qh)));
        if result.is_err() {
            tracing::error!(target: "render", "paint panicked — dropping island");
            self.islands.remove(idx);
        }
    }
}

/// Minimum vertical headroom (px) reserved for the slide overshoot even when no shadow is drawn.
const SLIDE_OVERSHOOT_PAD: f32 = 16.0;

// Rect-shader `meta.y` kind selectors (must match the WGSL in `gpu.rs`).
const KIND_FILL: f32 = 0.0;
const KIND_SHADOW: f32 = 2.0;
const KIND_GRADIENT: f32 = 3.0;

/// Resolve a style's surface finish into the shader `(kind, sheen)` pair for the background pill.
/// `op` (the island opacity) folds in so the sheen fades with Enter/Exit.
fn finish_params(style: &ResolvedStyle, op: f32) -> (f32, f32) {
    use dynamicnoti_core::theme::SurfaceFinish;
    let sheen = style.finish_intensity as f32 / 255.0 * op;
    match style.finish {
        SurfaceFinish::None => (KIND_FILL, 0.0),
        SurfaceFinish::Glossy => (KIND_FILL, sheen),
        SurfaceFinish::Gradient => (KIND_GRADIENT, sheen),
    }
}

/// Surface padding `(pad_x, pad_top, pad_bottom)` in px around the island's rest box, sized to
/// contain the soft shadow and the springy slide overshoot so neither is ever clipped.
fn surface_padding(style: &ResolvedStyle) -> (f32, f32, f32) {
    let shadow_extent =
        if style.shadow.is_some() { style.shadow_radius + style.shadow_spread } else { 0.0 };
    let pad_x = shadow_extent.ceil();
    let pad_top = shadow_extent.max(SLIDE_OVERSHOOT_PAD).ceil();
    let pad_bottom = (shadow_extent + style.shadow_offset_y.max(0.0)).max(SLIDE_OVERSHOOT_PAD).ceil();
    (pad_x, pad_top, pad_bottom)
}

/// Remaining-lifetime fraction (1 → just shown, 0 → expired) for a timed notification. Sticky
/// (timeout 0) notifications stay full.
fn lifetime_fraction(island: &Island) -> f32 {
    if island.timeout_ms == 0 {
        return 1.0;
    }
    (1.0 - island.lifetime_t * 1000.0 / island.timeout_ms as f32).clamp(0.0, 1.0)
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
    let iw = island.anim.width.value;
    let ih = island.anim.height.value;
    let radius = island.anim.corner_radius.value;
    // Top-anchored: the island rests `pad_top` below the surface top, centered horizontally, and
    // slides vertically by `translate_y`. The scale pivot is the island's own (moving) centre so
    // the pop scales in place rather than about the surface centre.
    let island_x = (sw - iw) / 2.0;
    let island_y = island.pad_top + island.anim.translate_y.value;
    let (cx, cy) = (island_x + iw / 2.0, island_y + ih / 2.0);

    let mut rects: Vec<RectInstance> = Vec::new();
    let mut text_draws: Vec<TextDraw> = Vec::new();
    let mut image_specs: Vec<(ImageInstance, String)> = Vec::new();

    // Soft drop shadow: a blurred rounded rect behind the pill, grown by the shadow extent and
    // biased downward. Drawn via the SDF shader's wide-feather "shadow" kind (kind 2).
    if let Some(shadow) = island.style.shadow {
        let grow = island.style.shadow_radius + island.style.shadow_spread;
        let r = scaled(
            layout::Rect {
                x: island_x - grow,
                y: island_y - grow + island.style.shadow_offset_y,
                w: iw + 2.0 * grow,
                h: ih + 2.0 * grow,
            },
            s,
            cx,
            cy,
        );
        let c = apply_alpha(premul_linear(shadow.r, shadow.g, shadow.b, shadow.a), op);
        let feather = island.style.shadow_radius.max(0.5) * s;
        rects.push(RectInstance { rect: r, color: c, meta: [(radius + grow) * s, KIND_SHADOW, feather, 0.0] });
    }

    // Island background pill (+ themed surface finish: a glossy/gradient sheen for depth).
    {
        let bg = island.style.background;
        let r = scaled(layout::Rect { x: island_x, y: island_y, w: iw, h: ih }, s, cx, cy);
        let c = apply_alpha(premul_linear(bg.r, bg.g, bg.b, bg.a), op);
        let (kind, sheen) = finish_params(&island.style, op);
        rects.push(RectInstance { rect: r, color: c, meta: [radius * s, kind, 0.0, sheen] });
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
            ItemKind::Progress { value, mode, track, fill } => {
                let sr = scaled(r, s, cx, cy);
                let h = sr[3];
                // A Lifetime bar ignores its (placeholder) field value and counts the
                // notification's remaining lifetime down from the render clock.
                let frac = match mode {
                    ProgressMode::Lifetime => lifetime_fraction(island),
                    ProgressMode::Value => value.clamp(0.0, 1.0),
                };
                let tc = apply_alpha(premul_linear(track.r, track.g, track.b, track.a), content_alpha);
                rects.push(RectInstance { rect: sr, color: tc, meta: [h * 0.5, KIND_FILL, 0.0, 0.0] });
                let fw = (sr[2] * frac).max(h);
                let fc = apply_alpha(premul_linear(fill.r, fill.g, fill.b, fill.a), content_alpha);
                rects.push(RectInstance {
                    rect: [sr[0], sr[1], fw, h],
                    color: fc,
                    meta: [h * 0.5, KIND_FILL, 0.0, 0.0],
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

    // Re-arm the next frame callback BEFORE present (present commits the surface). Keep animating
    // while a marquee scrolls OR a lifetime bar is still counting down (so it depletes smoothly);
    // once both are done and the springs settle, the loop idles → 0% GPU.
    let lifetime_running = island.layout.lifetime_active()
        && island.anim.phase != crate::phase::Phase::Exit
        && lifetime_fraction(island) > 0.0;
    if island.anim.needs_frame(island.layout.marquee_active() || lifetime_running) {
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
        surface: &wl_surface::WlSurface,
        time: u32,
    ) {
        self.advance_island(surface, time);
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
        if let Some(idx) = self.island_at(layer.wl_surface()) {
            self.islands.remove(idx);
            if self.islands.is_empty() && self.shutting_down {
                self.exit = true;
            }
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(idx) = self.island_at(layer.wl_surface()) else { return };
        let (mut w, mut h) = configure.new_size;
        let island = &self.islands[idx];
        if w == 0 {
            w = island.surf_w;
        }
        if h == 0 {
            h = island.surf_h;
        }
        tracing::debug!(target: "render", "configure {w}x{h} (requested {:?})", configure.new_size);
        self.configure_surface(idx, w, h);
        // First paint (and re-arm the frame loop). dt is established on the first real callback.
        self.paint_idx(idx);
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    /// A monitor connected. If a notification is live and this output is in scope (`all`, or the
    /// named connector), spawn a matching surface so it joins mid-life with its own slide-in.
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let Some(active) = self.active.as_ref() else { return };
        let wanted = match &self.monitor {
            MonitorSelect::All => true,
            MonitorSelect::Named(name) => {
                self.output_state.info(&output).and_then(|i| i.name).as_deref() == Some(name)
            }
            // `Auto` left the placement to the compositor — a new monitor doesn't add a surface.
            MonitorSelect::Auto => false,
        };
        if !wanted {
            return;
        }
        // Don't double up if a surface already targets this output.
        if self.islands.iter().any(|i| i.output.as_ref() == Some(&output)) {
            return;
        }
        let active = ActiveNotif {
            id: active.id,
            timeout_ms: active.timeout_ms,
            scene: active.scene.clone(),
            style: active.style.clone(),
            anim: active.anim,
        };
        match self.spawn_island(Some(output), &active) {
            Ok(island) => self.islands.push(island),
            Err(e) => tracing::error!(target: "render", "spawn island on hotplug failed: {e}"),
        }
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    /// A monitor disconnected: drop any surface pinned to it. (The compositor also sends a layer
    /// `closed` for it; both paths are idempotent.)
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.islands.retain(|i| i.output.as_ref() != Some(&output));
        if self.islands.is_empty() && self.shutting_down {
            self.exit = true;
        }
    }
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

#[cfg(test)]
mod tests {
    use super::MonitorSelect;

    #[test]
    fn monitor_select_parse() {
        assert!(matches!(MonitorSelect::parse("all"), MonitorSelect::All));
        assert!(matches!(MonitorSelect::parse("  all "), MonitorSelect::All));
        assert!(matches!(MonitorSelect::parse("auto"), MonitorSelect::Auto));
        assert!(matches!(MonitorSelect::parse(""), MonitorSelect::Auto));
        match MonitorSelect::parse("DP-1") {
            MonitorSelect::Named(n) => assert_eq!(n, "DP-1"),
            _ => panic!("expected Named(DP-1)"),
        }
    }
}
