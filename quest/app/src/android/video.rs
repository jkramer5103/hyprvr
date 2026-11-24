//! Hardware video decoding: one AMediaCodec per active video region, decoding into an
//! AImageReader whose AHardwareBuffers the render thread imports as external textures.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hyprvr_proto::msg::{Rect, ScreenId, VideoCodec};
use log::{debug, error, info, warn};
use ndk::hardware_buffer::HardwareBufferUsage;
use ndk::media::image_reader::{ImageFormat, ImageReader};
use ndk::media::media_codec::{DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;

use crate::net::VideoBackend;

/// ImageReader shared between the decoder thread (which only holds its window) and the
/// render thread (which acquires images). AImageReader is internally synchronized.
pub struct ReaderBox(pub ImageReader);
unsafe impl Send for ReaderBox {}
unsafe impl Sync for ReaderBox {}

struct Job {
    data: Vec<u8>,
    pts_us: u64,
    keyframe: bool,
}

struct Slot {
    generation: u64,
    tx: mpsc::Sender<Job>,
    reader: Arc<ReaderBox>,
    rect: Rect,
}

/// What the render thread needs to show one video region.
pub struct ActiveVideo {
    pub reader: Arc<ReaderBox>,
    pub rect: Rect,
}

#[derive(Default)]
pub struct AndroidVideo {
    slots: Mutex<HashMap<ScreenId, Slot>>,
    /// Total frames output by all decoders (for stats).
    pub decoded: Arc<AtomicU64>,
}

fn align16(v: u32) -> u32 {
    v.div_ceil(16) * 16
}

impl AndroidVideo {
    /// Region for `screen` if it is still the given generation.
    pub fn active(&self, screen: ScreenId, generation: u64) -> Option<ActiveVideo> {
        let slots = self.slots.lock().unwrap();
        let s = slots.get(&screen).filter(|s| s.generation == generation)?;
        Some(ActiveVideo { reader: s.reader.clone(), rect: s.rect })
    }
}

impl VideoBackend for AndroidVideo {
    fn start(&self, screen: ScreenId, generation: u64, codec: VideoCodec, rect: Rect) {
        let mut slots = self.slots.lock().unwrap();
        slots.remove(&screen);
        let (w, h) = (align16(rect.w.max(16)), align16(rect.h.max(16)));
        let reader = match ImageReader::new_with_usage(w as i32, h as i32, ImageFormat::PRIVATE, HardwareBufferUsage::GPU_SAMPLED_IMAGE, 4) {
            Ok(r) => Arc::new(ReaderBox(r)),
            Err(e) => {
                error!("screen {screen}: ImageReader {w}x{h}: {e:?}");
                return;
            }
        };
        let window = match reader.0.window() {
            Ok(w) => w,
            Err(e) => {
                error!("screen {screen}: ImageReader window: {e:?}");
                return;
            }
        };
        let (tx, rx) = mpsc::channel();
        let outputs = Arc::new(AtomicU64::new(0));
        let total = self.decoded.clone();
        let mime = match codec {
            VideoCodec::H264 => "video/avc",
            VideoCodec::Hevc => "video/hevc",
        };
        let spawned = std::thread::Builder::new()
            .name(format!("hyprvr-dec{screen}"))
            .spawn(move || {
                if let Err(e) = decoder_thread(screen, mime, w, h, window, rx, outputs, total) {
                    error!("screen {screen}: decoder: {e}");
                }
            });
        if let Err(e) = spawned {
            error!("screen {screen}: spawn decoder: {e}");
            return;
        }
        info!("screen {screen}: video gen {generation} {mime} rect {rect:?} (buffer {w}x{h})");
        slots.insert(screen, Slot { generation, tx, reader, rect });
    }

    fn frame(&self, screen: ScreenId, generation: u64, data: Vec<u8>, pts_us: u64, keyframe: bool) {
        let slots = self.slots.lock().unwrap();
        if let Some(s) = slots.get(&screen).filter(|s| s.generation == generation) {
            let _ = s.tx.send(Job { data, pts_us, keyframe });
        }
    }

    fn stop(&self, screen: ScreenId) {
        if self.slots.lock().unwrap().remove(&screen).is_some() {
            info!("screen {screen}: video stopped");
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn decoder_thread(
    screen: ScreenId,
    mime: &str,
    w: u32,
    h: u32,
    window: NativeWindow,
    rx: mpsc::Receiver<Job>,
    outputs: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
) -> Result<(), String> {
    let codec = MediaCodec::from_decoder_type(mime).ok_or_else(|| format!("no decoder for {mime}"))?;
    let name = codec.name().unwrap_or_default();
    let mut fmt = MediaFormat::new();
    fmt.set_str("mime", mime);
    fmt.set_i32("width", w as i32);
    fmt.set_i32("height", h as i32);
    fmt.set_i32("low-latency", 1);
    fmt.set_i32("vendor.qti-ext-dec-low-latency.enable", 1);
    fmt.set_i32("vendor.qti-ext-dec-picture-order.enable", 1);
    fmt.set_i32("priority", 0);
    fmt.set_i32("operating-rate", 32767);
    fmt.set_i32("max-input-size", (w * h * 3 / 2) as i32);
    codec.configure(&fmt, Some(&window), MediaCodecDirection::Decoder).map_err(|e| format!("configure {name}: {e:?}"))?;
    codec.start().map_err(|e| format!("start {name}: {e:?}"))?;
    info!("screen {screen}: decoder {name} started");

    let mut pending: VecDeque<Job> = VecDeque::new();
    let mut seen_key = false;
    let mut queued = 0u64;
    let result = loop {
        let timeout = if pending.is_empty() { Duration::from_millis(3) } else { Duration::ZERO };
        match rx.recv_timeout(timeout) {
            Ok(j) => pending.push_back(j),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
        }
        while let Ok(j) = rx.try_recv() {
            pending.push_back(j);
        }
        // Feed input.
        while let Some(job) = pending.front() {
            if !seen_key {
                if !job.keyframe {
                    pending.pop_front();
                    continue;
                }
                seen_key = true;
            }
            match codec.dequeue_input_buffer(Duration::from_millis(2)) {
                Ok(DequeuedInputBufferResult::Buffer(mut buf)) => {
                    let job = pending.pop_front().unwrap();
                    let dst = buf.buffer_mut();
                    if job.data.len() > dst.len() {
                        warn!("screen {screen}: frame {} B > input buffer {} B, dropped", job.data.len(), dst.len());
                        let _ = codec.queue_input_buffer(buf, 0, 0, job.pts_us, 0);
                        seen_key = false;
                        continue;
                    }
                    unsafe {
                        std::ptr::copy_nonoverlapping(job.data.as_ptr(), dst.as_mut_ptr() as *mut u8, job.data.len());
                    }
                    if let Err(e) = codec.queue_input_buffer(buf, 0, job.data.len(), job.pts_us, 0) {
                        warn!("screen {screen}: queue input: {e:?}");
                        break;
                    }
                    queued += 1;
                    if queued == 1 {
                        debug!("screen {screen}: first input queued");
                    }
                }
                Ok(DequeuedInputBufferResult::TryAgainLater) => break,
                Err(e) => {
                    warn!("screen {screen}: dequeue input: {e:?}");
                    break;
                }
            }
        }
        // Drain output (render = true pushes it into the ImageReader).
        loop {
            match codec.dequeue_output_buffer(Duration::ZERO) {
                Ok(DequeuedOutputBufferInfoResult::Buffer(b)) => {
                    let n = outputs.fetch_add(1, Ordering::Relaxed);
                    total.fetch_add(1, Ordering::Relaxed);
                    if n == 0 {
                        info!("screen {screen}: first decoded frame");
                    }
                    if let Err(e) = codec.release_output_buffer(b, true) {
                        warn!("screen {screen}: release output: {e:?}");
                    }
                }
                Ok(DequeuedOutputBufferInfoResult::TryAgainLater) => break,
                Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {
                    info!("screen {screen}: output format {:?}", codec.output_format());
                }
                Ok(DequeuedOutputBufferInfoResult::OutputBuffersChanged) => {}
                Err(e) => {
                    warn!("screen {screen}: dequeue output: {e:?}");
                    break;
                }
            }
        }
    };
    let _ = codec.stop();
    info!("screen {screen}: decoder stopped after {queued} frames");
    result
}
