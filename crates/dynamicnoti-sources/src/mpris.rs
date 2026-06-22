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
/// The card shows only `title`/`artist`/`album`/`art`; the progress bar is a notification-lifetime
/// countdown (see `song.toml`'s `value = "lifetime"`), not playback position — so neither position
/// nor status is carried as a field. The art handle is included only when a non-empty URL is
/// present, so a missing `mpris:artUrl` simply drops the image leaf.
pub fn build_song_fields(
    title: &str,
    artists: &[String],
    album: &str,
    art: Option<&str>,
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

    fields
}

/// Whether `(title, artist)` is a different track than `prev` — the gate that keeps the Cider card
/// from re-appearing on play/pause/seek. A fresh play (no `prev`) always counts as new. Pure so it
/// unit-tests without D-Bus.
pub fn is_new_track(prev: Option<&(String, String)>, title: &str, artist: &str) -> bool {
    match prev {
        Some((pt, pa)) => pt != title || pa != artist,
        None => true,
    }
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

    use super::{build_song_fields, is_new_track, REPLACE_KEY};
    use crate::{SourceMsg, SourceSender};

    /// The currently-shown track as `(title, artist)`, cached so play/pause/seek don't re-show it.
    type TrackId = Option<(String, String)>;

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
        let mut prev_track: TrackId = None;

        tracing::info!(target: "mpris", "watching for players: {:?}", cfg.identities);

        loop {
            match select_player(&conn, &dbus, &cfg).await {
                Some(name) => {
                    watch_player(
                        &conn,
                        &name,
                        &cfg,
                        &tx,
                        &mut owner_changes,
                        &mut fetched,
                        &mut shown,
                        &mut prev_track,
                    )
                    .await;
                }
                None => {
                    // No matching player right now: tear down any stale card and wait for the
                    // next bus name change before re-enumerating.
                    if shown {
                        let _ = tx.send(SourceMsg::Close { replace_key: REPLACE_KEY.into() });
                        shown = false;
                    }
                    prev_track = None;
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

    /// Watch one selected player until it quits. Coalesces metadata/status changes within
    /// `debounce_ms`; emits a `song` post only when the *track* changes (so play/pause/seek never
    /// re-show the card). Closes the card on Stopped/quit.
    #[allow(clippy::too_many_arguments)]
    async fn watch_player(
        conn: &Connection,
        name: &str,
        cfg: &MprisConfig,
        tx: &SourceSender,
        owner_changes: &mut zbus::fdo::NameOwnerChangedStream,
        fetched: &mut HashSet<String>,
        shown: &mut bool,
        prev_track: &mut TrackId,
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

        let debounce = Duration::from_millis(cfg.debounce_ms.max(1));
        let mut deadline = Some(Instant::now()); // evaluate current state immediately on entry
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
                        *prev_track = None;
                        return;
                    }
                }
                _ = sleep_until_opt(deadline), if deadline.is_some() => {
                    deadline = None;
                    emit_state(&player, tx, fetched, shown, prev_track).await;
                }
            }
        }
    }

    /// Read the player's current state and, *only on a track change*, emit a `song` post. Closes
    /// the card on Stopped. Spawns an off-thread art fetch the first time a given art URL is seen.
    async fn emit_state(
        player: &PlayerProxy<'_>,
        tx: &SourceSender,
        fetched: &mut HashSet<String>,
        shown: &mut bool,
        prev_track: &mut TrackId,
    ) {
        let status = player.playback_status().await.unwrap_or_default();
        if status.eq_ignore_ascii_case("stopped") {
            if *shown {
                let _ = tx.send(SourceMsg::Close { replace_key: REPLACE_KEY.into() });
                *shown = false;
            }
            *prev_track = None;
            return;
        }

        let metadata = player.metadata().await.unwrap_or_default();

        let title = str_field(&metadata, "xesam:title").unwrap_or_default();
        if title.is_empty() {
            return; // the `song` type requires a title — nothing useful to show yet
        }
        let artists = strs_field(&metadata, "xesam:artist");
        let artist = artists.join(", ");

        // The gate: a new card appears only when the track changes (play/pause/seek keep the same
        // title+artist and are ignored). The card then auto-dismisses via its own lifetime timeout.
        if !is_new_track(prev_track.as_ref(), &title, &artist) {
            return;
        }

        let album = str_field(&metadata, "xesam:album").unwrap_or_default();
        let art = str_field(&metadata, "mpris:artUrl").filter(|s| !s.is_empty());

        let fields = build_song_fields(&title, &artists, &album, art.as_deref());

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
        *prev_track = Some((title, artist));

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
    fn joins_artists() {
        let artists = vec!["Boards of Canada".to_string(), "Someone".to_string()];
        let f = build_song_fields("Aquarius", &artists, "Geogaddi", None);
        assert_eq!(text(&f, "title"), "Aquarius");
        assert_eq!(text(&f, "artist"), "Boards of Canada, Someone");
        assert_eq!(text(&f, "album"), "Geogaddi");
        assert!(!f.contains_key("art"), "no art handle when url absent");
        // The lifetime bar replaces playback position — no position/status fields are carried.
        assert!(!f.contains_key("position"));
        assert!(!f.contains_key("status"));
    }

    #[test]
    fn empty_metadata() {
        let f = build_song_fields("Song", &[], "", None);
        assert_eq!(text(&f, "artist"), "");
    }

    #[test]
    fn art_url_becomes_image_handle() {
        let f = build_song_fields("S", &[], "", Some("https://art/x.jpg"));
        assert!(matches!(f.get("art"), Some(Value::Image(u)) if u == "https://art/x.jpg"));
    }

    #[test]
    fn track_change_gate() {
        // No prior track → always new (first play shows).
        assert!(is_new_track(None, "A", "X"));
        let cur = ("A".to_string(), "X".to_string());
        // Same title+artist (play/pause/seek) → not new, card stays hidden.
        assert!(!is_new_track(Some(&cur), "A", "X"));
        // New title or new artist → new track.
        assert!(is_new_track(Some(&cur), "B", "X"));
        assert!(is_new_track(Some(&cur), "A", "Y"));
    }
}
