//! Tiny software-rendered UI: toolbar, PIN pad, status card, per-panel chrome.
//! Each UI surface is an RGBA (premultiplied alpha) image shown on a quad layer.

use std::collections::HashMap;

use hyprvr_proto::msg::ScreenId;

/// UI pixels per meter in VR.
pub const PX_PER_M: f32 = 1400.0;

static FONT_BYTES: &[u8] = include_bytes!("../assets/DejaVuSans.ttf");

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    AddScreen,
    ToggleAudio,
    /// Slider value 0..1 is passed alongside.
    Dim,
    NextEnv,
    KeyboardSetup,
    KeyboardToggle,
    Recenter,
    ToggleStats,
    ToggleHover,
    Digit(u8),
    Backspace,
    ForgetServer,
    PanelGrab(ScreenId),
    PanelCurve(ScreenId),
    PanelClose(ScreenId),
    /// Drag handle of the main menu.
    MenuDrag,
    /// Hide the main menu (bring it back with a left palm pinch or the menu button).
    MenuClose,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Button { label: String, active: bool },
    Slider { label: String, value: f32 },
    Handle,
    Text { text: String, size: f32 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Widget {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub kind: Kind,
    pub action: Option<Action>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UiPanel {
    pub w: u32,
    pub h: u32,
    pub widgets: Vec<Widget>,
}

impl UiPanel {
    pub fn size_m(&self) -> (f32, f32) {
        (self.w as f32 / PX_PER_M, self.h as f32 / PX_PER_M)
    }

    /// Widget action at pixel (x, y); slider value in 0..1 for sliders.
    pub fn hit(&self, x: f32, y: f32) -> Option<(Action, f32, usize)> {
        for (i, w) in self.widgets.iter().enumerate() {
            if let Some(a) = &w.action
                && x >= w.x
                && x <= w.x + w.w
                && y >= w.y
                && y <= w.y + w.h
            {
                let v = ((x - w.x - 12.0) / (w.w - 24.0)).clamp(0.0, 1.0);
                return Some((a.clone(), v, i));
            }
        }
        None
    }
}

/// State shown by the toolbar.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ToolbarState {
    pub audio_headset: bool,
    pub dim: f32,
    pub env_name: String,
    pub stats_on: bool,
    pub hover_on: bool,
    pub cutout_on: bool,
    pub cutout_exists: bool,
    pub can_add: bool,
    /// Extra lines (stats / status) shown above the buttons.
    pub info: Vec<String>,
}

const BTN_H: f32 = 84.0;
const PAD: f32 = 14.0;
const HEADER_H: f32 = 60.0;

pub fn toolbar(s: &ToolbarState) -> UiPanel {
    let info_h = if s.info.is_empty() { 0.0 } else { s.info.len() as f32 * 34.0 + PAD };
    let w = 1400u32;
    let h = (HEADER_H + BTN_H * 2.0 + PAD * 3.0 + info_h) as u32;
    let mut widgets = Vec::new();
    // Header: drag handle across the width + close button.
    let close_w = 150.0;
    widgets.push(Widget { x: 0.0, y: 0.0, w: w as f32 - close_w - PAD, h: HEADER_H, kind: Kind::Handle, action: Some(Action::MenuDrag) });
    widgets.push(Widget {
        x: PAD * 3.0,
        y: 12.0,
        w: 700.0,
        h: 36.0,
        kind: Kind::Text { text: "hyprvr  ·  drag to move  ·  left palm pinch to toggle".into(), size: 24.0 },
        action: None,
    });
    widgets.push(Widget {
        x: w as f32 - close_w - PAD,
        y: 4.0,
        w: close_w,
        h: HEADER_H - 8.0,
        kind: Kind::Button { label: "Hide".into(), active: false },
        action: Some(Action::MenuClose),
    });
    let mut y = HEADER_H + PAD;
    for line in &s.info {
        widgets.push(Widget { x: PAD * 2.0, y, w: w as f32 - PAD * 4.0, h: 32.0, kind: Kind::Text { text: line.clone(), size: 26.0 }, action: None });
        y += 34.0;
    }
    if !s.info.is_empty() {
        y += PAD;
    }
    let row1: Vec<(String, bool, Action)> = vec![
        ("+ Screen".into(), false, Action::AddScreen),
        (if s.audio_headset { "Audio: Headset" } else { "Audio: Laptop" }.into(), s.audio_headset, Action::ToggleAudio),
        (format!("Env: {}", s.env_name), false, Action::NextEnv),
        ("Recenter".into(), false, Action::Recenter),
    ];
    let row1: Vec<_> = row1.into_iter().filter(|r| s.can_add || r.2 != Action::AddScreen).collect();
    layout_row(&mut widgets, &row1, y, w as f32);
    y += BTN_H + PAD;
    // Second row: dim slider + keyboard + stats.
    let slider_w = 480.0;
    widgets.push(Widget { x: PAD, y, w: slider_w, h: BTN_H, kind: Kind::Slider { label: "Dim".into(), value: s.dim }, action: Some(Action::Dim) });
    let mut row2: Vec<(String, bool, Action)> = vec![("Keyboard: set up".into(), false, Action::KeyboardSetup)];
    if s.cutout_exists {
        row2.push((if s.cutout_on { "Keyboard: on" } else { "Keyboard: off" }.into(), s.cutout_on, Action::KeyboardToggle));
    }
    row2.push((if s.hover_on { "Hover: on" } else { "Hover: off" }.into(), s.hover_on, Action::ToggleHover));
    row2.push(("Stats".into(), s.stats_on, Action::ToggleStats));
    let x0 = PAD * 2.0 + slider_w;
    let bw = (w as f32 - x0 - PAD * row2.len() as f32) / row2.len() as f32;
    for (i, (label, active, action)) in row2.into_iter().enumerate() {
        widgets.push(Widget { x: x0 + i as f32 * (bw + PAD), y, w: bw, h: BTN_H, kind: Kind::Button { label, active }, action: Some(action) });
    }
    UiPanel { w, h, widgets }
}

fn layout_row(widgets: &mut Vec<Widget>, items: &[(String, bool, Action)], y: f32, w: f32) {
    let n = items.len().max(1) as f32;
    let bw = (w - PAD * (n + 1.0)) / n;
    for (i, (label, active, action)) in items.iter().enumerate() {
        widgets.push(Widget {
            x: PAD + i as f32 * (bw + PAD),
            y,
            w: bw,
            h: BTN_H,
            kind: Kind::Button { label: label.clone(), active: *active },
            action: Some(action.clone()),
        });
    }
}

/// Number pad for the pairing PIN.
pub fn pin_pad(entered: &str, server: &str, message: &str) -> UiPanel {
    let w = 760u32;
    let key = 110.0;
    let mut widgets = vec![
        Widget { x: PAD * 2.0, y: PAD, w: w as f32 - PAD * 4.0, h: 40.0, kind: Kind::Text { text: format!("Pair with {server}"), size: 32.0 }, action: None },
        Widget { x: PAD * 2.0, y: 60.0, w: w as f32 - PAD * 4.0, h: 34.0, kind: Kind::Text { text: message.into(), size: 24.0 }, action: None },
    ];
    let shown: String = (0..6).map(|i| entered.chars().nth(i).unwrap_or('_')).flat_map(|c| [c, ' ']).collect();
    widgets.push(Widget { x: PAD * 2.0, y: 104.0, w: w as f32 - PAD * 4.0, h: 80.0, kind: Kind::Text { text: shown, size: 64.0 }, action: None });
    let top = 200.0;
    let kw = (w as f32 - PAD * 4.0) / 3.0;
    let keys: [(&str, Option<Action>); 12] = [
        ("1", Some(Action::Digit(1))),
        ("2", Some(Action::Digit(2))),
        ("3", Some(Action::Digit(3))),
        ("4", Some(Action::Digit(4))),
        ("5", Some(Action::Digit(5))),
        ("6", Some(Action::Digit(6))),
        ("7", Some(Action::Digit(7))),
        ("8", Some(Action::Digit(8))),
        ("9", Some(Action::Digit(9))),
        ("Forget", Some(Action::ForgetServer)),
        ("0", Some(Action::Digit(0))),
        ("Del", Some(Action::Backspace)),
    ];
    for (i, (label, action)) in keys.into_iter().enumerate() {
        let (r, c) = (i / 3, i % 3);
        widgets.push(Widget {
            x: PAD + c as f32 * (kw + PAD),
            y: top + r as f32 * (key + PAD),
            w: kw,
            h: key,
            kind: Kind::Button { label: label.into(), active: false },
            action,
        });
    }
    let h = (top + 4.0 * (key + PAD) + PAD) as u32;
    UiPanel { w, h, widgets }
}

/// Card with a few lines of text (connecting, errors, keyboard setup instructions).
pub fn card(lines: &[String]) -> UiPanel {
    let w = 1100u32;
    let mut widgets = Vec::new();
    let mut y = PAD * 2.0;
    for (i, l) in lines.iter().enumerate() {
        let size = if i == 0 { 40.0 } else { 28.0 };
        widgets.push(Widget { x: PAD * 2.0, y, w: w as f32 - PAD * 4.0, h: size + 8.0, kind: Kind::Text { text: l.clone(), size }, action: None });
        y += size + 14.0;
    }
    UiPanel { w, h: (y + PAD) as u32, widgets }
}

/// Strip under each panel: grab handle + curve toggle + close.
pub fn panel_chrome(screen: ScreenId, curved: bool, label: &str) -> UiPanel {
    let (w, h) = (900u32, 70u32);
    let bw = 150.0;
    let widgets = vec![
        Widget { x: 0.0, y: 0.0, w: w as f32 - 2.0 * (bw + 10.0), h: h as f32, kind: Kind::Handle, action: Some(Action::PanelGrab(screen)) },
        Widget { x: 40.0, y: 14.0, w: 300.0, h: 40.0, kind: Kind::Text { text: label.into(), size: 26.0 }, action: None },
        Widget {
            x: w as f32 - 2.0 * bw - 10.0,
            y: 0.0,
            w: bw,
            h: h as f32,
            kind: Kind::Button { label: if curved { "Flat" } else { "Curve" }.into(), active: curved },
            action: Some(Action::PanelCurve(screen)),
        },
        Widget { x: w as f32 - bw, y: 0.0, w: bw, h: h as f32, kind: Kind::Button { label: "Close".into(), active: false }, action: Some(Action::PanelClose(screen)) },
    ];
    UiPanel { w, h, widgets }
}

// ---------------------------------------------------------------- rendering

pub struct Painter {
    font: fontdue::Font,
    cache: HashMap<(char, u32), (fontdue::Metrics, Vec<u8>)>,
}

impl Default for Painter {
    fn default() -> Self {
        Self::new()
    }
}

/// Premultiplied RGBA color.
type Rgba = [f32; 4];

const BG: Rgba = [0.08, 0.09, 0.12, 0.88];
const BTN: Rgba = [0.20, 0.22, 0.30, 1.0];
const BTN_ACTIVE: Rgba = [0.27, 0.42, 0.75, 1.0];
const BTN_HOVER_ADD: f32 = 0.12;
const TEXT: Rgba = [0.92, 0.93, 0.96, 1.0];

impl Painter {
    pub fn new() -> Self {
        let font = fontdue::Font::from_bytes(FONT_BYTES, fontdue::FontSettings::default()).expect("embedded font");
        Painter { font, cache: HashMap::new() }
    }

    /// Render a panel; `hover` = index of hovered widget (highlighted). Premultiplied RGBA8.
    pub fn render(&mut self, p: &UiPanel, hover: Option<usize>) -> Vec<u8> {
        let mut img = Img { w: p.w, h: p.h, px: vec![0; (p.w * p.h * 4) as usize] };
        img.round_rect(0.0, 0.0, p.w as f32, p.h as f32, 18.0, BG);
        for (i, w) in p.widgets.iter().enumerate() {
            let hov = hover == Some(i);
            match &w.kind {
                Kind::Button { label, active } => {
                    let mut c = if *active { BTN_ACTIVE } else { BTN };
                    if hov {
                        c = [c[0] + BTN_HOVER_ADD, c[1] + BTN_HOVER_ADD, c[2] + BTN_HOVER_ADD, 1.0];
                    }
                    img.round_rect(w.x, w.y, w.w, w.h, 12.0, c);
                    let size = (w.h * 0.36).clamp(18.0, 40.0);
                    self.text(&mut img, label, w.x + w.w / 2.0, w.y + w.h / 2.0, size, TEXT, true, w.w - 16.0);
                }
                Kind::Slider { label, value } => {
                    img.round_rect(w.x, w.y, w.w, w.h, 12.0, if hov { [0.26, 0.28, 0.36, 1.0] } else { BTN });
                    let fill = 12.0 + (w.w - 24.0) * value.clamp(0.0, 1.0);
                    img.round_rect(w.x + 6.0, w.y + w.h - 22.0, w.w - 12.0, 10.0, 5.0, [0.1, 0.1, 0.12, 1.0]);
                    img.round_rect(w.x + 6.0, w.y + w.h - 22.0, fill, 10.0, 5.0, BTN_ACTIVE);
                    img.round_rect(w.x + fill - 2.0, w.y + w.h - 32.0, 16.0, 30.0, 6.0, TEXT);
                    let t = format!("{label} {:.0}%", value * 100.0);
                    self.text(&mut img, &t, w.x + w.w / 2.0, w.y + w.h * 0.35, 28.0, TEXT, true, w.w);
                }
                Kind::Handle => {
                    let c = if hov { [0.45, 0.5, 0.65, 1.0] } else { [0.3, 0.33, 0.42, 1.0] };
                    img.round_rect(w.x + w.w * 0.3, w.y + w.h / 2.0 - 7.0, w.w * 0.4, 14.0, 7.0, c);
                }
                Kind::Text { text, size } => {
                    self.text(&mut img, text, w.x, w.y + w.h / 2.0, *size, TEXT, false, w.w);
                }
            }
        }
        img.px
    }

    #[allow(clippy::too_many_arguments)]
    fn text(&mut self, img: &mut Img, s: &str, x: f32, cy: f32, size: f32, color: Rgba, center: bool, max_w: f32) {
        let px = size.round() as u32;
        let glyphs: Vec<(fontdue::Metrics, Vec<u8>)> = s
            .chars()
            .map(|c| {
                self.cache
                    .entry((c, px))
                    .or_insert_with(|| self.font.rasterize(c, px as f32))
                    .clone()
            })
            .collect();
        let total: f32 = glyphs.iter().map(|g| g.0.advance_width).sum();
        // Text that is too long is clipped.
        let mut pen = if center { x - total.min(max_w) / 2.0 } else { x };
        let limit = if center { x + max_w / 2.0 } else { x + max_w };
        let ascent = size * 0.72;
        let baseline = cy + ascent / 2.0;
        for (m, cov) in glyphs {
            if pen + m.advance_width > limit + 1.0 {
                break;
            }
            let gx = (pen + m.xmin as f32).round() as i32;
            let gy = (baseline - m.height as f32 - m.ymin as f32).round() as i32;
            for row in 0..m.height {
                for col in 0..m.width {
                    let a = cov[row * m.width + col] as f32 / 255.0;
                    if a > 0.0 {
                        img.blend(gx + col as i32, gy + row as i32, [color[0] * a, color[1] * a, color[2] * a, color[3] * a]);
                    }
                }
            }
            pen += m.advance_width;
        }
    }
}

struct Img {
    w: u32,
    h: u32,
    px: Vec<u8>,
}

impl Img {
    /// Premultiplied "over".
    fn blend(&mut self, x: i32, y: i32, c: Rgba) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return;
        }
        let i = ((y as u32 * self.w + x as u32) * 4) as usize;
        let inv = 1.0 - c[3];
        for k in 0..4 {
            let d = self.px[i + k] as f32 / 255.0;
            self.px[i + k] = ((c[k] + d * inv).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
    }

    /// Anti-aliased rounded rectangle; `c` is straight alpha.
    fn round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Rgba) {
        let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
        let x0 = x.floor().max(0.0) as i32;
        let y0 = y.floor().max(0.0) as i32;
        let x1 = (x + w).ceil().min(self.w as f32) as i32;
        let y1 = (y + h).ceil().min(self.h as f32) as i32;
        for py in y0..y1 {
            for pxl in x0..x1 {
                let fx = pxl as f32 + 0.5;
                let fy = py as f32 + 0.5;
                // Signed distance to the rounded rect.
                let qx = (fx - (x + w / 2.0)).abs() - (w / 2.0 - r);
                let qy = (fy - (y + h / 2.0)).abs() - (h / 2.0 - r);
                let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r;
                let cov = (0.5 - outside).clamp(0.0, 1.0);
                if cov > 0.0 {
                    let a = c[3] * cov;
                    self.blend(pxl, py, [c[0] * a, c[1] * a, c[2] * a, a]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolbar_hits_and_renders() {
        let s = ToolbarState { dim: 0.3, env_name: "Passthrough".into(), can_add: true, ..Default::default() };
        let p = toolbar(&s);
        let w0 = &p.widgets[0];
        let (a, _, _) = p.hit(w0.x + 5.0, w0.y + 5.0).unwrap();
        assert_eq!(a, Action::MenuDrag);
        let add = p.widgets.iter().find(|w| w.action == Some(Action::AddScreen)).unwrap();
        assert_eq!(p.hit(add.x + 5.0, add.y + 5.0).unwrap().0, Action::AddScreen);
        let hide = p.widgets.iter().find(|w| w.action == Some(Action::MenuClose)).unwrap();
        assert_eq!(p.hit(hide.x + 5.0, hide.y + 5.0).unwrap().0, Action::MenuClose);
        let slider = p.widgets.iter().find(|w| matches!(w.kind, Kind::Slider { .. })).unwrap();
        let (a, v, _) = p.hit(slider.x + slider.w - 1.0, slider.y + 10.0).unwrap();
        assert_eq!(a, Action::Dim);
        assert!(v > 0.99);
        let mut painter = Painter::new();
        let img = painter.render(&p, Some(0));
        assert_eq!(img.len(), (p.w * p.h * 4) as usize);
        // Something bright (text) got drawn.
        assert!(img.chunks_exact(4).any(|px| px[0] > 200 && px[3] > 200));
    }

    #[test]
    fn pin_pad_digits() {
        let p = pin_pad("12", "laptop", "Enter the PIN shown on the PC");
        let five = p.widgets.iter().find(|w| w.action == Some(Action::Digit(5))).unwrap();
        assert_eq!(p.hit(five.x + 1.0, five.y + 1.0).unwrap().0, Action::Digit(5));
        assert!(p.hit(-5.0, -5.0).is_none());
        let mut painter = Painter::new();
        assert!(!painter.render(&p, None).is_empty());
    }

    #[test]
    fn chrome_actions() {
        let p = panel_chrome(7, false, "HYPRVR-1");
        assert_eq!(p.hit(10.0, 35.0).unwrap().0, Action::PanelGrab(7));
        assert_eq!(p.hit(899.0, 35.0).unwrap().0, Action::PanelClose(7));
        assert_eq!(p.hit(650.0, 35.0).unwrap().0, Action::PanelCurve(7));
    }
}
