//! Pure animation orchestration: the per-surface spring set and the Enter→Idle→Morph→Exit
//! state machine. No GPU, no Wayland — unit-tested on CI. The renderer ticks these springs off
//! frame callbacks and reads their `.value` each frame.

use dynamicnoti_anim::{Spring, SpringParams};
use dynamicnoti_core::style::ResolvedAnimProfile;
use dynamicnoti_core::theme::SpringPreset;

/// Lifecycle of one on-screen surface. Mirrors `crate::Phase` (kept separate so this module
/// stays dependency-light and testable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Enter,
    Idle,
    Morph,
    Exit,
}

fn params(p: &SpringPreset) -> SpringParams {
    SpringParams { stiffness: p.stiffness, damping: p.damping, mass: p.mass, rest_eps: p.rest_eps }
}

/// The animated geometry of one island surface.
pub struct SurfaceAnim {
    pub width: Spring,
    pub height: Spring,
    pub scale: Spring,
    pub opacity: Spring,
    pub corner_radius: Spring,
    /// 0 → fully showing `prev_scene`, 1 → fully showing the current scene. Only meaningful in Morph.
    pub crossfade: Spring,
    pub phase: Phase,
}

impl SurfaceAnim {
    /// Build the Enter animation: the island pops in (scale + fade) at its measured size.
    pub fn enter(target_w: f32, target_h: f32, radius: f32, anim: &ResolvedAnimProfile) -> Self {
        let geo = params(&anim.geometry);
        // Enter is a scale + fade pop; geometry sits at its measured size (width/height only
        // animate later, on Morph). Starting them settled avoids a no-op `set_target` freeze.
        let width = Spring::new(target_w, geo);
        let height = Spring::new(target_h, geo);

        let mut scale = Spring::new(0.86, params(&anim.scale));
        scale.set_target(1.0);
        let mut opacity = Spring::new(0.0, params(&anim.opacity));
        opacity.set_target(1.0);
        let corner_radius = Spring::new(radius, geo);
        // crossfade pinned at 1 (no previous scene) outside of Morph.
        let crossfade = Spring::new(1.0, params(&anim.crossfade));

        SurfaceAnim {
            width,
            height,
            scale,
            opacity,
            corner_radius,
            crossfade,
            phase: Phase::Enter,
        }
    }

    /// Retarget geometry to a new measured size and start a content crossfade (the signature
    /// island morph). `anim` may differ from the entering profile if the type changed.
    pub fn morph(&mut self, target_w: f32, target_h: f32, radius: f32, anim: &ResolvedAnimProfile) {
        self.width.params = params(&anim.geometry);
        self.height.params = params(&anim.geometry);
        self.corner_radius.params = params(&anim.geometry);
        self.crossfade.params = params(&anim.crossfade);
        self.width.set_target(target_w);
        self.height.set_target(target_h);
        self.corner_radius.set_target(radius);
        // Restart the crossfade from 0. Setting the fields directly (not `set_target`) is
        // deliberate: the target is already 1.0 from Enter, so `set_target(1.0)` would be a no-op
        // and leave `settled = true`, freezing the crossfade at 0.
        self.crossfade.value = 0.0;
        self.crossfade.vel = 0.0;
        self.crossfade.target = 1.0;
        self.crossfade.settled = false;
        self.phase = Phase::Morph;
    }

    /// Collapse and fade for dismissal.
    pub fn exit(&mut self) {
        self.scale.set_target(0.9);
        self.opacity.set_target(0.0);
        self.phase = Phase::Exit;
    }

    /// Advance every spring and update the phase. Returns the new phase.
    pub fn tick(&mut self, dt: f32) -> Phase {
        self.width.tick(dt);
        self.height.tick(dt);
        self.scale.tick(dt);
        self.opacity.tick(dt);
        self.corner_radius.tick(dt);
        self.crossfade.tick(dt);

        match self.phase {
            Phase::Enter if self.scale.settled && self.opacity.settled && self.width.settled => {
                self.phase = Phase::Idle;
            }
            Phase::Morph if self.crossfade.settled && self.width.settled && self.height.settled => {
                self.phase = Phase::Idle;
            }
            _ => {}
        }
        self.phase
    }

    /// True once the Exit fade has effectively completed and the surface can be destroyed.
    pub fn exit_done(&self) -> bool {
        self.phase == Phase::Exit && self.opacity.value <= 0.02
    }

    /// During Morph, has the crossfade passed its midpoint? (Drop `prev_scene` once true.)
    pub fn past_morph_midpoint(&self) -> bool {
        self.crossfade.value >= 0.5
    }

    /// Should the loop request another frame callback? Gate on unsettled springs, an active
    /// marquee, or an in-progress exit.
    pub fn needs_frame(&self, marquee_active: bool) -> bool {
        marquee_active
            || self.phase == Phase::Exit
            || !(self.width.settled
                && self.height.settled
                && self.scale.settled
                && self.opacity.settled
                && self.corner_radius.settled
                && self.crossfade.settled)
    }
}

/// Frame-callback delta time in seconds from two `wl_callback` timestamps (ms, monotonic, may
/// wrap at u32). Clamped to `[0, 1/30]` so a hitch or wrap never explodes the integrator; the
/// first frame (no previous time) uses a nominal 1/60.
pub fn compute_dt(prev: Option<u32>, now: u32) -> f32 {
    match prev {
        None => 1.0 / 60.0,
        Some(p) => {
            let delta_ms = now.wrapping_sub(p) as f32;
            (delta_ms / 1000.0).clamp(0.0, 1.0 / 30.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dynamicnoti_core::theme::Theme;

    fn anim() -> ResolvedAnimProfile {
        Theme::default().resolve_anim("island_soft", None)
    }

    fn settle(a: &mut SurfaceAnim, max_frames: usize) {
        for _ in 0..max_frames {
            a.tick(1.0 / 60.0);
        }
    }

    #[test]
    fn enter_settles_to_idle() {
        let mut a = SurfaceAnim::enter(400.0, 64.0, 28.0, &anim());
        assert_eq!(a.phase, Phase::Enter);
        assert!(a.needs_frame(false));
        settle(&mut a, 1200);
        assert_eq!(a.phase, Phase::Idle);
        assert!((a.scale.value - 1.0).abs() < 0.01);
        assert!((a.opacity.value - 1.0).abs() < 0.01);
        assert!(!a.needs_frame(false), "idle with no marquee must stop requesting frames");
    }

    #[test]
    fn idle_with_marquee_keeps_requesting() {
        let mut a = SurfaceAnim::enter(400.0, 64.0, 28.0, &anim());
        settle(&mut a, 1200);
        assert_eq!(a.phase, Phase::Idle);
        assert!(a.needs_frame(true), "an active marquee must keep the loop alive");
    }

    #[test]
    fn morph_crosses_midpoint_then_idles() {
        let mut a = SurfaceAnim::enter(400.0, 64.0, 28.0, &anim());
        settle(&mut a, 1200);
        a.morph(500.0, 80.0, 28.0, &anim());
        assert_eq!(a.phase, Phase::Morph);
        assert!(!a.past_morph_midpoint());
        // Drive until the crossfade passes the midpoint.
        let mut crossed = false;
        for _ in 0..1200 {
            a.tick(1.0 / 60.0);
            if a.past_morph_midpoint() {
                crossed = true;
                break;
            }
        }
        assert!(crossed, "crossfade never reached its midpoint");
        settle(&mut a, 1200);
        assert_eq!(a.phase, Phase::Idle);
        assert!((a.width.value - 500.0).abs() < 0.5);
    }

    #[test]
    fn exit_completes() {
        let mut a = SurfaceAnim::enter(400.0, 64.0, 28.0, &anim());
        settle(&mut a, 1200);
        a.exit();
        assert_eq!(a.phase, Phase::Exit);
        assert!(a.needs_frame(false));
        settle(&mut a, 1200);
        assert!(a.exit_done());
    }

    #[test]
    fn dt_clamps_and_wraps() {
        assert_eq!(compute_dt(None, 123), 1.0 / 60.0);
        assert!((compute_dt(Some(1000), 1016) - 0.016).abs() < 1e-4);
        // A 5-second hitch clamps to 1/30.
        assert_eq!(compute_dt(Some(0), 5000), 1.0 / 30.0);
        // u32 wraparound: now < prev numerically, but wrapping_sub gives the true small delta.
        assert!((compute_dt(Some(u32::MAX - 5), 10) - 0.016).abs() < 1e-3);
    }
}
