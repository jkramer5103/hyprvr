//! One screen's capture -> hybrid encode -> QUIC uni stream pipeline.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hyprvr_proto::framing;
use hyprvr_proto::msg::{ScreenId, StreamMsg};
use tokio::sync::mpsc;

use crate::encode::{EncodeStats, HybridConfig, ScreenEncoder};
use crate::wl::capture::Capturer;

pub struct Pipeline {
    stop: Arc<AtomicBool>,
    refresh: Arc<AtomicBool>,
    pub stats: Arc<Mutex<PipelineStats>>,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct PipelineStats {
    pub enc: EncodeStats,
    pub video_active: bool,
    pub encode_ms_avg: f32,
}

impl Pipeline {
    /// The capture thread or the network stream ended.
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub fn request_refresh(&self) {
        self.refresh.store(true, Ordering::Relaxed);
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_arguments)]
/// Start streaming `output` as screen `id` over a new uni stream on `conn`.
pub fn start(
    conn: quinn::Connection,
    rt: &tokio::runtime::Handle,
    id: ScreenId,
    output: String,
    width: u32,
    height: u32,
    cfg: HybridConfig,
    epoch: Instant,
) -> Pipeline {
    let stop = Arc::new(AtomicBool::new(false));
    let refresh = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Mutex::new(PipelineStats::default()));
    let (tx, mut rx) = mpsc::channel::<StreamMsg>(6);

    // Network writer.
    let stop_w = stop.clone();
    rt.spawn(async move {
        let mut s = match conn.open_uni().await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("screen {id}: open_uni: {e}");
                stop_w.store(true, Ordering::Relaxed);
                return;
            }
        };
        // Screen data is more important than environment images but less than control.
        let _ = s.set_priority(1);
        if framing::write_msg(&mut s, &StreamMsg::ScreenHeader { screen: id, width, height }).await.is_err() {
            stop_w.store(true, Ordering::Relaxed);
            return;
        }
        while let Some(m) = rx.recv().await {
            if let Err(e) = framing::write_msg(&mut s, &m).await {
                tracing::debug!("screen {id}: write: {e}");
                break;
            }
        }
        stop_w.store(true, Ordering::Relaxed);
        let _ = s.finish();
    });

    // Capture + encode thread.
    let (stop_c, refresh_c, stats_c) = (stop.clone(), refresh.clone(), stats.clone());
    std::thread::Builder::new()
        .name(format!("screen-{id}"))
        .spawn(move || {
            if let Err(e) = capture_loop(&output, width, height, cfg, epoch, &tx, &stop_c, &refresh_c, &stats_c) {
                tracing::warn!("screen {id} ({output}): {e:#}");
            }
            stop_c.store(true, Ordering::Relaxed);
        })
        .expect("spawn capture thread");
    Pipeline { stop, refresh, stats }
}

#[allow(clippy::too_many_arguments)]
fn capture_loop(
    output: &str,
    width: u32,
    height: u32,
    cfg: HybridConfig,
    epoch: Instant,
    tx: &mpsc::Sender<StreamMsg>,
    stop: &AtomicBool,
    refresh: &AtomicBool,
    stats: &Mutex<PipelineStats>,
) -> anyhow::Result<()> {
    let mut cap = None;
    let mut failures = 0u32;
    let mut enc = ScreenEncoder::new(cfg, width, height, epoch);
    let mut enc_ms = 0.0f32;
    while !stop.load(Ordering::Relaxed) {
        if cap.is_none() {
            match Capturer::new(output) {
                Ok(c) => {
                    cap = Some(c);
                    // A fresh capturer starts from a full frame anyway.
                }
                Err(e) => {
                    failures += 1;
                    if failures > 20 {
                        return Err(e.context("giving up on capture"));
                    }
                    std::thread::sleep(Duration::from_millis(250));
                    continue;
                }
            }
        }
        if refresh.swap(false, Ordering::Relaxed) {
            enc.invalidate();
        }
        let c = cap.as_mut().unwrap();
        let msgs = match c.next_frame(Duration::from_millis(50)) {
            Ok(Some(f)) => {
                failures = 0;
                let t0 = Instant::now();
                let m = enc.process(&f.frame, f.damage.as_deref(), f.captured_at)?;
                enc_ms = enc_ms * 0.95 + t0.elapsed().as_secs_f32() * 1000.0 * 0.05;
                m
            }
            Ok(None) => enc.idle(Instant::now()),
            Err(e) => {
                tracing::debug!("capture {output}: {e:#}; reconnecting");
                cap = None;
                failures += 1;
                if failures > 20 {
                    return Err(e.context("capture keeps failing"));
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        for m in msgs {
            if tx.blocking_send(m).is_err() {
                return Ok(());
            }
        }
        let mut s = stats.lock().unwrap();
        s.enc = enc.stats;
        s.video_active = enc.video_rect().is_some();
        s.encode_ms_avg = enc_ms;
    }
    Ok(())
}
