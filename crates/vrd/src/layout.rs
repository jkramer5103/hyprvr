//! 3D panel placement <-> Hyprland monitor positions.

use hyprvr_proto::msg::{Curvature, Pose};

use crate::store::SlotLayout;

/// Eye height used for default placement (STAGE space, meters above floor, seated).
pub const DEFAULT_EYE_Y: f32 = 1.15;
pub const DEFAULT_DISTANCE: f32 = 1.1;
/// Panel width for a 1920 px wide screen.
pub const METERS_PER_1920PX: f32 = 1.15;

fn yaw_quat(yaw: f32) -> [f32; 4] {
    [0.0, (yaw / 2.0).sin(), 0.0, (yaw / 2.0).cos()]
}

/// Rotate vector `v` by unit quaternion `q` ([x,y,z,w]).
pub fn rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let (qx, qy, qz, qw) = (q[0], q[1], q[2], q[3]);
    // t = 2 * cross(q.xyz, v)
    let t = [2.0 * (qy * v[2] - qz * v[1]), 2.0 * (qz * v[0] - qx * v[2]), 2.0 * (qx * v[1] - qy * v[0])];
    [
        v[0] + qw * t[0] + (qy * t[2] - qz * t[1]),
        v[1] + qw * t[1] + (qz * t[0] - qx * t[2]),
        v[2] + qw * t[2] + (qx * t[1] - qy * t[0]),
    ]
}

/// Default layout for slot `index` given the widths (px) of all slots: slot 0 in front,
/// then alternating right/left on an arc, the 6th above the center.
pub fn default_slot(index: usize, widths_px: &[u32], heights_px: &[u32]) -> SlotLayout {
    let width_of = |i: usize| widths_px.get(i).copied().unwrap_or(1920) as f32 / 1920.0 * METERS_PER_1920PX;
    let gap = 0.04;
    let ang = |w: f32| (w + gap) / DEFAULT_DISTANCE; // arc angle of a panel
    let w = width_of(index);
    let mut yaw = 0.0f32; // positive = to the right of the user
    let mut y = DEFAULT_EYE_Y;
    if index == 5 {
        let h0 = width_of(0) * heights_px.first().copied().unwrap_or(1080) as f32 / widths_px.first().copied().unwrap_or(1920) as f32;
        let h = w * heights_px.get(5).copied().unwrap_or(1080) as f32 / widths_px.get(5).copied().unwrap_or(1920) as f32;
        y += (h0 + h) / 2.0 + gap;
    } else if index > 0 {
        // Walk outward on the side of this slot, accumulating half-widths.
        let right = index % 2 == 1;
        let mut acc = ang(width_of(0)) / 2.0;
        let mut i = if right { 1 } else { 2 };
        while i < index {
            acc += ang(width_of(i));
            i += 2;
        }
        acc += ang(w) / 2.0;
        yaw = if right { acc } else { -acc };
    }
    let pos = [DEFAULT_DISTANCE * yaw.sin(), y, -DEFAULT_DISTANCE * yaw.cos()];
    SlotLayout { pose: Pose { position: pos, orientation: yaw_quat(-yaw) }, width_m: w, curvature: Curvature::Flat }
}

/// A VR panel for layout sync.
#[derive(Clone, Debug)]
pub struct SyncPanel {
    pub output: String,
    /// Logical size (pixels / scale).
    pub width: i32,
    pub height: i32,
    pub layout: SlotLayout,
}

/// A physical monitor (kept as a rigid block, moved below the VR row).
#[derive(Clone, Debug)]
pub struct SyncPhysical {
    pub output: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
}

/// Compute monitor positions (logical px) for VR panels and physical monitors.
pub fn sync_positions(panels: &[SyncPanel], physical: &[SyncPhysical]) -> Vec<(String, i32, i32)> {
    let mut out = Vec::new();
    if panels.is_empty() {
        return out;
    }
    // Average horizontal "right" direction of the panels.
    let mut r = [0.0f32; 3];
    for p in panels {
        let v = rotate(p.layout.pose.orientation, [1.0, 0.0, 0.0]);
        r[0] += v[0];
        r[2] += v[2];
    }
    let n = (r[0] * r[0] + r[2] * r[2]).sqrt();
    let r = if n > 1e-4 { [r[0] / n, 0.0, r[2] / n] } else { [1.0, 0.0, 0.0] };

    struct Item<'a> {
        p: &'a SyncPanel,
        u: f32,
        v: f32,
        h_m: f32,
    }
    let mut items: Vec<Item> = panels
        .iter()
        .map(|p| {
            let pos = p.layout.pose.position;
            Item {
                p,
                u: pos[0] * r[0] + pos[2] * r[2],
                v: pos[1],
                h_m: p.layout.width_m * p.height as f32 / p.width.max(1) as f32,
            }
        })
        .collect();
    items.sort_by(|a, b| b.v.total_cmp(&a.v));

    // Group into rows (top to bottom).
    let mut rows: Vec<Vec<Item>> = Vec::new();
    for it in items {
        if let Some(row) = rows.last_mut() {
            let mean_v = row.iter().map(|i| i.v).sum::<f32>() / row.len() as f32;
            let min_h = row.iter().map(|i| i.h_m).fold(it.h_m, f32::min);
            if (it.v - mean_v).abs() < 0.5 * min_h {
                row.push(it);
                continue;
            }
        }
        rows.push(vec![it]);
    }
    for row in &mut rows {
        row.sort_by(|a, b| a.u.total_cmp(&b.u));
    }
    let row_w: Vec<i32> = rows.iter().map(|row| row.iter().map(|i| i.p.width).sum()).collect();
    let max_w = row_w.iter().copied().max().unwrap_or(0);
    let mut y = 0;
    for (row, w) in rows.iter().zip(&row_w) {
        let mut x = (max_w - w) / 2;
        let row_h = row.iter().map(|i| i.p.height).max().unwrap_or(0);
        for it in row {
            // Bottom-align panels in a row so the mouse crosses where edges meet.
            out.push((it.p.output.clone(), x, y + row_h - it.p.height));
            x += it.p.width;
        }
        y += row_h;
    }
    // Physical block centered below.
    if !physical.is_empty() {
        let bx0 = physical.iter().map(|m| m.x).min().unwrap();
        let by0 = physical.iter().map(|m| m.y).min().unwrap();
        let bx1 = physical.iter().map(|m| m.x + m.width).max().unwrap();
        let dx = (max_w - (bx1 - bx0)) / 2 - bx0;
        let dy = y - by0;
        for m in physical {
            out.push((m.output.clone(), m.x + dx, m.y + dy));
        }
    }
    // Hyprland is happiest with non-negative coordinates.
    let minx = out.iter().map(|o| o.1).min().unwrap_or(0).min(0);
    for o in &mut out {
        o.1 -= minx;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel(name: &str, slot: &SlotLayout) -> SyncPanel {
        SyncPanel { output: name.into(), width: 1920, height: 1080, layout: *slot }
    }

    fn get(v: &[(String, i32, i32)], n: &str) -> (i32, i32) {
        let e = v.iter().find(|e| e.0 == n).unwrap();
        (e.1, e.2)
    }

    #[test]
    fn default_three_form_a_row_with_edp_below() {
        let ws = [1920, 1920, 1920];
        let hs = [1080, 1080, 1080];
        let s: Vec<_> = (0..3).map(|i| default_slot(i, &ws, &hs)).collect();
        // slot 1 right of center, slot 2 left.
        assert!(s[1].pose.position[0] > 0.3 && s[2].pose.position[0] < -0.3);
        let panels = vec![panel("VR-1", &s[0]), panel("VR-2", &s[1]), panel("VR-3", &s[2])];
        let phys = vec![SyncPhysical { output: "eDP-1".into(), x: 0, y: 0, width: 1920 }];
        let r = sync_positions(&panels, &phys);
        assert_eq!(get(&r, "VR-3"), (0, 0));
        assert_eq!(get(&r, "VR-1"), (1920, 0));
        assert_eq!(get(&r, "VR-2"), (3840, 0));
        assert_eq!(get(&r, "eDP-1"), (1920, 1080));
    }

    #[test]
    fn panel_above_stacks() {
        let ws = [1920; 6];
        let hs = [1080; 6];
        let s0 = default_slot(0, &ws, &hs);
        let s5 = default_slot(5, &ws, &hs);
        assert!(s5.pose.position[1] > s0.pose.position[1] + 0.5);
        let r = sync_positions(&[panel("A", &s0), panel("TOP", &s5)], &[]);
        assert_eq!(get(&r, "TOP"), (0, 0));
        assert_eq!(get(&r, "A"), (0, 1080));
    }

    #[test]
    fn small_height_jitter_keeps_one_row_and_mixed_sizes() {
        let mut a = default_slot(0, &[2560], &[1440]);
        let b0 = default_slot(1, &[2560, 1080], &[1440, 1920]);
        let mut b = b0;
        b.pose.position[1] += 0.1;
        a.pose.position[1] -= 0.05;
        let r = sync_positions(
            &[
                SyncPanel { output: "W".into(), width: 2560, height: 1440, layout: a },
                SyncPanel { output: "P".into(), width: 1080, height: 1920, layout: b },
            ],
            &[SyncPhysical { output: "eDP-1".into(), x: 100, y: 50, width: 1280 }],
        );
        assert_eq!(get(&r, "W"), (0, 480));
        assert_eq!(get(&r, "P"), (2560, 0));
        assert_eq!(get(&r, "eDP-1"), ((3640 - 1280) / 2, 1920));
    }

    #[test]
    fn rotate_yaw() {
        let q = yaw_quat(std::f32::consts::FRAC_PI_2);
        let v = rotate(q, [0.0, 0.0, 1.0]);
        assert!((v[0] - 1.0).abs() < 1e-5 && v[2].abs() < 1e-5);
    }
}
