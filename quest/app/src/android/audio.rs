//! Audio playback: an AAudio low-latency stream pulling from the jitter buffer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use hyprvr_proto::{AUDIO_CHANNELS, AUDIO_RATE};
use log::{info, warn};
use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioFormat, AudioPerformanceMode, AudioSharingMode, AudioStream, AudioStreamBuilder, AudioUsage,
    AudioContentType,
};

pub use crate::jitter::Jitter;
use crate::jitter::CH;
use crate::net::AudioBackend as _;

/// Owns the AAudio stream; reopens it after the device disconnects.
pub struct Player {
    jitter: Arc<Jitter>,
    stream: Option<AudioStream>,
    broken: Arc<AtomicBool>,
}

impl Player {
    pub fn new(jitter: Arc<Jitter>) -> Self {
        let mut p = Player { jitter, stream: None, broken: Arc::new(AtomicBool::new(false)) };
        p.open();
        p
    }

    fn open(&mut self) {
        self.stream = None;
        self.broken.store(false, Ordering::Relaxed);
        let j = self.jitter.clone();
        let broken = self.broken.clone();
        let res = AudioStreamBuilder::new().and_then(|b| {
            b.direction(AudioDirection::Output)
                .format(AudioFormat::PCM_I16)
                .channel_count(AUDIO_CHANNELS as i32)
                .sample_rate(AUDIO_RATE as i32)
                .performance_mode(AudioPerformanceMode::LowLatency)
                .sharing_mode(AudioSharingMode::Shared)
                .usage(AudioUsage::Game)
                .content_type(AudioContentType::Music)
                .data_callback(Box::new(move |_s, data, frames| {
                    let out = unsafe { std::slice::from_raw_parts_mut(data as *mut i16, frames as usize * CH) };
                    j.fill(out);
                    AudioCallbackResult::Continue
                }))
                .error_callback(Box::new(move |_s, e| {
                    warn!("audio stream error: {e:?}");
                    broken.store(true, Ordering::Relaxed);
                }))
                .open_stream()
        });
        match res {
            Ok(s) => {
                if let Err(e) = s.request_start() {
                    warn!("audio start: {e:?}");
                    return;
                }
                info!(
                    "audio stream: {} Hz, burst {} frames, buffer {} frames, mode {:?}",
                    s.sample_rate(),
                    s.frames_per_burst(),
                    s.buffer_size_in_frames(),
                    s.performance_mode()
                );
                // Keep the device buffer small; latency lives in the jitter buffer.
                let _ = s.set_buffer_size_in_frames(s.frames_per_burst() * 2);
                self.stream = Some(s);
            }
            Err(e) => warn!("audio open: {e:?}"),
        }
    }

    /// Call regularly from the main thread.
    pub fn poll(&mut self) {
        if self.broken.load(Ordering::Relaxed) {
            info!("reopening audio stream");
            self.open();
        }
    }

    pub fn pause(&mut self) {
        if let Some(s) = &self.stream {
            let _ = s.request_pause();
        }
        self.jitter.reset();
    }

    pub fn resume(&mut self) {
        match &self.stream {
            Some(s) => {
                let _ = s.request_start();
            }
            None => self.open(),
        }
    }
}
