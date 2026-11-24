//! `~/.config/hyprvr/config.toml`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodecChoice {
    H264,
    Hevc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EncoderChoice {
    /// VAAPI if a render node works, otherwise x264.
    Auto,
    Vaapi,
    X264,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Name shown in the headset. Defaults to the hostname.
    pub name: Option<String>,
    pub port: u16,
    /// Screens created on first connect when no saved screen list exists.
    pub default_screens: u32,
    pub default_width: u32,
    pub default_height: u32,
    pub refresh_hz: u32,
    pub codec: CodecChoice,
    pub encoder: EncoderChoice,
    /// VAAPI render node.
    pub render_node: PathBuf,
    /// Video bitrate for a full 1920x1080 region; scaled by region area.
    pub video_kbps_1080p: u32,
    /// Hard cap for the video bitrate of one region.
    pub video_kbps_max: u32,
    /// If lossless tiles exceed this rate (kbit/s) on one screen, the changing area is
    /// promoted to video.
    pub tile_kbps_budget: u32,
    /// zstd level for lossless tiles (1 = fastest).
    pub tile_zstd_level: i32,
    /// Seconds virtual outputs survive after the headset disconnects.
    pub grace_secs: u64,
    /// Directory with equirectangular .jpg/.png environments.
    pub environments_dir: Option<PathBuf>,
    /// Use the cursor shape from ext-image-copy-capture cursor sessions. Off by default:
    /// Hyprland 0.56 returns empty/black cursor images and can segfault in
    /// `CCursorshareSession::copy` while a session is open. Off = built-in arrow.
    pub cursor_capture: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            name: None,
            port: hyprvr_proto::DEFAULT_PORT,
            default_screens: 1,
            default_width: 1920,
            default_height: 1080,
            refresh_hz: 60,
            codec: CodecChoice::H264,
            encoder: EncoderChoice::Auto,
            render_node: PathBuf::from("/dev/dri/renderD128"),
            video_kbps_1080p: 25_000,
            video_kbps_max: 50_000,
            tile_kbps_budget: 60_000,
            tile_zstd_level: 1,
            grace_secs: 15,
            environments_dir: None,
            cursor_capture: false,
        }
    }
}

pub const MAX_W: u32 = 3840;
pub const MAX_H: u32 = 2160;
pub const MIN_W: u32 = 320;
pub const MIN_H: u32 = 240;
pub const MAX_SCREENS: usize = 6;

pub fn clamp_size(w: u32, h: u32) -> (u32, u32) {
    // Even sizes keep the video encoder happy.
    let w = w.clamp(MIN_W, MAX_W) & !1;
    let h = h.clamp(MIN_H, MAX_H) & !1;
    (w, h)
}

impl Config {
    pub fn path() -> PathBuf {
        dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("hyprvr/config.toml")
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let mut c: Config = toml::from_str(text)?;
        c.sanitize();
        Ok(c)
    }

    pub fn load() -> anyhow::Result<Self> {
        let p = Self::path();
        match std::fs::read_to_string(&p) {
            Ok(t) => Self::parse(&t).map_err(|e| anyhow::anyhow!("{}: {e}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    fn sanitize(&mut self) {
        self.default_screens = self.default_screens.clamp(1, MAX_SCREENS as u32);
        (self.default_width, self.default_height) = clamp_size(self.default_width, self.default_height);
        self.refresh_hz = self.refresh_hz.clamp(24, 144);
        self.video_kbps_max = self.video_kbps_max.clamp(1_000, 400_000);
        self.video_kbps_1080p = self.video_kbps_1080p.clamp(500, self.video_kbps_max);
        self.tile_zstd_level = self.tile_zstd_level.clamp(-5, 9);
    }

    pub fn server_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| rustix::system::uname().nodename().to_string_lossy().into_owned())
    }

    pub fn environments_dir(&self) -> PathBuf {
        self.environments_dir
            .clone()
            .unwrap_or_else(|| dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("hyprvr/environments"))
    }

    /// Bitrate for a video region of `w x h`.
    pub fn video_kbps_for(&self, w: u32, h: u32) -> u32 {
        let area = (w as f64 * h as f64) / (1920.0 * 1080.0);
        // Small regions need proportionally more bits per pixel; floor at 25 %.
        let k = (self.video_kbps_1080p as f64 * area.max(0.25)).round() as u32;
        k.clamp(1_000, self.video_kbps_max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_default() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn parses_and_clamps() {
        let c = Config::parse(
            "default_screens = 99\ndefault_width = 99999\ndefault_height = 1081\ncodec = \"hevc\"\nencoder = \"x264\"\n",
        )
        .unwrap();
        assert_eq!(c.default_screens, MAX_SCREENS as u32);
        assert_eq!((c.default_width, c.default_height), (MAX_W, 1080));
        assert_eq!(c.codec, CodecChoice::Hevc);
        assert_eq!(c.encoder, EncoderChoice::X264);
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(Config::parse("bogus = 1").is_err());
    }

    #[test]
    fn bitrate_scaling() {
        let c = Config::default();
        assert_eq!(c.video_kbps_for(1920, 1080), 25_000);
        assert_eq!(c.video_kbps_for(64, 64), 6_250);
        assert_eq!(c.video_kbps_for(3840, 2160), 50_000);
    }
}
