//! Main-menu placement and the "look at your left palm and pinch" gesture.

use glam::{Quat, Vec3};

use crate::geom::face_toward;

/// Palm must face the eyes at least this much (cosine).
const PALM_FACING_COS: f32 = 0.6;
/// Hand must be within this distance of the head.
const PALM_MAX_DIST: f32 = 0.75;

/// Direction the palm faces, from the OpenXR grip pose: +X points out of the left palm and
/// into the right palm.
pub fn palm_normal(grip_rot: Quat, left: bool) -> Vec3 {
    let x = grip_rot * Vec3::X;
    if left { x } else { -x }
}

/// True when the palm is turned toward the user's face (Meta-style menu gesture pose).
pub fn palm_facing_head(grip_pos: Vec3, grip_rot: Quat, left: bool, head: Vec3) -> bool {
    let to_head = head - grip_pos;
    let d = to_head.length();
    if !(0.05..=PALM_MAX_DIST).contains(&d) {
        return false;
    }
    palm_normal(grip_rot, left).dot(to_head / d) > PALM_FACING_COS
}

/// Default menu pose: in front of the user, below eye level, tilted toward the eyes, and
/// closer than the screens so it never hides behind them.
pub fn default_menu_pose(head_pos: Vec3, head_rot: Quat) -> (Vec3, Quat) {
    let fwd = head_rot * Vec3::NEG_Z;
    let flat = Vec3::new(fwd.x, 0.0, fwd.z).normalize_or(Vec3::NEG_Z);
    let pos = head_pos + flat * 0.5 + Vec3::new(0.0, -0.32, 0.0);
    (pos, face_toward(pos, head_pos))
}

/// Whether a remembered menu pose is still usable when the menu is reopened (close enough
/// and roughly in front of the user); otherwise it is re-placed with `default_menu_pose`.
pub fn menu_pose_reusable(pos: Vec3, head_pos: Vec3, head_rot: Quat) -> bool {
    let to = pos - head_pos;
    let d = to.length();
    if !(0.2..=1.5).contains(&d) {
        return false;
    }
    let fwd = head_rot * Vec3::NEG_Z;
    fwd.dot(to / d) > 0.5 // within ~60 degrees of the view direction
}

/// Dragging the menu along the laser: keeps the grabbed point at a fixed distance on the ray.
#[derive(Clone, Copy, Debug)]
pub struct MenuDrag {
    pub hand: usize,
    /// Distance along the ray to the grabbed point.
    dist: f32,
    /// Menu center minus grabbed point, in the menu's local frame.
    local_offset: Vec3,
    /// Drag started with select (pinch/trigger) and ends on its release; else squeeze.
    pub by_select: bool,
}

impl MenuDrag {
    pub fn start(hand: usize, by_select: bool, ray_origin: Vec3, ray_dir: Vec3, hit_t: f32, menu_pos: Vec3, menu_rot: Quat) -> Self {
        let hit = ray_origin + ray_dir * hit_t;
        MenuDrag { hand, dist: hit_t.max(0.15), local_offset: menu_rot.inverse() * (menu_pos - hit), by_select }
    }

    /// New menu pose for the current ray; `push` (meters) moves it nearer/farther.
    pub fn update(&mut self, ray_origin: Vec3, ray_dir: Vec3, head: Vec3, push: f32) -> (Vec3, Quat) {
        self.dist = (self.dist + push).clamp(0.2, 2.0);
        let hit = ray_origin + ray_dir * self.dist;
        let rot = face_toward(hit, head);
        (hit + rot * self.local_offset, rot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palm_toward_face_detected() {
        let head = Vec3::new(0.0, 1.6, 0.0);
        let hand = Vec3::new(-0.1, 1.2, -0.35);
        // Left grip whose +X points at the head.
        let to_head = (head - hand).normalize();
        let rot = Quat::from_rotation_arc(Vec3::X, to_head);
        assert!(palm_facing_head(hand, rot, true, head));
        // Same pose for the right hand means the back of the hand faces the head.
        assert!(!palm_facing_head(hand, rot, false, head));
        // Palm facing away.
        assert!(!palm_facing_head(hand, Quat::from_rotation_arc(Vec3::X, -to_head), true, head));
        // Too far away.
        assert!(!palm_facing_head(head + Vec3::new(0.0, 0.0, -1.0), rot, true, head));
    }

    #[test]
    fn default_pose_in_front_below_eyes_and_reusable() {
        let head = Vec3::new(0.0, 1.6, 0.0);
        let (p, r) = default_menu_pose(head, Quat::IDENTITY);
        assert!(p.z < -0.4 && p.y < 1.4);
        assert!((r * Vec3::Z).dot((head - p).normalize()) > 0.99);
        assert!(menu_pose_reusable(p, head, Quat::IDENTITY));
        // Turned around -> not reusable.
        assert!(!menu_pose_reusable(p, head, Quat::from_rotation_y(std::f32::consts::PI)));
    }

    #[test]
    fn drag_keeps_grab_point_on_ray() {
        let head = Vec3::new(0.0, 1.6, 0.0);
        let (mp, mr) = default_menu_pose(head, Quat::IDENTITY);
        let origin = Vec3::new(0.2, 1.3, -0.1);
        let dir = (mp + Vec3::new(0.1, 0.0, 0.0) - origin).normalize();
        let t = (mp + Vec3::new(0.1, 0.0, 0.0) - origin).length();
        let mut d = MenuDrag::start(1, true, origin, dir, t, mp, mr);
        let (p, _) = d.update(origin, dir, head, 0.0);
        assert!((p - mp).length() < 0.02, "{p} vs {mp}");
        let dir2 = Quat::from_rotation_y(0.3) * dir;
        let (p2, r2) = d.update(origin, dir2, head, 0.0);
        assert!((p2 - mp).length() > 0.05);
        assert!((r2 * Vec3::Z).dot((head - p2).normalize()) > 0.9);
    }
}
