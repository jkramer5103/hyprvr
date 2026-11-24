//! CLI <-> daemon protocol: JSON lines over `$XDG_RUNTIME_DIR/hyprvr.sock`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join("hyprvr.sock")
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    /// Start pairing mode explicitly (shows the PIN even before a headset asks).
    Pin,
    ScreenAdd { width: u32, height: u32 },
    ScreenRemove { id: u32 },
    ScreenResize { id: u32, width: u32, height: u32 },
    Mirror { output: String },
    Audio { target: String },
    LayoutReset,
    /// `which`: "all", or a client name / fingerprint prefix.
    Unpair { which: String },
    /// Stream `Status` lines on every change.
    Subscribe,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct ScreenStatus {
    pub id: u32,
    pub output: Option<String>,
    pub kind: String,
    pub width: u32,
    pub height: u32,
    pub video: bool,
    /// Mean capture->encoded time per frame, ms.
    #[serde(default)]
    pub encode_ms: f32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Status {
    pub server_name: String,
    pub compositor: String,
    pub encoder: String,
    pub port: u16,
    pub fingerprint: String,
    pub connected: Option<String>,
    pub pairing_pin: Option<String>,
    pub screens: Vec<ScreenStatus>,
    pub outputs_live: bool,
    pub grace_left_secs: Option<u64>,
    pub audio: String,
    pub bitrate_kbps: u32,
    pub latency_ms: Option<f32>,
    pub paired_clients: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Response {
    pub ok: bool,
    pub message: String,
    pub status: Option<Status>,
}

impl Response {
    pub fn ok(msg: impl Into<String>) -> Self {
        Response { ok: true, message: msg.into(), status: None }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Response { ok: false, message: msg.into(), status: None }
    }
}

/// Waybar custom module JSON for a status.
pub fn waybar_json(s: Option<&Status>) -> serde_json::Value {
    let Some(s) = s else {
        return serde_json::json!({"text": "", "tooltip": "hyprvr daemon not running", "class": "stopped"});
    };
    if let Some(pin) = &s.pairing_pin {
        return serde_json::json!({
            "text": format!("VR PIN {pin}"),
            "tooltip": format!("Enter PIN {pin} in the headset to pair"),
            "class": "pairing",
        });
    }
    let n = s.screens.len();
    let audio_icon = if s.audio == "headset" { "headset" } else { "laptop" };
    let (text, class) = match &s.connected {
        Some(_) => (format!("VR {n} · {:.1}M", s.bitrate_kbps as f32 / 1000.0), "connected"),
        None if s.outputs_live => (format!("VR {n} (idle)"), "grace"),
        None => ("VR".to_string(), "idle"),
    };
    let mut tip = vec![format!("hyprvr on {} ({}, {})", s.server_name, s.compositor, s.encoder)];
    tip.push(match &s.connected {
        Some(c) => format!("Connected: {c}"),
        None => "No headset connected".into(),
    });
    if let Some(g) = s.grace_left_secs {
        tip.push(format!("Virtual screens removed in {g}s"));
    }
    for sc in &s.screens {
        tip.push(format!(
            "  #{} {} {}x{} {}{}",
            sc.id,
            sc.output.as_deref().unwrap_or("-"),
            sc.width,
            sc.height,
            sc.kind,
            if sc.video { " [video]" } else { "" }
        ));
    }
    if let Some(l) = s.latency_ms {
        tip.push(format!("Latency: {l:.0} ms"));
    }
    tip.push(format!("Audio: {audio_icon} (click to toggle)"));
    serde_json::json!({"text": text, "tooltip": tip.join("\n"), "class": class, "alt": audio_icon})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_json_shape() {
        let r: Request = serde_json::from_str(r#"{"cmd":"screen_add","width":1920,"height":1080}"#).unwrap();
        assert_eq!(r, Request::ScreenAdd { width: 1920, height: 1080 });
    }

    #[test]
    fn waybar_pairing_shows_pin() {
        let s = Status { pairing_pin: Some("123456".into()), ..Default::default() };
        assert_eq!(waybar_json(Some(&s))["text"], "VR PIN 123456");
        assert_eq!(waybar_json(None)["class"], "stopped");
    }
}
