//! freedesktop source (build sequencing step 4). Behind the `dbus` feature: a zbus server that
//! implements `org.freedesktop.Notifications` (Notify / CloseNotification / GetCapabilities /
//! GetServerInformation) and emits NotificationClosed / ActionInvoked. Owning the bus name is
//! gated by config (`take_over`, off by default at the daemon level) so KDE keeps notifications
//! until the user opts in; `NameLost` (KDE reclaiming on Plasma restart) is handled with backoff.

/// Entry point. Without the `dbus` feature this is a no-op seam so the daemon still links.
#[cfg(not(feature = "dbus"))]
pub async fn run(
    _tx: crate::SourceSender,
    _signals: crate::FdSignalReceiver,
    _cfg: dynamicnoti_core::config::FreedesktopConfig,
) -> anyhow::Result<()> {
    tracing::debug!(target: "freedesktop", "dbus feature off — freedesktop source disabled");
    Ok(())
}

#[cfg(feature = "dbus")]
pub use imp::run;

#[cfg(feature = "dbus")]
mod imp {
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use dynamicnoti_core::config::FreedesktopConfig;
    use dynamicnoti_core::scene::Value;
    use dynamicnoti_core::{RawNotification, SourceKind};
    use tokio::sync::oneshot;
    use zbus::fdo::RequestNameFlags;
    use zbus::object_server::SignalEmitter;
    use zbus::zvariant::OwnedValue;
    use zbus::{interface, Connection};

    use crate::{FdSignal, FdSignalReceiver, SourceMsg, SourceSender};

    const PATH: &str = "/org/freedesktop/Notifications";
    const NAME: &str = "org.freedesktop.Notifications";

    struct Notifications {
        tx: SourceSender,
        next_id: Arc<AtomicU32>,
        live: Arc<Mutex<HashSet<u32>>>,
    }

    #[interface(name = "org.freedesktop.Notifications")]
    impl Notifications {
        /// The core method. We assign our own D-Bus id (or reuse `replaces_id`) and route the
        /// notification through the same pipeline as every other source, keyed by a
        /// `freedesktop:<id>` replace_key so CloseNotification / NotificationClosed line up.
        #[allow(clippy::too_many_arguments)]
        async fn notify(
            &self,
            app_name: String,
            replaces_id: u32,
            app_icon: String,
            summary: String,
            body: String,
            _actions: Vec<String>,
            hints: HashMap<String, OwnedValue>,
            _expire_timeout: i32,
        ) -> zbus::fdo::Result<u32> {
            let id = if replaces_id != 0 {
                replaces_id
            } else {
                self.next_id.fetch_add(1, Ordering::Relaxed)
            };

            let mut fields: HashMap<String, Value> = HashMap::new();
            fields.insert("title".into(), Value::Text(summary));
            if !body.is_empty() {
                fields.insert("body".into(), Value::Text(body));
            }
            if let Some(icon) = image_from_hints(&hints, &app_icon) {
                fields.insert("icon".into(), Value::Image(icon));
            }

            let raw = RawNotification {
                source: SourceKind::FreeDesktop,
                app_name,
                requested_type: None,
                replace_key: Some(format!("freedesktop:{id}")),
                fields,
            };

            let (reply_tx, reply_rx) = oneshot::channel();
            if self.tx.send(SourceMsg::Post { raw, reply: Some(reply_tx) }).is_err() {
                return Err(zbus::fdo::Error::Failed("notification pipeline closed".into()));
            }
            match reply_rx.await {
                Ok(Ok(_internal_id)) => {
                    self.live.lock().unwrap().insert(id);
                    Ok(id)
                }
                Ok(Err(reason)) => Err(zbus::fdo::Error::Failed(reason)),
                Err(_) => Err(zbus::fdo::Error::Failed("no reply from pipeline".into())),
            }
        }

        async fn close_notification(&self, id: u32) {
            let _ = self.tx.send(SourceMsg::Close { replace_key: format!("freedesktop:{id}") });
        }

        fn get_capabilities(&self) -> Vec<String> {
            vec![
                "body".into(),
                "body-markup".into(),
                "icon-static".into(),
                "actions".into(),
            ]
        }

        fn get_server_information(&self) -> (String, String, String, String) {
            (
                "dynamicnoti".into(),
                "dynamicnoti".into(),
                env!("CARGO_PKG_VERSION").into(),
                "1.2".into(),
            )
        }

        #[zbus(signal)]
        async fn notification_closed(emitter: &SignalEmitter<'_>, id: u32, reason: u32) -> zbus::Result<()>;

        #[zbus(signal)]
        async fn action_invoked(
            emitter: &SignalEmitter<'_>,
            id: u32,
            action_key: String,
        ) -> zbus::Result<()>;
    }

    /// Pull an image handle out of the `image-path` hint or the `app_icon` argument (a path or a
    /// freedesktop icon name). Raw `image-data` byte arrays are a documented follow-up.
    fn image_from_hints(hints: &HashMap<String, OwnedValue>, app_icon: &str) -> Option<String> {
        for key in ["image-path", "image_path"] {
            if let Some(v) = hints.get(key) {
                if let Ok(s) = String::try_from(v.try_clone().ok()?) {
                    if !s.is_empty() {
                        return Some(s);
                    }
                }
            }
        }
        (!app_icon.is_empty()).then(|| app_icon.to_string())
    }

    pub async fn run(
        tx: SourceSender,
        mut signals: FdSignalReceiver,
        cfg: FreedesktopConfig,
    ) -> anyhow::Result<()> {
        let live: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));
        let iface = Notifications {
            tx,
            next_id: Arc::new(AtomicU32::new(1)),
            live: Arc::clone(&live),
        };

        // Set up the object server first, then request the name (so no early method calls are
        // lost). Without `take_over` we serve the interface but never grab the name — KDE keeps
        // ownership and our server is dormant until the user opts in.
        let conn = zbus::connection::Builder::session()?
            .serve_at(PATH, iface)?
            .build()
            .await?;

        if cfg.take_over {
            let mut flags = RequestNameFlags::ReplaceExisting | RequestNameFlags::DoNotQueue;
            if cfg.replace_existing {
                flags |= RequestNameFlags::AllowReplacement;
            }
            match conn.request_name_with_flags(NAME, flags).await {
                Ok(reply) => tracing::info!(target: "freedesktop", "requested {NAME}: {reply:?}"),
                Err(e) => tracing::error!(target: "freedesktop", "could not take over {NAME}: {e}"),
            }
            spawn_name_lost_watch(conn.clone(), flags);
        } else {
            tracing::info!(target: "freedesktop", "serving {NAME} interface; take_over off (KDE keeps the name)");
        }

        // Drain driver→server signals and emit them on D-Bus.
        let emitter = SignalEmitter::new(&conn, PATH)?.into_owned();
        while let Some(sig) = signals.recv().await {
            match sig {
                FdSignal::Closed { id, reason } => {
                    if live.lock().unwrap().remove(&id) {
                        let _ = Notifications::notification_closed(&emitter, id, reason).await;
                    }
                }
                FdSignal::ActionInvoked { id, action_key } => {
                    let _ = Notifications::action_invoked(&emitter, id, action_key).await;
                }
            }
        }
        Ok(())
    }

    /// KDE may reclaim `org.freedesktop.Notifications` on a Plasma restart. Watch for NameLost and
    /// re-request with backoff so we recover ownership.
    fn spawn_name_lost_watch(conn: Connection, flags: enumflags2::BitFlags<RequestNameFlags>) {
        tokio::spawn(async move {
            use futures_util::StreamExt;
            let proxy = match zbus::fdo::DBusProxy::new(&conn).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(target: "freedesktop", "no DBus proxy for NameLost: {e}");
                    return;
                }
            };
            let mut lost = match proxy.receive_name_lost().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(target: "freedesktop", "cannot watch NameLost: {e}");
                    return;
                }
            };
            while let Some(sig) = lost.next().await {
                let Ok(args) = sig.args() else { continue };
                if args.name.as_str() != NAME {
                    continue;
                }
                tracing::warn!(target: "freedesktop", "lost {NAME}; re-requesting in 1s");
                tokio::time::sleep(Duration::from_secs(1)).await;
                match conn.request_name_with_flags(NAME, flags).await {
                    Ok(reply) => tracing::info!(target: "freedesktop", "re-acquired {NAME}: {reply:?}"),
                    Err(e) => tracing::error!(target: "freedesktop", "re-acquire failed: {e}"),
                }
            }
        });
    }
}
