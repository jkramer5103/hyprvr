//! Persistent daemon state in `~/.local/share/hyprvr/state.json`.

use std::path::PathBuf;

use hyprvr_proto::msg::{AudioTarget, Curvature, PanelLayout, Pose, Settings};
use hyprvr_proto::pairing::Fingerprint;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairedClient {
    pub name: String,
    pub fingerprint: String, // hex
    pub paired_at: u64,      // unix seconds
}

/// A screen the user wants, independent of runtime ids.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ScreenSpec {
    Virtual { width: u32, height: u32 },
    Mirror { output: String },
}

/// Layout of one slot (screen index in `screens`), without the runtime id.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SlotLayout {
    pub pose: Pose,
    pub width_m: f32,
    pub curvature: Curvature,
}

impl SlotLayout {
    pub fn with_id(&self, screen: u32) -> PanelLayout {
        PanelLayout { screen, pose: self.pose, width_m: self.width_m, curvature: self.curvature }
    }
    pub fn from_panel(p: &PanelLayout) -> Self {
        SlotLayout { pose: p.pose, width_m: p.width_m, curvature: p.curvature }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    pub paired: Vec<PairedClient>,
    /// `None` = never configured, use config defaults.
    pub screens: Option<Vec<ScreenSpec>>,
    /// Parallel to `screens`.
    pub layout: Vec<SlotLayout>,
    pub settings: Settings,
    pub audio: AudioTarget,
    /// Positions of physical monitors before hyprvr moved them (restored on teardown, also
    /// after a crash).
    pub original_positions: Vec<(String, i32, i32)>,
}

impl Default for Store {
    fn default() -> Self {
        Store { paired: vec![], screens: None, layout: vec![], settings: Settings::default(), audio: AudioTarget::Laptop, original_positions: vec![] }
    }
}

pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("hyprvr")
}

impl Store {
    fn path() -> PathBuf {
        data_dir().join("state.json")
    }

    pub fn load() -> Self {
        match std::fs::read(Self::path()) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                tracing::warn!("state.json unreadable ({e}); starting fresh");
                Store::default()
            }),
            Err(_) => Store::default(),
        }
    }

    pub fn save(&self) {
        let p = Self::path();
        let r = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(p.parent().unwrap())?;
            let tmp = p.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap())?;
            std::fs::rename(tmp, &p)
        })();
        if let Err(e) = r {
            tracing::error!("saving state: {e}");
        }
    }

    pub fn is_paired(&self, fp: &Fingerprint) -> bool {
        let h = hex::encode(fp);
        self.paired.iter().any(|c| c.fingerprint == h)
    }

    pub fn add_paired(&mut self, name: &str, fp: &Fingerprint) {
        let h = hex::encode(fp);
        self.paired.retain(|c| c.fingerprint != h);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        self.paired.push(PairedClient { name: name.to_string(), fingerprint: h, paired_at: now });
    }
}
