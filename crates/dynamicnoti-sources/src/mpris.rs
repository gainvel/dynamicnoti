//! mpris source (build sequencing step 5). Behind the `dbus` feature: a zbus client that
//! watches a media player over MPRIS and drives the `song` notification type.
//!
//! Cider's MPRIS bus name churns every launch (`org.mpris.MediaPlayer2.chromium.instanceNNNN`),
//! so we match players by their `Identity` property against a config allowlist and re-enumerate
//! on `NameOwnerChanged` — never by a hardcoded bus name. `PropertiesChanged` bursts are
//! debounced; album art is fetched + decoded off the main thread and shipped as `ImageReady`.
//!
//! The pure field-assembly ([`build_song_fields`]) lives at module top level so it unit-tests
//! without D-Bus; all zbus/network code is gated behind `#[cfg(feature = "dbus")]`.

use std::collections::HashMap;

use dynamicnoti_core::scene::Value;

/// All MPRIS updates collapse onto one live surface (the queue morphs same-key posts).
pub const REPLACE_KEY: &str = "mpris:single";

/// Assemble the `song` type's fields from already-extracted MPRIS metadata. Pure (core types
/// only) so it is testable without a D-Bus connection.
///
/// `position_us`/`length_us` are microseconds (MPRIS `Position` / `mpris:length`); `status` is
/// the raw `PlaybackStatus` ("Playing"/"Paused"/"Stopped"). The art handle is included only when
/// a non-empty URL is present, so a missing `mpris:artUrl` simply drops the image leaf.
pub fn build_song_fields(
    title: &str,
    artists: &[String],
    album: &str,
    art: Option<&str>,
    status: &str,
    position_us: i64,
    length_us: i64,
) -> HashMap<String, Value> {
    let mut fields = HashMap::new();
    fields.insert("title".into(), Value::Text(title.to_string()));
    fields.insert("artist".into(), Value::Text(artists.join(", ")));
    fields.insert("album".into(), Value::Text(album.to_string()));

    if let Some(url) = art {
        if !url.is_empty() {
            fields.insert("art".into(), Value::Image(url.to_string()));
        }
    }

    let fraction = if length_us > 0 {
        (position_us as f64 / length_us as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    fields.insert("position".into(), Value::Float(fraction));

    let status = if status.eq_ignore_ascii_case("playing") { "playing" } else { "paused" };
    fields.insert("status".into(), Value::Text(status.into()));

    fields
}

/// Entry point. Without the `dbus` feature this is a no-op seam so the daemon still links.
#[cfg(not(feature = "dbus"))]
#[allow(unused_variables)]
pub async fn run(
    tx: crate::SourceSender,
    cfg: dynamicnoti_core::config::MprisConfig,
) -> anyhow::Result<()> {
    tracing::debug!(target: "mpris", "dbus feature off — mpris source disabled");
    Ok(())
}

#[cfg(feature = "dbus")]
pub use imp::run;

#[cfg(feature = "dbus")]
mod imp {
    use std::collections::HashSet;
    use std::io::Read;
    use std::time::{Duration, Instant};

    use dynamicnoti_core::config::MprisConfig;
    use dynamicnoti_core::{ImageData, RawNotification, SourceKind};
    use futures_util::StreamExt;
    use zbus::proxy;
    use zbus::zvariant::{OwnedValue, Value as ZValue};
    use zbus::Connection;

    use super::{build_song_fields, REPLACE_KEY};
    use crate::{SourceMsg, SourceSender};

    const NAME_PREFIX: &str = "org.mpris.MediaPlayer2.";
    /// Cap a single art download so a hostile/huge URL can't exhaust memory.
    const MAX_ART_BYTES: u64 = 8 * 1024 * 1024;

    #[proxy(
        interface = "org.mpris.MediaPlayer2",
        default_path = "/org/mpris/MediaPlayer2",
        gen_blocking = false
    )]
    trait MediaPlayer2 {
        #[zbus(property)]
        fn identity(&self) -> zbus::Result<String>;
    }

    #[proxy(
        interface = "org.mpris.MediaPlayer2.Player",
        default_path = "/org/mpris/MediaPlayer2",
        gen_blocking = false
    )]
    trait Player {
        #[zbus(property)]
        fn metadata(&self) -> zbus::Result<std::collections::HashMap<String, OwnedValue>>;
        #[zbus(property)]
        fn playback_status(&self) -> zbus::Result<String>;
        #[zbus(property)]
        fn position(&self) -> zbus::Result<i64>;
        #[zbus(signal)]
        fn seeked(&self, position: i64) -> zbus::Result<()>;
    }

    pub async fn run(tx: SourceSender, cfg: MprisConfig) -> anyhow::Result<()> {
        if cfg.identities.is_empty() {
            tracing::info!(target: "mpris", "no mpris identities configured; source idle");
            return Ok(());
        }
        let conn = Connection::session().await?;
        let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
        let mut owner_changes = dbus.receive_name_owner_changed().await?;
        let mut fetched: HashSet<String> = HashSet::new();
        let mut shown = false;

        tracing::info!(target: "mpris", "watching for players: {:?}", cfg.identities);

        loop {
            match select_player(&conn, &dbus, &cfg).await {
                Some(name) => {
                    watch_player(&conn, &name, &cfg, &tx, &mut owner_changes, &mut fetched, &mut shown)
                        .await;
                }
                None => {
                    // No matching player right now: tear down any stale card and wait for the
                    // next bus name change before re-enumerating.
                    if shown {
                        let _ = tx.send(SourceMsg::Close { replace_key: REPLACE_KEY.into() });
                        shown = false;
                    }
                    if owner_changes.next().await.is_none() {
                        break; // bus gone
                    }
                }
            }
        }
        Ok(())
    }

    /// Enumerate `org.mpris.MediaPlayer2.*` bus names and return the first whose `Identity` is in
    /// the allowlist (case-insensitive).
    async fn select_player(
        conn: &Connection,
        dbus: &zbus::fdo::DBusProxy<'_>,
        cfg: &MprisConfig,
    ) -> Option<String> {
        let names = dbus.list_names().await.ok()?;
        for n in names {
            let name = n.as_str();
            if !name.starts_with(NAME_PREFIX) {
                continue;
            }
            let Ok(root) = MediaPlayer2Proxy::builder(conn)
                .destination(name.to_string())
                .ok()?
                .build()
                .await
            else {
                continue;
            };
            if let Ok(identity) = root.identity().await {
                if cfg.identities.iter().any(|i| i.eq_ignore_ascii_case(&identity)) {
                    tracing::info!(target: "mpris", "matched player '{identity}' at {name}");
                    return Some(name.to_string());
                }
            }
        }
        None
    }

    /// Watch one selected player until it quits. Coalesces metadata/status/seek changes within
    /// `debounce_ms` and emits a `song` post per settled change; closes the card on Stopped/quit.
    #[allow(clippy::too_many_arguments)]
    async fn watch_player(
        conn: &Connection,
        name: &str,
        cfg: &MprisConfig,
        tx: &SourceSender,
        owner_changes: &mut zbus::fdo::NameOwnerChangedStream,
        fetched: &mut HashSet<String>,
        shown: &mut bool,
    ) {
        let player = match PlayerProxy::builder(conn).destination(name.to_string()) {
            Ok(b) => match b.build().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(target: "mpris", "player proxy build failed: {e}");
                    return;
                }
            },
            Err(e) => {
                tracing::warn!(target: "mpris", "bad player destination {name}: {e}");
                return;
            }
        };

        let mut meta_changes = player.receive_metadata_changed().await;
        let mut status_changes = player.receive_playback_status_changed().await;
        let mut seeked = match player.receive_seeked().await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(target: "mpris", "no Seeked signal ({e}); continuing");
                // A player without Seeked is fine; we just won't get seek wakeups.
                return watch_without_seeked(
                    &player, name, cfg, tx, owner_changes, fetched, shown,
                )
                .await;
            }
        };

        let debounce = Duration::from_millis(cfg.debounce_ms.max(1));
        let mut deadline = Some(Instant::now()); // emit current state immediately on entry
        loop {
            tokio::select! {
                _ = meta_changes.next() => deadline = Some(Instant::now() + debounce),
                _ = status_changes.next() => deadline = Some(Instant::now() + debounce),
                _ = seeked.next() => deadline = Some(Instant::now() + debounce),
                Some(sig) = owner_changes.next() => {
                    if player_quit(&sig, name) {
                        if *shown {
                            let _ = tx.send(SourceMsg::Close { replace_key: REPLACE_KEY.into() });
                            *shown = false;
                        }
                        return;
                    }
                }
                _ = sleep_until_opt(deadline), if deadline.is_some() => {
                    deadline = None;
                    emit_state(&player, tx, fetched, shown).await;
                }
            }
        }
    }

    /// Same as [`watch_player`] minus the Seeked branch (for players that don't expose it).
    #[allow(clippy::too_many_arguments)]
    async fn watch_without_seeked(
        player: &PlayerProxy<'_>,
        name: &str,
        cfg: &MprisConfig,
        tx: &SourceSender,
        owner_changes: &mut zbus::fdo::NameOwnerChangedStream,
        fetched: &mut HashSet<String>,
        shown: &mut bool,
    ) {
        let mut meta_changes = player.receive_metadata_changed().await;
        let mut status_changes = player.receive_playback_status_changed().await;
        let debounce = Duration::from_millis(cfg.debounce_ms.max(1));
        let mut deadline = Some(Instant::now());
        loop {
            tokio::select! {
                _ = meta_changes.next() => deadline = Some(Instant::now() + debounce),
                _ = status_changes.next() => deadline = Some(Instant::now() + debounce),
                Some(sig) = owner_changes.next() => {
                    if player_quit(&sig, name) {
                        if *shown {
                            let _ = tx.send(SourceMsg::Close { replace_key: REPLACE_KEY.into() });
                            *shown = false;
                        }
                        return;
                    }
                }
                _ = sleep_until_opt(deadline), if deadline.is_some() => {
                    deadline = None;
                    emit_state(player, tx, fetched, shown).await;
                }
            }
        }
    }

    /// Read the player's current state and emit a `song` post (or a close on Stopped). Spawns an
    /// off-thread art fetch the first time a given art URL is seen.
    async fn emit_state(
        player: &PlayerProxy<'_>,
        tx: &SourceSender,
        fetched: &mut HashSet<String>,
        shown: &mut bool,
    ) {
        let status = player.playback_status().await.unwrap_or_default();
        if status.eq_ignore_ascii_case("stopped") {
            if *shown {
                let _ = tx.send(SourceMsg::Close { replace_key: REPLACE_KEY.into() });
                *shown = false;
            }
            return;
        }

        let metadata = player.metadata().await.unwrap_or_default();
        let position_us = player.position().await.unwrap_or(0);

        let title = str_field(&metadata, "xesam:title").unwrap_or_default();
        if title.is_empty() {
            return; // the `song` type requires a title — nothing useful to show yet
        }
        let artists = strs_field(&metadata, "xesam:artist");
        let album = str_field(&metadata, "xesam:album").unwrap_or_default();
        let art = str_field(&metadata, "mpris:artUrl").filter(|s| !s.is_empty());
        let length_us = i64_field(&metadata, "mpris:length").unwrap_or(0);

        let fields = build_song_fields(
            &title,
            &artists,
            &album,
            art.as_deref(),
            &status,
            position_us,
            length_us,
        );

        let raw = RawNotification {
            source: SourceKind::Mpris,
            app_name: "mpris".into(),
            requested_type: None, // resolver maps Mpris → "song"
            replace_key: Some(REPLACE_KEY.into()),
            fields,
        };
        if tx.send(SourceMsg::Post { raw, reply: None }).is_err() {
            return;
        }
        *shown = true;

        if let Some(url) = art {
            if fetched.insert(url.clone()) {
                spawn_art_fetch(tx.clone(), url);
            }
        }
    }

    /// Download (http(s)/file/path) + decode album art off the main thread, then ship the RGBA to
    /// the renderer keyed by the same URL the scene's `Image` handle uses.
    fn spawn_art_fetch(tx: SourceSender, url: String) {
        tokio::task::spawn_blocking(move || match fetch_decode(&url) {
            Some(image) => {
                let _ = tx.send(SourceMsg::ImageReady { key: url, image });
            }
            None => tracing::debug!(target: "mpris", "art fetch/decode failed: {url}"),
        });
    }

    fn fetch_decode(url: &str) -> Option<ImageData> {
        let bytes = if let Some(path) = url.strip_prefix("file://") {
            std::fs::read(path).ok()?
        } else if url.starts_with("http://") || url.starts_with("https://") {
            let resp = ureq::get(url).call().ok()?;
            let mut buf = Vec::new();
            resp.into_reader().take(MAX_ART_BYTES).read_to_end(&mut buf).ok()?;
            buf
        } else {
            std::fs::read(url).ok()?
        };
        let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
        let (w, h) = img.dimensions();
        ImageData::from_rgba(w, h, img.into_raw())
    }

    /// True when a NameOwnerChanged signal reports `name` losing its owner (the player quit).
    fn player_quit(sig: &zbus::fdo::NameOwnerChanged, name: &str) -> bool {
        match sig.args() {
            Ok(args) => args.name.as_str() == name && args.new_owner.is_none(),
            Err(_) => false,
        }
    }

    async fn sleep_until_opt(deadline: Option<Instant>) {
        match deadline {
            Some(t) => tokio::time::sleep_until(tokio::time::Instant::from_std(t)).await,
            None => std::future::pending::<()>().await,
        }
    }

    // ── zvariant extraction helpers ─────────────────────────────────────────────────────────

    fn str_field(meta: &std::collections::HashMap<String, OwnedValue>, key: &str) -> Option<String> {
        match meta.get(key).map(|v| v as &ZValue) {
            Some(ZValue::Str(s)) => Some(s.to_string()),
            _ => None,
        }
    }

    fn strs_field(meta: &std::collections::HashMap<String, OwnedValue>, key: &str) -> Vec<String> {
        match meta.get(key).map(|v| v as &ZValue) {
            Some(ZValue::Array(a)) => a
                .iter()
                .filter_map(|e| if let ZValue::Str(s) = e { Some(s.to_string()) } else { None })
                .collect(),
            Some(ZValue::Str(s)) => vec![s.to_string()],
            _ => Vec::new(),
        }
    }

    fn i64_field(meta: &std::collections::HashMap<String, OwnedValue>, key: &str) -> Option<i64> {
        match meta.get(key).map(|v| v as &ZValue) {
            Some(ZValue::I64(n)) => Some(*n),
            Some(ZValue::U64(n)) => Some(*n as i64),
            Some(ZValue::I32(n)) => Some(*n as i64),
            Some(ZValue::U32(n)) => Some(*n as i64),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(fields: &HashMap<String, Value>, key: &str) -> String {
        match fields.get(key) {
            Some(Value::Text(s)) => s.clone(),
            other => panic!("expected text for {key}, got {other:?}"),
        }
    }

    #[test]
    fn joins_artists_and_maps_status() {
        let artists = vec!["Boards of Canada".to_string(), "Someone".to_string()];
        let f = build_song_fields("Aquarius", &artists, "Geogaddi", None, "Playing", 0, 0);
        assert_eq!(text(&f, "title"), "Aquarius");
        assert_eq!(text(&f, "artist"), "Boards of Canada, Someone");
        assert_eq!(text(&f, "album"), "Geogaddi");
        assert_eq!(text(&f, "status"), "playing");
        assert!(!f.contains_key("art"), "no art handle when url absent");
    }

    #[test]
    fn paused_and_empty_metadata() {
        let f = build_song_fields("Song", &[], "", None, "Paused", 0, 0);
        assert_eq!(text(&f, "status"), "paused");
        assert_eq!(text(&f, "artist"), "");
    }

    #[test]
    fn unknown_status_falls_back_to_paused() {
        let f = build_song_fields("S", &[], "", None, "Whatever", 0, 0);
        assert_eq!(text(&f, "status"), "paused");
    }

    #[test]
    fn position_fraction_is_clamped() {
        let frac = |pos, len| match build_song_fields("S", &[], "", None, "Playing", pos, len)
            .get("position")
        {
            Some(Value::Float(v)) => *v,
            other => panic!("expected float, got {other:?}"),
        };
        assert!((frac(30_000_000, 120_000_000) - 0.25).abs() < 1e-9);
        assert_eq!(frac(0, 0), 0.0, "zero length → 0 (no divide-by-zero)");
        assert_eq!(frac(500, 100), 1.0, "over-length clamps to 1.0");
    }

    #[test]
    fn art_url_becomes_image_handle() {
        let f = build_song_fields("S", &[], "", Some("https://art/x.jpg"), "Playing", 0, 0);
        assert!(matches!(f.get("art"), Some(Value::Image(u)) if u == "https://art/x.jpg"));
    }
}
