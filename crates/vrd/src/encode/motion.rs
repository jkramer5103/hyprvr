//! Decides which part of a screen is "moving" and should be sent as video.
//! Pure logic (time is injected) so it is unit-testable.

use hyprvr_proto::TILE_SIZE;
use hyprvr_proto::msg::Rect;

#[derive(Clone, Debug)]
pub struct MotionParams {
    /// Time constant of the per-tile change-rate filter, seconds.
    pub tau: f32,
    /// A tile is hot when its filtered rate is >= this (~ tau * changes/second).
    pub hot: f32,
    /// Hot tiles needed to consider a region moving.
    pub min_hot_tiles: usize,
    /// Consecutive frames with enough hot tiles before starting video.
    pub enter_frames: u32,
    /// Stop video after the region has been completely still this long.
    pub exit_still: f32,
    /// Stop video if hot tiles stay below `min_hot_tiles / 2` this long.
    pub exit_cold: f32,
    /// Grow the region if hot tiles outside it persist this many frames.
    pub grow_frames: u32,
    /// Shrink if the hot bounding box covers < `shrink_fill` of the region for `shrink_after` s.
    pub shrink_fill: f32,
    pub shrink_after: f32,
    /// Minimum video region size in pixels (each axis, before clipping to the screen).
    pub min_size: u32,
}

impl Default for MotionParams {
    fn default() -> Self {
        MotionParams {
            tau: 0.25,
            hot: 2.5,
            min_hot_tiles: 12,
            enter_frames: 3,
            exit_still: 0.15,
            exit_cold: 0.5,
            grow_frames: 4,
            shrink_fill: 0.4,
            shrink_after: 1.5,
            min_size: 128,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Keep the current mode.
    Keep,
    StartVideo(Rect),
    /// Replace the region (stop the old one, start this).
    MoveVideo(Rect),
    StopVideo,
}

pub struct MotionTracker {
    p: MotionParams,
    width: u32,
    height: u32,
    cols: u32,
    rate: Vec<f32>,
    last_t: Option<f64>,
    hot_frames: u32,
    region: Option<Rect>,
    last_change_in_region: f64,
    cold_since: Option<f64>,
    outside_frames: u32,
    sparse_since: Option<f64>,
}

fn union(a: Rect, b: Rect) -> Rect {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = (a.x + a.w).max(b.x + b.w);
    let y1 = (a.y + a.h).max(b.y + b.h);
    Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
}

pub fn intersects(a: &Rect, b: &Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

pub fn contains(outer: &Rect, inner: &Rect) -> bool {
    inner.x >= outer.x && inner.y >= outer.y && inner.x + inner.w <= outer.x + outer.w && inner.y + inner.h <= outer.y + outer.h
}

impl MotionTracker {
    pub fn new(width: u32, height: u32, p: MotionParams) -> Self {
        let cols = width.div_ceil(TILE_SIZE);
        let rows = height.div_ceil(TILE_SIZE);
        MotionTracker {
            p,
            width,
            height,
            cols,
            rate: vec![0.0; (cols * rows) as usize],
            last_t: None,
            hot_frames: 0,
            region: None,
            last_change_in_region: 0.0,
            cold_since: None,
            outside_frames: 0,
            sparse_since: None,
        }
    }

    #[cfg(test)]
    pub fn region(&self) -> Option<Rect> {
        self.region
    }

    /// Forget the region (e.g. the encoder failed). Rates are kept.
    pub fn clear_region(&mut self) {
        self.region = None;
        self.hot_frames = 0;
    }

    fn tile_rect(&self, i: usize) -> Rect {
        let (c, r) = (i as u32 % self.cols, i as u32 / self.cols);
        Rect { x: c * TILE_SIZE, y: r * TILE_SIZE, w: TILE_SIZE.min(self.width - c * TILE_SIZE), h: TILE_SIZE.min(self.height - r * TILE_SIZE) }
    }

    /// Expand to the minimum size (centered), clip to the screen, keep even sizes.
    fn finish_rect(&self, r: Rect) -> Rect {
        let grow = |pos: u32, len: u32, max: u32| -> (u32, u32) {
            let min = self.p.min_size.min(max);
            if len >= min {
                return (pos, len);
            }
            let extra = min - len;
            let start = pos.saturating_sub(extra / 2).min(max - min);
            (start, min)
        };
        let (x, w) = grow(r.x, r.w, self.width);
        let (y, h) = grow(r.y, r.h, self.height);
        // Align the origin down to 16 px (tiles already are), size up to the edge or even.
        let (ax, ay) = (x / 16 * 16, y / 16 * 16);
        let (w, h) = (w + (x - ax), h + (y - ay));
        let w = w.div_ceil(16) * 16;
        let h = h.div_ceil(16) * 16;
        let w = w.min(self.width - ax) & !1;
        let h = h.min(self.height - ay) & !1;
        Rect { x: ax, y: ay, w, h }
    }

    fn decay(&mut self, now: f64) {
        if let Some(t) = self.last_t {
            let k = (-((now - t).max(0.0) as f32) / self.p.tau).exp();
            self.rate.iter_mut().for_each(|r| *r *= k);
        }
        self.last_t = Some(now);
    }

    /// A captured frame with `changed` tiles. `over_budget`: lossless tiles currently exceed
    /// the bandwidth budget, so any sizable change should become video right away.
    pub fn update(&mut self, now: f64, changed: &[Rect], over_budget: bool) -> Decision {
        self.decay(now);
        let mut changed_box: Option<Rect> = None;
        for t in changed {
            let i = ((t.y / TILE_SIZE) * self.cols + t.x / TILE_SIZE) as usize;
            if let Some(r) = self.rate.get_mut(i) {
                *r += 1.0;
            }
            changed_box = Some(changed_box.map_or(*t, |b| union(b, *t)));
        }
        let hot: Vec<Rect> =
            self.rate.iter().enumerate().filter(|(_, r)| **r >= self.p.hot).map(|(i, _)| self.tile_rect(i)).collect();
        let hot_box = hot.iter().copied().reduce(union);

        match self.region {
            None => {
                if hot.len() >= self.p.min_hot_tiles {
                    self.hot_frames += 1;
                } else {
                    self.hot_frames = 0;
                }
                let start = if self.hot_frames >= self.p.enter_frames {
                    hot_box
                } else if over_budget && changed.len() >= self.p.min_hot_tiles {
                    Some(match hot_box {
                        Some(h) => union(h, changed_box.unwrap()),
                        None => changed_box.unwrap(),
                    })
                } else {
                    None
                };
                if let Some(r) = start {
                    let r = self.finish_rect(r);
                    self.start(now, r);
                    return Decision::StartVideo(r);
                }
                Decision::Keep
            }
            Some(region) => {
                if changed.iter().any(|t| intersects(t, &region)) {
                    self.last_change_in_region = now;
                }
                if now - self.last_change_in_region >= self.p.exit_still as f64 {
                    return self.stop();
                }
                let hot_in = hot.iter().filter(|t| intersects(t, &region)).count();
                if hot_in * 2 < self.p.min_hot_tiles {
                    let since = *self.cold_since.get_or_insert(now);
                    if now - since >= self.p.exit_cold as f64 {
                        return self.stop();
                    }
                } else {
                    self.cold_since = None;
                }
                // Grow: sustained hot tiles outside the region.
                let outside: Vec<Rect> = hot.iter().filter(|t| !contains(&region, t)).copied().collect();
                if outside.len() * 2 >= self.p.min_hot_tiles || (over_budget && !outside.is_empty()) {
                    self.outside_frames += 1;
                } else {
                    self.outside_frames = 0;
                }
                if self.outside_frames >= self.p.grow_frames || (over_budget && self.outside_frames > 0) {
                    let r = self.finish_rect(outside.into_iter().fold(region, union));
                    if r != region {
                        self.start(now, r);
                        return Decision::MoveVideo(r);
                    }
                }
                // Shrink: the moving part is much smaller than the region.
                if let Some(hb) = hot_box {
                    let fill = (hb.w * hb.h) as f32 / (region.w * region.h).max(1) as f32;
                    if fill < self.p.shrink_fill && hot.len() >= self.p.min_hot_tiles {
                        let since = *self.sparse_since.get_or_insert(now);
                        if now - since >= self.p.shrink_after as f64 {
                            let r = self.finish_rect(hb);
                            if r != region {
                                self.start(now, r);
                                return Decision::MoveVideo(r);
                            }
                        }
                    } else {
                        self.sparse_since = None;
                    }
                }
                Decision::Keep
            }
        }
    }

    /// No new frame arrived until `now` (the screen is static).
    pub fn tick(&mut self, now: f64) -> Decision {
        match self.region {
            Some(_) if now - self.last_change_in_region >= self.p.exit_still as f64 => {
                self.decay(now);
                self.stop()
            }
            _ => Decision::Keep,
        }
    }

    fn start(&mut self, now: f64, r: Rect) {
        self.region = Some(r);
        self.last_change_in_region = now;
        self.cold_since = None;
        self.outside_frames = 0;
        self.sparse_since = None;
    }

    fn stop(&mut self) -> Decision {
        self.region = None;
        self.hot_frames = 0;
        // Require fresh evidence before re-entering video.
        self.rate.iter_mut().for_each(|r| *r = r.min(self.p.hot * 0.5));
        Decision::StopVideo
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(x0: u32, y0: u32, cols: u32, rows: u32) -> Vec<Rect> {
        let mut v = vec![];
        for r in 0..rows {
            for c in 0..cols {
                v.push(Rect { x: (x0 + c) * 64, y: (y0 + r) * 64, w: 64, h: 64 });
            }
        }
        v
    }

    #[test]
    fn typing_stays_lossless() {
        let mut m = MotionTracker::new(1920, 1080, MotionParams::default());
        // Two tiles changing at 10 Hz for 3 s.
        for i in 0..30 {
            let d = m.update(i as f64 * 0.1, &block(3 + (i % 5), 4, 2, 1), false);
            assert_eq!(d, Decision::Keep);
        }
    }

    #[test]
    fn video_starts_tracks_and_stops() {
        let mut m = MotionTracker::new(1920, 1080, MotionParams::default());
        let video = block(4, 2, 10, 6); // 640x384 at (256,128)
        let mut t = 0.0;
        let mut started = None;
        for _ in 0..20 {
            t += 1.0 / 30.0;
            if let Decision::StartVideo(r) = m.update(t, &video, false) {
                started = Some(r);
                break;
            }
        }
        let r = started.expect("video should start");
        assert_eq!(r, Rect { x: 256, y: 128, w: 640, h: 384 });
        // Keeps going.
        for _ in 0..30 {
            t += 1.0 / 30.0;
            assert_eq!(m.update(t, &video, false), Decision::Keep);
        }
        // Stops after it goes still (no frames -> tick).
        assert_eq!(m.tick(t + 0.05), Decision::Keep);
        assert_eq!(m.tick(t + 0.2), Decision::StopVideo);
        assert_eq!(m.region(), None);
        // A single later change doesn't restart immediately.
        assert_eq!(m.update(t + 0.3, &video, false), Decision::Keep);
    }

    #[test]
    fn grows_when_motion_spreads() {
        let mut m = MotionTracker::new(1920, 1080, MotionParams::default());
        let mut t = 0.0;
        let a = block(0, 0, 4, 4);
        loop {
            t += 1.0 / 60.0;
            if let Decision::StartVideo(_) = m.update(t, &a, false) {
                break;
            }
        }
        let mut both = a.clone();
        both.extend(block(10, 0, 4, 4));
        let mut moved = None;
        for _ in 0..30 {
            t += 1.0 / 60.0;
            if let Decision::MoveVideo(r) = m.update(t, &both, false) {
                moved = Some(r);
                break;
            }
        }
        assert_eq!(moved.unwrap(), Rect { x: 0, y: 0, w: 14 * 64, h: 256 });
    }

    #[test]
    fn over_budget_promotes_immediately() {
        let mut m = MotionTracker::new(1920, 1080, MotionParams::default());
        let d = m.update(0.0, &block(0, 0, 30, 16), true);
        assert!(matches!(d, Decision::StartVideo(Rect { x: 0, y: 0, w: 1920, h: 1024 })));
    }

    #[test]
    fn small_region_is_expanded_and_clipped() {
        let m = MotionTracker::new(1000, 600, MotionParams::default());
        let r = m.finish_rect(Rect { x: 960, y: 576, w: 40, h: 24 });
        assert_eq!(r, Rect { x: 864, y: 464, w: 136, h: 136 });
        assert!(r.x + r.w <= 1000 && r.y + r.h <= 600);
    }
}
