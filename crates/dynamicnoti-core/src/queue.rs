//! `QueueManager` — the pure, time-injected state machine that arbitrates which notification
//! owns the single island surface.
//!
//! It is deliberately free of channels, clocks, and templates: the daemon runs the
//! resolve→bind→build pipeline, feeds the resulting [`Scene`] in via [`QueueInput`], and
//! forwards each [`QueueOutput`] to the renderer. Time is passed in as monotonic milliseconds
//! so the whole thing is deterministic and unit-testable.

use crate::config::QueuePolicy;
use crate::scene::Scene;
use crate::style::{ResolvedAnimProfile, ResolvedStyle};

/// Monotonic milliseconds since the daemon started. Injected by the driver.
pub type TimeMs = u64;

/// Resolved queue policy (mirrors [`crate::config::QueueConfig`], minus the parts the manager
/// doesn't need).
#[derive(Clone, Copy, Debug)]
pub struct QueueSettings {
    pub policy: QueuePolicy,
    pub coalesce_replace: bool,
}

/// What the driver feeds the manager. The `Scene` is already built, and the per-notification
/// `style`/`anim` are already resolved on the tokio side (the renderer stays theming-agnostic).
// Post dominates and is low-frequency (once per notification); boxing it to shrink Close/Tick
// would just add an allocation on the hot path.
#[allow(clippy::large_enum_variant)]
pub enum QueueInput {
    Post {
        id: u64,
        replace_key: Option<String>,
        priority: i32,
        /// 0 = sticky (never times out).
        timeout_ms: u32,
        scene: Scene,
        style: ResolvedStyle,
        anim: ResolvedAnimProfile,
    },
    /// Close whatever notification carries this replace_key.
    Close { replace_key: String },
    /// Close a specific notification by id.
    CloseId { id: u64 },
    /// A timer wakeup — drives timeout expiry.
    Tick,
}

/// What the manager emits; the driver maps these onto render events.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum QueueOutput {
    /// Show a fresh surface.
    Show { id: u64, scene: Scene, style: ResolvedStyle, anim: ResolvedAnimProfile },
    /// Replace the live surface's content in place (the signature island morph).
    Morph { id: u64, scene: Scene, style: ResolvedStyle, anim: ResolvedAnimProfile },
    /// Tear the surface down. `replace_key` echoes the closed notification's key so the daemon
    /// can route a freedesktop `NotificationClosed` signal back to the right D-Bus id.
    Close { id: u64, reason: CloseReason, replace_key: Option<String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    Timeout,
    Replaced,
    Explicit,
    Preempted,
}

struct Entry {
    id: u64,
    replace_key: Option<String>,
    priority: i32,
    timeout_ms: u32,
    scene: Scene,
    style: ResolvedStyle,
    anim: ResolvedAnimProfile,
    /// `Some(deadline)` once shown and non-sticky; `None` while sticky or waiting.
    expires_at: Option<TimeMs>,
}

impl Entry {
    #[allow(clippy::too_many_arguments)]
    fn from_post(
        id: u64,
        replace_key: Option<String>,
        priority: i32,
        timeout_ms: u32,
        scene: Scene,
        style: ResolvedStyle,
        anim: ResolvedAnimProfile,
    ) -> Self {
        Entry { id, replace_key, priority, timeout_ms, scene, style, anim, expires_at: None }
    }
}

pub struct QueueManager {
    settings: QueueSettings,
    live: Option<Entry>,
    waiting: Vec<Entry>,
}

impl QueueManager {
    pub fn new(settings: QueueSettings) -> Self {
        QueueManager { settings, live: None, waiting: Vec::new() }
    }

    /// Apply new settings (e.g. after a config reload). Takes effect on the next input.
    pub fn set_settings(&mut self, settings: QueueSettings) {
        self.settings = settings;
    }

    /// The next timer deadline the driver should arm, if any.
    pub fn next_deadline(&self) -> Option<TimeMs> {
        self.live.as_ref().and_then(|e| e.expires_at)
    }

    /// Drive the machine with one input at logical time `now`. Returns the outputs to emit.
    pub fn handle(&mut self, input: QueueInput, now: TimeMs) -> Vec<QueueOutput> {
        match input {
            QueueInput::Post { id, replace_key, priority, timeout_ms, scene, style, anim } => self
                .post(
                    now,
                    Entry::from_post(id, replace_key, priority, timeout_ms, scene, style, anim),
                ),
            QueueInput::Close { replace_key } => self.close_matching(now, |e| {
                e.replace_key.as_deref() == Some(replace_key.as_str())
            }),
            QueueInput::CloseId { id } => self.close_matching(now, |e| e.id == id),
            QueueInput::Tick => self.tick(now),
        }
    }

    fn post(&mut self, now: TimeMs, mut entry: Entry) -> Vec<QueueOutput> {
        // 1. Coalesce by replace_key.
        if self.settings.coalesce_replace {
            if let Some(key) = entry.replace_key.clone() {
                if let Some(live) = self.live.as_mut() {
                    if live.replace_key.as_deref() == Some(key.as_str()) {
                        // Update the live surface in place — keep its id, morph the content.
                        live.priority = entry.priority;
                        live.timeout_ms = entry.timeout_ms;
                        live.scene = entry.scene.clone();
                        live.style = entry.style.clone();
                        live.anim = entry.anim;
                        live.expires_at = deadline(now, entry.timeout_ms);
                        return vec![QueueOutput::Morph {
                            id: live.id,
                            scene: entry.scene,
                            style: entry.style,
                            anim: entry.anim,
                        }];
                    }
                }
                // A waiting entry with the same key is replaced silently (not visible yet).
                if let Some(slot) = self
                    .waiting
                    .iter_mut()
                    .find(|e| e.replace_key.as_deref() == Some(key.as_str()))
                {
                    entry.expires_at = None;
                    *slot = entry;
                    return Vec::new();
                }
            }
        }

        // 2. Insert fresh.
        match self.live.take() {
            None => self.promote(now, entry),
            Some(live) => {
                let preempts = matches!(self.settings.policy, QueuePolicy::PriorityPreempt)
                    && entry.priority > live.priority;
                if preempts {
                    let old_id = live.id;
                    let old_rk = live.replace_key.clone();
                    self.park(live);
                    let mut out = vec![QueueOutput::Close {
                        id: old_id,
                        reason: CloseReason::Preempted,
                        replace_key: old_rk,
                    }];
                    out.extend(self.promote(now, entry));
                    out
                } else {
                    // Live keeps the surface; new one waits.
                    self.live = Some(live);
                    self.park(entry);
                    Vec::new()
                }
            }
        }
    }

    /// Make `entry` the live surface and emit its `Show`.
    fn promote(&mut self, now: TimeMs, mut entry: Entry) -> Vec<QueueOutput> {
        entry.expires_at = deadline(now, entry.timeout_ms);
        let id = entry.id;
        let scene = entry.scene.clone();
        let style = entry.style.clone();
        let anim = entry.anim;
        self.live = Some(entry);
        vec![QueueOutput::Show { id, scene, style, anim }]
    }

    /// Park an entry in the waiting set, ordered by policy.
    fn park(&mut self, mut entry: Entry) {
        entry.expires_at = None;
        self.waiting.push(entry);
        if matches!(self.settings.policy, QueuePolicy::PriorityPreempt) {
            // Stable sort by priority desc keeps arrival order within a priority band.
            self.waiting.sort_by_key(|e| std::cmp::Reverse(e.priority));
        }
    }

    /// Pull the next waiting entry (front — already policy-ordered) and show it.
    fn promote_next(&mut self, now: TimeMs) -> Vec<QueueOutput> {
        if self.waiting.is_empty() {
            self.live = None;
            return Vec::new();
        }
        let next = self.waiting.remove(0);
        self.promote(now, next)
    }

    fn close_matching(&mut self, now: TimeMs, pred: impl Fn(&Entry) -> bool) -> Vec<QueueOutput> {
        // Live first.
        if let Some(live) = self.live.as_ref() {
            if pred(live) {
                let id = live.id;
                let rk = live.replace_key.clone();
                self.live = None;
                let mut out = vec![QueueOutput::Close {
                    id,
                    reason: CloseReason::Explicit,
                    replace_key: rk,
                }];
                out.extend(self.promote_next(now));
                return out;
            }
        }
        // Otherwise drop any waiting match silently.
        self.waiting.retain(|e| !pred(e));
        Vec::new()
    }

    fn tick(&mut self, now: TimeMs) -> Vec<QueueOutput> {
        let expired = matches!(self.live.as_ref().and_then(|e| e.expires_at), Some(d) if d <= now);
        if !expired {
            return Vec::new();
        }
        let (id, rk) = self.live.take().map(|e| (e.id, e.replace_key)).unwrap_or((0, None));
        let mut out = vec![QueueOutput::Close { id, reason: CloseReason::Timeout, replace_key: rk }];
        out.extend(self.promote_next(now));
        out
    }
}

/// `None` for a sticky (timeout 0) notification; otherwise `now + timeout`.
fn deadline(now: TimeMs, timeout_ms: u32) -> Option<TimeMs> {
    (timeout_ms > 0).then(|| now + timeout_ms as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{LayoutAttrs, Scene};
    use crate::theme::Theme;

    fn scene() -> Scene {
        Scene::Row { attrs: LayoutAttrs::default(), children: Vec::new() }
    }

    fn style() -> ResolvedStyle {
        Theme::default().resolve_style(None)
    }

    fn anim() -> ResolvedAnimProfile {
        Theme::default().resolve_anim("island_soft", None)
    }

    fn post(id: u64, key: Option<&str>, prio: i32, timeout: u32) -> QueueInput {
        QueueInput::Post {
            id,
            replace_key: key.map(str::to_string),
            priority: prio,
            timeout_ms: timeout,
            scene: scene(),
            style: style(),
            anim: anim(),
        }
    }

    fn mgr(policy: QueuePolicy) -> QueueManager {
        QueueManager::new(QueueSettings { policy, coalesce_replace: true })
    }

    #[test]
    fn empty_post_shows() {
        let mut m = mgr(QueuePolicy::PriorityPreempt);
        let out = m.handle(post(1, None, 10, 5000), 0);
        assert!(matches!(out.as_slice(), [QueueOutput::Show { id: 1, .. }]));
        assert_eq!(m.next_deadline(), Some(5000));
    }

    #[test]
    fn coalesce_morphs_same_key_keeping_id() {
        let mut m = mgr(QueuePolicy::PriorityPreempt);
        m.handle(post(1, Some("k"), 10, 0), 0);
        let out = m.handle(post(2, Some("k"), 10, 0), 100);
        assert!(matches!(out.as_slice(), [QueueOutput::Morph { id: 1, .. }]));
    }

    #[test]
    fn no_coalesce_queues_second() {
        let mut m = QueueManager::new(QueueSettings {
            policy: QueuePolicy::PriorityPreempt,
            coalesce_replace: false,
        });
        m.handle(post(1, Some("k"), 10, 0), 0);
        let out = m.handle(post(2, Some("k"), 10, 0), 0);
        assert!(out.is_empty()); // second waits (no coalesce, equal priority)
    }

    #[test]
    fn higher_priority_preempts() {
        let mut m = mgr(QueuePolicy::PriorityPreempt);
        m.handle(post(1, None, 10, 0), 0);
        let out = m.handle(post(2, None, 50, 0), 0);
        assert!(matches!(
            out.as_slice(),
            [QueueOutput::Close { id: 1, reason: CloseReason::Preempted, .. }, QueueOutput::Show { id: 2, .. }]
        ));
    }

    #[test]
    fn lower_priority_waits_then_resumes() {
        let mut m = mgr(QueuePolicy::PriorityPreempt);
        m.handle(post(1, None, 50, 1000), 0);
        let out = m.handle(post(2, None, 10, 1000), 0);
        assert!(out.is_empty());
        // Live (id 1) times out → id 2 promoted.
        let out = m.handle(QueueInput::Tick, 1000);
        assert!(matches!(
            out.as_slice(),
            [QueueOutput::Close { id: 1, reason: CloseReason::Timeout, .. }, QueueOutput::Show { id: 2, .. }]
        ));
    }

    #[test]
    fn fifo_never_preempts() {
        let mut m = mgr(QueuePolicy::Fifo);
        m.handle(post(1, None, 10, 1000), 0);
        let out = m.handle(post(2, None, 99, 1000), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn sticky_never_times_out() {
        let mut m = mgr(QueuePolicy::PriorityPreempt);
        m.handle(post(1, None, 10, 0), 0);
        assert_eq!(m.next_deadline(), None);
        assert!(m.handle(QueueInput::Tick, 1_000_000).is_empty());
    }

    #[test]
    fn timeout_closes_and_promotes() {
        let mut m = mgr(QueuePolicy::PriorityPreempt);
        m.handle(post(1, None, 10, 100), 0);
        // Before deadline: nothing.
        assert!(m.handle(QueueInput::Tick, 50).is_empty());
        // At deadline: close.
        let out = m.handle(QueueInput::Tick, 100);
        assert!(matches!(out.as_slice(), [QueueOutput::Close { id: 1, reason: CloseReason::Timeout, .. }]));
    }

    #[test]
    fn close_by_key_promotes_waiting() {
        let mut m = mgr(QueuePolicy::PriorityPreempt);
        m.handle(post(1, Some("a"), 10, 0), 0);
        m.handle(post(2, Some("b"), 5, 0), 0); // waits
        let out = m.handle(QueueInput::Close { replace_key: "a".into() }, 10);
        assert!(matches!(
            out.as_slice(),
            [QueueOutput::Close { id: 1, reason: CloseReason::Explicit, .. }, QueueOutput::Show { id: 2, .. }]
        ));
    }

    #[test]
    fn close_waiting_is_silent() {
        let mut m = mgr(QueuePolicy::Fifo);
        m.handle(post(1, Some("a"), 10, 0), 0);
        m.handle(post(2, Some("b"), 10, 0), 0); // waits
        let out = m.handle(QueueInput::Close { replace_key: "b".into() }, 0);
        assert!(out.is_empty());
        // Closing the live one now has nothing to promote.
        let out = m.handle(QueueInput::Close { replace_key: "a".into() }, 0);
        assert!(matches!(out.as_slice(), [QueueOutput::Close { id: 1, .. }]));
    }
}
