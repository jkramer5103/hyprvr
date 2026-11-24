//! Audio jitter buffer: PCM datagrams (s16le stereo 48 kHz) in, a steady stream out.
//! Primes ~40 ms before playing and caps queued audio at ~150 ms.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use hyprvr_proto::{AUDIO_CHANNELS, AUDIO_RATE};

use crate::net::AudioBackend;

pub const CH: usize = AUDIO_CHANNELS as usize;
const PRIME_SAMPLES: usize = (AUDIO_RATE as usize / 25) * CH; // 40 ms
const CAP_SAMPLES: usize = (AUDIO_RATE as usize * 15 / 100) * CH; // 150 ms

#[derive(Default)]
struct Ring {
    buf: VecDeque<i16>,
    primed: bool,
    last_seq: Option<u32>,
}

#[derive(Default)]
pub struct Jitter {
    ring: Mutex<Ring>,
    pub underruns: AtomicU64,
    pub dropped: AtomicU64,
    pub packets: AtomicU64,
}

impl AudioBackend for Jitter {
    fn push(&self, seq: u32, pcm: &[u8]) {
        self.packets.fetch_add(1, Ordering::Relaxed);
        let mut r = self.ring.lock().unwrap();
        if let Some(last) = r.last_seq {
            // Drop late/duplicate packets (datagrams may reorder).
            if seq.wrapping_sub(last) as i32 <= 0 {
                return;
            }
        }
        r.last_seq = Some(seq);
        r.buf.extend(pcm.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
        if r.buf.len() > CAP_SAMPLES {
            // Too far behind: skip ahead to the priming level to restore low latency.
            let excess = r.buf.len() - PRIME_SAMPLES;
            let excess = excess - excess % CH;
            r.buf.drain(..excess);
            self.dropped.fetch_add(excess as u64, Ordering::Relaxed);
        }
    }

    fn reset(&self) {
        let mut r = self.ring.lock().unwrap();
        r.buf.clear();
        r.primed = false;
        r.last_seq = None;
    }
}

impl Jitter {
    /// Fill an output buffer (interleaved i16); silence while (re)priming.
    pub fn fill(&self, out: &mut [i16]) {
        let mut r = self.ring.lock().unwrap();
        if !r.primed && r.buf.len() >= PRIME_SAMPLES {
            r.primed = true;
        }
        let mut n = 0;
        if r.primed {
            n = out.len().min(r.buf.len());
            for (o, s) in out[..n].iter_mut().zip(r.buf.drain(..n)) {
                *o = s;
            }
            if n < out.len() {
                r.primed = false;
                self.underruns.fetch_add(1, Ordering::Relaxed);
            }
        }
        out[n..].fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(n_frames: usize, v: i16) -> Vec<u8> {
        (0..n_frames * CH).flat_map(|_| v.to_le_bytes()).collect()
    }

    #[test]
    fn primes_then_plays_then_underruns() {
        let j = Jitter::default();
        let mut out = vec![1i16; 480];
        j.push(1, &pkt(240, 5));
        j.fill(&mut out);
        assert!(out.iter().all(|s| *s == 0), "silence until primed");
        for seq in 2..=8 {
            j.push(seq, &pkt(240, 5));
        }
        j.fill(&mut out);
        assert!(out.iter().all(|s| *s == 5));
        // Drain everything: the remainder plays, then silence and an underrun.
        let mut big = vec![1i16; 48_000];
        j.fill(&mut big);
        assert_eq!(j.underruns.load(Ordering::Relaxed), 1);
        assert_eq!(*big.last().unwrap(), 0);
    }

    #[test]
    fn drops_late_packets_and_caps_latency() {
        let j = Jitter::default();
        j.push(10, &pkt(240, 1));
        j.push(9, &pkt(240, 2)); // late
        j.push(10, &pkt(240, 3)); // duplicate
        assert_eq!(j.ring.lock().unwrap().buf.len(), 240 * CH);
        for seq in 11..200 {
            j.push(seq, &pkt(240, 4));
        }
        let len = j.ring.lock().unwrap().buf.len();
        assert!(len <= CAP_SAMPLES, "{len}");
        assert!(j.dropped.load(Ordering::Relaxed) > 0);
        assert_eq!(len % CH, 0, "stays frame aligned");
    }

    #[test]
    fn reset_clears() {
        let j = Jitter::default();
        for seq in 0..20 {
            j.push(seq, &pkt(240, 1));
        }
        j.reset();
        j.push(0, &pkt(240, 1)); // seq restarts after reset
        assert_eq!(j.ring.lock().unwrap().buf.len(), 240 * CH);
    }
}
