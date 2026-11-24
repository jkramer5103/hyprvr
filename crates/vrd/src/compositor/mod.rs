//! Compositor control (output management, positions, cursor position).
//! Hyprland is the real target; sway exists for headless end-to-end testing.

pub mod hyprland;
pub mod sway;

use anyhow::Result;

#[derive(Clone, Debug, PartialEq)]
pub struct Monitor {
    pub name: String,
    /// Logical position.
    pub x: i32,
    pub y: i32,
    /// Mode size in physical pixels.
    pub width: u32,
    pub height: u32,
    pub refresh: f32,
    pub scale: f32,
    /// Created by hyprvr (headless).
    pub headless: bool,
}

impl Monitor {
    pub fn logical_size(&self) -> (i32, i32) {
        ((self.width as f32 / self.scale).round() as i32, (self.height as f32 / self.scale).round() as i32)
    }
}

pub trait Compositor: Send + Sync {
    fn kind(&self) -> &'static str;
    fn monitors(&self) -> Result<Vec<Monitor>>;
    /// Create a headless output named after `name` (best effort) and return its real name.
    fn create_output(&self, name: &str, width: u32, height: u32, hz: u32) -> Result<String>;
    fn remove_output(&self, name: &str) -> Result<()>;
    /// Set mode of a headless output.
    fn set_mode(&self, name: &str, width: u32, height: u32, hz: u32) -> Result<()>;
    /// Move several monitors at once (logical coordinates).
    fn set_positions(&self, positions: &[(String, i32, i32)]) -> Result<()>;
    /// Global logical cursor position, if the compositor exposes it.
    fn cursor_pos(&self) -> Result<Option<(f64, f64)>>;
}

pub fn detect() -> Result<Box<dyn Compositor>> {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return Ok(Box::new(hyprland::Hyprland::from_env()?));
    }
    if std::env::var_os("SWAYSOCK").is_some() {
        return Ok(Box::new(sway::Sway::from_env()?));
    }
    anyhow::bail!("no supported compositor found (HYPRLAND_INSTANCE_SIGNATURE / SWAYSOCK unset)")
}

/// Wait until an output with `name` shows up (compositors create outputs asynchronously).
pub fn wait_for_output(c: &dyn Compositor, name: &str) -> Result<Monitor> {
    for _ in 0..50 {
        if let Some(m) = c.monitors()?.into_iter().find(|m| m.name == name) {
            return Ok(m);
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    anyhow::bail!("output {name} did not appear")
}
