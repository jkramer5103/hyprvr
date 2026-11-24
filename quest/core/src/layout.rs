//! Panel arrangement: default arc, recentering, adding panels, grabbing.

use glam::{Quat, Vec3};
use hyprvr_proto::msg::{Curvature, PanelLayout, ScreenId};

use crate::geom::{face_toward, from_glam, to_glam, yaw_of_forward};

pub const DEFAULT_DISTANCE: f32 = 1.0;
pub const DEFAULT_WIDTH: f32 = 0.95;
pub const GAP: f32 = 0.03;

/// Head pose reduced to what layout needs.
#[derive(Clone, Copy, Debug)]
pub struct Head {
    pub position: Vec3,
    /// Yaw about +Y; 0 = facing -Z.
    pub yaw: f32,
}

impl Head {
    pub fn from_pose(position: Vec3, rot: Quat) -> Self {
        Head { position, yaw: yaw_of_forward(rot * Vec3::NEG_Z) }
    }
}

/// Place `screens` (ordered left to right, each with its aspect = w/h) in an arc around the head.
pub fn default_arc(screens: &[(ScreenId, f32)], head: Head) -> Vec<PanelLayout> {
    let n = screens.len();
    if n == 0 {
        return vec![];
    }
    // Angle occupied by each panel at DEFAULT_DISTANCE, plus gaps.
    let step = (DEFAULT_WIDTH + GAP) / DEFAULT_DISTANCE;
    let total = step * (n as f32 - 1.0);
    screens
        .iter()
        .enumerate()
        .map(|(i, (id, _aspect))| {
            // i = 0 is leftmost; positive yaw turns left.
            let a = head.yaw + total / 2.0 - step * i as f32;
            let dir = Quat::from_rotation_y(a) * Vec3::NEG_Z;
            let center = head.position + dir * DEFAULT_DISTANCE;
            let rot = face_toward(center, head.position + Vec3::new(0.0, center.y - head.position.y, 0.0));
            PanelLayout { screen: *id, pose: from_glam(center, rot), width_m: DEFAULT_WIDTH, curvature: Curvature::Flat }
        })
        .collect()
}

/// Rigidly move a whole layout so its "front" is in front of `head`, keeping the panels'
/// relative arrangement. The layout's viewer position is estimated from the panels' normals.
pub fn recenter(layout: &[PanelLayout], head: Head) -> Vec<PanelLayout> {
    if layout.is_empty() {
        return vec![];
    }
    let n = layout.len() as f32;
    let mut centroid = Vec3::ZERO;
    let mut normal = Vec3::ZERO;
    for p in layout {
        let (pos, rot) = to_glam(&p.pose);
        centroid += pos;
        normal += rot * Vec3::Z;
    }
    centroid /= n;
    let flat_n = Vec3::new(normal.x, 0.0, normal.z);
    if flat_n.length_squared() < 1e-6 {
        let ids: Vec<_> = layout.iter().map(|p| (p.screen, 16.0 / 9.0)).collect();
        return default_arc(&ids, head);
    }
    let flat_n = flat_n.normalize();
    // Old viewer: DEFAULT_DISTANCE in front of the centroid, at the centroid's height.
    let old_view = centroid + flat_n * DEFAULT_DISTANCE;
    // Old viewer looks along -flat_n.
    let old_yaw = yaw_of_forward(-flat_n);
    let delta_rot = Quat::from_rotation_y(head.yaw - old_yaw);
    layout
        .iter()
        .map(|p| {
            let (pos, rot) = to_glam(&p.pose);
            let new_pos = head.position + delta_rot * (pos - old_view);
            PanelLayout { pose: from_glam(new_pos, delta_rot * rot), ..*p }
        })
        .collect()
}

/// Pose for a new panel: to the right of the rightmost existing one (as seen from `head`),
/// or straight ahead if there are none.
pub fn place_new(layout: &[PanelLayout], screen: ScreenId, head: Head) -> PanelLayout {
    let fwd = Quat::from_rotation_y(head.yaw) * Vec3::NEG_Z;
    let right = Quat::from_rotation_y(head.yaw) * Vec3::X;
    let mut best: Option<(f32, &PanelLayout)> = None;
    for p in layout {
        let d = Vec3::from_array(p.pose.position) - head.position;
        let ang = d.dot(right).atan2(d.dot(fwd));
        if best.is_none_or(|(a, _)| ang > a) {
            best = Some((ang, p));
        }
    }
    let (ang, dist, y) = match best {
        None => (0.0, DEFAULT_DISTANCE, head.position.y),
        Some((a, p)) => {
            let d = Vec3::from_array(p.pose.position) - head.position;
            let dist = Vec3::new(d.x, 0.0, d.z).length().max(0.4);
            let half = (p.width_m / 2.0 + GAP + DEFAULT_WIDTH / 2.0) / dist;
            (a + half, dist, p.pose.position[1])
        }
    };
    // `ang` is measured toward +right; yaw rotates toward the left.
    let dir = Quat::from_rotation_y(head.yaw - ang) * Vec3::NEG_Z;
    let mut center = head.position + dir * dist;
    center.y = y;
    let rot = face_toward(center, Vec3::new(head.position.x, y, head.position.z));
    PanelLayout { screen, pose: from_glam(center, rot), width_m: DEFAULT_WIDTH, curvature: Curvature::Flat }
}

/// Ensure every screen has a layout entry; missing ones are placed with `place_new`.
pub fn reconcile(layout: &[PanelLayout], screens: &[ScreenId], head: Head) -> Vec<PanelLayout> {
    let mut out: Vec<PanelLayout> = layout.iter().filter(|p| screens.contains(&p.screen)).copied().collect();
    for s in screens {
        if !out.iter().any(|p| p.screen == *s) {
            let p = place_new(&out, *s, head);
            out.push(p);
        }
    }
    out
}

/// Grabbing a panel with a controller/hand: keeps the controller-relative offset, keeps the
/// panel upright and facing the head; thumbstick pushes/pulls and resizes.
#[derive(Clone, Copy, Debug)]
pub struct Grab {
    pub screen: ScreenId,
    /// Panel center in the controller's local frame at grab time.
    local_offset: Vec3,
    pub width_m: f32,
}

impl Grab {
    pub fn start(panel: &PanelLayout, ctrl_pos: Vec3, ctrl_rot: Quat) -> Self {
        let (pos, _) = to_glam(&panel.pose);
        Grab { screen: panel.screen, local_offset: ctrl_rot.inverse() * (pos - ctrl_pos), width_m: panel.width_m }
    }

    /// `push`: meters to move away (+) / toward (-) the controller this frame.
    /// `grow`: relative size change this frame (e.g. 0.01 = +1%).
    pub fn update(&mut self, panel: &mut PanelLayout, ctrl_pos: Vec3, ctrl_rot: Quat, head_pos: Vec3, push: f32, grow: f32) {
        let len = self.local_offset.length();
        if len > 1e-4 {
            let new_len = (len + push).clamp(0.25, 6.0);
            self.local_offset *= new_len / len;
        }
        self.width_m = (self.width_m * (1.0 + grow)).clamp(0.2, 6.0);
        let pos = ctrl_pos + ctrl_rot * self.local_offset;
        panel.pose = from_glam(pos, face_toward(pos, head_pos));
        panel.width_m = self.width_m;
        if let Curvature::Curved { .. } = panel.curvature {
            // Keep the curve centered on the viewer.
            panel.curvature = Curvature::Curved { radius_m: (pos - head_pos).length().max(0.3) };
        }
    }
}

/// Toggle a panel between flat and a cylinder centered on the viewer.
pub fn toggle_curve(panel: &mut PanelLayout, head_pos: Vec3) {
    panel.curvature = match panel.curvature {
        Curvature::Flat => {
            let d = (Vec3::from_array(panel.pose.position) - head_pos).length().max(0.3);
            Curvature::Curved { radius_m: d }
        }
        Curvature::Curved { .. } => Curvature::Flat,
    };
}

/// Pose of a UI strip just below a panel (center), tilted with the panel.
pub fn below_panel(panel_center: Vec3, panel_rot: Quat, panel_height: f32, strip_height: f32, gap: f32) -> (Vec3, Quat) {
    let down = panel_rot * Vec3::NEG_Y;
    (panel_center + down * (panel_height / 2.0 + gap + strip_height / 2.0), panel_rot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::PanelGeom;

    fn head() -> Head {
        Head { position: Vec3::new(0.0, 1.2, 0.0), yaw: 0.0 }
    }

    #[test]
    fn arc_is_symmetric_and_faces_user() {
        let l = default_arc(&[(1, 1.78), (2, 1.78), (3, 1.78)], head());
        assert_eq!(l.len(), 3);
        let p0 = Vec3::from_array(l[0].pose.position);
        let p1 = Vec3::from_array(l[1].pose.position);
        let p2 = Vec3::from_array(l[2].pose.position);
        assert!(p0.x < -0.5 && p2.x > 0.5 && p1.x.abs() < 1e-5);
        assert!((p0.x + p2.x).abs() < 1e-5);
        for p in &l {
            let (pos, rot) = to_glam(&p.pose);
            let to_head = (head().position - pos).normalize();
            assert!((rot * Vec3::Z).dot(to_head) > 0.999);
        }
        // Middle panel is hit straight ahead.
        let g = PanelGeom::new(&l[1], 1920, 1080);
        assert!(g.hit(&crate::geom::Ray::new(head().position, Vec3::NEG_Z)).is_some());
    }

    #[test]
    fn recenter_moves_layout_in_front_of_head() {
        let l = default_arc(&[(1, 1.78), (2, 1.78)], head());
        let h2 = Head { position: Vec3::new(2.0, 0.8, 3.0), yaw: 1.2 };
        let r = recenter(&l, h2);
        // Relative distances preserved.
        let d_old = Vec3::from_array(l[0].pose.position).distance(Vec3::from_array(l[1].pose.position));
        let d_new = Vec3::from_array(r[0].pose.position).distance(Vec3::from_array(r[1].pose.position));
        assert!((d_old - d_new).abs() < 1e-4);
        // Centroid lies in the new forward direction, at head height.
        let c = (Vec3::from_array(r[0].pose.position) + Vec3::from_array(r[1].pose.position)) / 2.0;
        let fwd = Quat::from_rotation_y(h2.yaw) * Vec3::NEG_Z;
        let d = c - h2.position;
        assert!(d.y.abs() < 1e-4, "{d}");
        assert!(Vec3::new(d.x, 0.0, d.z).normalize().dot(fwd) > 0.999, "{d} {fwd}");
        // Recentering twice is stable.
        let r2 = recenter(&r, h2);
        for (a, b) in r.iter().zip(&r2) {
            assert!(Vec3::from_array(a.pose.position).distance(Vec3::from_array(b.pose.position)) < 1e-3);
        }
    }

    #[test]
    fn place_new_goes_right() {
        let l = default_arc(&[(1, 1.78)], head());
        let p = place_new(&l, 2, head());
        let pos = Vec3::from_array(p.pose.position);
        assert!(pos.x > 0.8, "{pos}");
        assert!((pos.y - 1.2).abs() < 1e-4);
        let first = place_new(&[], 1, head());
        assert!(Vec3::from_array(first.pose.position).x.abs() < 1e-5);
        let rec = reconcile(&l, &[1, 2, 3], head());
        assert_eq!(rec.iter().map(|p| p.screen).collect::<Vec<_>>(), vec![1, 2, 3]);
        let rec = reconcile(&rec, &[3], head());
        assert_eq!(rec.len(), 1);
    }

    #[test]
    fn grab_follows_controller_and_pushes() {
        let mut p = default_arc(&[(1, 1.78)], head()).remove(0);
        let c0 = Vec3::new(0.2, 1.0, -0.3);
        let mut g = Grab::start(&p, c0, Quat::IDENTITY);
        g.update(&mut p, c0 + Vec3::new(0.5, 0.0, 0.0), Quat::IDENTITY, head().position, 0.0, 0.0);
        let pos = Vec3::from_array(p.pose.position);
        assert!((pos.x - 0.5).abs() < 1e-4, "{pos}");
        let before = pos.distance(c0 + Vec3::X * 0.5);
        g.update(&mut p, c0 + Vec3::new(0.5, 0.0, 0.0), Quat::IDENTITY, head().position, 0.2, 0.1);
        let after = Vec3::from_array(p.pose.position).distance(c0 + Vec3::X * 0.5);
        assert!((after - before - 0.2).abs() < 1e-4);
        assert!((p.width_m - DEFAULT_WIDTH * 1.1).abs() < 1e-4);
        toggle_curve(&mut p, head().position);
        assert!(matches!(p.curvature, Curvature::Curved { .. }));
        toggle_curve(&mut p, head().position);
        assert_eq!(p.curvature, Curvature::Flat);
    }
}
