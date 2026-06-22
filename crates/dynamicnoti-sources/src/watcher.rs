//! Config watcher — watches `~/.config/dynamicnoti/` and signals the driver which artifact
//! changed. The watcher itself is dumb: it does NOT parse. The driver re-reads and keeps the
//! last good config on a broken edit, so a malformed TOML never crashes the daemon.

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{Event, RecursiveMode, Watcher};

use crate::{Reloaded, SourceMsg, SourceSender};

/// Watch `config_dir` for changes, debouncing bursts (editors emit several events per save),
/// and forward a [`SourceMsg::ConfigChanged`] for each affected artifact.
pub async fn run(config_dir: PathBuf, tx: SourceSender) -> anyhow::Result<()> {
    // notify's watcher callback runs on its own thread; bridge into tokio via an mpsc channel.
    let (raw_tx, mut raw_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<PathBuf>>();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(event) = res {
            let _ = raw_tx.send(event.paths);
        }
    })?;

    if config_dir.exists() {
        watcher.watch(&config_dir, RecursiveMode::Recursive)?;
        tracing::info!(target: "watcher", "watching {config_dir:?}");
    } else {
        tracing::warn!(target: "watcher", "config dir {config_dir:?} absent; not watching");
        return Ok(());
    }

    // Debounce: collect paths for a short window, then emit one signal per distinct artifact.
    loop {
        let Some(first) = raw_rx.recv().await else {
            return Ok(()); // sender dropped → watcher gone
        };
        let mut paths = first;
        let debounce = tokio::time::sleep(Duration::from_millis(200));
        tokio::pin!(debounce);
        loop {
            tokio::select! {
                _ = &mut debounce => break,
                more = raw_rx.recv() => match more {
                    Some(mut p) => paths.append(&mut p),
                    None => break,
                },
            }
        }

        let mut config = false;
        let mut theme = false;
        let mut types = false;
        for p in &paths {
            match classify(p) {
                Some(Reloaded::Config) => config = true,
                Some(Reloaded::Theme) => theme = true,
                Some(Reloaded::Types) => types = true,
                None => {}
            }
        }
        for which in [
            config.then_some(Reloaded::Config),
            theme.then_some(Reloaded::Theme),
            types.then_some(Reloaded::Types),
        ]
        .into_iter()
        .flatten()
        {
            let _ = tx.send(SourceMsg::ConfigChanged(which));
        }
    }
}

fn classify(path: &Path) -> Option<Reloaded> {
    let name = path.file_name()?.to_str()?;
    if path.components().any(|c| c.as_os_str() == "types") {
        return Some(Reloaded::Types);
    }
    match name {
        "config.toml" => Some(Reloaded::Config),
        "theme.toml" => Some(Reloaded::Theme),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_paths() {
        assert_eq!(classify(Path::new("/c/config.toml")), Some(Reloaded::Config));
        assert_eq!(classify(Path::new("/c/theme.toml")), Some(Reloaded::Theme));
        assert_eq!(classify(Path::new("/c/types/song.toml")), Some(Reloaded::Types));
        assert_eq!(classify(Path::new("/c/notes.md")), None);
    }
}
