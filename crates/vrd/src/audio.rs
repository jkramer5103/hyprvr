//! Audio routing: a PulseAudio/PipeWire null sink "hyprvr" whose monitor is streamed to the
//! headset as raw PCM datagrams. Switching moves the default sink and existing streams.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use hyprvr_proto::msg::{Datagram, encode_datagram};
use hyprvr_proto::{AUDIO_CHANNELS, AUDIO_FRAMES_PER_PACKET, AUDIO_RATE};

pub const SINK: &str = "hyprvr";

pub struct Audio {
    module: Mutex<Option<String>>,
    previous_default: Mutex<Option<String>>,
    capture: Mutex<Option<Child>>,
    /// Where PCM goes. `None` = drop.
    pub target: Arc<Mutex<Option<quinn::Connection>>>,
}

fn pactl(args: &[&str]) -> Result<String> {
    let out = Command::new("pactl").args(args).stderr(Stdio::piped()).output().context("running pactl")?;
    if !out.status.success() {
        bail!("pactl {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn sinks() -> Result<Vec<String>> {
    Ok(pactl(&["list", "short", "sinks"])?.lines().filter_map(|l| l.split('\t').nth(1).map(String::from)).collect())
}

fn move_all_inputs(to: &str) {
    if let Ok(list) = pactl(&["list", "short", "sink-inputs"]) {
        for id in list.lines().filter_map(|l| l.split('\t').next()) {
            let _ = pactl(&["move-sink-input", id, to]);
        }
    }
}

impl Audio {
    pub fn new() -> Self {
        let a = Audio {
            module: Mutex::new(None),
            previous_default: Mutex::new(None),
            capture: Mutex::new(None),
            target: Arc::new(Mutex::new(None)),
        };
        // Recover from a crash that left our sink as the default.
        if let Ok(d) = pactl(&["get-default-sink"])
            && d.trim() == SINK
            && let Ok(s) = sinks()
            && let Some(other) = s.iter().find(|s| *s != SINK)
        {
            let _ = pactl(&["set-default-sink", other]);
            move_all_inputs(other);
        }
        a
    }

    fn ensure_sink(&self) -> Result<()> {
        if sinks()?.iter().any(|s| s == SINK) {
            return Ok(());
        }
        let id = pactl(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={SINK}"),
            "rate=48000",
            "channels=2",
            "sink_properties=device.description=HyprVR-Headset",
        ])?;
        *self.module.lock().unwrap() = Some(id.trim().to_string());
        Ok(())
    }

    /// Route all audio to the headset and start streaming PCM.
    pub fn to_headset(&self) -> Result<()> {
        self.ensure_sink()?;
        let cur = pactl(&["get-default-sink"])?.trim().to_string();
        if cur != SINK {
            *self.previous_default.lock().unwrap() = Some(cur);
            pactl(&["set-default-sink", SINK])?;
        }
        move_all_inputs(SINK);
        self.start_capture()
    }

    /// Route audio back to the laptop's previous default sink and stop streaming.
    pub fn to_laptop(&self) -> Result<()> {
        self.stop_capture();
        let cur = pactl(&["get-default-sink"]).map(|s| s.trim().to_string()).unwrap_or_default();
        if cur != SINK {
            return Ok(());
        }
        let prev = self.previous_default.lock().unwrap().take();
        let all = sinks()?;
        let back = prev.filter(|p| all.contains(p)).or_else(|| all.into_iter().find(|s| s != SINK));
        if let Some(b) = back {
            pactl(&["set-default-sink", &b])?;
            move_all_inputs(&b);
        }
        Ok(())
    }

    fn start_capture(&self) -> Result<()> {
        let mut cap = self.capture.lock().unwrap();
        if let Some(c) = cap.as_mut()
            && c.try_wait().ok().flatten().is_none()
        {
            return Ok(());
        }
        let mut child = Command::new("parec")
            .args([
                "-d",
                &format!("{SINK}.monitor"),
                "--format=s16le",
                &format!("--rate={AUDIO_RATE}"),
                &format!("--channels={AUDIO_CHANNELS}"),
                "--latency-msec=10",
                "--process-time-msec=5",
                "--raw",
                "--client-name=hyprvr",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("starting parec")?;
        let mut out = child.stdout.take().unwrap();
        let target = self.target.clone();
        std::thread::Builder::new().name("audio".into()).spawn(move || {
            let mut buf = vec![0u8; AUDIO_FRAMES_PER_PACKET * AUDIO_CHANNELS as usize * 2];
            let mut seq = 0u32;
            while out.read_exact(&mut buf).is_ok() {
                seq = seq.wrapping_add(1);
                if let Some(c) = target.lock().unwrap().as_ref() {
                    let _ = c.send_datagram(encode_datagram(&Datagram::Audio { seq, pcm: buf.clone() }).into());
                }
            }
        })?;
        *cap = Some(child);
        Ok(())
    }

    fn stop_capture(&self) {
        if let Some(mut c) = self.capture.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Restore the laptop default and unload our sink.
    pub fn shutdown(&self) {
        let _ = self.to_laptop();
        if let Some(m) = self.module.lock().unwrap().take() {
            let _ = pactl(&["unload-module", &m]);
        }
    }
}
