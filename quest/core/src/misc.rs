//! Keyboard cutout, pointer ownership and clock-offset estimation.

use std::collections::VecDeque;

use glam::{Quat, Vec3};
use hyprvr_proto::msg::{KeyboardCutout, ScreenId};

use crate::geom::from_glam;

/// Depth (front to back) of the passthrough window behind the touched front edge. Covers a
/// laptop's palm rest, touchpad and keyboard (X280 deck is ~21 cm).
pub const CUTOUT_DEPTH: f32 = 0.24;
/// Extra margin around the touched width.
pub const CUTOUT_MARGIN: f32 = 0.03;

/// Build the keyboard cutout from the two touched **front** corners of the laptop base
/// (left, right — order doesn't matter). `user` is the head position, used to know which
/// side is "front". The rectangle extends `CUTOUT_DEPTH` away from the user.
pub fn cutout_from_corners(a: Vec3, b: Vec3, user: Vec3) -> KeyboardCutout {
    let mid = (a + b) / 2.0;
    let mut across = Vec3::new(b.x - a.x, 0.0, b.z - a.z);
    if across.length_squared() < 1e-6 {
        across = Vec3::X;
    }
    let width = across.length().max(0.05);
    let across = across.normalize();
    // Horizontal direction perpendicular to the edge, pointing away from the user.
    let mut back = Vec3::new(-across.z, 0.0, across.x);
    let to_user = Vec3::new(user.x - mid.x, 0.0, user.z - mid.z);
    if back.dot(to_user) > 0.0 {
        back = -back;
    }
    let center = mid + back * (CUTOUT_DEPTH / 2.0 - CUTOUT_MARGIN / 2.0);
    // Local X along the edge, local Z toward the user (=-back), local Y up.
    let z = -back;
    let x = Vec3::Y.cross(z).normalize();
    let rot = Quat::from_mat3(&glam::Mat3::from_cols(x, Vec3::Y, z));
    KeyboardCutout {
        pose: from_glam(center, rot),
        size_m: [width + 2.0 * CUTOUT_MARGIN, CUTOUT_DEPTH + CUTOUT_MARGIN],
        enabled: true,
    }
}

/// The four corners of a cutout (for rendering), counter-clockwise seen from above.
pub fn cutout_corners(c: &KeyboardCutout) -> [Vec3; 4] {
    let (pos, rot) = crate::geom::to_glam(&c.pose);
    let hx = c.size_m[0] / 2.0;
    let hz = c.size_m[1] / 2.0;
    [(-hx, hz), (hx, hz), (hx, -hz), (-hx, -hz)].map(|(x, z)| pos + rot * Vec3::new(x, 0.0, z))
}

/// Who drives the PC cursor: the headset laser or the physical mouse/touchpad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Laser,
    Physical,
}

/// Last-used-wins arbitration between the laser pointer and the laptop's own mouse.
#[derive(Debug)]
pub struct PointerArbiter {
    pub owner: Owner,
    last_sent: Option<(ScreenId, f32, f32, f64)>,
    /// Laser position at the moment we lost ownership.
    parked: Option<(ScreenId, f32, f32)>,
}

/// Pixels the server cursor must differ from what we sent to count as physical motion.
const PHYS_EPS: f32 = 3.0;
/// Grace after our own send during which server echoes are expected to lag.
const ECHO_GRACE: f64 = 0.25;
/// Pixels the laser must move to steal ownership back.
const LASER_EPS: f32 = 6.0;

impl Default for PointerArbiter {
    fn default() -> Self {
        PointerArbiter { owner: Owner::Physical, last_sent: None, parked: None }
    }
}

impl PointerArbiter {
    /// Laser currently hits `screen` at pixel (x, y). Returns a move to send, if any.
    pub fn on_laser(&mut self, hit: Option<(ScreenId, f32, f32)>, now: f64) -> Option<(ScreenId, f32, f32)> {
        let (s, x, y) = hit?;
        if self.owner == Owner::Physical {
            match self.parked {
                Some((ps, px, py)) if ps == s && (px - x).hypot(py - y) < LASER_EPS => return None,
                Some(_) => {}
                None => {
                    // First observation after losing ownership: remember where the laser was.
                    self.parked = Some((s, x, y));
                    return None;
                }
            }
            self.owner = Owner::Laser;
            self.parked = None;
        }
        if let Some((ls, lx, ly, _)) = self.last_sent
            && ls == s
            && (lx - x).abs() < 0.25
            && (ly - y).abs() < 0.25
        {
            return None;
        }
        self.last_sent = Some((s, x, y, now));
        Some((s, x, y))
    }

    /// A trigger/pinch press always takes ownership.
    pub fn on_press(&mut self) {
        self.owner = Owner::Laser;
        self.parked = None;
    }

    /// Server reported the cursor at (screen, x, y).
    pub fn on_server_cursor(&mut self, screen: Option<ScreenId>, x: f32, y: f32, now: f64) {
        if self.owner != Owner::Laser {
            return;
        }
        let Some((ls, lx, ly, t)) = self.last_sent else { return };
        if now - t < ECHO_GRACE {
            return;
        }
        let moved = match screen {
            Some(s) => s != ls || (lx - x).hypot(ly - y) > PHYS_EPS,
            None => true,
        };
        if moved {
            self.owner = Owner::Physical;
            self.parked = None;
        }
    }
}

/// Estimates `server_clock - client_clock` (µs) from Ping/Pong, using the lowest-RTT
/// samples of a sliding window.
#[derive(Default, Debug)]
pub struct ClockSync {
    samples: VecDeque<(u64, i64)>, // (rtt, offset)
}

impl ClockSync {
    pub fn add(&mut self, client_send_us: u64, server_us: u64, client_recv_us: u64) {
        if client_recv_us < client_send_us {
            return;
        }
        let rtt = client_recv_us - client_send_us;
        let mid = client_send_us + rtt / 2;
        self.samples.push_back((rtt, server_us as i64 - mid as i64));
        if self.samples.len() > 32 {
            self.samples.pop_front();
        }
    }

    pub fn offset_us(&self) -> Option<i64> {
        self.samples.iter().min_by_key(|s| s.0).map(|s| s.1)
    }

    pub fn rtt_us(&self) -> Option<u64> {
        self.samples.iter().map(|s| s.0).min()
    }

    /// Convert a server timestamp to the client clock.
    pub fn to_client(&self, server_us: u64) -> Option<u64> {
        self.offset_us().map(|o| (server_us as i64 - o).max(0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cutout_from_front_corners() {
        let user = Vec3::new(0.0, 1.2, 0.3);
        let c = cutout_from_corners(Vec3::new(-0.15, 0.75, -0.2), Vec3::new(0.15, 0.75, -0.2), user);
        assert!((c.size_m[0] - 0.36).abs() < 1e-5);
        let pos = Vec3::from_array(c.pose.position);
        // Extends away from the user (toward -Z).
        assert!(pos.z < -0.2, "{pos}");
        assert!((pos.y - 0.75).abs() < 1e-5);
        let corners = cutout_corners(&c);
        let minz = corners.iter().map(|p| p.z).fold(f32::MAX, f32::min);
        let maxz = corners.iter().map(|p| p.z).fold(f32::MIN, f32::max);
        assert!((maxz - (-0.2 + CUTOUT_MARGIN / 2.0 + CUTOUT_MARGIN / 2.0)).abs() < 0.02, "{maxz}");
        assert!((maxz - minz - (CUTOUT_DEPTH + CUTOUT_MARGIN)).abs() < 1e-4);
        // Swapped order gives the same rectangle.
        let c2 = cutout_from_corners(Vec3::new(0.15, 0.75, -0.2), Vec3::new(-0.15, 0.75, -0.2), user);
        assert!(Vec3::from_array(c2.pose.position).distance(pos) < 1e-5);
    }

    #[test]
    fn arbiter_last_used_wins() {
        let mut a = PointerArbiter::default();
        // Laser starts parked: the first observation doesn't steal.
        assert_eq!(a.on_laser(Some((1, 100.0, 100.0)), 0.0), None);
        assert_eq!(a.owner, Owner::Physical);
        // Real laser motion takes over.
        assert_eq!(a.on_laser(Some((1, 120.0, 100.0)), 0.1), Some((1, 120.0, 100.0)));
        assert_eq!(a.owner, Owner::Laser);
        // Server echo of our own position keeps laser ownership.
        a.on_server_cursor(Some(1), 120.0, 100.0, 0.5);
        assert_eq!(a.owner, Owner::Laser);
        // Echo lag within grace is ignored even if stale.
        a.on_laser(Some((1, 200.0, 100.0)), 1.0);
        a.on_server_cursor(Some(1), 150.0, 100.0, 1.1);
        assert_eq!(a.owner, Owner::Laser);
        // Physical mouse moves the cursor elsewhere after the grace -> physical wins.
        a.on_server_cursor(Some(2), 10.0, 10.0, 2.0);
        assert_eq!(a.owner, Owner::Physical);
        // Resting laser doesn't steal back.
        assert_eq!(a.on_laser(Some((1, 200.0, 100.0)), 2.1), None);
        assert_eq!(a.on_laser(Some((1, 202.0, 101.0)), 2.2), None);
        // Trigger does.
        a.on_press();
        assert_eq!(a.owner, Owner::Laser);
        assert!(a.on_laser(Some((1, 210.0, 100.0)), 2.3).is_some());
    }

    #[test]
    fn clock_picks_min_rtt() {
        let mut c = ClockSync::default();
        c.add(1000, 50_000 + 1005, 1010); // rtt 10, offset 50000
        c.add(2000, 50_000 + 2100, 2400); // rtt 400, asymmetric
        assert_eq!(c.offset_us(), Some(50_000));
        assert_eq!(c.to_client(60_000), Some(10_000));
        assert_eq!(c.rtt_us(), Some(10));
    }
}
