//! Spring physics for dynamicnoti's animations. Pure math — no GPU, no I/O.
//!
//! Every animated surface property (width, height, scale, opacity, corner_radius,
//! content_crossfade, marquee_offset) is its own [`Spring`]. The render loop calls
//! [`Spring::tick`] once per frame with the real delta time, then reads `.value`.
//!
//! Integrator: clamped semi-implicit (symplectic) Euler. Stable for stiff springs as long
//! as `dt` is bounded — we clamp to 1/120s and the caller substeps for larger gaps. RK4 is
//! unnecessary here and not worth the cost.

use serde::Deserialize;

/// Tunable per-spring constants. See named presets in `theme.toml` (`island_soft`, etc.).
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct SpringParams {
    pub stiffness: f32,
    pub damping: f32,
    pub mass: f32,
    /// Below this distance AND velocity, the spring snaps to target and reports `settled`.
    #[serde(default = "default_rest_eps")]
    pub rest_eps: f32,
}

fn default_rest_eps() -> f32 {
    0.01
}

impl SpringParams {
    /// The default "island_soft" feel: a soft, barely-overshooting settle.
    pub const ISLAND_SOFT: SpringParams = SpringParams {
        stiffness: 170.0,
        damping: 26.0,
        mass: 1.0,
        rest_eps: 0.01,
    };
}

/// A single animated scalar driven toward `target` by spring force.
#[derive(Clone, Copy, Debug)]
pub struct Spring {
    pub value: f32,
    pub target: f32,
    pub vel: f32,
    pub params: SpringParams,
    pub settled: bool,
}

impl Spring {
    pub fn new(value: f32, params: SpringParams) -> Self {
        Self { value, target: value, vel: 0.0, params, settled: true }
    }

    /// Retarget without snapping — the spring animates toward the new target.
    pub fn set_target(&mut self, target: f32) {
        if (target - self.target).abs() > f32::EPSILON {
            self.target = target;
            self.settled = false;
        }
    }

    /// Advance one frame. `dt` is real seconds; large gaps are substepped internally so a
    /// hitch never explodes the integrator.
    pub fn tick(&mut self, dt: f32) {
        if self.settled {
            return;
        }
        const MAX_STEP: f32 = 1.0 / 120.0;
        let mut remaining = dt.max(0.0);
        while remaining > 0.0 {
            let step = remaining.min(MAX_STEP);
            let force = -self.params.stiffness * (self.value - self.target)
                - self.params.damping * self.vel;
            let accel = force / self.params.mass;
            self.vel += accel * step; // semi-implicit: velocity first...
            self.value += self.vel * step; // ...then position uses the new velocity.
            remaining -= step;
        }
        if (self.value - self.target).abs() < self.params.rest_eps
            && self.vel.abs() < self.params.rest_eps
        {
            self.value = self.target;
            self.vel = 0.0;
            self.settled = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spring_settles_at_target() {
        let mut s = Spring::new(0.0, SpringParams::ISLAND_SOFT);
        s.set_target(100.0);
        // ~5 seconds of 60fps frames is far more than enough to settle.
        for _ in 0..300 {
            s.tick(1.0 / 60.0);
            if s.settled {
                break;
            }
        }
        assert!(s.settled, "spring failed to settle");
        assert!((s.value - 100.0).abs() < 0.1, "settled at wrong value: {}", s.value);
    }

    #[test]
    fn idle_spring_is_a_noop() {
        let mut s = Spring::new(42.0, SpringParams::ISLAND_SOFT);
        s.tick(1.0 / 60.0);
        assert_eq!(s.value, 42.0);
        assert!(s.settled);
    }
}
