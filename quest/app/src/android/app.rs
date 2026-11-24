//! The headset application: screens, UI, input handling and per-frame rendering.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use glam::{Mat4, Quat, Vec2, Vec3};
use glow::HasContext;
use hyprvr_proto::msg::{
    AudioTarget, ClientMsg, ClientStats, Curvature, Environment, PanelLayout, PointerButton, Rect, ScreenId, ServerState, Settings,
};
use hyprvr_quest_core::geom::{PanelGeom, Ray, hit_rect};
use hyprvr_quest_core::layout::{self, Grab, Head};
use hyprvr_quest_core::layout::{CornerResize, TwoHandScale, resize_handles, ray_point_distance, HANDLE_RADIUS};
use hyprvr_quest_core::menu::{self as menu_logic, MenuDrag};
use hyprvr_quest_core::touch::{self, PinchTouch};
use hyprvr_quest_core::aim::{AimFilter, ClickSlop};
use hyprvr_quest_core::misc::{self, PointerArbiter};
use hyprvr_quest_core::ui::{self, Action, Painter, ToolbarState, UiPanel};
use log::{info, warn};
use ndk::media::image_reader::{AcquireResult, Image};
use openxr as oxr;

use super::audio::{Jitter, Player};
use super::egl::{Egl, EglImage};
use super::gfx::{Blend, Gfx, PxRect, Vertex, projection};
use super::video::{ActiveVideo, AndroidVideo};
use super::xr::{HandInput, Layers, Passthrough, Sc, Session, pick_format};
use crate::net::{self, NetCmd, NetConfig, NetEvent, NetHandle, now_us};

const NEW_SCREEN: (u32, u32) = (1920, 1080);
const MAX_SCREENS: usize = 4;
const MAIN_UI: (u32, u32) = (1400, 760);
const STATS_UI: (u32, u32) = (1100, 420);
const CHROME_GAP: f32 = 0.015;
const SELECT_ON: f32 = 0.7;
const SELECT_OFF: f32 = 0.4;
// Hand tracking reports pinch strength well before the fingers touch and keeps it high after
// they part; only a (nearly) closed pinch counts, and opening it a bit releases.
const HAND_PINCH_ON: f32 = 0.99;
const HAND_PINCH_OFF: f32 = 0.8;
const DEADZONE: f32 = 0.2;
const SCROLL_NOTCHES_PER_S: f32 = 12.0;
const PUSH_M_PER_S: f32 = 1.5;
const GROW_PER_S: f32 = 0.8;
const RAY_LEN: f32 = 4.0;
const CANVAS_BG: [f32; 4] = [0.02, 0.02, 0.025, 1.0];
const VOID_COLOR: [f32; 4] = [0.015, 0.017, 0.025, 1.0];

pub struct Init<'a> {
    pub gl: glow::Context,
    pub egl: &'a Egl,
    pub session: Session,
    pub files: PathBuf,
    pub passthrough: Option<Passthrough>,
    pub eye_size: (u32, u32),
    pub cylinder: bool,
}

pub struct Frame<'a> {
    pub views: &'a [oxr::View],
    pub head_pos: Vec3,
    pub head_rot: Quat,
    pub head_valid: bool,
    pub hands: [HandInput; 2],
    pub world: &'a oxr::Space,
    pub view_space: &'a oxr::Space,
}

// ------------------------------------------------------------------ state types

struct VideoState {
    active: ActiveVideo,
    tex: glow::Texture,
    /// The image currently bound to `tex` (kept alive until replaced).
    held: Option<(Image, EglImage)>,
}

struct Screen {
    id: ScreenId,
    w: u32,
    h: u32,
    canvas: glow::Texture,
    sc: Sc,
    dirty: bool,
    video: Option<VideoState>,
    chrome: UiSurface,
}

struct UiSurface {
    sc: Sc,
    shown: Option<(UiPanel, Option<usize>)>,
    w_px: u32,
    h_px: u32,
}

impl UiSurface {
    fn new(session: &Session, format: u32, w: u32, h: u32) -> Result<Self> {
        Ok(UiSurface { sc: Sc::new(session, format, w, h)?, shown: None, w_px: 0, h_px: 0 })
    }

    fn panel(&self) -> Option<&UiPanel> {
        self.shown.as_ref().map(|s| &s.0)
    }

    fn set(&mut self, gfx: &Gfx, painter: &mut Painter, panel: UiPanel, hover: Option<usize>) -> Result<()> {
        if self.shown.as_ref().is_some_and(|(p, h)| *p == panel && *h == hover) {
            return Ok(());
        }
        let w = panel.w.min(self.sc.w);
        let h = panel.h.min(self.sc.h);
        let img = painter.render(&panel, hover);
        let stride = panel.w as usize * 4;
        let mut flipped = vec![0u8; (w * h * 4) as usize];
        for row in 0..h as usize {
            let src = &img[row * stride..row * stride + w as usize * 4];
            let dst_row = h as usize - 1 - row;
            flipped[dst_row * w as usize * 4..(dst_row + 1) * w as usize * 4].copy_from_slice(src);
        }
        let tex = self.sc.acquire()?;
        gfx.upload(tex, 0, 0, w, h, &flipped);
        self.sc.release();
        self.w_px = w;
        self.h_px = h;
        self.shown = Some((panel, hover));
        Ok(())
    }

    fn size_m(&self) -> (f32, f32) {
        (self.w_px as f32 / ui::PX_PER_M, self.h_px as f32 / ui::PX_PER_M)
    }

    /// Pixel coordinates in the panel for a hit at (u, v).
    fn to_px(&self, u: f32, v: f32) -> (f32, f32) {
        (u * self.w_px as f32, v * self.h_px as f32)
    }
}

struct CursorShape {
    tex: glow::Texture,
    w: u32,
    h: u32,
    hot: (u32, u32),
}

#[derive(Default)]
struct CursorState {
    seq: Option<u32>,
    screen: Option<ScreenId>,
    x: f32,
    y: f32,
    serial: u32,
}

#[derive(Clone, Debug, PartialEq)]
enum Conn {
    Searching(String),
    Pin { server: String, entered: String, message: String },
    Connected { server: String },
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Surface {
    Main,
    Chrome(ScreenId),
}

#[derive(Clone, Debug)]
enum Target {
    Panel { screen: ScreenId, x: f32, y: f32 },
    Ui { surface: Surface, x: f32, y: f32 },
    /// Corner resize handle of a panel.
    Resize { screen: ScreenId },
}

#[derive(Clone, Copy, Debug)]
enum Press {
    /// Button held on a panel; `slop.anchor` is where the button currently is.
    Panel { screen: ScreenId, button: PointerButton, slop: ClickSlop },
    Slider,
    Ui,
    Grab,
    Setup,
    /// Hand pinch on a panel: click / scroll / drag decided by `PinchTouch`.
    Touch { screen: ScreenId, touch: PinchTouch },
    Nothing,
}

#[derive(Default)]
struct HandState {
    inp: HandInput,
    select: bool,
    squeeze: bool,
    alt: bool,
    menu: bool,
    target: Option<(Target, f32)>,
    /// Target while the trigger was still at rest (before it tilted the controller).
    rest_target: Option<(Target, f32)>,
    filter: AimFilter,
    select_press: Option<Press>,
    alt_press: Option<Press>,
    scroll: Vec2,
    last_scroll: Option<Instant>,
}

struct GrabState {
    hand: usize,
    grab: Grab,
    /// Grab started with the trigger on the chrome handle (ends when the trigger is released).
    by_select: bool,
}

struct Stats {
    t0: Instant,
    frames: u32,
    lat_sum: f64,
    lat_n: u32,
    drawn: u64,
    last_decoded: u64,
    last_drawn: u64,
    last_rx: u64,
    lines: Vec<String>,
}

struct EnvState {
    sc: Sc,
    eye_w: u32,
    eye_h: u32,
    image: Option<(String, glow::Texture)>,
    requested: HashSet<String>,
}

pub struct App {
    session: Session,
    format: u32,
    files: PathBuf,
    gfx: Gfx,
    egl_images: EglFns,
    painter: Painter,
    net: NetHandle,
    video: Arc<AndroidVideo>,
    jitter: Arc<Jitter>,
    player: Player,
    passthrough: Option<Passthrough>,
    cylinder: bool,

    screens: HashMap<ScreenId, Screen>,
    state: Option<ServerState>,
    layout: Vec<PanelLayout>,
    settings: Settings,
    /// Screens seen in any State (persists across reconnects, so kept outputs aren't re-placed).
    known_screens: HashSet<ScreenId>,
    layout_initialized: bool,
    conn: Conn,

    shapes: HashMap<u32, CursorShape>,
    cursor: CursorState,
    arbiter: PointerArbiter,
    hands: [HandState; 2],
    primary: usize,
    grab: Option<GrabState>,
    /// Second hand scaling the grabbed panel.
    two_hand: Option<TwoHandScale>,
    /// Corner handle drag: (hand, by_select, resize).
    corner: Option<(usize, bool, CornerResize)>,

    main_ui: UiSurface,
    stats_ui: UiSurface,
    toolbar_visible: bool,
    ui_pose: Option<(Vec3, Quat)>,
    /// Menu being dragged by its handle.
    menu_drag: Option<MenuDrag>,
    /// Left palm currently faces the user (pinch toggles the menu).
    palm_armed: bool,
    last_menu_toggle: Option<Instant>,
    head: (Vec3, Quat),
    kb_setup: Option<Vec<Vec3>>,
    dim_dragging: bool,
    last_settings_send: Option<Instant>,
    settings_dirty: bool,

    env: EnvState,
    overlay: Sc,
    overlay_eye: (u32, u32),
    stats: Stats,
    start: Instant,
    last_frame: Instant,
}

/// Copyable EGL entry points (so `App` doesn't borrow `Egl`).
struct EglFns {
    egl: *const Egl,
}

impl EglFns {
    fn get(&self) -> &Egl {
        // SAFETY: `Egl` lives on the stack of `run()` for the whole lifetime of `App`.
        unsafe { &*self.egl }
    }
}

fn align_even(v: f32) -> u32 {
    ((v as u32) / 2 * 2).max(64)
}

impl App {
    pub fn new(init: Init<'_>) -> Result<Self> {
        let gfx = Gfx::new(init.gl)?;
        let session = init.session;
        let format = pick_format(&session);
        info!("swapchain format 0x{format:X}");
        let video = Arc::new(AndroidVideo::default());
        let jitter = Arc::new(Jitter::default());
        let player = Player::new(jitter.clone());
        let net = net::spawn(
            NetConfig { files_dir: init.files.clone(), device_name: "Meta Quest 3".into() },
            video.clone(),
            jitter.clone(),
        );
        let (ew, eh) = (align_even(init.eye_size.0 as f32 * 0.6), align_even(init.eye_size.1 as f32 * 0.6));
        let (ow, oh) = (align_even(init.eye_size.0 as f32 * 0.8), align_even(init.eye_size.1 as f32 * 0.8));
        let env = EnvState { sc: Sc::new(&session, format, ew * 2, eh)?, eye_w: ew, eye_h: eh, image: None, requested: HashSet::new() };
        let overlay = Sc::new(&session, format, ow * 2, oh)?;
        let main_ui = UiSurface::new(&session, format, MAIN_UI.0, MAIN_UI.1)?;
        let stats_ui = UiSurface::new(&session, format, STATS_UI.0, STATS_UI.1)?;
        let mut app = App {
            format,
            files: init.files,
            gfx,
            egl_images: EglFns { egl: init.egl as *const Egl },
            painter: Painter::new(),
            net,
            video,
            jitter,
            player,
            passthrough: init.passthrough,
            cylinder: init.cylinder,
            screens: HashMap::new(),
            state: None,
            layout: Vec::new(),
            settings: Settings::default(),
            known_screens: HashSet::new(),
            layout_initialized: false,
            conn: Conn::Searching("Looking for hyprvr on the network…".into()),
            shapes: HashMap::new(),
            cursor: CursorState::default(),
            arbiter: PointerArbiter::default(),
            hands: Default::default(),
            primary: 1,
            grab: None,
            two_hand: None,
            corner: None,
            main_ui,
            stats_ui,
            toolbar_visible: true,
            ui_pose: None,
            menu_drag: None,
            palm_armed: false,
            last_menu_toggle: None,
            head: (Vec3::new(0.0, 1.6, 0.0), Quat::IDENTITY),
            kb_setup: None,
            dim_dragging: false,
            last_settings_send: None,
            settings_dirty: false,
            env,
            overlay,
            overlay_eye: (ow, oh),
            stats: Stats {
                t0: Instant::now(),
                frames: 0,
                lat_sum: 0.0,
                lat_n: 0,
                drawn: 0,
                last_decoded: 0,
                last_drawn: 0,
                last_rx: 0,
                lines: Vec::new(),
            },
            start: Instant::now(),
            last_frame: Instant::now(),
            session,
        };
        app.player.pause();
        Ok(app)
    }

    fn now_s(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn send(&self, m: ClientMsg) {
        info!("-> {}", short_debug(&m));
        let _ = self.net.cmd.send(NetCmd::Send(m));
    }

    pub fn set_active(&mut self, active: bool) {
        if active {
            if self.state.as_ref().is_some_and(|s| s.audio == AudioTarget::Headset) {
                self.player.resume();
            }
        } else {
            self.player.pause();
        }
    }

    /// Session not running: keep draining network events so they don't pile up.
    pub fn idle(&mut self) {
        if let Err(e) = self.process_events() {
            warn!("events: {e:#}");
        }
        self.player.poll();
    }

    // -------------------------------------------------------------- network events

    fn process_events(&mut self) -> Result<()> {
        let mut budget = 400;
        while budget > 0 {
            let Ok(ev) = self.net.events.try_recv() else { break };
            budget -= 1;
            self.handle_event(ev)?;
        }
        Ok(())
    }

    fn handle_event(&mut self, ev: NetEvent) -> Result<()> {
        match ev {
            NetEvent::Status(s) => {
                info!("status: {s}");
                match &mut self.conn {
                    Conn::Searching(t) => *t = s,
                    Conn::Pin { message, .. } => *message = s,
                    Conn::Connected { .. } => {}
                }
            }
            NetEvent::NeedPin { server } => {
                info!("pairing needed with {server}");
                self.conn = Conn::Pin { server, entered: String::new(), message: "Enter the PIN shown by `vrd pin` on the PC".into() };
                self.toolbar_visible = true;
                self.ui_pose = None;
            }
            NetEvent::PairFailed(reason) => {
                warn!("pairing failed: {reason}");
                if let Conn::Pin { entered, message, .. } = &mut self.conn {
                    entered.clear();
                    *message = format!("Pairing failed: {reason}");
                }
            }
            NetEvent::Connected { server } => {
                info!("connected to {server}");
                self.conn = Conn::Connected { server };
                // The menu starts hidden once connected; left palm pinch / menu button opens it.
                self.toolbar_visible = false;
                self.menu_drag = None;
                self.ui_pose = None;
            }
            NetEvent::Disconnected(reason) => {
                warn!("disconnected: {reason}");
                if !matches!(self.conn, Conn::Pin { .. }) {
                    self.conn = Conn::Searching(format!("Disconnected: {reason}"));
                }
                self.state = None;
                self.grab = None;
                self.player.pause();
                self.ui_pose = None;
            }
            NetEvent::State(s) => self.on_state(s),
            NetEvent::CursorShape { serial, width, height, hot_x, hot_y, rgba } => {
                if width == 0 || height == 0 || rgba.len() < (width * height * 4) as usize {
                    return Ok(());
                }
                let tex = self.gfx.texture(width, height, true, false)?;
                self.gfx.upload(tex, 0, 0, width, height, &rgba);
                if let Some(old) = self.shapes.insert(serial, CursorShape { tex, w: width, h: height, hot: (hot_x, hot_y) }) {
                    self.gfx.delete_texture(old.tex);
                }
                if self.shapes.len() > 24 {
                    let mut keys: Vec<u32> = self.shapes.keys().copied().collect();
                    keys.sort_unstable();
                    for k in keys.into_iter().take(self.shapes.len() - 24) {
                        if k != self.cursor.serial
                            && let Some(s) = self.shapes.remove(&k)
                        {
                            self.gfx.delete_texture(s.tex);
                        }
                    }
                }
                self.mark_cursor_screen_dirty();
            }
            NetEvent::Cursor { seq, screen, x, y, serial } => {
                if let Some(last) = self.cursor.seq
                    && (seq.wrapping_sub(last) as i32) <= 0
                {
                    return Ok(());
                }
                let moved = self.cursor.screen != screen || self.cursor.x != x || self.cursor.y != y || self.cursor.serial != serial;
                if moved {
                    self.mark_cursor_screen_dirty();
                }
                self.cursor = CursorState { seq: Some(seq), screen, x, y, serial };
                if moved {
                    self.mark_cursor_screen_dirty();
                }
                let now = self.now_s();
                self.arbiter.on_server_cursor(screen, x, y, now);
            }
            NetEvent::ScreenOpen { screen, width, height } => {
                self.close_screen(screen);
                self.open_screen(screen, width, height)?;
            }
            NetEvent::ScreenClosed { screen } => self.close_screen(screen),
            NetEvent::Resized { screen, width, height } => {
                info!("screen {screen} resized to {width}x{height}");
                self.close_screen(screen);
                self.open_screen(screen, width, height)?;
            }
            NetEvent::Tiles { screen, capture_us, rects, data } => {
                let Some(s) = self.screens.get_mut(&screen) else { return Ok(()) };
                let mut off = 0usize;
                for r in &rects {
                    let n = (r.w * r.h * 4) as usize;
                    if off + n > data.len() {
                        break;
                    }
                    self.gfx.upload(s.canvas, r.x, r.y, r.w, r.h, &data[off..off + n]);
                    off += n;
                }
                s.dirty = true;
                self.record_latency(capture_us);
            }
            NetEvent::VideoStart { screen, generation, rect } => {
                let Some(active) = self.video.active(screen, generation) else {
                    warn!("screen {screen}: video gen {generation} not available (decoder failed?)");
                    return Ok(());
                };
                let tex = self.gfx.external_texture()?;
                if let Some(s) = self.screens.get_mut(&screen) {
                    let old = s.video.replace(VideoState { active, tex, held: None });
                    if let Some(old) = old {
                        drop_video(&self.gfx, self.egl_images.get(), old);
                    }
                    info!("screen {screen}: video region {rect:?} active");
                }
            }
            NetEvent::VideoStop { screen } => {
                if let Some(s) = self.screens.get_mut(&screen)
                    && let Some(v) = s.video.take()
                {
                    drop_video(&self.gfx, self.egl_images.get(), v);
                }
            }
            NetEvent::Environment { name, width, height, rgba } => {
                info!("environment {name}: {width}x{height}");
                let tex = self.gfx.texture(width, height, true, true)?;
                self.gfx.upload(tex, 0, 0, width, height, &rgba);
                self.gfx.generate_mipmaps(tex);
                unsafe {
                    self.gfx.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    self.gfx.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::REPEAT as i32);
                }
                if let Some((_, old)) = self.env.image.replace((name, tex)) {
                    self.gfx.delete_texture(old);
                }
            }
        }
        Ok(())
    }

    fn record_latency(&mut self, capture_us: u64) {
        if let Some(c) = self.net.shared.clock.lock().unwrap().to_client(capture_us) {
            let lat = now_us() as i64 - c as i64;
            if (0..5_000_000).contains(&lat) {
                self.stats.lat_sum += lat as f64 / 1000.0;
                self.stats.lat_n += 1;
            }
        }
    }

    fn mark_cursor_screen_dirty(&mut self) {
        if let Some(id) = self.cursor.screen
            && let Some(s) = self.screens.get_mut(&id)
        {
            s.dirty = true;
        }
    }

    fn open_screen(&mut self, id: ScreenId, w: u32, h: u32) -> Result<()> {
        info!("screen {id}: open {w}x{h}");
        let canvas = self.gfx.texture(w, h, true, false)?;
        if self.gfx.target(canvas, w, h) {
            self.gfx.clear(CANVAS_BG);
        }
        self.gfx.finish_target();
        let sc = Sc::new(&self.session, self.format, w, h)?;
        let chrome = UiSurface::new(&self.session, self.format, 900, 70)?;
        self.screens.insert(id, Screen { id, w, h, canvas, sc, dirty: true, video: None, chrome });
        let _ = self.net.cmd.send(NetCmd::Send(ClientMsg::RequestRefresh { screen: id }));
        Ok(())
    }

    fn close_screen(&mut self, id: ScreenId) {
        if let Some(mut s) = self.screens.remove(&id) {
            info!("screen {id}: closed");
            if let Some(v) = s.video.take() {
                drop_video(&self.gfx, self.egl_images.get(), v);
            }
            self.gfx.delete_texture(s.canvas);
        }
    }

    fn on_state(&mut self, s: ServerState) {
        info!(
            "state: {} screens, audio {:?}, env {:?}, dim {:.2}, cutout {}",
            s.screens.len(),
            s.audio,
            s.settings.environment,
            s.settings.dim,
            s.settings.keyboard_cutout.is_some()
        );
        let ids: Vec<ScreenId> = s.screens.iter().map(|x| x.id).collect();
        let head = self.head_struct();
        let mut layout = s.layout.clone();
        // Keep a panel that is being dragged where the hand has it.
        if let Some(g) = &self.grab
            && let Some(local) = self.layout.iter().find(|p| p.screen == g.grab.screen)
        {
            for p in layout.iter_mut() {
                if p.screen == g.grab.screen {
                    *p = *local;
                }
            }
        }
        let mut changed = false;
        if !self.layout_initialized {
            self.layout_initialized = true;
            let marker = self.files.join("layout_init");
            let unreasonable = layout.iter().any(|p| {
                let d = (Vec3::from_array(p.pose.position) - head.position).length();
                !(0.2..=8.0).contains(&d)
            });
            if !marker.exists() || unreasonable {
                info!("initial layout: arranging {} panels around the viewer", layout.len());
                let aspects: Vec<(ScreenId, f32)> =
                    s.screens.iter().map(|x| (x.id, x.width as f32 / x.height.max(1) as f32)).collect();
                layout = layout::default_arc(&aspects, head);
                let _ = std::fs::write(&marker, b"1");
                changed = true;
            }
        } else {
            // Screens that appeared since the last state get placed next to the others.
            for id in &ids {
                if !self.known_screens.contains(id) {
                    let others: Vec<PanelLayout> = layout.iter().filter(|p| p.screen != *id).copied().collect();
                    let placed = layout::place_new(&others, *id, head);
                    layout.retain(|p| p.screen != *id);
                    layout.push(placed);
                    changed = true;
                    info!("screen {id}: placed new panel");
                }
            }
        }
        self.known_screens.extend(ids.iter().copied());
        let reconciled = layout::reconcile(&layout, &ids, head);
        if reconciled != layout {
            changed = true;
        }
        self.layout = reconciled;
        if !self.dim_dragging && !self.settings_dirty {
            self.settings = s.settings.clone();
        }
        let headset_audio = s.audio == AudioTarget::Headset;
        let was_headset = self.state.as_ref().is_some_and(|o| o.audio == AudioTarget::Headset);
        if headset_audio && !was_headset {
            self.player.resume();
        } else if !headset_audio && was_headset {
            self.player.pause();
        }
        if let Environment::Image(name) = &s.settings.environment
            && self.env.image.as_ref().is_none_or(|(n, _)| n != name)
            && self.env.requested.insert(name.clone())
        {
            self.send(ClientMsg::GetEnvironment { name: name.clone() });
        }
        self.state = Some(s);
        if changed {
            self.send(ClientMsg::SetLayout(self.layout.clone()));
        }
    }

    fn head_struct(&self) -> Head {
        Head::from_pose(self.head.0, self.head.1)
    }

    // -------------------------------------------------------------- geometry helpers

    fn panel_geom(&self, p: &PanelLayout) -> Option<PanelGeom> {
        let s = self.screens.get(&p.screen)?;
        let mut g = PanelGeom::new(p, s.w, s.h);
        if !self.cylinder {
            g.curvature = Curvature::Flat;
        }
        Some(g)
    }

    fn chrome_pose(&self, g: &PanelGeom, chrome: &UiSurface) -> (Vec3, Quat) {
        let (_, ch) = chrome.size_m();
        let bottom = g.point_at(0.5, 1.0, 0.0);
        let center = bottom + g.rot * Vec3::new(0.0, -(CHROME_GAP + ch / 2.0), 0.0);
        (center, g.rot)
    }

    fn main_ui_visible(&self) -> bool {
        match self.conn {
            Conn::Connected { .. } => self.toolbar_visible || self.kb_setup.is_some(),
            _ => true,
        }
    }

    fn summon_ui(&mut self) {
        let (hp, hr) = self.head;
        self.ui_pose = Some(menu_logic::default_menu_pose(hp, hr));
    }

    /// Show/hide the main menu. Reopening keeps the place it was dragged to if that is still
    /// in front of the user, otherwise it comes back in front, below eye level.
    fn toggle_menu(&mut self) {
        if self.last_menu_toggle.is_some_and(|t| t.elapsed().as_millis() < 400) {
            return; // the system menu gesture and our palm pinch may both fire
        }
        self.last_menu_toggle = Some(Instant::now());
        if self.toolbar_visible {
            self.toolbar_visible = false;
            self.menu_drag = None;
            info!("menu hidden");
        } else {
            self.toolbar_visible = true;
            let (hp, hr) = self.head;
            if !self.ui_pose.is_some_and(|(p, _)| menu_logic::menu_pose_reusable(p, hp, hr)) {
                self.summon_ui();
            }
            info!("menu shown");
        }
    }

    /// Hit test the whole scene along a ray: nearest of main UI, chromes and panels.
    fn hit_scene(&self, ray: &Ray) -> Option<(Target, f32)> {
        let mut best: Option<(Target, f32)> = None;
        let mut consider = |t: Target, d: f32| {
            if best.as_ref().is_none_or(|(_, bd)| d < *bd) {
                best = Some((t, d));
            }
        };
        if self.main_ui_visible()
            && let Some((pos, rot)) = self.ui_pose
            && self.main_ui.shown.is_some()
        {
            let (w, h) = self.main_ui.size_m();
            if let Some(hit) = hit_rect(pos, rot, w, h, ray) {
                let (x, y) = self.main_ui.to_px(hit.u, hit.v);
                consider(Target::Ui { surface: Surface::Main, x, y }, hit.t);
            }
        }
        for p in &self.layout {
            let Some(g) = self.panel_geom(p) else { continue };
            let s = &self.screens[&p.screen];
            if let Some(hit) = g.hit(ray) {
                consider(Target::Panel { screen: p.screen, x: hit.u * s.w as f32, y: hit.v * s.h as f32 }, hit.t);
            }
            for h in resize_handles(&g) {
                if let Some((d, t)) = ray_point_distance(ray.origin, ray.dir, h)
                    && d < HANDLE_RADIUS
                {
                    // Slight preference so the handle wins over a panel right behind it.
                    consider(Target::Resize { screen: p.screen }, (t - 0.02).max(0.0));
                }
            }
            if s.chrome.shown.is_some() {
                let (cp, cr) = self.chrome_pose(&g, &s.chrome);
                let (w, h) = s.chrome.size_m();
                if let Some(hit) = hit_rect(cp, cr, w, h, ray) {
                    let (x, y) = s.chrome.to_px(hit.u, hit.v);
                    consider(Target::Ui { surface: Surface::Chrome(p.screen), x, y }, hit.t);
                }
            }
        }
        best
    }

    fn ui_hit(&self, surface: Surface, x: f32, y: f32) -> Option<(Action, f32, usize)> {
        let panel = match surface {
            Surface::Main => self.main_ui.panel()?,
            Surface::Chrome(id) => self.screens.get(&id)?.chrome.panel()?,
        };
        panel.hit(x, y)
    }

    // -------------------------------------------------------------- input

    fn handle_input(&mut self, f: &Frame<'_>, dt: f32) {
        let now = self.now_s();
        for i in 0..2 {
            let mut inp = f.hands[i];
            let h = &mut self.hands[i];
            // Controllers jitter; smooth the aim (hands are already smooth enough).
            if inp.active && !inp.is_hand {
                (inp.aim_pos, inp.aim_rot) = h.filter.update(inp.aim_pos, inp.aim_rot, dt);
            } else {
                h.filter.reset();
            }
            h.inp = inp;
            let (sel_on, sel_off, sq_on, sq_off) = if inp.is_hand {
                (HAND_PINCH_ON, HAND_PINCH_OFF, SELECT_ON, SELECT_OFF)
            } else {
                (SELECT_ON, SELECT_OFF, SELECT_ON, SELECT_OFF)
            };
            let sel = if h.select { inp.select > sel_off } else { inp.select >= sel_on };
            // Hand "grasp" fires on a relaxed/curled hand, so hands never grab by grasping;
            // with hands, panels move only via their bar (pinch).
            let sq = !inp.is_hand && if h.squeeze { inp.squeeze > sq_off } else { inp.squeeze >= sq_on };
            let sel_edge = (sel != h.select).then_some(sel);
            let sq_edge = (sq != h.squeeze).then_some(sq);
            let alt_edge = (inp.alt != h.alt).then_some(inp.alt);
            let menu_edge = (inp.menu != h.menu).then_some(inp.menu);
            h.select = sel;
            h.squeeze = sq;
            h.alt = inp.alt;
            h.menu = inp.menu;
            let ray = Ray::new(inp.aim_pos, inp.aim_rot * Vec3::NEG_Z);
            let target = if inp.active { self.hit_scene(&ray) } else { None };
            self.hands[i].target = target.clone();
            if !self.hands[i].select && inp.select < 0.15 {
                self.hands[i].rest_target = target.clone();
            }

            if menu_edge == Some(true) && matches!(self.conn, Conn::Connected { .. }) {
                self.toggle_menu();
            }
            // Left palm turned toward the face + pinch toggles the menu (hands only).
            if i == 0 {
                let armed = inp.is_hand
                    && inp.grip_valid
                    && matches!(self.conn, Conn::Connected { .. })
                    && menu_logic::palm_facing_head(inp.grip_pos, inp.grip_rot, true, self.head.0);
                self.palm_armed = armed;
                if armed {
                    if sel_edge == Some(true) {
                        self.toggle_menu();
                        self.hands[i].select_press = Some(Press::Nothing);
                    }
                    self.hands[i].target = None;
                    continue;
                }
            }
            if sel_edge == Some(true) || sq_edge == Some(true) {
                self.primary = i;
            }

            // Second hand pinching/grabbing while a panel is held: scale with both hands.
            if self.grab.as_ref().is_some_and(|g| g.hand != i)
                && self.two_hand.is_none()
                && (sel_edge == Some(true) || sq_edge == Some(true))
                && let Some(g) = &self.grab
            {
                let by_select = sel_edge == Some(true);
                let a = self.hands[g.hand].inp.aim_pos;
                self.two_hand = Some(TwoHandScale::start(i, by_select, a, inp.aim_pos, g.grab.width_m));
                if by_select {
                    self.hands[i].select_press = Some(Press::Grab);
                }
                info!("two-hand scale started");
                continue;
            }
            if sq_edge == Some(false) && self.two_hand.is_some_and(|t| t.other_hand == i && !t.by_select) {
                self.two_hand = None;
            }
            // Squeeze on a corner handle resizes.
            if sq_edge == Some(true)
                && self.corner.is_none()
                && let Some((Target::Resize { screen }, _)) = &target
            {
                self.start_corner(i, *screen, false);
            }
            if sq_edge == Some(false) && self.corner.is_some_and(|c| c.0 == i && !c.1) {
                self.end_corner();
            }
            // Squeeze on the menu drags it.
            if sq_edge == Some(true)
                && self.grab.is_none()
                && self.menu_drag.is_none()
                && let Some((Target::Ui { surface: Surface::Main, .. }, t)) = &target
            {
                self.start_menu_drag(i, false, *t);
            }
            if sq_edge == Some(false) && self.menu_drag.is_some_and(|d| d.hand == i && !d.by_select) {
                self.menu_drag = None;
            }
            // Grab with squeeze on a panel or its chrome.
            if sq_edge == Some(true) && self.grab.is_none() && self.menu_drag.is_none() && self.corner.is_none() {
                let screen = match &target {
                    Some((Target::Panel { screen, .. }, _)) => Some(*screen),
                    Some((Target::Ui { surface: Surface::Chrome(id), .. }, _)) => Some(*id),
                    _ => None,
                };
                if let Some(id) = screen {
                    self.start_grab(i, id, false);
                }
            }
            if sq_edge == Some(false) && self.grab.as_ref().is_some_and(|g| g.hand == i && !g.by_select) {
                self.end_grab();
            }

            if sel_edge == Some(true) {
                let press = self.on_select_press(i, target.as_ref().map(|t| &t.0));
                self.hands[i].select_press = Some(press);
            }
            if sel_edge == Some(false)
                && let Some(press) = self.hands[i].select_press.take()
            {
                self.on_release(i, press);
            }
            if alt_edge == Some(true) {
                if let Some((Target::Panel { screen, x, y }, _)) = target {
                    self.primary = i;
                    self.arbiter.on_press();
                    self.send(ClientMsg::Button { screen, x, y, button: PointerButton::Right, pressed: true });
                    let slop = ClickSlop::new(x, y, self.screen_h(screen));
                    self.hands[i].alt_press = Some(Press::Panel { screen, button: PointerButton::Right, slop });
                }
            }
            if alt_edge == Some(false)
                && let Some(press) = self.hands[i].alt_press.take()
            {
                self.on_release(i, press);
            }

            // Dragging the dim slider.
            if matches!(self.hands[i].select_press, Some(Press::Slider))
                && let Some((Target::Ui { surface, x, y }, _)) = &self.hands[i].target
                && let Some((Action::Dim, v, _)) = self.ui_hit(*surface, *x, *y)
            {
                self.set_dim(v);
            }

            // Hand pinch on a panel: click / scroll / drag.
            if let Some(Press::Touch { screen, mut touch }) = self.hands[i].select_press {
                let pos = match &self.hands[i].target {
                    Some((Target::Panel { screen: s2, x, y }, _)) if *s2 == screen => Some((*x, *y)),
                    _ => None,
                };
                match touch.update(pos, dt) {
                    touch::Out::Press { x, y } => {
                        let _ = self.net.cmd.send(NetCmd::PointerMove { screen, x, y });
                        self.send(ClientMsg::Button { screen, x, y, button: PointerButton::Left, pressed: true });
                        let (x, y) = pos.unwrap_or((x, y));
                        // Now a plain held button that follows the ray.
                        self.hands[i].select_press =
                            Some(Press::Panel { screen, button: PointerButton::Left, slop: ClickSlop::moving(x, y) });
                    }
                    touch::Out::Scroll { dx, dy } => {
                        self.hands[i].scroll += Vec2::new(dx, dy);
                        self.hands[i].select_press = Some(Press::Touch { screen, touch });
                    }
                    touch::Out::None => self.hands[i].select_press = Some(Press::Touch { screen, touch }),
                }
            }

            // Track the pressed panel position while held (pinned until it clearly moves).
            if let Some((Target::Panel { screen: s2, x, y }, _)) = self.hands[i].target.clone() {
                let h = &mut self.hands[i];
                for press in [&mut h.select_press, &mut h.alt_press] {
                    if let Some(Press::Panel { screen, slop, .. }) = press
                        && *screen == s2
                    {
                        slop.update(x, y);
                    }
                }
            }

            // Thumbstick scroll over a panel (when not grabbing with this hand).
            let grabbing = self.grab.as_ref().is_some_and(|g| g.hand == i);
            if !grabbing
                && let Some((Target::Panel { screen, .. }, _)) = &self.hands[i].target
            {
                let screen = *screen;
                let st = inp.stick;
                let h = &mut self.hands[i];
                if !inp.is_hand && st.length() > DEADZONE {
                    h.scroll += Vec2::new(st.x, -st.y) * SCROLL_NOTCHES_PER_S * dt;
                }
                let due = h.last_scroll.is_none_or(|t| t.elapsed().as_secs_f32() > 0.033);
                if due && (h.scroll.x.abs() > 0.05 || h.scroll.y.abs() > 0.05) {
                    let d = h.scroll;
                    h.scroll = Vec2::ZERO;
                    h.last_scroll = Some(Instant::now());
                    let _ = self.net.cmd.send(NetCmd::Send(ClientMsg::Scroll { screen, dx: d.x, dy: d.y }));
                }
            } else if !matches!(self.hands[i].select_press, Some(Press::Touch { .. })) {
                self.hands[i].scroll = Vec2::ZERO;
            }
        }

        // Grab update.
        if let Some(g) = &mut self.grab {
            let h = &self.hands[g.hand];
            let st = h.inp.stick;
            let push = if st.y.abs() > DEADZONE { st.y * PUSH_M_PER_S * dt } else { 0.0 };
            let grow = if st.x.abs() > DEADZONE { st.x * GROW_PER_S * dt } else { 0.0 };
            if let Some(t) = &self.two_hand {
                g.grab.width_m = t.width(h.inp.aim_pos, self.hands[t.other_hand].inp.aim_pos);
            }
            if let Some(p) = self.layout.iter_mut().find(|p| p.screen == g.grab.screen) {
                g.grab.update(p, h.inp.aim_pos, h.inp.aim_rot, self.head.0, push, grow);
            }
        }

        if self.grab.is_none() {
            self.two_hand = None;
        }
        if let Some((hand, _, r)) = &self.corner {
            let inp = &self.hands[*hand].inp;
            if let Some(w) = r.width(inp.aim_pos, inp.aim_rot * Vec3::NEG_Z)
                && let Some(p) = self.layout.iter_mut().find(|p| p.screen == r.screen)
            {
                p.width_m = w;
            }
        }

        if let Some(d) = &mut self.menu_drag {
            let inp = &self.hands[d.hand].inp;
            let st = inp.stick;
            let push = if st.y.abs() > DEADZONE { st.y * PUSH_M_PER_S * dt } else { 0.0 };
            self.ui_pose = Some(d.update(inp.aim_pos, inp.aim_rot * Vec3::NEG_Z, self.head.0, push));
        }

        // Laser drives the desktop pointer (primary hand only).
        let touching = matches!(self.hands[self.primary].select_press, Some(Press::Touch { .. }));
        let pointer_busy =
            self.grab.is_some() || self.menu_drag.is_some() || self.corner.is_some() || self.kb_setup.is_some() || touching;
        // Without hover the pointer only follows the laser while a button is held.
        let held = |p: &Option<Press>| match p {
            Some(Press::Panel { screen, slop, .. }) => Some((*screen, slop.anchor.0, slop.anchor.1)),
            _ => None,
        };
        let pressed = held(&self.hands[self.primary].select_press).or(held(&self.hands[self.primary].alt_press));
        let hit = match (&self.hands[self.primary].target, pointer_busy, pressed) {
            (_, false, Some(p)) => Some(p),
            (Some((Target::Panel { screen, x, y }, _)), false, None) if self.settings.laser_hover => Some((*screen, *x, *y)),
            _ => None,
        };
        if let Some((s, x, y)) = self.arbiter.on_laser(hit, now) {
            let _ = self.net.cmd.send(NetCmd::PointerMove { screen: s, x, y });
        }

        // Throttled settings while dragging the dim slider.
        if self.settings_dirty && self.last_settings_send.is_none_or(|t| t.elapsed().as_millis() >= 100) {
            self.push_settings();
        }
    }

    fn on_select_press(&mut self, hand: usize, target: Option<&Target>) -> Press {
        if let Some(pts) = &mut self.kb_setup {
            let p = self.hands[hand].inp.aim_pos;
            pts.push(p);
            info!("keyboard corner {} at {p:?}", pts.len());
            if pts.len() >= 2 {
                let c = misc::cutout_from_corners(pts[0], pts[1], self.head.0);
                self.kb_setup = None;
                self.settings.keyboard_cutout = Some(c);
                self.push_settings();
            }
            return Press::Setup;
        }
        match target {
            Some(Target::Resize { screen }) => {
                self.start_corner(hand, *screen, true);
                Press::Grab
            }
            Some(Target::Panel { screen, x, y }) if self.hands[hand].inp.is_hand => {
                // Decided on movement/hold; the pointer stays where the pinch started.
                self.arbiter.on_press();
                let _ = self.net.cmd.send(NetCmd::PointerMove { screen: *screen, x: *x, y: *y });
                let h = self.screen_h(*screen);
                Press::Touch { screen: *screen, touch: PinchTouch::new(*x, *y, h) }
            }
            Some(Target::Panel { screen, x, y }) => {
                // Pulling the trigger tilts the controller: click where it pointed at rest.
                let (x, y) = match &self.hands[hand].rest_target {
                    Some((Target::Panel { screen: s2, x: rx, y: ry }, _))
                        if s2 == screen && (rx - x).hypot(ry - y) < self.screen_h(*screen) as f32 * 0.05 =>
                    {
                        (*rx, *ry)
                    }
                    _ => (*x, *y),
                };
                let screen = *screen;
                self.arbiter.on_press();
                let _ = self.net.cmd.send(NetCmd::PointerMove { screen, x, y });
                self.send(ClientMsg::Button { screen, x, y, button: PointerButton::Left, pressed: true });
                Press::Panel { screen, button: PointerButton::Left, slop: ClickSlop::new(x, y, self.screen_h(screen)) }
            }
            Some(Target::Ui { surface, x, y }) => match self.ui_hit(*surface, *x, *y) {
                Some((Action::Dim, v, _)) => {
                    self.dim_dragging = true;
                    self.set_dim(v);
                    Press::Slider
                }
                Some((Action::PanelGrab(id), _, _)) => {
                    self.start_grab(hand, id, true);
                    Press::Grab
                }
                Some((Action::MenuDrag, _, _)) => {
                    let t = self.hands[hand].target.as_ref().map_or(0.5, |(_, t)| *t);
                    self.start_menu_drag(hand, true, t);
                    Press::Grab
                }
                Some((a, _, _)) => {
                    self.do_action(a);
                    Press::Ui
                }
                None => Press::Nothing,
            },
            _ => Press::Nothing,
        }
    }

    fn screen_h(&self, screen: ScreenId) -> u32 {
        self.screens.get(&screen).map_or(1080, |s| s.h)
    }

    fn on_release(&mut self, hand: usize, press: Press) {
        match press {
            Press::Panel { screen, button, slop } => {
                let (x, y) = slop.anchor;
                self.send(ClientMsg::Button { screen, x, y, button, pressed: false });
            }
            Press::Slider => {
                self.dim_dragging = false;
                self.push_settings();
            }
            Press::Grab => {
                if self.grab.as_ref().is_some_and(|g| g.hand == hand && g.by_select) {
                    self.end_grab();
                }
                if self.menu_drag.is_some_and(|d| d.hand == hand && d.by_select) {
                    self.menu_drag = None;
                }
                if self.two_hand.is_some_and(|t| t.other_hand == hand && t.by_select) {
                    self.two_hand = None;
                }
                if self.corner.is_some_and(|c| c.0 == hand && c.1) {
                    self.end_corner();
                }
            }
            Press::Touch { screen, touch } => {
                if touch.is_click() {
                    let (x, y) = touch.start;
                    self.send(ClientMsg::Button { screen, x, y, button: PointerButton::Left, pressed: true });
                    self.send(ClientMsg::Button { screen, x, y, button: PointerButton::Left, pressed: false });
                }
            }
            Press::Ui | Press::Setup | Press::Nothing => {}
        }
    }

    fn start_corner(&mut self, hand: usize, screen: ScreenId, by_select: bool) {
        let (Some(p), Some(s)) = (self.layout.iter().find(|p| p.screen == screen), self.screens.get(&screen)) else { return };
        let aspect = s.w.max(1) as f32 / s.h.max(1) as f32;
        let inp = &self.hands[hand].inp;
        if let Some(r) = CornerResize::start(p, aspect, inp.aim_pos, inp.aim_rot * Vec3::NEG_Z) {
            info!("resize screen {screen}");
            self.corner = Some((hand, by_select, r));
        }
    }

    fn end_corner(&mut self) {
        if self.corner.take().is_some() {
            self.send(ClientMsg::SetLayout(self.layout.clone()));
        }
    }

    fn start_menu_drag(&mut self, hand: usize, by_select: bool, hit_t: f32) {
        let Some((pos, rot)) = self.ui_pose else { return };
        let inp = &self.hands[hand].inp;
        self.menu_drag = Some(MenuDrag::start(hand, by_select, inp.aim_pos, inp.aim_rot * Vec3::NEG_Z, hit_t, pos, rot));
    }

    fn start_grab(&mut self, hand: usize, screen: ScreenId, by_select: bool) {
        let Some(p) = self.layout.iter().find(|p| p.screen == screen) else { return };
        let h = &self.hands[hand].inp;
        info!("grab screen {screen} with hand {hand}");
        self.grab = Some(GrabState { hand, grab: Grab::start(p, h.aim_pos, h.aim_rot), by_select });
    }

    fn end_grab(&mut self) {
        self.two_hand = None;
        if let Some(g) = self.grab.take() {
            info!("released screen {}", g.grab.screen);
            self.send(ClientMsg::SetLayout(self.layout.clone()));
        }
    }

    fn set_dim(&mut self, v: f32) {
        if (self.settings.dim - v).abs() > 0.002 {
            self.settings.dim = v;
            self.settings_dirty = true;
        }
    }

    fn push_settings(&mut self) {
        self.settings_dirty = false;
        self.last_settings_send = Some(Instant::now());
        self.send(ClientMsg::SetSettings(self.settings.clone()));
    }

    fn do_action(&mut self, a: Action) {
        info!("ui action {a:?}");
        match a {
            Action::AddScreen => {
                if self.screens.len() < MAX_SCREENS {
                    self.send(ClientMsg::AddScreen { width: NEW_SCREEN.0, height: NEW_SCREEN.1 });
                }
            }
            Action::ToggleAudio => {
                let cur = self.state.as_ref().map(|s| s.audio).unwrap_or(AudioTarget::Laptop);
                let next = if cur == AudioTarget::Headset { AudioTarget::Laptop } else { AudioTarget::Headset };
                self.send(ClientMsg::SetAudio(next));
            }
            Action::Dim => {}
            Action::NextEnv => {
                let envs = self.state.as_ref().map(|s| s.environments.clone()).unwrap_or_default();
                let mut cycle = vec![Environment::Passthrough, Environment::Void];
                cycle.extend(envs.into_iter().map(Environment::Image));
                if self.passthrough.is_none() {
                    cycle.retain(|e| *e != Environment::Passthrough);
                }
                let idx = cycle.iter().position(|e| *e == self.settings.environment).map_or(0, |i| (i + 1) % cycle.len());
                self.settings.environment = cycle[idx].clone();
                if let Environment::Image(name) = &self.settings.environment
                    && self.env.image.as_ref().is_none_or(|(n, _)| n != name)
                {
                    self.env.requested.insert(name.clone());
                    self.send(ClientMsg::GetEnvironment { name: name.clone() });
                }
                self.push_settings();
            }
            Action::KeyboardSetup => {
                self.kb_setup = Some(Vec::new());
            }
            Action::KeyboardToggle => {
                if let Some(c) = &mut self.settings.keyboard_cutout {
                    c.enabled = !c.enabled;
                    self.push_settings();
                }
            }
            Action::Recenter => {
                let head = self.head_struct();
                self.layout = layout::reset(&self.layout, head);
                self.summon_ui();
                self.send(ClientMsg::SetLayout(self.layout.clone()));
            }
            Action::ToggleHover => {
                self.settings.laser_hover = !self.settings.laser_hover;
                self.push_settings();
            }
            Action::ToggleStats => {
                self.settings.show_stats = !self.settings.show_stats;
                self.push_settings();
            }
            Action::Digit(d) => {
                if let Conn::Pin { entered, message, .. } = &mut self.conn
                    && entered.len() < 6
                {
                    entered.push(char::from(b'0' + d));
                    if entered.len() == 6 {
                        *message = "Verifying…".into();
                        let _ = self.net.cmd.send(NetCmd::Pin(entered.clone()));
                    }
                }
            }
            Action::Backspace => {
                if let Conn::Pin { entered, .. } = &mut self.conn {
                    entered.pop();
                }
            }
            Action::ForgetServer => {
                let _ = self.net.cmd.send(NetCmd::ForgetServer);
                self.conn = Conn::Searching("Forgot the paired PC; searching…".into());
            }
            Action::PanelGrab(_) | Action::MenuDrag => {}
            Action::MenuClose => {
                self.toolbar_visible = false;
                self.menu_drag = None;
                self.last_menu_toggle = Some(Instant::now());
            }
            Action::PanelCurve(id) => {
                let hp = self.head.0;
                if let Some(p) = self.layout.iter_mut().find(|p| p.screen == id) {
                    layout::toggle_curve(p, hp);
                }
                self.send(ClientMsg::SetLayout(self.layout.clone()));
            }
            Action::PanelClose(id) => self.send(ClientMsg::RemoveScreen { id }),
        }
    }

    // -------------------------------------------------------------- frame

    pub fn frame(&mut self, f: &Frame<'_>, layers: &mut Layers) -> Result<()> {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32().clamp(0.0, 0.1);
        self.last_frame = now;
        if f.head_valid {
            self.head = (f.head_pos, f.head_rot);
        }
        if self.ui_pose.is_none() && f.head_valid {
            self.summon_ui();
        }
        self.player.poll();

        // 1. New decoded video goes into the canvases first, then queued tiles/stops.
        self.draw_video();
        self.process_events()?;

        // 2. Input.
        self.handle_input(f, dt);

        // 3. UI surfaces.
        self.update_ui()?;

        // 4. Panels (canvas + cursor -> swapchain).
        self.blit_panels()?;

        // 5. Stats.
        self.update_stats()?;

        // 6. Compose layers.
        let env_mode = self.effective_env();
        let cutout = self.settings.keyboard_cutout.filter(|c| c.enabled);
        let want_passthrough = self.passthrough.is_some() && (env_mode == EnvMode::Passthrough || cutout.is_some());
        if want_passthrough && let Some(pt) = &self.passthrough {
            layers.passthrough(pt.layer, f.world);
        }
        self.render_env(f, env_mode, cutout.as_ref())?;
        layers.projection(f.world, f.views, &self.env.sc, self.env.eye_w as i32, self.env.eye_h as i32, want_passthrough);

        for p in &self.layout {
            let Some(g) = self.panel_geom(p) else { continue };
            let s = &self.screens[&p.screen];
            if !s.sc.released_once {
                continue;
            }
            match g.cylinder_params() {
                Some((axis, r, angle, aspect)) => layers.cylinder(f.world, s.sc.full(), axis, g.rot, r, angle, aspect),
                None => layers.quad(f.world, s.sc.full(), g.center, g.rot, g.width, g.height, false, true),
            }
            if s.chrome.sc.released_once && self.chrome_visible(p.screen) {
                let (cp, cr) = self.chrome_pose(&g, &s.chrome);
                let (w, h) = s.chrome.size_m();
                layers.quad(f.world, s.chrome.sc.sub_image(0, 0, s.chrome.w_px as i32, s.chrome.h_px as i32), cp, cr, w, h, true, false);
            }
        }
        if self.main_ui_visible()
            && let Some((pos, rot)) = self.ui_pose
            && self.main_ui.sc.released_once
        {
            let (w, h) = self.main_ui.size_m();
            layers.quad(f.world, self.main_ui.sc.sub_image(0, 0, self.main_ui.w_px as i32, self.main_ui.h_px as i32), pos, rot, w, h, true, false);
        }
        if self.settings.show_stats && self.stats_ui.sc.released_once {
            let (w, h) = self.stats_ui.size_m();
            let pos = Vec3::new(0.0, -0.32, -1.1);
            let rot = Quat::from_rotation_x(0.35);
            layers.quad(f.view_space, self.stats_ui.sc.sub_image(0, 0, self.stats_ui.w_px as i32, self.stats_ui.h_px as i32), pos, rot, w, h, true, false);
        }
        if self.render_overlay(f)? {
            layers.projection(f.world, f.views, &self.overlay, self.overlay_eye.0 as i32, self.overlay_eye.1 as i32, true);
        }
        self.stats.frames += 1;
        Ok(())
    }

    fn chrome_visible(&self, id: ScreenId) -> bool {
        // Chromes show while the toolbar is open, while grabbing, or when a ray points near the panel.
        if self.toolbar_visible || self.grab.is_some() {
            return true;
        }
        self.hands.iter().any(|h| match &h.target {
            Some((Target::Panel { screen, .. }, _)) => *screen == id,
            Some((Target::Ui { surface: Surface::Chrome(s), .. }, _)) => *s == id,
            Some((Target::Resize { screen }, _)) => *screen == id,
            _ => false,
        })
    }

    fn draw_video(&mut self) {
        let egl = self.egl_images.get();
        let mut lat = Vec::new();
        for s in self.screens.values_mut() {
            let Some(v) = &mut s.video else { continue };
            let img = match v.active.reader.0.acquire_latest_image() {
                Ok(AcquireResult::Image(img)) => img,
                Ok(_) => continue,
                Err(e) => {
                    warn!("screen {}: acquire image: {e:?}", s.id);
                    continue;
                }
            };
            let Ok(hb) = img.hardware_buffer() else { continue };
            let desc = hb.describe();
            let Some(eimg) = egl.image_from_hardware_buffer(hb.as_ptr() as *mut _) else {
                warn!("screen {}: eglCreateImageKHR failed", s.id);
                continue;
            };
            self.gfx.bind_external(v.tex);
            egl.bind_external(eimg);
            if let Some((_, old)) = v.held.replace((img, eimg)) {
                egl.destroy_image(old);
            }
            let img = &v.held.as_ref().unwrap().0;
            let rect: Rect = v.active.rect;
            let (cx, cy) = match img.crop_rect() {
                Ok(c) if c.right > c.left && c.bottom > c.top => (c.left as f32, c.top as f32),
                _ => (0.0, 0.0),
            };
            let (bw, bh) = (desc.width.max(1) as f32, desc.height.max(1) as f32);
            let uv = [cx / bw, cy / bh, (cx + rect.w as f32) / bw, (cy + rect.h as f32) / bh];
            if self.gfx.target(s.canvas, s.w, s.h) {
                self.gfx.blend(Blend::None);
                self.gfx.draw_tex(
                    v.tex,
                    true,
                    uv,
                    PxRect { x: rect.x as f32, y: rect.y as f32, w: rect.w.min(s.w - rect.x.min(s.w)) as f32, h: rect.h.min(s.h - rect.y.min(s.h)) as f32 },
                    s.w,
                    s.h,
                    false,
                );
            }
            self.gfx.finish_target();
            s.dirty = true;
            self.stats.drawn += 1;
            if let Ok(ts) = img.timestamp() {
                lat.push(ts as u64 / 1000);
            }
        }
        for t in lat {
            self.record_latency(t);
        }
    }

    fn blit_panels(&mut self) -> Result<()> {
        let cursor = &self.cursor;
        let shape = self.shapes.get(&cursor.serial);
        for s in self.screens.values_mut() {
            if !s.dirty {
                continue;
            }
            s.dirty = false;
            let tex = s.sc.acquire()?;
            if self.gfx.target(tex, s.w, s.h) {
                self.gfx.blend(Blend::None);
                self.gfx.draw_tex(s.canvas, false, [0.0, 0.0, 1.0, 1.0], PxRect { x: 0.0, y: 0.0, w: s.w as f32, h: s.h as f32 }, s.w, s.h, true);
                if cursor.screen == Some(s.id)
                    && let Some(cs) = shape
                {
                    self.gfx.blend(Blend::Straight);
                    let dst = PxRect { x: cursor.x - cs.hot.0 as f32, y: cursor.y - cs.hot.1 as f32, w: cs.w as f32, h: cs.h as f32 };
                    self.gfx.draw_tex(cs.tex, false, [0.0, 0.0, 1.0, 1.0], dst, s.w, s.h, true);
                    self.gfx.blend(Blend::None);
                }
            } else {
                warn!("screen {}: swapchain framebuffer incomplete", s.id);
            }
            self.gfx.finish_target();
            s.sc.release();
        }
        Ok(())
    }

    fn update_ui(&mut self) -> Result<()> {
        // Hover index per surface (from either hand).
        let mut hover: HashMap<SurfaceKey, usize> = HashMap::new();
        for h in &self.hands {
            if let Some((Target::Ui { surface, x, y }, _)) = &h.target
                && let Some((_, _, idx)) = self.ui_hit(*surface, *x, *y)
            {
                hover.insert(SurfaceKey::from(*surface), idx);
            }
        }
        let main = self.main_panel();
        let main_hover = hover.get(&SurfaceKey::Main).copied();
        self.main_ui.set(&self.gfx, &mut self.painter, main, main_hover)?;

        let labels: HashMap<ScreenId, String> = self
            .state
            .as_ref()
            .map(|s| s.screens.iter().map(|x| (x.id, format!("{}  {}x{}", x.output, x.width, x.height))).collect())
            .unwrap_or_default();
        let curved: HashMap<ScreenId, bool> =
            self.layout.iter().map(|p| (p.screen, matches!(p.curvature, Curvature::Curved { .. }))).collect();
        for s in self.screens.values_mut() {
            let label = labels.get(&s.id).cloned().unwrap_or_else(|| format!("Screen {}", s.id));
            let panel = ui::panel_chrome(s.id, curved.get(&s.id).copied().unwrap_or(false), &label);
            let hv = hover.get(&SurfaceKey::Chrome(s.id)).copied();
            s.chrome.set(&self.gfx, &mut self.painter, panel, hv)?;
        }
        if self.settings.show_stats && !self.stats.lines.is_empty() {
            let panel = ui::card(&self.stats.lines);
            self.stats_ui.set(&self.gfx, &mut self.painter, panel, None)?;
        }
        Ok(())
    }

    fn main_panel(&self) -> UiPanel {
        if let Some(pts) = &self.kb_setup {
            let which = if pts.is_empty() { "FRONT-LEFT" } else { "FRONT-RIGHT" };
            return ui::card(&[
                "Keyboard cutout setup".into(),
                format!("Touch the {which} corner of the keyboard"),
                "with the tip of a controller and pull the trigger.".into(),
            ]);
        }
        match &self.conn {
            Conn::Searching(status) => ui::card(&[
                "hyprvr".into(),
                status.clone(),
                "Start the daemon on the PC: systemctl --user start vrd".into(),
                "Optional: put host = \"<pc ip>\" in files/config.toml".into(),
            ]),
            Conn::Pin { server, entered, message } => ui::pin_pad(entered, server, message),
            Conn::Connected { server } => {
                let st = self.state.as_ref();
                let env_name = match &self.settings.environment {
                    Environment::Passthrough => "Passthrough".to_string(),
                    Environment::Void => "Void".to_string(),
                    Environment::Image(n) => n.clone(),
                };
                let n = self.screens.len();
                let ts = ToolbarState {
                    audio_headset: st.is_some_and(|s| s.audio == AudioTarget::Headset),
                    dim: self.settings.dim,
                    env_name,
                    stats_on: self.settings.show_stats,
                    hover_on: self.settings.laser_hover,
                    cutout_on: self.settings.keyboard_cutout.is_some_and(|c| c.enabled),
                    cutout_exists: self.settings.keyboard_cutout.is_some(),
                    can_add: n < MAX_SCREENS,
                    info: vec![format!("{server}  ·  {n} screen{}", if n == 1 { "" } else { "s" })],
                };
                ui::toolbar(&ts)
            }
        }
    }

    fn update_stats(&mut self) -> Result<()> {
        let el = self.stats.t0.elapsed().as_secs_f32();
        if el < 1.0 {
            return Ok(());
        }
        let st = &mut self.stats;
        let decoded = self.video.decoded.load(Ordering::Relaxed);
        let d_dec = decoded - st.last_decoded;
        let d_drawn = st.drawn - st.last_drawn;
        let rx = self.net.shared.rx_bytes.load(Ordering::Relaxed);
        let mbit = (rx - st.last_rx) as f32 * 8.0 / 1e6 / el;
        let lat = if st.lat_n > 0 { (st.lat_sum / st.lat_n as f64) as f32 } else { 0.0 };
        let rtt = self.net.shared.clock.lock().unwrap().rtt_us().map(|r| r as f32 / 1000.0);
        let stats = ClientStats {
            latency_ms: lat,
            decoded_fps: d_dec as f32 / el,
            dropped_frames: d_dec.saturating_sub(d_drawn) as u32,
            render_fps: st.frames as f32 / el,
        };
        st.lines = vec![
            "Stats".into(),
            format!("latency {:.1} ms   rtt {}", stats.latency_ms, rtt.map_or("-".into(), |r| format!("{r:.1} ms"))),
            format!("render {:.0} fps   video {:.0} fps   dropped {}", stats.render_fps, stats.decoded_fps, stats.dropped_frames),
            format!("network {:.1} Mbit/s", mbit),
            format!(
                "audio: {} pkts, {} underruns",
                self.jitter.packets.load(Ordering::Relaxed),
                self.jitter.underruns.load(Ordering::Relaxed)
            ),
        ];
        st.t0 = Instant::now();
        st.frames = 0;
        st.lat_sum = 0.0;
        st.lat_n = 0;
        st.last_decoded = decoded;
        st.last_drawn = st.drawn;
        st.last_rx = rx;
        if self.net.shared.connected.load(Ordering::Relaxed) {
            let _ = self.net.cmd.send(NetCmd::Send(ClientMsg::Stats(stats)));
        }
        Ok(())
    }

    fn effective_env(&self) -> EnvMode {
        match &self.settings.environment {
            Environment::Passthrough if self.passthrough.is_some() => EnvMode::Passthrough,
            Environment::Passthrough | Environment::Void => EnvMode::Void,
            Environment::Image(name) => match &self.env.image {
                Some((n, _)) if n == name => EnvMode::Image,
                _ => EnvMode::Void,
            },
        }
    }

    fn render_env(&mut self, f: &Frame<'_>, mode: EnvMode, cutout: Option<&hyprvr_proto::msg::KeyboardCutout>) -> Result<()> {
        let tex = self.env.sc.acquire()?;
        let (ew, eh) = (self.env.eye_w, self.env.eye_h);
        let ok = self.gfx.target(tex, ew * 2, eh);
        if ok {
            for (eye, view) in f.views.iter().enumerate().take(2) {
                self.gfx.viewport(eye as i32 * ew as i32, 0, ew as i32, eh as i32);
                let proj = projection(&view.fov, 0.05, 100.0);
                let (vp, vr) = (super::xr::to_vec3(view.pose.position), super::xr::to_quat(view.pose.orientation));
                self.gfx.blend(Blend::None);
                match mode {
                    EnvMode::Passthrough => self.gfx.clear([0.0, 0.0, 0.0, self.settings.dim.clamp(0.0, 1.0)]),
                    EnvMode::Void => self.gfx.clear(VOID_COLOR),
                    EnvMode::Image => {
                        let inv = (proj * Mat4::from_quat(vr.inverse())).inverse();
                        let t = self.env.image.as_ref().map(|(_, t)| *t);
                        self.gfx.draw_env(&inv, t, [0.0; 4]);
                    }
                }
                if let Some(c) = cutout {
                    let view_m = Mat4::from_rotation_translation(vr, vp).inverse();
                    let mvp = proj * view_m;
                    let k = misc::cutout_corners(c);
                    let v = |p: Vec3| Vertex { pos: p.to_array(), color: [0.0; 4], uvk: [0.0; 3] };
                    let verts = [v(k[0]), v(k[1]), v(k[2]), v(k[0]), v(k[2]), v(k[3])];
                    self.gfx.draw_triangles(&mvp, &verts);
                }
            }
        }
        self.gfx.finish_target();
        self.env.sc.release();
        Ok(())
    }

    /// Rays and hit dots. Returns false if nothing needs drawing.
    fn render_overlay(&mut self, f: &Frame<'_>) -> Result<bool> {
        let eye = self.head.0;
        let mut verts: Vec<Vertex> = Vec::new();
        for (i, h) in self.hands.iter().enumerate() {
            if !h.inp.active {
                continue;
            }
            if i == 0 && self.palm_armed {
                // Gesture ready: a dot above the palm instead of the laser.
                let n = menu_logic::palm_normal(h.inp.grip_rot, true);
                let c = h.inp.grip_pos + n * 0.06;
                let col = if h.select { [0.35, 0.6, 1.0, 1.0] } else { [0.6, 0.75, 1.0, 0.7] };
                push_disc(&mut verts, c, eye, 0.014, col);
                continue;
            }
            let dir = h.inp.aim_rot * Vec3::NEG_Z;
            let len = h.target.as_ref().map_or(RAY_LEN, |(_, t)| *t);
            let primary = i == self.primary;
            let a = if primary { 0.85 } else { 0.45 };
            let start = h.inp.aim_pos + dir * 0.03;
            let end = h.inp.aim_pos + dir * len;
            let fade_end = if h.target.is_some() { a } else { 0.0 };
            push_beam(&mut verts, start, end, eye, 0.0015, [a, a, a, a], [fade_end, fade_end, fade_end, fade_end]);
            if h.target.is_some() {
                let r = if h.select { 0.006 } else { 0.009 } * (len.max(0.3));
                let c = if primary { [0.35, 0.6, 1.0, 1.0] } else { [0.8, 0.8, 0.8, 0.8] };
                push_disc(&mut verts, end - dir * 0.004, eye, r * 0.9, c);
            }
        }
        // Corner resize handles (L brackets) for panels the user is pointing at / resizing.
        for p in &self.layout {
            let Some(g) = self.panel_geom(p) else { continue };
            let resizing = self.corner.is_some_and(|c| c.2.screen == p.screen);
            let hovered = self.hands.iter().any(|h| matches!(&h.target, Some((Target::Resize { screen }, _)) if *screen == p.screen));
            if !(resizing || hovered || self.chrome_visible(p.screen)) {
                continue;
            }
            let c = if resizing || hovered { [0.35, 0.6, 1.0, 1.0] } else { [0.85, 0.85, 0.85, 0.75] };
            let right = g.rot * Vec3::X;
            let up = g.rot * Vec3::Y;
            let arm = 0.045;
            let w = if resizing || hovered { 0.007 } else { 0.005 };
            for (k, h) in resize_handles(&g).into_iter().enumerate() {
                let side = if k == 0 { -1.0 } else { 1.0 };
                // Bracket corner points outward-down; arms run back along the panel edges.
                push_beam(&mut verts, h, h - right * side * arm, eye, w, c, c);
                push_beam(&mut verts, h, h + up * arm, eye, w, c, c);
            }
        }
        if verts.is_empty() {
            return Ok(false);
        }
        let tex = self.overlay.acquire()?;
        let (ow, oh) = self.overlay_eye;
        if self.gfx.target(tex, ow * 2, oh) {
            for (e, view) in f.views.iter().enumerate().take(2) {
                self.gfx.viewport(e as i32 * ow as i32, 0, ow as i32, oh as i32);
                self.gfx.blend(Blend::None);
                self.gfx.clear([0.0; 4]);
                self.gfx.blend(Blend::Premultiplied);
                let proj = projection(&view.fov, 0.05, 100.0);
                let view_m = Mat4::from_rotation_translation(super::xr::to_quat(view.pose.orientation), super::xr::to_vec3(view.pose.position)).inverse();
                self.gfx.draw_triangles(&(proj * view_m), &verts);
            }
            self.gfx.blend(Blend::None);
        }
        self.gfx.finish_target();
        self.overlay.release();
        Ok(true)
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let ids: Vec<ScreenId> = self.screens.keys().copied().collect();
        for id in ids {
            self.close_screen(id);
        }
        self.player.pause();
        // Clean goodbye so the PC removes its virtual screens right away.
        let _ = self.net.cmd.send(NetCmd::Quit);
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(600)
            && !(self.net.shared.quitting.load(Ordering::Relaxed) && !self.net.shared.connected.load(Ordering::Relaxed))
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        info!("goodbye sent: {}", self.net.shared.quitting.load(Ordering::Relaxed));
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EnvMode {
    Passthrough,
    Void,
    Image,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum SurfaceKey {
    Main,
    Chrome(ScreenId),
}

impl From<Surface> for SurfaceKey {
    fn from(s: Surface) -> Self {
        match s {
            Surface::Main => SurfaceKey::Main,
            Surface::Chrome(id) => SurfaceKey::Chrome(id),
        }
    }
}

fn drop_video(gfx: &Gfx, egl: &Egl, v: VideoState) {
    if let Some((img, eimg)) = v.held {
        egl.destroy_image(eimg);
        drop(img);
    }
    gfx.delete_texture(v.tex);
}

fn short_debug(m: &ClientMsg) -> String {
    let s = format!("{m:?}");
    if s.len() > 200 { format!("{}…", &s[..s.floor_char_boundary(200)]) } else { s }
}

/// Camera-facing beam from `a` to `b` (premultiplied colors).
fn push_beam(out: &mut Vec<Vertex>, a: Vec3, b: Vec3, eye: Vec3, width: f32, ca: [f32; 4], cb: [f32; 4]) {
    let dir = (b - a).normalize_or_zero();
    let mid = (a + b) * 0.5;
    let side = dir.cross(eye - mid).normalize_or_zero() * (width * 0.5);
    let v = |p: Vec3, c: [f32; 4]| Vertex { pos: p.to_array(), color: c, uvk: [0.0; 3] };
    out.extend_from_slice(&[v(a - side, ca), v(a + side, ca), v(b + side, cb), v(a - side, ca), v(b + side, cb), v(b - side, cb)]);
}

/// Camera-facing disc at `c`.
fn push_disc(out: &mut Vec<Vertex>, c: Vec3, eye: Vec3, r: f32, color: [f32; 4]) {
    let n = (eye - c).normalize_or_zero();
    let up = if n.y.abs() > 0.95 { Vec3::X } else { Vec3::Y };
    let x = up.cross(n).normalize_or_zero() * r;
    let y = n.cross(x).normalize_or_zero() * r;
    let v = |p: Vec3, u: f32, w: f32| Vertex { pos: p.to_array(), color, uvk: [u, w, 1.0] };
    out.extend_from_slice(&[
        v(c - x - y, -1.0, -1.0),
        v(c + x - y, 1.0, -1.0),
        v(c + x + y, 1.0, 1.0),
        v(c - x - y, -1.0, -1.0),
        v(c + x + y, 1.0, 1.0),
        v(c - x + y, -1.0, 1.0),
    ]);
}
