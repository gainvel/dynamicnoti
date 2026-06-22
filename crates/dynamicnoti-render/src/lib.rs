//! dynamicnoti-render — the Dynamic Island UI. MAIN-THREAD ONLY.
//!
//! Owns the wlr-layer-shell surface (via smithay-client-toolkit), the wgpu device/queue/
//! surface, the glyphon text atlas, the image cache, and live spring state. Every type in
//! here is effectively `!Send` — NEVER hand one to a tokio task. The async sources reach the
//! loop only through a `calloop::channel`.
//!
//! ## Render-loop rules (violating these = blank surface or frozen animation)
//! 1. A layer surface starts 0x0. Wait for the first `configure`, ack it, set size, THEN
//!    create/configure the wgpu surface. Do not render before the first configure ack.
//! 2. Drive animation off Wayland frame callbacks. After each commit that is NOT fully
//!    settled, request the next frame. When `!any_spring_unsettled && !marquee_active`, stop
//!    requesting frames -> 0% GPU. A new NotificationEvent re-arms the loop.
//! 3. Use `wgpu::PresentMode::Fifo` (vsync) to stay locked to frame callbacks.
//! 4. Anchor TOP only (centered island), `exclusive_zone = -1`, layer OVERLAY,
//!    `keyboard_interactivity = None`.
//! 5. Prefer a low-power adapter — this is a tiny always-on overlay; don't wake the dGPU hard.
//!
//! The pure, GPU-free pieces — [`layout`] (Scene → positioned rects) and [`phase`] (the spring
//! state machine) — are unit-tested without a compositor. Everything else needs a live
//! Wayland + GPU session.

mod app;
mod blur;
mod gpu;
mod image_cache;
mod layout;
mod phase;
mod text;

use dynamicnoti_core::scene::Scene;
use dynamicnoti_core::style::{ResolvedAnimProfile, ResolvedStyle};
use dynamicnoti_core::ImageData;

/// Lifecycle of one on-screen surface. Maps to spring targets (see dynamicnoti-anim).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Spawning: small + faint -> measured size, scale 1, opacity 1.
    Enter,
    /// Settled: only marquee/progress move.
    Idle,
    /// Content swap: crossfade out, swap Scene at midpoint, geometry-spring to new size.
    Morph,
    /// Dismissing: opacity/scale/height collapse, then destroy the surface.
    Exit,
}

/// Events the async side pushes into the main loop via `calloop::channel`. The daemon's queue
/// manager has already arbitrated which surface is live and resolved the per-notification
/// `style`/`anim` on the tokio side, so these are concrete surface commands keyed by id.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum NotificationEvent {
    /// Spawn a fresh surface showing `scene`. `timeout_ms` (0 = sticky) drives the lifetime bar.
    Show { id: u64, timeout_ms: u32, scene: Scene, style: ResolvedStyle, anim: ResolvedAnimProfile },
    /// Swap the live surface's content in place — the signature island morph.
    Morph { id: u64, timeout_ms: u32, scene: Scene, style: ResolvedStyle, anim: ResolvedAnimProfile },
    /// Tear down the surface with this id.
    Close { id: u64 },
    /// Decoded image bytes ready for GPU upload, keyed by the scene's `Image` handle. Lets the
    /// renderer reveal album art that a source fetched asynchronously after the `Show`/`Morph`.
    ImageReady { key: String, image: ImageData },
    /// Config/theme/types changed on disk; swap the active config.
    ConfigReloaded,
    /// Graceful shutdown: collapse and exit the loop.
    Shutdown,
}

/// Events the main loop sends back to the async side (zbus signals, action results). Wired but
/// unused until actions land; the freedesktop source (step 4) drains these onto D-Bus signals.
#[derive(Debug)]
pub enum OutboundEvent {
    Closed { id: u64, reason: u32 },
    ActionInvoked { id: u64, action_key: String },
}

/// Entry point the daemon calls on the main thread. Owns calloop + wgpu; returns only on
/// shutdown. `rx` is the tokio→main event channel; `outbound` is the main→tokio return path.
/// `monitor` is the config's output selection (`"all"` | `"auto"` | a connector name).
pub fn run(
    rx: calloop::channel::Channel<NotificationEvent>,
    outbound: flume::Sender<OutboundEvent>,
    monitor: String,
) -> anyhow::Result<()> {
    app::run(rx, outbound, monitor)
}
