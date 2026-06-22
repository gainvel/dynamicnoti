//! dynamicnoti-core — the pure domain layer.
//!
//! NO async, NO GPU, NO Wayland, NO zbus. This crate defines the notification model, the
//! data-driven [`scene`] type system, and config/theme loading. Sources produce
//! [`RawNotification`]s; the resolver maps each onto a type template and binds its fields;
//! [`scene::Scene`] is the immutable handoff to the renderer.
//!
//! Pipeline:  RawNotification -> TypeResolver::resolve -> bind() -> scene::build() -> Scene

pub mod bind;
pub mod config;
pub mod image;
pub mod introspect;
pub mod queue;
pub mod resolver;
pub mod scene;
pub mod style;
pub mod template;
pub mod theme;

pub use bind::{bind, BoundNotification};
pub use config::Config;
pub use image::ImageData;
pub use resolver::TypeResolver;
pub use template::TypeTemplate;
pub use theme::Theme;

use std::collections::HashMap;

/// Which source produced a notification — drives default type selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// org.freedesktop.Notifications (we own the name; replaces KDE).
    FreeDesktop,
    /// MPRIS watcher (Cider et al.) — rich, live "song" notifications.
    Mpris,
    /// Our own Unix-socket IPC — custom scripts posting typed notifications.
    Ipc,
}

impl SourceKind {
    /// Type name used when a notification doesn't request one explicitly.
    pub fn default_type(&self) -> &'static str {
        match self {
            SourceKind::FreeDesktop => "generic",
            SourceKind::Mpris => "song",
            SourceKind::Ipc => "generic",
        }
    }
}

/// What a source hands to core: a bag of fields plus routing metadata. Source-agnostic on
/// purpose — freedesktop hints, MPRIS metadata, and IPC JSON all normalize to this shape.
#[derive(Clone, Debug)]
pub struct RawNotification {
    pub source: SourceKind,
    pub app_name: String,
    /// Explicit type request (e.g. IPC `"type":"deal"`); falls back to `source.default_type()`.
    pub requested_type: Option<String>,
    /// Collapses updates onto one live surface (e.g. all MPRIS updates share one key).
    pub replace_key: Option<String>,
    pub fields: HashMap<String, scene::Value>,
}

/// Priority/lifecycle resolved for a bound notification.
#[derive(Clone, Copy, Debug)]
pub struct Behavior {
    pub priority: i32,
    /// 0 = sticky (closed explicitly, e.g. song while playing).
    pub timeout_ms: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("no type template named '{0}' and no 'generic' fallback")]
    UnknownType(String),
    #[error("field '{field}' is required by type '{ty}'")]
    MissingField { ty: String, field: String },
    #[error("config parse error: {0}")]
    Config(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_defaults_route_correctly() {
        assert_eq!(SourceKind::Mpris.default_type(), "song");
        assert_eq!(SourceKind::FreeDesktop.default_type(), "generic");
        assert_eq!(SourceKind::Ipc.default_type(), "generic");
    }
}
