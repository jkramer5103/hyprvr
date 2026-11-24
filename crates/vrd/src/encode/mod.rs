//! Hybrid per-screen encoder: lossless tiles for small changes, a video region for motion.

pub mod motion;
pub mod video;

use std::collections::VecDeque;
use std::time::Instant;

use anyhow::Result;
use hyprvr_proto::msg::{Rect, StreamMsg, VideoCodec};
use hyprvr_proto::tiles::{Frame, TileDiffer, encode_tiles};

use motion::{Decision, MotionParams, MotionTracker, contains, intersects};
use video::{Backend, EncoderParams, VideoEncoder};

#[derive(Clone)]
pub struct HybridConfig {
    pub backend: Option<Backend>,
    pub codec: VideoCodec,
    pub fps: u32,
    pub zstd_level: i32,
    pub tile_kbps_budget: u32,
    pub kbps_for: std::sync::Arc<dyn Fn(u32, u32) -> u32 + Send + Sync>,
}

struct ActiveVideo {
    rect: Rect,
    enc: VideoEncoder,
}

/// Counters for status / Waybar.
#[derive(Default, Clone, Copy, Debug)]
pub struct EncodeStats {
    pub tile_bytes: u64,
    pub video_bytes: u64,
    pub frames: u64,
    pub video_frames: u64,
}

pub struct ScreenEncoder {
    cfg: HybridConfig,
    width: u32,
    height: u32,
    differ: TileDiffer,
    motion: MotionTracker,
    video: Option<ActiveVideo>,
    spare: Option<VideoEncoder>,
    seq: u64,
    epoch: Instant,
    tile_window: VecDeque<(f64, usize)>,
    video_failed_until: f64,
    /// Copy of the last frame, maintained while video is active so a stopped region can be
    /// sharpened even though the capture buffer is busy waiting for the next damage.
    snapshot: Vec<u8>,
    snapshot_format: hyprvr_proto::tiles::PixelFormat,
    pub stats: EncodeStats,
}

impl ScreenEncoder {
    pub fn new(cfg: HybridConfig, width: u32, height: u32, epoch: Instant) -> Self {
        ScreenEncoder {
            differ: TileDiffer::new(width, height),
            motion: MotionTracker::new(width, height, MotionParams::default()),
            cfg,
            width,
            height,
            video: None,
            spare: None,
            seq: 0,
            epoch,
            tile_window: VecDeque::new(),
            video_failed_until: 0.0,
            snapshot: Vec::new(),
            snapshot_format: hyprvr_proto::tiles::PixelFormat::Bgrx,
            stats: EncodeStats::default(),
        }
    }

    pub fn video_rect(&self) -> Option<Rect> {
        self.video.as_ref().map(|v| v.rect)
    }

    fn secs(&self, t: Instant) -> f64 {
        t.duration_since(self.epoch).as_secs_f64()
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Client asked for a full lossless refresh (or a new client attached).
    pub fn invalidate(&mut self) {
        self.differ.invalidate();
        if let Some(v) = &mut self.video {
            v.enc.request_idr();
        }
    }

    fn over_budget(&mut self, now: f64) -> bool {
        while self.tile_window.front().is_some_and(|(t, _)| now - t > 0.5) {
            self.tile_window.pop_front();
        }
        let bytes: usize = self.tile_window.iter().map(|(_, b)| b).sum();
        // bytes per 0.5 s -> kbit/s
        (bytes as f64 * 8.0 * 2.0 / 1000.0) > self.cfg.tile_kbps_budget as f64
    }

    fn stop_video(&mut self, out: &mut Vec<StreamMsg>) {
        if let Some(v) = self.video.take() {
            let seq = self.next_seq();
            out.push(StreamMsg::VideoStop { seq });
            self.differ.invalidate_rect(v.rect);
            self.spare = Some(v.enc);
        }
    }

    fn start_video(&mut self, rect: Rect, now: f64, out: &mut Vec<StreamMsg>) {
        let Some(backend) = self.cfg.backend.clone() else {
            self.motion.clear_region();
            return;
        };
        if now < self.video_failed_until {
            self.motion.clear_region();
            return;
        }
        let enc = match self.spare.take() {
            Some(mut e) if e.width == rect.w && e.height == rect.h => {
                e.request_idr();
                Ok(e)
            }
            _ => VideoEncoder::new(&EncoderParams {
                backend,
                codec: self.cfg.codec,
                width: rect.w,
                height: rect.h,
                fps: self.cfg.fps,
                kbps: (self.cfg.kbps_for)(rect.w, rect.h),
            }),
        };
        match enc {
            Ok(enc) => {
                let seq = self.next_seq();
                out.push(StreamMsg::VideoStart { seq, codec: self.cfg.codec, rect });
                self.video = Some(ActiveVideo { rect, enc });
            }
            Err(e) => {
                tracing::warn!("video encoder for {}x{} failed: {e:#}; staying lossless for 5 s", rect.w, rect.h);
                self.video_failed_until = now + 5.0;
                self.motion.clear_region();
            }
        }
    }

    /// Process a captured frame and return the stream messages to send (in order).
    pub fn process(&mut self, frame: &Frame, damage: Option<&[Rect]>, captured_at: Instant) -> Result<Vec<StreamMsg>> {
        let mut out = Vec::new();
        if (frame.width, frame.height) != (self.width, self.height) {
            self.video = None;
            self.spare = None;
            self.width = frame.width;
            self.height = frame.height;
            self.differ = TileDiffer::new(frame.width, frame.height);
            self.motion = MotionTracker::new(frame.width, frame.height, MotionParams::default());
            out.push(StreamMsg::Resized { width: frame.width, height: frame.height });
        }
        let now = self.secs(captured_at);
        let capture_us = captured_at.duration_since(self.epoch).as_micros() as u64;
        self.stats.frames += 1;

        let changed = self.differ.diff(frame, damage);
        let over = self.over_budget(now);
        match self.motion.update(now, &changed, over) {
            Decision::Keep => {}
            Decision::StartVideo(r) => self.start_video(r, now, &mut out),
            Decision::MoveVideo(r) => {
                self.stop_video(&mut out);
                self.start_video(r, now, &mut out);
            }
            Decision::StopVideo => self.stop_video(&mut out),
        }

        // Tiles invalidated by a stop are re-examined in full.
        let mut tiles = changed;
        let refresh = self.differ.diff(frame, Some(&[]));
        for t in refresh {
            if !tiles.contains(&t) {
                tiles.push(t);
            }
        }

        let mut video_dirty = false;
        if let Some(v) = &self.video {
            let r = v.rect;
            video_dirty = tiles.iter().any(|t| intersects(t, &r));
            // Tiles fully inside the region are covered by video; partially covered edge
            // tiles are still sent lossless (they are drawn before the next video frame).
            tiles.retain(|t| !contains(&r, t));
        }
        if !tiles.is_empty() {
            let payload = encode_tiles(frame, &tiles, self.cfg.zstd_level);
            self.tile_window.push_back((now, payload.len()));
            self.stats.tile_bytes += payload.len() as u64;
            let seq = self.next_seq();
            out.push(StreamMsg::Tiles { seq, capture_us, payload });
        }
        if video_dirty {
            let v = self.video.as_mut().unwrap();
            let rect = v.rect;
            match v.enc.encode(frame, rect, capture_us as i64) {
                Ok(pkts) => {
                    for p in pkts {
                        self.stats.video_bytes += p.data.len() as u64;
                        self.stats.video_frames += 1;
                        self.seq += 1;
                        out.push(StreamMsg::VideoFrame { seq: self.seq, capture_us, keyframe: p.keyframe, data: p.data });
                    }
                }
                Err(e) => {
                    tracing::warn!("video encode failed: {e:#}");
                    self.video_failed_until = now + 5.0;
                    self.spare = None;
                    self.stop_video(&mut out);
                    self.spare = None;
                    self.motion.clear_region();
                    // Send the region lossless right away.
                    let refresh = self.differ.diff(frame, Some(&[]));
                    if !refresh.is_empty() {
                        let payload = encode_tiles(frame, &refresh, self.cfg.zstd_level);
                        self.stats.tile_bytes += payload.len() as u64;
                        let seq = self.next_seq();
                        out.push(StreamMsg::Tiles { seq, capture_us, payload });
                    }
                }
            }
        }
        if self.video.is_some() {
            let row = self.width as usize * 4;
            self.snapshot.resize(row * self.height as usize, 0);
            for y in 0..self.height as usize {
                let s = y * frame.stride;
                self.snapshot[y * row..(y + 1) * row].copy_from_slice(&frame.data[s..s + row]);
            }
            self.snapshot_format = frame.format;
        }
        Ok(out)
    }

    /// Called when no frame arrived for a while: may end the video region and sharpen it.
    pub fn idle(&mut self, now: Instant) -> Vec<StreamMsg> {
        let mut out = Vec::new();
        let t = self.secs(now);
        if self.motion.tick(t) == Decision::StopVideo {
            self.stop_video(&mut out);
            let snap = std::mem::take(&mut self.snapshot);
            if snap.len() == (self.width * self.height * 4) as usize {
                let f = &Frame { data: &snap, stride: self.width as usize * 4, width: self.width, height: self.height, format: self.snapshot_format };
                let tiles = self.differ.diff(f, Some(&[]));
                if !tiles.is_empty() {
                    let payload = encode_tiles(f, &tiles, self.cfg.zstd_level);
                    self.stats.tile_bytes += payload.len() as u64;
                    let seq = self.next_seq();
                    out.push(StreamMsg::Tiles { seq, capture_us: now.duration_since(self.epoch).as_micros() as u64, payload });
                }
            }
            self.snapshot = snap;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyprvr_proto::tiles::{PixelFormat, apply_tiles};
    use std::time::Duration;

    fn cfg(backend: Option<Backend>) -> HybridConfig {
        HybridConfig {
            backend,
            codec: VideoCodec::H264,
            fps: 60,
            zstd_level: 1,
            tile_kbps_budget: 1_000_000,
            kbps_for: std::sync::Arc::new(|_, _| 5000),
        }
    }

    /// A moving square produces video, then stopping returns to an exact lossless image.
    #[test]
    fn moving_content_round_trip() {
        let (w, h) = (640u32, 480u32);
        let epoch = Instant::now();
        let mut enc = ScreenEncoder::new(cfg(Some(Backend::X264)), w, h, epoch);
        let mut canvas = vec![0u8; (w * h * 4) as usize];
        let mut buf = vec![40u8; (w * h * 4) as usize];
        let mut saw_video = false;
        let mut t = epoch;
        let draw = |buf: &mut Vec<u8>, i: u32| {
            buf.iter_mut().for_each(|b| *b = 40);
            for y in 100..400 {
                for x in 50..600 {
                    let o = ((y * w + x) * 4) as usize;
                    buf[o] = ((x + i * 9) % 256) as u8;
                    buf[o + 1] = ((y * 3 + i * 5) % 256) as u8;
                }
            }
        };
        let apply = |msgs: Vec<StreamMsg>, canvas: &mut Vec<u8>, saw: &mut bool| {
            for m in msgs {
                match m {
                    StreamMsg::Tiles { payload, .. } => {
                        apply_tiles(&payload, canvas, w, h).unwrap();
                    }
                    StreamMsg::VideoStart { .. } | StreamMsg::VideoFrame { .. } => *saw = true,
                    _ => {}
                }
            }
        };
        for i in 0..30 {
            draw(&mut buf, i);
            t += Duration::from_millis(16);
            let f = Frame { data: &buf, stride: (w * 4) as usize, width: w, height: h, format: PixelFormat::Bgrx };
            let msgs = enc.process(&f, None, t).unwrap();
            apply(msgs, &mut canvas, &mut saw_video);
        }
        assert!(saw_video, "motion should switch to video");
        assert!(enc.video_rect().is_some());
        // Goes still.
        let msgs = enc.idle(t + Duration::from_millis(300));
        assert!(matches!(msgs.first(), Some(StreamMsg::VideoStop { .. })));
        apply(msgs, &mut canvas, &mut saw_video);
        assert!(enc.video_rect().is_none());
        for i in (0..buf.len()).step_by(4) {
            assert_eq!([canvas[i], canvas[i + 1], canvas[i + 2]], [buf[i + 2], buf[i + 1], buf[i]], "pixel {}", i / 4);
        }
    }

    #[test]
    fn without_backend_everything_is_lossless() {
        let (w, h) = (256u32, 256u32);
        let epoch = Instant::now();
        let mut enc = ScreenEncoder::new(cfg(None), w, h, epoch);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        for i in 0..20u32 {
            buf.iter_mut().enumerate().for_each(|(j, b)| *b = ((j as u32 + i) % 255) as u8);
            let f = Frame { data: &buf, stride: (w * 4) as usize, width: w, height: h, format: PixelFormat::Bgrx };
            let msgs = enc.process(&f, None, epoch + Duration::from_millis(16 * i as u64)).unwrap();
            assert!(msgs.iter().all(|m| matches!(m, StreamMsg::Tiles { .. })));
        }
    }

    /// Per-frame CPU cost at 1080p. Run with
    /// `cargo test --release -p vrd bench_1080p -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_1080p() {
        use hyprvr_proto::tiles::TileDiffer;
        let (w, h) = (1920u32, 1080u32);
        // Terminal-like content: 8x16 glyph cells with ~25% ink, scrolled by one line per frame.
        let mut rng = 0x1234_5678u32;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            rng
        };
        let lines: Vec<Vec<u8>> = (0..(h / 16 + 200))
            .map(|_| {
                let mut row = vec![0x23u8; (w * 16 * 4) as usize];
                let chars = 40 + next() % 180;
                for c in 0..chars.min(w / 8) {
                    if next() % 6 == 0 {
                        continue;
                    }
                    for gy in 2..14 {
                        for gx in 1..7 {
                            if next() % 4 == 0 {
                                let o = ((gy * w + c * 8 + gx) * 4) as usize;
                                row[o..o + 3].copy_from_slice(&[0xdc, 0xdc, 0xdc]);
                            }
                        }
                    }
                }
                row
            })
            .collect();
        let frame_at = |top: usize| -> Vec<u8> {
            let mut f = Vec::with_capacity((w * h * 4) as usize);
            for l in 0..(h / 16 + 1) as usize {
                f.extend_from_slice(&lines[(top + l) % lines.len()]);
            }
            f.truncate((w * h * 4) as usize);
            f
        };
        let frames: Vec<Vec<u8>> = (0..60).map(frame_at).collect();
        fn fr(d: &[u8]) -> Frame<'_> {
            Frame { data: d, stride: 1920 * 4, width: 1920, height: 1080, format: PixelFormat::Bgrx }
        }

        let mut differ = TileDiffer::new(w, h);
        differ.diff(&fr(&frames[0]), None);
        let t = Instant::now();
        let mut bytes = 0;
        for f in &frames[1..] {
            let tiles = differ.diff(&fr(f), None);
            bytes += encode_tiles(&fr(f), &tiles, 1).len();
        }
        let n = (frames.len() - 1) as f64;
        println!(
            "lossless full-frame scroll: {:.2} ms/frame, {:.0} KB/frame",
            t.elapsed().as_secs_f64() * 1000.0 / n,
            bytes as f64 / n / 1024.0
        );

        let t = Instant::now();
        for _ in 0..60 {
            differ.diff(&fr(&frames[59]), None);
        }
        println!("diff of an unchanged frame: {:.2} ms/frame", t.elapsed().as_secs_f64() * 1000.0 / 60.0);

        let mut enc = VideoEncoder::new(&EncoderParams {
            backend: Backend::X264,
            codec: VideoCodec::H264,
            width: w,
            height: h,
            fps: 60,
            kbps: 25_000,
        })
        .unwrap();
        let t = Instant::now();
        let mut vbytes = 0;
        for (i, f) in frames.iter().enumerate() {
            for p in enc.encode(&fr(f), Rect { x: 0, y: 0, w, h }, i as i64 * 16_666).unwrap() {
                vbytes += p.data.len();
            }
        }
        println!(
            "x264 ultrafast 1080p (incl. BGRx->YUV): {:.2} ms/frame, {:.0} KB/frame",
            t.elapsed().as_secs_f64() * 1000.0 / frames.len() as f64,
            vbytes as f64 / frames.len() as f64 / 1024.0
        );

        // Whole hybrid path (diff + motion + video region + bookkeeping), as in the daemon.
        let mut cfg = cfg(Some(Backend::X264));
        cfg.tile_kbps_budget = 60_000;
        cfg.kbps_for = std::sync::Arc::new(|_, _| 25_000);
        let epoch = Instant::now();
        let mut se = ScreenEncoder::new(cfg, w, h, epoch);
        let (mut tv, mut nv, mut tl, mut nl) = (0.0, 0, 0.0, 0);
        for i in 0..240usize {
            let f = &frames[i % 60];
            let t = Instant::now();
            se.process(&fr(f), None, epoch + Duration::from_micros(16_666 * i as u64)).unwrap();
            let dt = t.elapsed().as_secs_f64() * 1000.0;
            if se.video_rect().is_some() {
                tv += dt;
                nv += 1;
            } else {
                tl += dt;
                nl += 1;
            }
        }
        println!(
            "hybrid process(): {nl} lossless frames {:.2} ms avg, {nv} video frames {:.2} ms avg (region {:?})",
            tl / nl.max(1) as f64,
            tv / nv.max(1) as f64,
            se.video_rect()
        );
    }
}
