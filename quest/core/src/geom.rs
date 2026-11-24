//! Panel geometry and ray hit tests (STAGE space, meters, +Y up, panels face +Z).

use glam::{Quat, Vec3};
use hyprvr_proto::msg::{Curvature, PanelLayout, Pose};

pub fn to_glam(p: &Pose) -> (Vec3, Quat) {
    (Vec3::from_array(p.position), Quat::from_array(p.orientation).normalize())
}

pub fn from_glam(pos: Vec3, rot: Quat) -> Pose {
    Pose { position: pos.to_array(), orientation: rot.normalize().to_array() }
}

#[derive(Clone, Copy, Debug)]
pub struct Ray {
    pub origin: Vec3,
    pub dir: Vec3,
}

impl Ray {
    pub fn new(origin: Vec3, dir: Vec3) -> Self {
        Ray { origin, dir: dir.normalize_or(Vec3::NEG_Z) }
    }
    pub fn at(&self, t: f32) -> Vec3 {
        self.origin + self.dir * t
    }
}

/// Hit on a panel: distance along the ray and normalized panel coordinates
/// (u: 0 = left, 1 = right; v: 0 = top, 1 = bottom).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    pub t: f32,
    pub u: f32,
    pub v: f32,
}

/// Resolved 3D geometry of one panel.
#[derive(Clone, Copy, Debug)]
pub struct PanelGeom {
    /// Center of the panel surface.
    pub center: Vec3,
    pub rot: Quat,
    pub width: f32,
    pub height: f32,
    pub curvature: Curvature,
}

impl PanelGeom {
    pub fn new(layout: &PanelLayout, px_w: u32, px_h: u32) -> Self {
        let (center, rot) = to_glam(&layout.pose);
        let aspect = if px_h == 0 { 16.0 / 9.0 } else { px_w as f32 / px_h as f32 };
        let width = layout.width_m.max(0.05);
        PanelGeom { center, rot, width, height: width / aspect, curvature: layout.curvature }
    }

    pub fn normal(&self) -> Vec3 {
        self.rot * Vec3::Z
    }

    /// Effective radius (None for flat).
    pub fn radius(&self) -> Option<f32> {
        match self.curvature {
            Curvature::Flat => None,
            Curvature::Curved { radius_m } => {
                // A cylinder section can't exceed ~2*pi; clamp so the panel stays sane.
                let min_r = self.width / (std::f32::consts::PI * 1.5);
                Some(radius_m.max(min_r).max(0.1))
            }
        }
    }

    /// Parameters for XrCompositionLayerCylinderKHR: (axis center, central angle, aspect).
    pub fn cylinder_params(&self) -> Option<(Vec3, f32, f32, f32)> {
        let r = self.radius()?;
        let center = self.center + self.rot * Vec3::new(0.0, 0.0, r);
        Some((center, r, self.width / r, self.width / self.height))
    }

    /// World point at panel coordinates (u, v), plus a small offset along the surface normal.
    pub fn point_at(&self, u: f32, v: f32, lift: f32) -> Vec3 {
        let y = (0.5 - v) * self.height;
        match self.cylinder_params() {
            None => self.center + self.rot * Vec3::new((u - 0.5) * self.width, y, lift),
            Some((axis, r, angle, _)) => {
                let a = (u - 0.5) * angle;
                let rr = r - lift;
                axis + self.rot * Vec3::new(rr * a.sin(), y, -rr * a.cos())
            }
        }
    }

    pub fn hit(&self, ray: &Ray) -> Option<Hit> {
        let inv = self.rot.inverse();
        match self.cylinder_params() {
            None => {
                let o = inv * (ray.origin - self.center);
                let d = inv * ray.dir;
                if d.z.abs() < 1e-6 {
                    return None;
                }
                let t = -o.z / d.z;
                if t <= 0.0 {
                    return None;
                }
                let p = o + d * t;
                let u = p.x / self.width + 0.5;
                let v = 0.5 - p.y / self.height;
                ((0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v)).then_some(Hit { t, u, v })
            }
            Some((axis, r, angle, _)) => {
                let o = inv * (ray.origin - axis);
                let d = inv * ray.dir;
                // Solve (o.x + t d.x)^2 + (o.z + t d.z)^2 = r^2
                let a = d.x * d.x + d.z * d.z;
                if a < 1e-9 {
                    return None;
                }
                let b = 2.0 * (o.x * d.x + o.z * d.z);
                let c = o.x * o.x + o.z * o.z - r * r;
                let disc = b * b - 4.0 * a * c;
                if disc < 0.0 {
                    return None;
                }
                let sq = disc.sqrt();
                let mut best: Option<Hit> = None;
                for t in [(-b - sq) / (2.0 * a), (-b + sq) / (2.0 * a)] {
                    if t <= 0.0 {
                        continue;
                    }
                    let p = o + d * t;
                    // Angle measured from -Z toward +X.
                    let ang = p.x.atan2(-p.z);
                    let u = ang / angle + 0.5;
                    let v = 0.5 - p.y / self.height;
                    if (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v) && best.is_none_or(|h| t < h.t) {
                        best = Some(Hit { t, u, v });
                    }
                }
                best
            }
        }
    }
}

/// Generic hit test for a flat rectangle (used for UI quads): center/rot/size.
pub fn hit_rect(center: Vec3, rot: Quat, w: f32, h: f32, ray: &Ray) -> Option<Hit> {
    PanelGeom { center, rot, width: w, height: h, curvature: Curvature::Flat }.hit(ray)
}

/// Rotation that makes +Z point from `from` toward `to`, keeping world up.
pub fn face_toward(from: Vec3, to: Vec3) -> Quat {
    let dir = to - from;
    let flat = Vec3::new(dir.x, 0.0, dir.z);
    if flat.length_squared() < 1e-8 {
        return Quat::IDENTITY;
    }
    let yaw = flat.x.atan2(flat.z);
    let pitch = -(dir.y).atan2(flat.length());
    Quat::from_rotation_y(yaw) * Quat::from_rotation_x(pitch)
}

/// Yaw (radians, about +Y) of the horizontal part of `forward`; 0 = looking down -Z.
pub fn yaw_of_forward(forward: Vec3) -> f32 {
    (-forward.x).atan2(-forward.z)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(curv: Curvature) -> PanelLayout {
        PanelLayout {
            screen: 1,
            pose: from_glam(Vec3::new(0.0, 1.2, -1.0), Quat::IDENTITY),
            width_m: 1.6,
            curvature: curv,
        }
    }

    #[test]
    fn flat_hit_center_and_corner() {
        let g = PanelGeom::new(&layout(Curvature::Flat), 1600, 900);
        assert!((g.height - 0.9).abs() < 1e-5);
        let h = g.hit(&Ray::new(Vec3::new(0.0, 1.2, 0.0), Vec3::NEG_Z)).unwrap();
        assert!((h.u - 0.5).abs() < 1e-5 && (h.v - 0.5).abs() < 1e-5 && (h.t - 1.0).abs() < 1e-5);
        // Aim at the top-left area.
        let target = Vec3::new(-0.7, 1.2 + 0.4, -1.0);
        let h = g.hit(&Ray::new(Vec3::new(0.0, 1.2, 0.0), target - Vec3::new(0.0, 1.2, 0.0))).unwrap();
        assert!((h.u - 0.0625).abs() < 1e-4, "{h:?}");
        assert!((h.v - (0.5 - 0.4 / 0.9)).abs() < 1e-4);
        // Miss.
        assert!(g.hit(&Ray::new(Vec3::new(0.0, 1.2, 0.0), Vec3::Z)).is_none());
        assert!(g.hit(&Ray::new(Vec3::new(5.0, 1.2, 0.0), Vec3::NEG_Z)).is_none());
    }

    #[test]
    fn flat_rotated_roundtrip() {
        let mut l = layout(Curvature::Flat);
        l.pose = from_glam(Vec3::new(1.0, 1.0, -0.5), Quat::from_rotation_y(-0.8));
        let g = PanelGeom::new(&l, 1920, 1080);
        let eye = Vec3::new(0.0, 1.1, 0.3);
        for (u, v) in [(0.2, 0.3), (0.9, 0.9), (0.5, 0.1)] {
            let p = g.point_at(u, v, 0.0);
            let h = g.hit(&Ray::new(eye, p - eye)).unwrap();
            assert!((h.u - u).abs() < 1e-4 && (h.v - v).abs() < 1e-4, "{h:?} vs {u},{v}");
        }
    }

    #[test]
    fn cylinder_hit_from_inside() {
        let g = PanelGeom::new(&layout(Curvature::Curved { radius_m: 1.0 }), 1600, 900);
        let (axis, r, angle, aspect) = g.cylinder_params().unwrap();
        assert!((axis - Vec3::new(0.0, 1.2, 0.0)).length() < 1e-5);
        assert!((r - 1.0).abs() < 1e-6 && (angle - 1.6).abs() < 1e-6 && (aspect - 16.0 / 9.0).abs() < 1e-5);
        // From the axis, straight ahead hits the center at distance r.
        let h = g.hit(&Ray::new(Vec3::new(0.0, 1.2, 0.0), Vec3::NEG_Z)).unwrap();
        assert!((h.u - 0.5).abs() < 1e-5 && (h.t - 1.0).abs() < 1e-5);
        // Roundtrip arbitrary points from an off-axis eye.
        let eye = Vec3::new(0.1, 1.3, 0.2);
        for (u, v) in [(0.05, 0.5), (0.7, 0.2), (0.99, 0.95)] {
            let p = g.point_at(u, v, 0.0);
            let h = g.hit(&Ray::new(eye, p - eye)).unwrap();
            assert!((h.u - u).abs() < 1e-4 && (h.v - v).abs() < 1e-4, "{h:?} vs {u},{v}");
        }
        // Behind the arc (outside the angle) misses.
        assert!(g.hit(&Ray::new(Vec3::new(0.0, 1.2, 0.0), Vec3::Z)).is_none());
    }

    #[test]
    fn face_toward_points_z_at_target() {
        let q = face_toward(Vec3::new(1.0, 1.0, -1.0), Vec3::new(0.0, 1.5, 0.0));
        let z = q * Vec3::Z;
        let want = (Vec3::new(0.0, 1.5, 0.0) - Vec3::new(1.0, 1.0, -1.0)).normalize();
        assert!((z - want).length() < 1e-5, "{z} {want}");
        assert!((q * Vec3::X).y.abs() < 1e-5, "no roll");
    }

    #[test]
    fn yaw() {
        assert!(yaw_of_forward(Vec3::NEG_Z).abs() < 1e-6);
        let q = Quat::from_rotation_y(0.7);
        assert!((yaw_of_forward(q * Vec3::NEG_Z) - 0.7).abs() < 1e-5);
    }
}
