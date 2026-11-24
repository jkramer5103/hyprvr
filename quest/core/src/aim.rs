//! Controller aim stabilisation.
//!
//! [`AimFilter`] is a One Euro filter on the aim pose: heavy smoothing while the
//! controller is (nearly) still, so the laser doesn't shake, and almost none while
//! it moves fast, so it doesn't lag.
//! [`ClickSlop`] keeps a button press pinned to where it started until the hit point
//! has clearly moved, so pulling the trigger (which tilts the controller) clicks
//! instead of dragging.

use glam::{Quat, Vec3};
use std::f32::consts::TAU;

/// Cutoff (Hz) while still. Lower = steadier but laggier at slow speed.
pub const MIN_CUTOFF_HZ: f32 = 0.6;
/// Cutoff increase per rad/s of angular speed.
pub const BETA_ROT: f32 = 6.0;
/// Cutoff increase per m/s of linear speed.
pub const BETA_POS: f32 = 20.0;
/// Cutoff for the speed estimate itself.
const D_CUTOFF_HZ: f32 = 2.0;

fn alpha(cutoff_hz: f32, dt: f32) -> f32 {
    let tau = 1.0 / (TAU * cutoff_hz);
    1.0 / (1.0 + tau / dt)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AimFilter {
    state: Option<(Vec3, Quat)>,
    speed_rot: f32,
    speed_pos: f32,
}

impl AimFilter {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Feed the raw aim pose; returns the filtered one.
    pub fn update(&mut self, pos: Vec3, rot: Quat, dt: f32) -> (Vec3, Quat) {
        let dt = dt.clamp(1e-4, 0.1);
        let Some((p0, r0)) = self.state else {
            self.state = Some((pos, rot));
            return (pos, rot);
        };
        // Raw speeds relative to the filtered pose.
        let rot = if r0.dot(rot) < 0.0 { -rot } else { rot };
        let ang = r0.angle_between(rot);
        let lin = p0.distance(pos);
        let ad = alpha(D_CUTOFF_HZ, dt);
        self.speed_rot += ad * (ang / dt - self.speed_rot);
        self.speed_pos += ad * (lin / dt - self.speed_pos);

        let ar = alpha(MIN_CUTOFF_HZ + BETA_ROT * self.speed_rot, dt);
        let ap = alpha(MIN_CUTOFF_HZ + BETA_POS * self.speed_pos, dt);
        let r = r0.slerp(rot, ar).normalize();
        let p = p0.lerp(pos, ap);
        self.state = Some((p, r));
        (p, r)
    }
}

/// Press anchor that ignores small motion of the hit point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClickSlop {
    pub anchor: (f32, f32),
    slop_px: f32,
    pub moved: bool,
}

impl ClickSlop {
    /// `panel_h_px`: panel height in pixels; the slop scales with it.
    pub fn new(x: f32, y: f32, panel_h_px: u32) -> Self {
        let h = panel_h_px.max(240) as f32;
        ClickSlop { anchor: (x, y), slop_px: (h * 0.025).max(16.0), moved: false }
    }

    /// Already dragging (no slop).
    pub fn moving(x: f32, y: f32) -> Self {
        ClickSlop { anchor: (x, y), slop_px: 0.0, moved: true }
    }

    /// Current hit point; returns the point the button should be at.
    pub fn update(&mut self, x: f32, y: f32) -> (f32, f32) {
        if !self.moved && (x - self.anchor.0).hypot(y - self.anchor.1) > self.slop_px {
            self.moved = true;
        }
        if self.moved {
            self.anchor = (x, y);
        }
        self.anchor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_is_damped_when_still() {
        let mut f = AimFilter::default();
        let base = Quat::IDENTITY;
        f.update(Vec3::ZERO, base, 1.0 / 72.0);
        let mut worst: f32 = 0.0;
        for i in 0..200 {
            // ±0.3° alternating jitter.
            let a = if i % 2 == 0 { 0.3f32 } else { -0.3 }.to_radians();
            let (_, r) = f.update(Vec3::ZERO, Quat::from_rotation_y(a), 1.0 / 72.0);
            if i > 20 {
                worst = worst.max(r.angle_between(base).to_degrees());
            }
        }
        assert!(worst < 0.06, "residual jitter {worst}°");
    }

    #[test]
    fn fast_motion_follows_closely() {
        let mut f = AimFilter::default();
        let dt = 1.0 / 72.0;
        let mut last = Quat::IDENTITY;
        let mut out = Quat::IDENTITY;
        // Sweep at 2 rad/s for half a second.
        for i in 0..36 {
            last = Quat::from_rotation_y(2.0 * dt * i as f32);
            out = f.update(Vec3::ZERO, last, dt).1;
        }
        let lag = out.angle_between(last).to_degrees();
        assert!(lag < 3.0, "lag {lag}°");
    }

    #[test]
    fn converges_when_stopped() {
        let mut f = AimFilter::default();
        let dt = 1.0 / 72.0;
        f.update(Vec3::ZERO, Quat::IDENTITY, dt);
        let target = Quat::from_rotation_x(0.2);
        let mut out = Quat::IDENTITY;
        for _ in 0..72 {
            out = f.update(Vec3::new(0.0, 0.1, 0.0), target, dt).1;
        }
        assert!(out.angle_between(target).to_degrees() < 0.5);
    }

    #[test]
    fn slop_pins_small_motion_then_follows() {
        let mut s = ClickSlop::new(500.0, 500.0, 1080); // 27 px slop
        assert_eq!(s.update(510.0, 515.0), (500.0, 500.0));
        assert!(!s.moved);
        assert_eq!(s.update(540.0, 500.0), (540.0, 500.0));
        assert!(s.moved);
        // After moving it tracks exactly, even back inside the circle.
        assert_eq!(s.update(501.0, 500.0), (501.0, 500.0));
    }
}
