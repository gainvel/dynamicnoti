//! `TypeResolver` — loads every `types/*.toml` and maps a notification onto its template.
//!
//! The resolver is immutable once built; hot-reload is a fresh `load_dir` + atomic swap done
//! by the daemon (keeps core free of `ArcSwap`/locking).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::template::TypeTemplate;
use crate::{CoreError, SourceKind};

pub struct TypeResolver {
    types: HashMap<String, Arc<TypeTemplate>>,
}

impl TypeResolver {
    /// Load every `*.toml` in `dir`. A file that fails to parse is logged and skipped so one
    /// bad type can't take down the daemon. A `generic` type is required as the fallback.
    pub fn load_dir(dir: &Path) -> Result<TypeResolver, CoreError> {
        let mut types = HashMap::new();
        let entries = std::fs::read_dir(dir)
            .map_err(|e| CoreError::Config(format!("cannot read types dir {dir:?}: {e}")))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|s| {
                TypeTemplate::from_toml(&s).map_err(|e| e.to_string())
            }) {
                Ok(t) => {
                    types.insert(t.meta.name.clone(), Arc::new(t));
                }
                Err(e) => tracing::warn!(target: "core", "skipping bad type {path:?}: {e}"),
            }
        }
        Self::finish(types)
    }

    /// Build from already-parsed templates (tests, defaults).
    pub fn from_templates(v: Vec<TypeTemplate>) -> Result<TypeResolver, CoreError> {
        let types = v.into_iter().map(|t| (t.meta.name.clone(), Arc::new(t))).collect();
        Self::finish(types)
    }

    fn finish(types: HashMap<String, Arc<TypeTemplate>>) -> Result<TypeResolver, CoreError> {
        if !types.contains_key("generic") {
            return Err(CoreError::UnknownType("generic".into()));
        }
        Ok(TypeResolver { types })
    }

    /// Resolve a notification to a template: explicit request → source default → `generic`.
    pub fn resolve(
        &self,
        requested: Option<&str>,
        source: SourceKind,
    ) -> Result<Arc<TypeTemplate>, CoreError> {
        if let Some(name) = requested {
            if let Some(t) = self.types.get(name) {
                return Ok(t.clone());
            }
            // An explicit-but-unknown type falls through to the source default rather than
            // dropping the notification entirely.
            tracing::warn!(target: "core", "unknown requested type '{name}', falling back");
        }
        let default = source.default_type();
        if let Some(t) = self.types.get(default) {
            return Ok(t.clone());
        }
        self.types
            .get("generic")
            .cloned()
            .ok_or_else(|| CoreError::UnknownType("generic".into()))
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.types.keys().map(String::as_str)
    }

    pub fn get(&self, name: &str) -> Option<Arc<TypeTemplate>> {
        self.types.get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GENERIC: &str = include_str!("../../../config.example/types/generic.toml");
    const SONG: &str = include_str!("../../../config.example/types/song.toml");

    fn resolver() -> TypeResolver {
        TypeResolver::from_templates(vec![
            TypeTemplate::from_toml(GENERIC).unwrap(),
            TypeTemplate::from_toml(SONG).unwrap(),
        ])
        .unwrap()
    }

    #[test]
    fn explicit_request_wins() {
        let r = resolver();
        assert_eq!(r.resolve(Some("song"), SourceKind::Ipc).unwrap().meta.name, "song");
    }

    #[test]
    fn source_default_used_when_unrequested() {
        let r = resolver();
        assert_eq!(r.resolve(None, SourceKind::Mpris).unwrap().meta.name, "song");
        assert_eq!(r.resolve(None, SourceKind::FreeDesktop).unwrap().meta.name, "generic");
    }

    #[test]
    fn unknown_request_falls_back() {
        let r = resolver();
        // mpris default is song; an unknown explicit type falls to the source default.
        assert_eq!(r.resolve(Some("nope"), SourceKind::Mpris).unwrap().meta.name, "song");
        // ipc default is generic.
        assert_eq!(r.resolve(Some("nope"), SourceKind::Ipc).unwrap().meta.name, "generic");
    }

    #[test]
    fn missing_generic_is_rejected() {
        let r = TypeResolver::from_templates(vec![TypeTemplate::from_toml(SONG).unwrap()]);
        assert!(r.is_err());
    }
}
