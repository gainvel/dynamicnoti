//! Daemon configuration loaded from `config.toml`. Kept lossless for the config TUI: the raw
//! `socket` string is preserved as written and expanded only on demand via [`Config::socket_path`].

use serde::{Deserialize, Serialize};

/// How competing notifications share the single island surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueuePolicy {
    /// A higher-priority notification preempts the live one.
    #[default]
    PriorityPreempt,
    /// Strict arrival order; the live notification finishes first.
    Fifo,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueConfig {
    #[serde(default)]
    pub policy: QueuePolicy,
    #[serde(default = "one")]
    pub max_visible: usize,
    /// Updates sharing a `replace_key` collapse onto one live surface.
    #[serde(default = "tru")]
    pub coalesce_replace: bool,
}

impl Default for QueueConfig {
    fn default() -> Self {
        QueueConfig { policy: QueuePolicy::default(), max_visible: 1, coalesce_replace: true }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FreedesktopConfig {
    /// Own `org.freedesktop.Notifications` (replacing KDE). OFF by default so a `--features dbus`
    /// build never seizes the bus from KDE without an explicit opt-in.
    #[serde(default)]
    pub take_over: bool,
    #[serde(default = "tru")]
    pub replace_existing: bool,
}

impl Default for FreedesktopConfig {
    fn default() -> Self {
        FreedesktopConfig { take_over: false, replace_existing: true }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MprisConfig {
    /// Match players by MPRIS `Identity`, never the churning bus name.
    #[serde(default)]
    pub identities: Vec<String>,
    /// Players whose own freedesktop notifications we drop (to avoid duplicating the song card).
    #[serde(default)]
    pub suppress_freedesktop_from: Vec<String>,
    #[serde(default = "debounce_default")]
    pub debounce_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IpcConfig {
    #[serde(default = "tru")]
    pub enabled: bool,
}

impl Default for IpcConfig {
    fn default() -> Self {
        IpcConfig { enabled: true }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SourcesConfig {
    #[serde(default)]
    pub freedesktop: FreedesktopConfig,
    #[serde(default)]
    pub mpris: MprisConfig,
    #[serde(default)]
    pub ipc: IpcConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_socket")]
    pub socket: String,
    #[serde(default = "default_monitor")]
    pub monitor: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub queue: QueueConfig,
    #[serde(default)]
    pub sources: SourcesConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            socket: default_socket(),
            monitor: default_monitor(),
            log_level: default_log_level(),
            queue: QueueConfig::default(),
            sources: SourcesConfig::default(),
        }
    }
}

impl Config {
    pub fn from_toml(s: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(s)
    }

    /// The socket path with `$XDG_RUNTIME_DIR` (and `$HOME`/`~`) expanded.
    pub fn socket_path(&self) -> String {
        expand_xdg(&self.socket)
    }
}

/// Expand a leading `$XDG_RUNTIME_DIR`, `$HOME`, or `~` against the environment. Falls back to
/// `/tmp` when `XDG_RUNTIME_DIR` is unset so the daemon still has a usable socket path.
pub fn expand_xdg(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("$XDG_RUNTIME_DIR") {
        let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
        return format!("{base}{rest}");
    }
    if let Some(rest) = s.strip_prefix("$HOME") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}{rest}");
        }
    }
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    s.to_string()
}

fn default_socket() -> String {
    "$XDG_RUNTIME_DIR/dynamicnoti.sock".into()
}
fn default_monitor() -> String {
    "all".into()
}
fn default_log_level() -> String {
    "info".into()
}
fn one() -> usize {
    1
}
fn tru() -> bool {
    true
}
fn debounce_default() -> u64 {
    250
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = include_str!("../../../config.example/config.toml");

    #[test]
    fn example_config_parses() {
        let c = Config::from_toml(CONFIG).expect("config.toml parses");
        assert_eq!(c.queue.policy, QueuePolicy::PriorityPreempt);
        assert_eq!(c.queue.max_visible, 1);
        assert!(c.sources.mpris.identities.iter().any(|i| i == "Cider"));
    }

    #[test]
    fn xdg_expansion() {
        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        assert_eq!(expand_xdg("$XDG_RUNTIME_DIR/x.sock"), "/run/user/1000/x.sock");
    }

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert!(c.queue.coalesce_replace);
        assert_eq!(c.monitor, "all");
    }
}
