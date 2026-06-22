//! dynamicnoti-sources — the async/tokio input layer. Owns the IPC socket (and, behind the
//! `dbus` feature, the freedesktop server + mpris client). Produces
//! [`dynamicnoti_core::RawNotification`]s; NEVER touches wgpu/Wayland.
//!
//! Each source runs as a tokio task and pushes [`SourceMsg`]s into a `tokio::sync::mpsc`
//! channel owned by the daemon's driver. The driver runs the resolve→bind→build→queue
//! pipeline; only the finished `Scene` then crosses to the (main-thread) renderer. This keeps
//! `calloop` out of this crate entirely.
//!
//! Per-message handling is wrapped so a single malformed notification can never kill a task
//! (fault-isolation boundary #1).

pub mod freedesktop;
pub mod ipc;
pub mod mpris;
pub mod watcher;

use dynamicnoti_core::scene::Value;
use dynamicnoti_core::{ImageData, RawNotification};
use dynamicnoti_proto::WireValue;
use tokio::sync::{mpsc, oneshot};

/// Common interface for an input source. Each is spawned as a tokio task by the daemon.
pub trait Source {
    /// Human-readable name for logs.
    fn name(&self) -> &'static str;
}

/// Which on-disk artifact changed, so the driver knows what to re-read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reloaded {
    Config,
    Theme,
    Types,
}

/// Reply channel for a posted notification: the assigned id on success, or a human-readable
/// reason (unknown type, missing required field) on failure.
pub type PostReply = oneshot::Sender<Result<u64, String>>;

/// What a source sends to the driver.
pub enum SourceMsg {
    /// A new/updating notification. `reply` (if present) receives the result once the driver
    /// has resolved/bound it, so the IPC handler can answer the client.
    Post { raw: RawNotification, reply: Option<PostReply> },
    /// Close a notification by replace_key.
    Close { replace_key: String },
    /// Decoded image bytes (e.g. album art a source fetched + decoded off the main thread). The
    /// driver passes these straight to the renderer for GPU upload — they do NOT go through
    /// resolve/bind/build. `key` matches the `Value::Image(handle)` used in the notification's
    /// fields, so the renderer can associate the bytes with the right `Image` leaf.
    ImageReady { key: String, image: ImageData },
    /// A config/theme/types file changed on disk.
    ConfigChanged(Reloaded),
}

/// The sender half handed to every source task.
pub type SourceSender = mpsc::UnboundedSender<SourceMsg>;
/// The receiver half owned by the driver.
pub type SourceReceiver = mpsc::UnboundedReceiver<SourceMsg>;

/// A signal the daemon asks the freedesktop server to emit back onto D-Bus. The driver produces
/// `Closed` when the queue closes a freedesktop-originated notification (it carries the D-Bus id,
/// parsed from the `freedesktop:<id>` replace_key); `ActionInvoked` is forwarded from the
/// renderer's [`OutboundEvent`] once actions land. The CLI/ipc paths never produce these.
#[derive(Clone, Debug)]
pub enum FdSignal {
    Closed { id: u32, reason: u32 },
    ActionInvoked { id: u32, action_key: String },
}

/// Channel the driver uses to ask the freedesktop server to emit signals.
pub type FdSignalSender = mpsc::UnboundedSender<FdSignal>;
pub type FdSignalReceiver = mpsc::UnboundedReceiver<FdSignal>;

/// Map a wire value onto a core field value. Image-ness is decided later by the type schema in
/// `bind()` (a text path becomes an `Image` when the field is declared `type = "image"`), so a
/// text wire value stays text here.
pub fn wire_to_value(w: &WireValue) -> Value {
    match w {
        WireValue::Text(s) => Value::Text(s.clone()),
        WireValue::Float(f) => Value::Float(*f),
        WireValue::Bool(b) => Value::Bool(*b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_mapping() {
        assert_eq!(wire_to_value(&WireValue::Text("x".into())), Value::Text("x".into()));
        assert_eq!(wire_to_value(&WireValue::Float(0.5)), Value::Float(0.5));
        assert_eq!(wire_to_value(&WireValue::Bool(true)), Value::Bool(true));
    }
}
