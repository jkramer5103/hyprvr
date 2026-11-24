//! Hyprland IPC (`.socket.sock` requests).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::{Compositor, Monitor};

pub struct Hyprland {
    dir: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HMonitor {
    name: String,
    width: u32,
    height: u32,
    refresh_rate: f32,
    x: i32,
    y: i32,
    scale: f32,
    #[serde(default)]
    transform: i32,
    #[serde(default)]
    disabled: bool,
}

#[derive(Deserialize)]
struct CursorPos {
    x: f64,
    y: f64,
}

impl Hyprland {
    pub fn from_env() -> Result<Self> {
        let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").context("HYPRLAND_INSTANCE_SIGNATURE")?;
        let mut candidates = vec![];
        if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
            candidates.push(PathBuf::from(rt).join("hypr").join(&sig));
        }
        candidates.push(PathBuf::from("/tmp/hypr").join(&sig));
        let dir = candidates
            .into_iter()
            .find(|d| d.join(".socket.sock").exists())
            .context("Hyprland IPC socket not found")?;
        Ok(Hyprland { dir })
    }

    /// Send one raw request and return the full reply.
    pub fn request(&self, cmd: &str) -> Result<String> {
        let mut s = UnixStream::connect(self.dir.join(".socket.sock")).context("connect Hyprland socket")?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        s.write_all(cmd.as_bytes())?;
        let mut out = String::new();
        s.read_to_string(&mut out)?;
        Ok(out)
    }

    fn command_ok(&self, cmd: &str) -> Result<()> {
        let r = self.request(cmd)?;
        // Batches return one reply per command, concatenated.
        // Batches concatenate replies ("ok\n\nok" or "okok" depending on version).
        let squashed: String = r.split_whitespace().collect();
        if squashed.is_empty() || !squashed.len().is_multiple_of(2) || !squashed.as_bytes().chunks(2).all(|c| c == b"ok") {
            bail!("hyprctl `{cmd}` failed: {}", r.trim());
        }
        Ok(())
    }

    fn raw_monitors(&self) -> Result<Vec<HMonitor>> {
        let r = self.request("j/monitors all")?;
        let v: Vec<HMonitor> = serde_json::from_str(&r).with_context(|| format!("parsing monitors: {r}"))?;
        Ok(v.into_iter().filter(|m| !m.disabled).collect())
    }

    fn fmt_scale(s: f32) -> String {
        let t = format!("{s:.6}");
        t.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

impl Compositor for Hyprland {
    fn kind(&self) -> &'static str {
        "hyprland"
    }

    fn monitors(&self) -> Result<Vec<Monitor>> {
        Ok(self
            .raw_monitors()?
            .into_iter()
            .map(|m| Monitor {
                headless: m.name.starts_with("HYPRVR-"),
                name: m.name,
                x: m.x,
                y: m.y,
                width: m.width,
                height: m.height,
                refresh: m.refresh_rate,
                scale: m.scale,
            })
            .collect())
    }

    fn create_output(&self, name: &str, width: u32, height: u32, hz: u32) -> Result<String> {
        // Rule first, so the output comes up with the right mode.
        self.command_ok(&format!("keyword monitor {name},{width}x{height}@{hz},auto,1"))?;
        self.command_ok(&format!("output create headless {name}"))?;
        let m = super::wait_for_output(self, name)?;
        Ok(m.name)
    }

    fn remove_output(&self, name: &str) -> Result<()> {
        self.command_ok(&format!("output remove {name}"))
    }

    fn set_mode(&self, name: &str, width: u32, height: u32, hz: u32) -> Result<()> {
        let pos = self
            .raw_monitors()?
            .into_iter()
            .find(|m| m.name == name)
            .map(|m| format!("{}x{}", m.x, m.y))
            .unwrap_or_else(|| "auto".into());
        self.command_ok(&format!("keyword monitor {name},{width}x{height}@{hz},{pos},1"))
    }

    fn set_positions(&self, positions: &[(String, i32, i32)]) -> Result<()> {
        let mons = self.raw_monitors()?;
        let mut cmds = Vec::new();
        for (name, x, y) in positions {
            let Some(m) = mons.iter().find(|m| &m.name == name) else { continue };
            if (m.x, m.y) == (*x, *y) {
                continue;
            }
            let mut rule =
                format!("keyword monitor {name},{}x{}@{:.3},{x}x{y},{}", m.width, m.height, m.refresh_rate, Self::fmt_scale(m.scale));
            if m.transform != 0 {
                rule.push_str(&format!(",transform,{}", m.transform));
            }
            cmds.push(rule);
        }
        if cmds.is_empty() {
            return Ok(());
        }
        self.command_ok(&format!("[[BATCH]]{}", cmds.join(";")))
    }

    fn cursor_pos(&self) -> Result<Option<(f64, f64)>> {
        let r = self.request("j/cursorpos")?;
        let c: CursorPos = serde_json::from_str(&r).with_context(|| format!("cursorpos: {r}"))?;
        Ok(Some((c.x, c.y)))
    }

}
