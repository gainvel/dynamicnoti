//! dynamicnotid — the daemon. Wires the two-thread architecture together.
//!
//! THREADING (the #1 source of mistakes — see CLAUDE.md):
//!   Main thread  = the render loop. Owns calloop + Wayland + wgpu + anim state. All !Send.
//!                  This milestone runs a HEADLESS renderer (logs scenes; no GPU).
//!   Worker thread = a tokio runtime running every Source + the config watcher + the queue
//!                   driver. All async/Send.
//!   Bridge: calloop::channel (tokio -> main, wakes the loop). The flume return path (main ->
//!           tokio) lands with the freedesktop source (build sequencing step 4).
//!
//! `main()` MUST stay free for calloop/wgpu — do NOT slap `#[tokio::main]` on it. The runtime
//! runs on a std::thread instead.
//!
//! FAULT ISOLATION (we own org.freedesktop.Notifications; a crash drops ALL notifications):
//!   - per-message source ingestion (in dynamicnoti-sources::ipc)  -- boundary #1
//!   - bind() + scene::build() before display (in driver.rs)       -- boundary #2
//!   - per-surface render (headless stand-in in headless.rs)       -- boundary #3
//!
//! Release profile uses panic = "unwind" so catch_unwind works (set in workspace Cargo.toml).

mod driver;
mod headless;
mod lock;

use std::path::PathBuf;

use dynamicnoti_core::config::expand_xdg;
use dynamicnoti_core::Config;
use dynamicnoti_render::{NotificationEvent, OutboundEvent};

fn main() -> anyhow::Result<()> {
    // This is a tiny always-on Wayland overlay — a Vulkan HUD (MangoHUD) drawing its FPS/temps
    // on top of the notification island is never wanted. The user's environment exports
    // MANGOHUD=1 globally, so neutralize it for our process before wgpu creates the Vulkan
    // instance (down in `dynamicnoti_render::run`). Setting both the implicit-layer toggle and
    // MangoHUD's own kill switch covers either activation path. No other threads run yet (the
    // tokio worker is spawned further down), and the crate is edition 2021 where set_var is safe.
    std::env::set_var("MANGOHUD", "0");
    std::env::set_var("DISABLE_MANGOHUD", "1");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dynamicnotid=info,dynamicnoti_sources=info".into()),
        )
        // Daemon logs go to stderr (unbuffered) so they're visible immediately when piped.
        .with_writer(std::io::stderr)
        .init();

    std::panic::set_hook(Box::new(|info| {
        // Global hook: log every panic. The catch_unwind fences keep individual panics from
        // taking down the loop, but we still want a record.
        tracing::error!(target: "dynamicnotid", "panic: {info}");
    }));

    let config_dir = config_dir();
    let runtime_dir = runtime_dir();
    let lock_path = runtime_dir.join("dynamicnoti.lock");

    // The socket path comes from config (default `$XDG_RUNTIME_DIR/dynamicnoti.sock`).
    let socket = startup_config(&config_dir).socket_path();
    let socket = PathBuf::from(socket);

    // Single-instance guard: a second daemon would fight over the socket and (later) the bus name.
    let _lock = match lock::InstanceLock::acquire(&lock_path)? {
        Some(l) => l,
        None => {
            tracing::error!(target: "dynamicnotid", "another dynamicnotid is already running");
            std::process::exit(1);
        }
    };

    tracing::info!(target: "dynamicnotid", "starting (config dir {config_dir:?}, socket {socket:?})");

    // tokio -> main bridge (events) and the main -> tokio return path (flume, for D-Bus signals).
    let (to_main, rx) = calloop::channel::channel::<NotificationEvent>();
    let (outbound_tx, outbound_rx) = flume::unbounded::<OutboundEvent>();

    // Spawn the tokio runtime on its own thread; the driver runs there.
    let paths = driver::Paths { config_dir, socket };
    let worker = std::thread::Builder::new()
        .name("dynamicnoti-tokio".into())
        .spawn(move || -> anyhow::Result<()> {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(driver::run(to_main, outbound_rx, paths))
        })?;

    // The renderer owns the main thread until shutdown. `DYNAMICNOTI_HEADLESS=1` selects the
    // GPU-free scene logger instead (CI / no-compositor smoke).
    if std::env::var_os("DYNAMICNOTI_HEADLESS").is_some() {
        headless::run(rx)?;
    } else if let Err(e) = dynamicnoti_render::run(rx, outbound_tx) {
        tracing::error!(target: "dynamicnotid", "render loop failed: {e}");
    }

    // Headless returned (shutdown). Join the worker so its sockets/tasks wind down cleanly.
    match worker.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!(target: "dynamicnotid", "driver error: {e}"),
        Err(_) => tracing::error!(target: "dynamicnotid", "tokio thread panicked"),
    }

    tracing::info!(target: "dynamicnotid", "stopped");
    // _lock drops here: releases the flock and removes the lockfile. Exit 0 so the supervisor
    // (dist/dynamicnotid-supervise) treats this as a clean shutdown and stops respawning.
    Ok(())
}

/// `$XDG_CONFIG_HOME/dynamicnoti` or `~/.config/dynamicnoti`.
fn config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("dynamicnoti");
        }
    }
    PathBuf::from(expand_xdg("$HOME/.config")).join("dynamicnoti")
}

/// `$XDG_RUNTIME_DIR`, falling back to `/tmp`.
fn runtime_dir() -> PathBuf {
    PathBuf::from(std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string()))
}

/// Load config once at startup just to learn the socket path; the driver owns its own copy and
/// hot-reloads it. A broken/absent file yields defaults.
fn startup_config(dir: &std::path::Path) -> Config {
    match std::fs::read_to_string(dir.join("config.toml")) {
        Ok(s) => Config::from_toml(&s).unwrap_or_default(),
        Err(_) => Config::default(),
    }
}
