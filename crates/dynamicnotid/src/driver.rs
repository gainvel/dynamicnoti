//! The tokio-side pipeline. Owns the config artifacts and the queue manager; runs every source
//! task; drives `RawNotification → resolve → bind → build → QueueManager → render event`.
//!
//! The queue manager is pure and `Send`, so it (and bind/build, which are pure too) live here
//! on the tokio thread — the main thread does only the (later) `!Send` rendering. Only finished
//! [`NotificationEvent`]s, carrying an already-built `Scene`, cross to main.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dynamicnoti_core::queue::{CloseReason, QueueInput, QueueManager, QueueOutput, QueueSettings};
use dynamicnoti_core::{bind, scene, Config, RawNotification, Theme, TypeResolver, TypeTemplate};
use dynamicnoti_render::{NotificationEvent, OutboundEvent};
use dynamicnoti_sources::{ipc, watcher, FdSignal, FdSignalSender, PostReply, Reloaded, SourceMsg};
use tokio::sync::mpsc;

/// Embedded fallback types so the daemon always has at least `generic`/`song`/`deal`, even
/// before the user copies `config.example/` into their config dir.
const GENERIC: &str = include_str!("../../../config.example/types/generic.toml");
const SONG: &str = include_str!("../../../config.example/types/song.toml");
const DEAL: &str = include_str!("../../../config.example/types/deal.toml");

pub struct Paths {
    pub config_dir: PathBuf,
    pub socket: PathBuf,
}

/// Owns all mutable pipeline state on the tokio thread.
struct Driver {
    config: Config,
    theme: Theme,
    resolver: TypeResolver,
    queue: QueueManager,
    next_id: u64,
    start: Instant,
    config_dir: PathBuf,
    to_main: calloop::channel::Sender<NotificationEvent>,
    /// Asks the freedesktop server to emit `NotificationClosed`/`ActionInvoked` on D-Bus.
    fd_tx: FdSignalSender,
}

/// Map a queue close reason onto a freedesktop `NotificationClosed` reason code.
fn reason_code(reason: CloseReason) -> u32 {
    match reason {
        CloseReason::Timeout => 1,   // expired
        CloseReason::Explicit => 3,  // closed by CloseNotification
        CloseReason::Replaced | CloseReason::Preempted => 4, // undefined/other
    }
}

impl Driver {
    fn now(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn handle(&mut self, msg: SourceMsg) {
        match msg {
            SourceMsg::Post { raw, reply } => self.post(raw, reply),
            SourceMsg::Close { replace_key } => {
                let now = self.now();
                let outs = self.queue.handle(QueueInput::Close { replace_key }, now);
                self.emit(outs);
            }
            // Decoded art bypasses the queue/bind/build pipeline — it's just bytes the renderer
            // uploads and associates with the matching `Image` handle.
            SourceMsg::ImageReady { key, image } => {
                if self.to_main.send(NotificationEvent::ImageReady { key, image }).is_err() {
                    tracing::warn!(target: "driver", "render channel closed (image)");
                }
            }
            SourceMsg::ConfigChanged(which) => self.reload(which),
        }
    }

    fn post(&mut self, raw: RawNotification, reply: Option<PostReply>) {
        // Suppress a player's own freedesktop toast when the MPRIS card already covers it (e.g.
        // Cider) — avoids a duplicate notification. The freedesktop server has already answered
        // the D-Bus caller with an id, so we just decline to show it.
        if raw.source == dynamicnoti_core::SourceKind::FreeDesktop
            && self
                .config
                .sources
                .mpris
                .suppress_freedesktop_from
                .iter()
                .any(|a| a.eq_ignore_ascii_case(&raw.app_name))
        {
            tracing::debug!(target: "driver", "suppressing freedesktop notification from {}", raw.app_name);
            if let Some(r) = reply {
                let _ = r.send(Ok(0));
            }
            return;
        }

        let template = match self.resolver.resolve(raw.requested_type.as_deref(), raw.source) {
            Ok(t) => t,
            Err(e) => return reply_err(reply, e.to_string()),
        };

        // Fault boundary #2: bind validates/coerces user-supplied fields.
        let bound = match catch_unwind(AssertUnwindSafe(|| bind(template, raw))) {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => return reply_err(reply, e.to_string()),
            Err(_) => {
                tracing::error!(target: "driver", "bind panicked");
                return reply_err(reply, "internal error during bind".into());
            }
        };

        let ov = bound.template.overrides.as_ref();
        let priority = ov.and_then(|o| o.priority).unwrap_or(bound.behavior.priority);
        let timeout_ms = ov.and_then(|o| o.timeout_ms).unwrap_or(bound.behavior.timeout_ms);
        let style = self.theme.resolve_style(ov);
        // The renderer is theming-agnostic: resolve the spring profile here and ship it alongside
        // the Scene so render just converts presets → springs.
        let anim = self.theme.resolve_anim(&bound.template.meta.anim_profile, ov);

        // Fault boundary #2 (cont.): scene construction.
        let built = catch_unwind(AssertUnwindSafe(|| scene::build(&bound, &style)));
        let scene = match built {
            Ok(s) => s,
            Err(_) => {
                tracing::error!(target: "driver", "scene::build panicked");
                return reply_err(reply, "internal error during build".into());
            }
        };

        let id = self.next_id;
        self.next_id += 1;
        if let Some(r) = reply {
            let _ = r.send(Ok(id));
        }

        let now = self.now();
        let outs = self.queue.handle(
            QueueInput::Post {
                id,
                replace_key: bound.replace_key.clone(),
                priority,
                timeout_ms,
                scene,
                style,
                anim,
            },
            now,
        );
        self.emit(outs);
    }

    fn tick(&mut self) {
        let now = self.now();
        let outs = self.queue.handle(QueueInput::Tick, now);
        self.emit(outs);
    }

    fn emit(&self, outs: Vec<QueueOutput>) {
        for o in outs {
            let ev = match o {
                QueueOutput::Show { id, timeout_ms, scene, style, anim } => {
                    NotificationEvent::Show { id, timeout_ms, scene, style, anim }
                }
                QueueOutput::Morph { id, timeout_ms, scene, style, anim } => {
                    NotificationEvent::Morph { id, timeout_ms, scene, style, anim }
                }
                QueueOutput::Close { id, reason, replace_key } => {
                    tracing::debug!(target: "driver", "close #{id}: {reason:?}");
                    // Route freedesktop-originated closes back to a D-Bus NotificationClosed.
                    if let Some(fd_id) = replace_key
                        .as_deref()
                        .and_then(|k| k.strip_prefix("freedesktop:"))
                        .and_then(|n| n.parse::<u32>().ok())
                    {
                        let _ = self.fd_tx.send(FdSignal::Closed { id: fd_id, reason: reason_code(reason) });
                    }
                    NotificationEvent::Close { id }
                }
            };
            if self.to_main.send(ev).is_err() {
                tracing::warn!(target: "driver", "render channel closed");
            }
        }
    }

    fn reload(&mut self, which: Reloaded) {
        match which {
            Reloaded::Config => {
                self.config = load_config(&self.config_dir);
                self.queue.set_settings(queue_settings(&self.config));
                tracing::info!(target: "driver", "config.toml reloaded");
            }
            Reloaded::Theme => {
                self.theme = load_theme(&self.config_dir);
                tracing::info!(target: "driver", "theme.toml reloaded");
            }
            Reloaded::Types => {
                self.resolver = load_resolver(&self.config_dir);
                tracing::info!(target: "driver", "types reloaded");
            }
        }
        let _ = self.to_main.send(NotificationEvent::ConfigReloaded);
    }
}

fn reply_err(reply: Option<PostReply>, message: String) {
    tracing::warn!(target: "driver", "rejecting notification: {message}");
    if let Some(r) = reply {
        let _ = r.send(Err(message));
    }
}

pub async fn run(
    to_main: calloop::channel::Sender<NotificationEvent>,
    outbound: flume::Receiver<OutboundEvent>,
    paths: Paths,
) -> anyhow::Result<()> {
    let config = load_config(&paths.config_dir);
    let theme = load_theme(&paths.config_dir);
    let resolver = load_resolver(&paths.config_dir);

    let (src_tx, mut src_rx) = mpsc::unbounded_channel::<SourceMsg>();
    let (fd_tx, fd_rx) = mpsc::unbounded_channel::<FdSignal>();

    // Forward the renderer's outbound events (actions, user-dismissals) to the freedesktop server.
    {
        let fd_tx = fd_tx.clone();
        tokio::spawn(async move {
            while let Ok(ev) = outbound.recv_async().await {
                let sig = match ev {
                    OutboundEvent::Closed { id, reason } => {
                        FdSignal::Closed { id: id as u32, reason }
                    }
                    OutboundEvent::ActionInvoked { id, action_key } => {
                        FdSignal::ActionInvoked { id: id as u32, action_key }
                    }
                };
                let _ = fd_tx.send(sig);
            }
        });
    }

    // IPC source (always on).
    {
        let tx = src_tx.clone();
        let socket = paths.socket.clone();
        tokio::spawn(async move {
            if let Err(e) = ipc::run(socket, tx).await {
                tracing::error!(target: "ipc", "ipc source exited: {e}");
            }
        });
    }
    // Config watcher.
    {
        let tx = src_tx.clone();
        let dir = paths.config_dir.clone();
        tokio::spawn(async move {
            if let Err(e) = watcher::run(dir, tx).await {
                tracing::warn!(target: "watcher", "watcher exited: {e}");
            }
        });
    }
    // freedesktop server (real behind `--features dbus`; a no-op seam otherwise). Owns the bus
    // name only when config `take_over` is set.
    {
        let tx = src_tx.clone();
        let cfg = config.sources.freedesktop.clone();
        tokio::spawn(async move {
            if let Err(e) = dynamicnoti_sources::freedesktop::run(tx, fd_rx, cfg).await {
                tracing::error!(target: "freedesktop", "freedesktop source exited: {e}");
            }
        });
    }
    // mpris (Cider song cards) — build sequencing step 5. Real behind `--features dbus`.
    {
        let tx = src_tx.clone();
        let cfg = config.sources.mpris.clone();
        tokio::spawn(async move {
            if let Err(e) = dynamicnoti_sources::mpris::run(tx, cfg).await {
                tracing::warn!(target: "mpris", "mpris source exited: {e}");
            }
        });
    }

    let mut driver = Driver {
        queue: QueueManager::new(queue_settings(&config)),
        config,
        theme,
        resolver,
        next_id: 1,
        start: Instant::now(),
        config_dir: paths.config_dir.clone(),
        to_main: to_main.clone(),
        fd_tx,
    };

    let shutdown = wait_for_shutdown();
    tokio::pin!(shutdown);

    tracing::info!(target: "driver", "pipeline ready");

    loop {
        let deadline = driver.queue.next_deadline();
        let now = driver.now();
        tokio::select! {
            _ = &mut shutdown => {
                tracing::info!(target: "driver", "shutdown signal received");
                break;
            }
            msg = src_rx.recv() => {
                match msg {
                    Some(msg) => driver.handle(msg),
                    None => break,
                }
            }
            _ = sleep_until(deadline, now) => driver.tick(),
        }
    }

    // Tell the renderer to wind down so the main thread's loop returns.
    let _ = to_main.send(NotificationEvent::Shutdown);
    Ok(())
}

/// Sleep until `deadline` (absolute ms), or forever when there is no timer to arm.
async fn sleep_until(deadline: Option<u64>, now: u64) {
    match deadline {
        Some(d) => tokio::time::sleep(Duration::from_millis(d.saturating_sub(now))).await,
        None => std::future::pending::<()>().await,
    }
}

/// Resolve on SIGINT or SIGTERM.
async fn wait_for_shutdown() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

fn queue_settings(config: &Config) -> QueueSettings {
    QueueSettings { policy: config.queue.policy, coalesce_replace: config.queue.coalesce_replace }
}

fn load_config(dir: &Path) -> Config {
    match std::fs::read_to_string(dir.join("config.toml")) {
        Ok(s) => Config::from_toml(&s).unwrap_or_else(|e| {
            tracing::warn!(target: "driver", "bad config.toml ({e}); using defaults");
            Config::default()
        }),
        Err(_) => Config::default(),
    }
}

fn load_theme(dir: &Path) -> Theme {
    match std::fs::read_to_string(dir.join("theme.toml")) {
        Ok(s) => Theme::from_toml(&s).unwrap_or_else(|e| {
            tracing::warn!(target: "driver", "bad theme.toml ({e}); using defaults");
            Theme::default()
        }),
        Err(_) => Theme::default(),
    }
}

fn load_resolver(dir: &Path) -> TypeResolver {
    let types_dir = dir.join("types");
    if types_dir.is_dir() {
        match TypeResolver::load_dir(&types_dir) {
            Ok(r) => return r,
            Err(e) => tracing::warn!(target: "driver", "types dir unusable ({e}); using built-ins"),
        }
    }
    default_resolver()
}

fn default_resolver() -> TypeResolver {
    let templates = [GENERIC, SONG, DEAL]
        .iter()
        .filter_map(|s| TypeTemplate::from_toml(s).ok())
        .collect();
    TypeResolver::from_templates(templates).expect("built-in types include generic")
}

#[cfg(test)]
mod tests {
    use super::*;
    use dynamicnoti_core::scene::Value;
    use dynamicnoti_core::SourceKind;
    use std::collections::HashMap;

    /// Build a Driver wired to a fresh calloop channel; returns it plus the receiver so the
    /// test can drain emitted events GPU-free (exactly the headless path).
    fn driver_and_rx() -> (Driver, calloop::channel::Channel<NotificationEvent>) {
        let (to_main, rx) = calloop::channel::channel::<NotificationEvent>();
        let config = Config::default();
        let driver = Driver {
            queue: QueueManager::new(queue_settings(&config)),
            config,
            theme: Theme::default(),
            resolver: default_resolver(),
            next_id: 1,
            start: Instant::now(),
            config_dir: PathBuf::from("/nonexistent"),
            to_main,
            fd_tx: mpsc::unbounded_channel().0,
        };
        (driver, rx)
    }

    /// Dispatch the calloop channel once and collect whatever events are queued.
    fn drain(rx: calloop::channel::Channel<NotificationEvent>) -> Vec<NotificationEvent> {
        use calloop::channel::Event;
        let mut event_loop = calloop::EventLoop::<Vec<NotificationEvent>>::try_new().unwrap();
        event_loop
            .handle()
            .insert_source(rx, |event, _, out: &mut Vec<NotificationEvent>| {
                if let Event::Msg(ev) = event {
                    out.push(ev);
                }
            })
            .unwrap();
        let mut out = Vec::new();
        event_loop.dispatch(Some(Duration::from_millis(10)), &mut out).unwrap();
        out
    }

    fn post(kind: &str, fields: &[(&str, Value)], replace_key: Option<&str>) -> SourceMsg {
        SourceMsg::Post {
            raw: RawNotification {
                source: SourceKind::Ipc,
                app_name: "test".into(),
                requested_type: Some(kind.into()),
                replace_key: replace_key.map(str::to_string),
                fields: fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect::<HashMap<_, _>>(),
            },
            reply: None,
        }
    }

    #[test]
    fn generic_post_emits_show() {
        let (mut d, rx) = driver_and_rx();
        d.handle(post("generic", &[("title", Value::Text("hi".into()))], None));
        let evs = drain(rx);
        assert!(matches!(evs.as_slice(), [NotificationEvent::Show { id: 1, .. }]), "{evs:?}");
    }

    #[test]
    fn same_replace_key_morphs() {
        let (mut d, rx) = driver_and_rx();
        d.handle(post("song", &[("title", Value::Text("a".into()))], Some("k")));
        d.handle(post("song", &[("title", Value::Text("b".into()))], Some("k")));
        let evs = drain(rx);
        assert!(
            matches!(
                evs.as_slice(),
                [NotificationEvent::Show { id: 1, .. }, NotificationEvent::Morph { id: 1, .. }]
            ),
            "{evs:?}"
        );
    }

    #[test]
    fn explicit_close_emits_close() {
        let (mut d, rx) = driver_and_rx();
        d.handle(post("song", &[("title", Value::Text("a".into()))], Some("k")));
        d.handle(SourceMsg::Close { replace_key: "k".into() });
        let evs = drain(rx);
        assert!(
            matches!(
                evs.as_slice(),
                [NotificationEvent::Show { id: 1, .. }, NotificationEvent::Close { id: 1 }]
            ),
            "{evs:?}"
        );
    }

    #[test]
    fn suppressed_freedesktop_app_emits_nothing() {
        let (to_main, rx) = calloop::channel::channel::<NotificationEvent>();
        let mut config = Config::default();
        config.sources.mpris.suppress_freedesktop_from = vec!["Cider".into()];
        let mut d = Driver {
            queue: QueueManager::new(queue_settings(&config)),
            config,
            theme: Theme::default(),
            resolver: default_resolver(),
            next_id: 1,
            start: Instant::now(),
            config_dir: PathBuf::from("/nonexistent"),
            to_main,
            fd_tx: mpsc::unbounded_channel().0,
        };
        d.handle(SourceMsg::Post {
            raw: RawNotification {
                source: SourceKind::FreeDesktop,
                app_name: "cider".into(), // case-insensitive match
                requested_type: None,
                replace_key: Some("freedesktop:7".into()),
                fields: [("title".to_string(), Value::Text("hi".into()))].into_iter().collect(),
            },
            reply: None,
        });
        assert!(drain(rx).is_empty(), "suppressed app must not produce any render event");
    }

    #[test]
    fn missing_required_field_is_rejected_via_reply() {
        let (mut d, _rx) = driver_and_rx();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        d.handle(SourceMsg::Post {
            raw: RawNotification {
                source: SourceKind::Ipc,
                app_name: "t".into(),
                requested_type: Some("generic".into()),
                replace_key: None,
                fields: HashMap::new(), // missing required `title`
            },
            reply: Some(tx),
        });
        // The reply must be an Err, and nothing should have been shown.
        assert!(matches!(rx.try_recv(), Ok(Err(_))));
    }
}
