//! sway / i3 IPC. Used for headless end-to-end testing (and works on a real sway session).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::{Compositor, Monitor};

pub struct Sway {
    sock: PathBuf,
}

#[derive(Deserialize)]
struct SRect {
    x: i32,
    y: i32,
}
#[derive(Deserialize)]
struct SMode {
    width: u32,
    height: u32,
    refresh: u32,
}
#[derive(Deserialize)]
struct SOutput {
    name: String,
    rect: SRect,
    active: bool,
    scale: Option<f32>,
    current_mode: Option<SMode>,
}

const RUN_COMMAND: u32 = 0;
const GET_OUTPUTS: u32 = 3;

impl Sway {
    pub fn from_env() -> Result<Self> {
        Ok(Sway { sock: std::env::var_os("SWAYSOCK").context("SWAYSOCK")?.into() })
    }

    fn ipc(&self, ty: u32, payload: &str) -> Result<String> {
        let mut s = UnixStream::connect(&self.sock)?;
        let mut msg = b"i3-ipc".to_vec();
        msg.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
        msg.extend_from_slice(&ty.to_ne_bytes());
        msg.extend_from_slice(payload.as_bytes());
        s.write_all(&msg)?;
        let mut hdr = [0u8; 14];
        s.read_exact(&mut hdr)?;
        let len = u32::from_ne_bytes(hdr[6..10].try_into().unwrap()) as usize;
        let mut body = vec![0u8; len];
        s.read_exact(&mut body)?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    fn run(&self, cmd: &str) -> Result<()> {
        let r = self.ipc(RUN_COMMAND, cmd)?;
        let v: Vec<serde_json::Value> = serde_json::from_str(&r)?;
        if v.iter().all(|x| x["success"].as_bool() == Some(true)) {
            Ok(())
        } else {
            bail!("sway `{cmd}` failed: {r}")
        }
    }

    fn outputs(&self) -> Result<Vec<SOutput>> {
        Ok(serde_json::from_str(&self.ipc(GET_OUTPUTS, "")?)?)
    }
}

impl Compositor for Sway {
    fn kind(&self) -> &'static str {
        "sway"
    }

    fn monitors(&self) -> Result<Vec<Monitor>> {
        Ok(self
            .outputs()?
            .into_iter()
            .filter(|o| o.active)
            .filter_map(|o| {
                let m = o.current_mode?;
                Some(Monitor {
                    headless: false,
                    name: o.name,
                    x: o.rect.x,
                    y: o.rect.y,
                    width: m.width,
                    height: m.height,
                    refresh: m.refresh as f32 / 1000.0,
                    scale: o.scale.unwrap_or(1.0),
                })
            })
            .collect())
    }

    fn create_output(&self, _name: &str, width: u32, height: u32, hz: u32) -> Result<String> {
        let before: Vec<String> = self.outputs()?.into_iter().map(|o| o.name).collect();
        self.run("create_output")?;
        let mut name = None;
        for _ in 0..50 {
            if let Some(o) = self.outputs()?.into_iter().find(|o| !before.contains(&o.name)) {
                name = Some(o.name);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        let name = name.context("sway did not create an output")?;
        self.set_mode(&name, width, height, hz)?;
        Ok(name)
    }

    fn remove_output(&self, name: &str) -> Result<()> {
        self.run(&format!("output {name} unplug"))
    }

    fn set_mode(&self, name: &str, width: u32, height: u32, hz: u32) -> Result<()> {
        self.run(&format!("output {name} mode --custom {width}x{height}@{hz}Hz"))?;
        // The mode change is applied asynchronously.
        for _ in 0..50 {
            if self.monitors()?.iter().any(|m| m.name == name && m.width == width && m.height == height) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        bail!("mode {width}x{height} not applied to {name}")
    }

    fn set_positions(&self, positions: &[(String, i32, i32)]) -> Result<()> {
        for (n, x, y) in positions {
            self.run(&format!("output {n} position {x} {y}"))?;
        }
        Ok(())
    }

    fn cursor_pos(&self) -> Result<Option<(f64, f64)>> {
        Ok(None)
    }
}
