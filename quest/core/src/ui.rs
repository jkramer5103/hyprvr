//! Tiny software-rendered UI: toolbar, PIN pad, status card, per-panel chrome.
//! Each UI surface is an RGBA (premultiplied alpha) image shown on a quad layer.

use std::collections::HashMap;

use hyprvr_proto::msg::{Environment, ScreenId};

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
    /// Open the environment picker.
    EnvPicker,
    /// Pick an environment (picker stays open).
    EnvPick(Environment),
    /// Picker page +/- 1.
    EnvPage(i32),
    /// Back from the picker to the main menu.
    EnvBack,
    /// Turn the current environment by 90°.
    EnvRotate,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Button { label: String, active: bool },
    Slider { label: String, value: f32 },
    /// Drag area; draws a grip icon at its left edge.
    Handle,
    Text { text: String, size: f32 },
    /// Dimmer secondary text.
    Muted { text: String, size: f32 },
    /// Button with a small caption above a larger value.
    Tile { caption: String, value: String, active: bool },
    /// Environment card: preview image (by key, see `Painter::set_image`) + name.
    /// `loaded` makes the panel change (and re-render) when the preview arrives.
    EnvCard { label: String, image: Option<String>, loaded: bool, active: bool, fill: [f32; 3] },
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
    /// Current environment is an image (it can be rotated).
    pub env_is_image: bool,
    pub cutout_on: bool,
    pub cutout_exists: bool,
    pub can_add: bool,
    /// Extra lines (stats / status) shown above the buttons.
    pub info: Vec<String>,
}

const PAD: f32 = 14.0;
const HEADER_H: f32 = 76.0;
const TILE_H: f32 = 112.0;
const GAP: f32 = 16.0;
const MARGIN: f32 = 22.0;

/// "the_sky_is_on_fire.jpg" -> "The sky is on fire".
pub fn pretty_name(n: &str) -> String {
    let stem = n.rsplit_once('.').map_or(n, |(a, b)| if b.len() <= 4 { a } else { n });
    let t = stem.replace(['_', '-'], " ");
    let mut c = t.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => t,
    }
}

pub fn toolbar(s: &ToolbarState) -> UiPanel {
    let w = 1400.0f32;
    let mut widgets = Vec::new();

    // Header: grip + title + status on the left (all of it drags), Hide on the right.
    let hide_w = 140.0;
    widgets.push(Widget { x: 0.0, y: 0.0, w: w - hide_w - MARGIN * 2.0, h: HEADER_H, kind: Kind::Handle, action: Some(Action::MenuDrag) });
    widgets.push(Widget { x: 64.0, y: 0.0, w: 160.0, h: HEADER_H, kind: Kind::Text { text: "hyprvr".into(), size: 34.0 }, action: None });
    if let Some(status) = s.info.first() {
        widgets.push(Widget { x: 200.0, y: 0.0, w: w - 200.0 - hide_w - MARGIN * 3.0, h: HEADER_H, kind: Kind::Muted { text: status.clone(), size: 24.0 }, action: None });
    }
    widgets.push(Widget {
        x: w - hide_w - MARGIN,
        y: 14.0,
        w: hide_w,
        h: HEADER_H - 28.0,
        kind: Kind::Button { label: "Hide".into(), active: false },
        action: Some(Action::MenuClose),
    });
    let mut y = HEADER_H + 4.0;
    for line in s.info.iter().skip(1) {
        widgets.push(Widget { x: MARGIN, y, w: w - MARGIN * 2.0, h: 32.0, kind: Kind::Muted { text: line.clone(), size: 22.0 }, action: None });
        y += 34.0;
    }

    let tile = |caption: &str, value: String, active: bool, action: Action| (caption.to_string(), value, active, action);
    let mut tiles = Vec::new();
    if s.can_add {
        tiles.push(tile("Screens", "+ Add".into(), false, Action::AddScreen));
    }
    tiles.push(tile("Layout", "Recenter".into(), false, Action::Recenter));
    tiles.push(tile("Audio", if s.audio_headset { "Headset" } else { "Laptop" }.into(), s.audio_headset, Action::ToggleAudio));
    tiles.push(tile("Environment", pretty_name(&s.env_name), false, Action::EnvPicker));
    if s.env_is_image {
        tiles.push(tile("Environment", "Rotate 90°".into(), false, Action::EnvRotate));
    }
    // Slot for the dim slider.
    let slider_slot = tiles.len();
    tiles.push(tile("", String::new(), false, Action::Dim));
    tiles.push(tile("Laser hover", if s.hover_on { "On" } else { "Off" }.into(), s.hover_on, Action::ToggleHover));
    if s.cutout_exists {
        tiles.push(tile("Keyboard", if s.cutout_on { "Shown" } else { "Hidden" }.into(), s.cutout_on, Action::KeyboardToggle));
        tiles.push(tile("Keyboard", "Redo setup".into(), false, Action::KeyboardSetup));
    } else {
        tiles.push(tile("Keyboard", "Set up".into(), false, Action::KeyboardSetup));
    }
    tiles.push(tile("Stats", if s.stats_on { "On" } else { "Off" }.into(), s.stats_on, Action::ToggleStats));

    let cols = if tiles.len() > 8 { 5 } else { 4 };
    let tw = (w - MARGIN * 2.0 - GAP * (cols as f32 - 1.0)) / cols as f32;
    y += 6.0;
    let rows = tiles.len().div_ceil(cols);
    for (i, (caption, value, active, action)) in tiles.into_iter().enumerate() {
        let (r, c) = (i / cols, i % cols);
        let (x, ty) = (MARGIN + c as f32 * (tw + GAP), y + r as f32 * (TILE_H + GAP));
        let kind = if i == slider_slot {
            Kind::Slider { label: "Dim".into(), value: s.dim }
        } else {
            Kind::Tile { caption, value, active }
        };
        widgets.push(Widget { x, y: ty, w: tw, h: TILE_H, kind, action: Some(action) });
    }
    let h = y + rows as f32 * (TILE_H + GAP) - GAP + MARGIN;
    UiPanel { w: w as u32, h: h.ceil() as u32, widgets }
}

/// Environments per picker page.
pub const PICKER_COLS: usize = 5;
pub const PICKER_PER_PAGE: usize = PICKER_COLS * 3;

/// Environment picker: Passthrough / Void / every image, as preview cards.
/// `loaded(name)` tells whether a preview image has arrived.
pub fn env_picker(envs: &[String], current: &Environment, passthrough: bool, page: usize, loaded: &dyn Fn(&str) -> bool) -> UiPanel {
    let w = 1400.0f32;
    let mut items: Vec<Environment> = Vec::new();
    if passthrough {
        items.push(Environment::Passthrough);
    }
    items.push(Environment::Void);
    items.extend(envs.iter().cloned().map(Environment::Image));
    let pages = items.len().div_ceil(PICKER_PER_PAGE).max(1);
    let page = page.min(pages - 1);

    let mut widgets = Vec::new();
    let back_w = 140.0;
    let nav_w = 76.0;
    let right = w - MARGIN;
    let back_x = right - back_w;
    let nav_x = back_x - GAP - nav_w * 2.0 - 8.0 - 90.0;
    widgets.push(Widget { x: 0.0, y: 0.0, w: if pages > 1 { nav_x - GAP } else { back_x - GAP }, h: HEADER_H, kind: Kind::Handle, action: Some(Action::MenuDrag) });
    widgets.push(Widget { x: 64.0, y: 0.0, w: 400.0, h: HEADER_H, kind: Kind::Text { text: "Environment".into(), size: 34.0 }, action: None });
    if pages > 1 {
        let by = 14.0;
        let bh = HEADER_H - 28.0;
        widgets.push(Widget { x: nav_x, y: by, w: nav_w, h: bh, kind: Kind::Button { label: "‹".into(), active: false }, action: (page > 0).then_some(Action::EnvPage(-1)) });
        widgets.push(Widget { x: nav_x + nav_w + 4.0, y: 0.0, w: 90.0, h: HEADER_H, kind: Kind::Muted { text: format!(" {}/{}", page + 1, pages), size: 24.0 }, action: None });
        widgets.push(Widget { x: nav_x + nav_w + 98.0, y: by, w: nav_w, h: bh, kind: Kind::Button { label: "›".into(), active: false }, action: (page + 1 < pages).then_some(Action::EnvPage(1)) });
    }
    widgets.push(Widget { x: back_x, y: 14.0, w: back_w, h: HEADER_H - 28.0, kind: Kind::Button { label: "Back".into(), active: false }, action: Some(Action::EnvBack) });
    if matches!(current, Environment::Image(_)) {
        let rw = 200.0;
        // First, so it wins the hit test over the drag handle underneath.
        widgets.insert(0, Widget { x: 330.0, y: 14.0, w: rw, h: HEADER_H - 28.0, kind: Kind::Button { label: "Rotate 90°".into(), active: false }, action: Some(Action::EnvRotate) });
    }

    let cw = (w - MARGIN * 2.0 - GAP * (PICKER_COLS as f32 - 1.0)) / PICKER_COLS as f32;
    let ch = (cw - 16.0) / 2.0 + 16.0 + 46.0;
    let y0 = HEADER_H + 6.0;
    for (i, env) in items.iter().skip(page * PICKER_PER_PAGE).take(PICKER_PER_PAGE).enumerate() {
        let (r, c) = (i / PICKER_COLS, i % PICKER_COLS);
        let (label, image, fill) = match env {
            Environment::Passthrough => ("Passthrough".to_string(), None, [0.42, 0.45, 0.50]),
            Environment::Void => ("Void".to_string(), None, [0.02, 0.02, 0.03]),
            Environment::Image(n) => (pretty_name(n), Some(n.clone()), [0.12, 0.13, 0.17]),
        };
        let loaded = image.as_deref().is_some_and(loaded);
        widgets.push(Widget {
            x: MARGIN + c as f32 * (cw + GAP),
            y: y0 + r as f32 * (ch + GAP),
            w: cw,
            h: ch,
            kind: Kind::EnvCard { label, image, loaded, active: env == current, fill },
            action: Some(Action::EnvPick(env.clone())),
        });
    }
    // Fixed height (3 rows), so the panel doesn't jump between pages.
    let h = y0 + 3.0 * (ch + GAP) - GAP + MARGIN;
    UiPanel { w: w as u32, h: h.ceil() as u32, widgets }
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

/// Strip under each panel: grip + label (drag area), curve toggle, close.
pub fn panel_chrome(screen: ScreenId, curved: bool, label: &str) -> UiPanel {
    let (w, h) = (900.0f32, 72.0f32);
    let bw = 132.0;
    let gap = 10.0;
    let m = 10.0;
    let close_x = w - m - bw;
    let curve_x = close_x - gap - bw;
    let widgets = vec![
        Widget { x: 0.0, y: 0.0, w: curve_x - gap, h, kind: Kind::Handle, action: Some(Action::PanelGrab(screen)) },
        Widget { x: 60.0, y: 0.0, w: curve_x - gap - 70.0, h, kind: Kind::Muted { text: label.into(), size: 24.0 }, action: None },
        Widget {
            x: curve_x,
            y: m,
            w: bw,
            h: h - 2.0 * m,
            kind: Kind::Button { label: if curved { "Flat" } else { "Curve" }.into(), active: curved },
            action: Some(Action::PanelCurve(screen)),
        },
        Widget { x: close_x, y: m, w: bw, h: h - 2.0 * m, kind: Kind::Button { label: "Close".into(), active: false }, action: Some(Action::PanelClose(screen)) },
    ];
    UiPanel { w: w as u32, h: h as u32, widgets }
}

// ---------------------------------------------------------------- rendering

pub struct Painter {
    font: fontdue::Font,
    cache: HashMap<(char, u32), (fontdue::Metrics, Vec<u8>)>,
    /// Straight-alpha RGBA images by key (environment previews).
    images: HashMap<String, (u32, u32, Vec<u8>)>,
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
const MUTED: Rgba = [0.60, 0.63, 0.72, 1.0];

impl Painter {
    pub fn new() -> Self {
        let font = fontdue::Font::from_bytes(FONT_BYTES, fontdue::FontSettings::default()).expect("embedded font");
        Painter { font, cache: HashMap::new(), images: HashMap::new() }
    }

    pub fn set_image(&mut self, key: &str, w: u32, h: u32, rgba: Vec<u8>) {
        self.images.insert(key.to_string(), (w, h, rgba));
    }

    pub fn has_image(&self, key: &str) -> bool {
        self.images.contains_key(key)
    }

    pub fn clear_images(&mut self) {
        self.images.clear();
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
                    let size = (w.h * 0.42).clamp(22.0, 34.0);
                    self.text(&mut img, label, w.x + w.w / 2.0, w.y + w.h / 2.0, size, TEXT, true, w.w - 24.0);
                }
                Kind::Tile { caption, value, active } => {
                    let mut c = if *active { BTN_ACTIVE } else { BTN };
                    if hov {
                        c = [c[0] + BTN_HOVER_ADD, c[1] + BTN_HOVER_ADD, c[2] + BTN_HOVER_ADD, 1.0];
                    }
                    img.round_rect(w.x, w.y, w.w, w.h, 14.0, c);
                    let muted = if *active { [0.85, 0.9, 1.0, 1.0] } else { MUTED };
                    self.text(&mut img, caption, w.x + 20.0, w.y + w.h * 0.30, 22.0, muted, false, w.w - 40.0);
                    self.text(&mut img, value, w.x + 20.0, w.y + w.h * 0.66, 32.0, TEXT, false, w.w - 40.0);
                }
                Kind::Slider { label, value } => {
                    img.round_rect(w.x, w.y, w.w, w.h, 14.0, if hov { [0.26, 0.28, 0.36, 1.0] } else { BTN });
                    // Track inset by 20 px; hit() maps x - 12 .. w - 12 to 0..1, close enough.
                    let (tx, tw) = (w.x + 20.0, w.w - 40.0);
                    let ty = w.y + w.h * 0.70;
                    let v = value.clamp(0.0, 1.0);
                    img.round_rect(tx, ty - 4.0, tw, 8.0, 4.0, [0.1, 0.1, 0.12, 1.0]);
                    img.round_rect(tx, ty - 4.0, tw * v, 8.0, 4.0, BTN_ACTIVE);
                    img.round_rect(tx + tw * v - 11.0, ty - 11.0, 22.0, 22.0, 11.0, TEXT);
                    self.text(&mut img, label, w.x + 20.0, w.y + w.h * 0.30, 22.0, MUTED, false, w.w - 40.0);
                    let pct = format!("{:.0}%", v * 100.0);
                    self.text(&mut img, &pct, w.x + w.w - 20.0 - 60.0, w.y + w.h * 0.30, 22.0, TEXT, false, 60.0);
                }
                Kind::Handle => {
                    if hov {
                        img.round_rect(w.x + 6.0, w.y + 6.0, w.w - 12.0, w.h - 12.0, 12.0, [1.0, 1.0, 1.0, 0.06]);
                    }
                    // 2x3 grip dots.
                    let c = if hov { [0.75, 0.8, 0.95, 1.0] } else { [0.45, 0.48, 0.58, 1.0] };
                    let cy = w.y + w.h / 2.0;
                    for (dx, dy) in [(0.0, -12.0), (0.0, 0.0), (0.0, 12.0), (11.0, -12.0), (11.0, 0.0), (11.0, 12.0)] {
                        img.round_rect(w.x + 26.0 + dx - 3.5, cy + dy - 3.5, 7.0, 7.0, 3.5, c);
                    }
                }
                Kind::Text { text, size } => {
                    self.text(&mut img, text, w.x, w.y + w.h / 2.0, *size, TEXT, false, w.w);
                }
                Kind::EnvCard { label, image, active, fill, .. } => {
                    if *active {
                        img.round_rect(w.x - 5.0, w.y - 5.0, w.w + 10.0, w.h + 10.0, 18.0, [0.40, 0.58, 0.95, 1.0]);
                    }
                    let mut c = BTN;
                    if hov {
                        c = [c[0] + BTN_HOVER_ADD, c[1] + BTN_HOVER_ADD, c[2] + BTN_HOVER_ADD, 1.0];
                    }
                    img.round_rect(w.x, w.y, w.w, w.h, 14.0, c);
                    let (ix, iy, iw) = (w.x + 8.0, w.y + 8.0, w.w - 16.0);
                    let ih = iw / 2.0;
                    match image.as_ref().and_then(|k| self.images.get(k)) {
                        Some((sw, sh, px)) => img.image(ix, iy, iw, ih, *sw, *sh, px, if hov { 1.12 } else { 1.0 }),
                        None => img.round_rect(ix, iy, iw, ih, 8.0, [fill[0], fill[1], fill[2], 1.0]),
                    }
                    let lc = if *active { TEXT } else { [0.85, 0.87, 0.92, 1.0] };
                    self.text(&mut img, label, w.x + w.w / 2.0, iy + ih + 23.0, 24.0, lc, true, w.w - 20.0);
                }
                Kind::Muted { text, size } => {
                    self.text(&mut img, text, w.x, w.y + w.h / 2.0, *size, MUTED, false, w.w);
                }
            }
        }
        img.px
    }

    fn width(&mut self, s: &str, px: u32) -> f32 {
        s.chars().map(|c| self.glyph(c, px).0.advance_width).sum()
    }

    fn glyph(&mut self, c: char, px: u32) -> (fontdue::Metrics, Vec<u8>) {
        let font = &self.font;
        self.cache.entry((c, px)).or_insert_with(|| font.rasterize(c, px as f32)).clone()
    }

    /// Draws `s` within `max_w`: shrinks it (down to 75 %) and then ellipsizes, never overflows.
    #[allow(clippy::too_many_arguments)]
    fn text(&mut self, img: &mut Img, s: &str, x: f32, cy: f32, size: f32, color: Rgba, center: bool, max_w: f32) {
        let mut px = size.round() as u32;
        let min_px = ((size * 0.75).round() as u32).max(12);
        while px > min_px && self.width(s, px) > max_w {
            px -= 1;
        }
        let mut s = s.to_string();
        if self.width(&s, px) > max_w {
            while !s.is_empty() && self.width(&format!("{s}…"), px) > max_w {
                s.pop();
            }
            s = format!("{}…", s.trim_end());
        }
        self.draw_text(img, &s, x, cy, px, color, center, max_w);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text(&mut self, img: &mut Img, s: &str, x: f32, cy: f32, px: u32, color: Rgba, center: bool, max_w: f32) {
        let size = px as f32;
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

    /// Bilinear-scaled opaque image into (x, y, w, h); `gain` brightens (hover).
    #[allow(clippy::too_many_arguments)]
    fn image(&mut self, x: f32, y: f32, w: f32, h: f32, sw: u32, sh: u32, px: &[u8], gain: f32) {
        let (x0, y0) = (x.round() as i32, y.round() as i32);
        let (dw, dh) = (w.round() as i32, h.round() as i32);
        for dy in 0..dh {
            for dx in 0..dw {
                let u = ((dx as f32 + 0.5) / dw as f32 * sw as f32 - 0.5).clamp(0.0, sw as f32 - 1.0);
                let v = ((dy as f32 + 0.5) / dh as f32 * sh as f32 - 0.5).clamp(0.0, sh as f32 - 1.0);
                let (u0, v0) = (u.floor() as u32, v.floor() as u32);
                let (u1, v1) = ((u0 + 1).min(sw - 1), (v0 + 1).min(sh - 1));
                let (fu, fv) = (u - u0 as f32, v - v0 as f32);
                let at = |uu: u32, vv: u32, k: usize| px[((vv * sw + uu) * 4) as usize + k] as f32 / 255.0;
                let mut c = [0.0f32; 4];
                for (k, ck) in c.iter_mut().enumerate().take(3) {
                    let top = at(u0, v0, k) * (1.0 - fu) + at(u1, v0, k) * fu;
                    let bot = at(u0, v1, k) * (1.0 - fu) + at(u1, v1, k) * fu;
                    *ck = ((top * (1.0 - fv) + bot * fv) * gain).min(1.0);
                }
                c[3] = 1.0;
                self.blend(x0 + dx, y0 + dy, c);
            }
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
        assert_eq!(p.hit(880.0, 35.0).unwrap().0, Action::PanelClose(7));
        assert_eq!(p.hit(650.0, 35.0).unwrap().0, Action::PanelCurve(7));
    }

    #[test]
    fn tiles_fit_inside_panel_and_dump_previews() {
        let mut painter = Painter::new();
        for (name, s) in [
            ("basic", ToolbarState { dim: 0.67, env_name: "the_sky_is_on_fire.jpg".into(), can_add: true, info: vec!["jk-laptop3  ·  2 screens".into()], ..Default::default() }),
            ("full", ToolbarState { env_is_image: true, dim: 0.2, env_name: "Passthrough".into(), can_add: true, cutout_exists: true, cutout_on: true, hover_on: true, stats_on: true, audio_headset: true, info: vec!["jk-laptop3  ·  3 screens".into()] }),
        ] {
            let p = toolbar(&s);
            for w in &p.widgets {
                assert!(w.x >= 0.0 && w.y >= 0.0 && w.x + w.w <= p.w as f32 && w.y + w.h <= p.h as f32, "{w:?} outside {}x{}", p.w, p.h);
            }
            // No two interactive widgets overlap (the drag handle excepted).
            let act: Vec<_> = p.widgets.iter().filter(|w| w.action.is_some() && w.kind != Kind::Handle).collect();
            for (i, a) in act.iter().enumerate() {
                for b in &act[i + 1..] {
                    let sep = a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y;
                    assert!(sep, "{a:?} overlaps {b:?}");
                }
            }
            if let Ok(dir) = std::env::var("UI_PREVIEW_DIR") {
                let img = painter.render(&p, Some(5));
                dump_ppm(&format!("{dir}/toolbar_{name}.ppm"), p.w, p.h, &img);
                let c = panel_chrome(1, false, "HYPRVR-1  1920x1080");
                let img = painter.render(&c, None);
                dump_ppm(&format!("{dir}/chrome.ppm"), c.w, c.h, &img);
            }
        }
    }

    #[test]
    fn env_picker_pages_and_picks() {
        let envs: Vec<String> = (0..20).map(|i| format!("env_{i:02}.jpg")).collect();
        let cur = Environment::Image("env_03.jpg".into());
        let p = env_picker(&envs, &cur, true, 0, &|n| n != "env_01.jpg");
        let cards: Vec<_> = p.widgets.iter().filter(|w| matches!(w.kind, Kind::EnvCard { .. })).collect();
        assert_eq!(cards.len(), PICKER_PER_PAGE);
        assert_eq!(cards[0].action, Some(Action::EnvPick(Environment::Passthrough)));
        assert!(matches!(cards[5].kind, Kind::EnvCard { active: true, .. }));
        assert!(p.h <= 760, "picker must fit the menu surface: {}", p.h);
        let rot = p.widgets.iter().find(|w| w.action == Some(Action::EnvRotate)).unwrap();
        assert_eq!(p.hit(rot.x + 5.0, rot.y + 5.0).unwrap().0, Action::EnvRotate);
        for w in &p.widgets {
            assert!(w.x >= 0.0 && w.x + w.w <= p.w as f32 && w.y + w.h <= p.h as f32, "{w:?}");
        }
        // Last page: the rest, and page index clamps.
        let last = env_picker(&envs, &cur, true, 99, &|_| true);
        let n = last.widgets.iter().filter(|w| matches!(w.kind, Kind::EnvCard { .. })).count();
        assert_eq!(n, 22 - PICKER_PER_PAGE);
        if let Ok(dir) = std::env::var("UI_PREVIEW_DIR") {
            let mut painter = Painter::new();
            // Fake previews: gradients.
            for (i, e) in envs.iter().enumerate() {
                let px: Vec<u8> = (0..64 * 32).flat_map(|j| [(j % 64 * 4) as u8, (i * 12) as u8, (j / 64 * 8) as u8, 255]).collect();
                painter.set_image(e, 64, 32, px);
            }
            let img = painter.render(&p, Some(8));
            dump_ppm(&format!("{dir}/picker.ppm"), p.w, p.h, &img);
        }
    }

    fn dump_ppm(path: &str, w: u32, h: u32, rgba: &[u8]) {
        // Composite over a mid grey so the translucent background is visible.
        let mut out = format!("P6 {w} {h} 255\n").into_bytes();
        for px in rgba.chunks_exact(4) {
            let a = px[3] as f32 / 255.0;
            for k in 0..3 {
                out.push((px[k] as f32 + 90.0 * (1.0 - a)) as u8);
            }
        }
        std::fs::write(path, out).unwrap();
    }
}
