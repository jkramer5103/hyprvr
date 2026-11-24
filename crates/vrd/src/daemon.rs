//! Daemon core: screens, outputs, the connected headset session, pairing, audio, cursor.
//!
//! All mutating operations are synchronous (they talk to the compositor over blocking IPC)
//! and serialized by `inner`. Async callers go through `spawn_blocking`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hyprvr_proto::msg::{
    AudioTarget, ClientStats, Datagram, Environment, PanelLayout, PointerButton, ScreenId, ScreenInfo, ScreenKind,
    ServerMsg, ServerState, Settings, encode_datagram,
};
use hyprvr_proto::pairing::{Fingerprint, MAX_ATTEMPTS, generate_pin};
use hyprvr_proto::tls::{Identity, short_fingerprint};
use tokio::sync::{mpsc, watch};

use crate::audio::Audio;
use crate::compositor::{Compositor, Monitor};
use crate::config::{self, Config};
use crate::encode::HybridConfig;
use crate::ipc::{ScreenStatus, Status};
use crate::layout::{SyncPanel, SyncPhysical, default_slot, sync_positions};
use crate::pipeline::{self, Pipeline};
use crate::store::{ScreenSpec, SlotLayout, Store};
use crate::wl::cursor::{CursorEvent, CursorWatcher, arrow_sprite};
use crate::wl::vpointer::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, PointerCmd, VirtualPointer};

/// How long a PIN stays valid.
const PIN_LIFETIME: Duration = Duration::from_secs(300);
/// Output name prefix for our headless outputs (Hyprland honors the name).
pub const OUTPUT_PREFIX: &str = "HYPRVR-";

struct Screen {
    id: ScreenId,
    spec: ScreenSpec,
    /// Live compositor output, if any.
    output: Option<String>,
    width: u32,
    height: u32,
    refresh: u32,
    pipeline: Option<Pipeline>,
}

impl Screen {
    fn is_virtual(&self) -> bool {
        matches!(self.spec, ScreenSpec::Virtual { .. })
    }
}

pub struct Session {
    pub id: u64,
    pub conn: quinn::Connection,
    pub name: String,
    pub fingerprint: Fingerprint,
    pub tx: mpsc::UnboundedSender<ServerMsg>,
    pub stats: Option<ClientStats>,
}

struct Pairing {
    pin: String,
    attempts: u32,
    created: Instant,
}

struct Inner {
    store: Store,
    screens: Vec<Screen>,
    next_id: ScreenId,
    next_session: u64,
    session: Option<Session>,
    pairing: Option<Pairing>,
    grace_deadline: Option<Instant>,
    outputs_live: bool,
    /// Physical (non-hyprvr) monitors as of the last refresh.
    physical: Vec<Monitor>,
    last_bytes: HashMap<ScreenId, u64>,
    bitrate_kbps: u32,
    comp_failures: u32,
}

/// What the cursor / pointer paths need, without taking `inner`.
#[derive(Clone, Debug)]
struct Route {
    id: ScreenId,
    output: String,
    width: u32,
    height: u32,
    /// Logical position and scale of the output (for global cursor positions).
    x: i32,
    y: i32,
    scale: f32,
}

#[derive(Default)]
struct CursorState {
    seq: u32,
    serial: u32,
    shape: Option<ServerMsg>,
    /// (screen, x, y) of the hotspot in screen pixels.
    pos: Option<(ScreenId, f32, f32)>,
    /// Output the capture session last saw the cursor on.
    on_output: Option<String>,
}

pub struct Daemon {
    pub cfg: Config,
    pub identity: Identity,
    pub server_name: String,
    comp: Box<dyn Compositor>,
    hybrid: HybridConfig,
    encoder_name: String,
    rt: tokio::runtime::Handle,
    pub epoch: Instant,
    inner: Mutex<Inner>,
    routes: RwLock<Vec<Route>>,
    /// Authenticated connection for datagrams (cursor, audio).
    active: RwLock<Option<quinn::Connection>>,
    cursor: Mutex<CursorState>,
    cursor_watch: Mutex<Option<CursorWatcher>>,
    vpointer: Option<VirtualPointer>,
    audio: Audio,
    status_tx: watch::Sender<Status>,
}

fn now_us(epoch: Instant) -> u64 {
    epoch.elapsed().as_micros() as u64
}

fn notify(summary: &str, body: &str) {
    let r = std::process::Command::new("notify-send")
        .args(["-a", "hyprvr", "-u", "critical", "-i", "video-display", summary, body])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Ok(mut c) = r {
        std::thread::spawn(move || c.wait());
    }
}

impl Daemon {
    pub fn new(
        cfg: Config,
        identity: Identity,
        comp: Box<dyn Compositor>,
        hybrid: HybridConfig,
        encoder_name: String,
        rt: tokio::runtime::Handle,
    ) -> Arc<Self> {
        let store = Store::load();
        let audio = Audio::new();
        let vpointer = match VirtualPointer::spawn() {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("headset pointer input disabled: {e:#}");
                None
            }
        };
        let (status_tx, _) = watch::channel(Status::default());
        let d = Arc::new(Daemon {
            server_name: cfg.server_name(),
            cfg,
            identity,
            comp,
            hybrid,
            encoder_name,
            rt,
            epoch: Instant::now(),
            inner: Mutex::new(Inner {
                store,
                screens: vec![],
                next_id: 1,
                next_session: 1,
                session: None,
                pairing: None,
                grace_deadline: None,
                outputs_live: false,
                physical: vec![],
                last_bytes: HashMap::new(),
                bitrate_kbps: 0,
                comp_failures: 0,
            }),
            routes: RwLock::new(vec![]),
            active: RwLock::new(None),
            cursor: Mutex::new(CursorState::default()),
            cursor_watch: Mutex::new(None),
            vpointer,
            audio,
            status_tx,
        });
        d.recover();
        d.set_shape(arrow_sprite());
        d.start_cursor();
        d.publish(&d.inner.lock().unwrap());
        d
    }

    pub fn subscribe(&self) -> watch::Receiver<Status> {
        self.status_tx.subscribe()
    }

    pub fn status(&self) -> Status {
        self.status_tx.borrow().clone()
    }

    pub fn kind(&self) -> &'static str {
        self.comp.kind()
    }

    // ---------------------------------------------------------------- startup / shutdown

    /// Clean up after a crash: stale outputs and moved physical monitors.
    fn recover(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Ok(mons) = self.comp.monitors() {
            for m in mons.iter().filter(|m| m.name.starts_with(OUTPUT_PREFIX)) {
                tracing::info!("removing stale output {}", m.name);
                let _ = self.comp.remove_output(&m.name);
            }
        }
        if !inner.store.original_positions.is_empty() {
            tracing::info!("restoring monitor positions from a previous run");
            let _ = self.comp.set_positions(&inner.store.original_positions);
            inner.store.original_positions.clear();
            inner.store.save();
        }
        self.refresh_physical(&mut inner);
        self.build_screens(&mut inner);
    }

    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(s) = inner.session.take() {
            s.conn.close(0u32.into(), b"daemon stopping");
        }
        *self.active.write().unwrap() = None;
        self.teardown(&mut inner);
        self.audio.shutdown();
    }

    // ---------------------------------------------------------------- screens / outputs

    fn build_screens(&self, inner: &mut Inner) {
        if !inner.screens.is_empty() {
            return;
        }
        // Virtual screens live only as long as a session (plus the reconnect grace), so a new
        // session starts from the saved mirrors; with none, the PC's main monitor is shown.
        let saved = inner.store.screens.clone().unwrap_or_default();
        let keep: Vec<bool> = saved.iter().map(|s| !matches!(s, ScreenSpec::Virtual { .. })).collect();
        let mut specs: Vec<ScreenSpec> = saved.into_iter().filter(|s| !matches!(s, ScreenSpec::Virtual { .. })).collect();
        let mut i = 0;
        inner.store.layout.retain(|_| {
            let k = keep.get(i).copied().unwrap_or(false);
            i += 1;
            k
        });
        if specs.is_empty() {
            self.refresh_physical(inner);
            match main_monitor(&inner.physical) {
                Some(m) => specs.push(ScreenSpec::Mirror { output: m.to_string() }),
                None => specs.extend((0..self.cfg.default_screens).map(|_| ScreenSpec::Virtual {
                    width: self.cfg.default_width,
                    height: self.cfg.default_height,
                })),
            }
        }
        for spec in specs.into_iter().take(config::MAX_SCREENS) {
            let spec = match spec {
                ScreenSpec::Virtual { width, height } => {
                    let (width, height) = config::clamp_size(width, height);
                    ScreenSpec::Virtual { width, height }
                }
                m => m,
            };
            let (width, height) = match &spec {
                ScreenSpec::Virtual { width, height } => (*width, *height),
                // Real size arrives with the output; this only sizes the default 3D slot.
                ScreenSpec::Mirror { output } => {
                    inner.physical.iter().find(|m| &m.name == output).map_or((1920, 1080), |m| (m.width, m.height))
                }
            };
            let id = inner.next_id;
            inner.next_id += 1;
            inner.screens.push(Screen { id, spec, output: None, width, height, refresh: self.cfg.refresh_hz, pipeline: None });
        }
        self.fill_layout(inner);
    }

    /// Make `store.screens` / `store.layout` match `screens` and fill missing layouts.
    fn fill_layout(&self, inner: &mut Inner) {
        let widths: Vec<u32> = inner.screens.iter().map(|s| s.width.max(1)).collect();
        let heights: Vec<u32> = inner.screens.iter().map(|s| s.height.max(1)).collect();
        let n = inner.screens.len();
        inner.store.layout.truncate(n);
        while inner.store.layout.len() < n {
            let i = inner.store.layout.len();
            inner.store.layout.push(default_slot(i, &widths, &heights));
        }
        inner.store.screens = Some(inner.screens.iter().map(|s| s.spec.clone()).collect());
    }

    fn refresh_physical(&self, inner: &mut Inner) {
        match self.comp.monitors() {
            Ok(m) => {
                let ours: Vec<&str> =
                    inner.screens.iter().filter(|s| s.is_virtual()).filter_map(|s| s.output.as_deref()).collect();
                inner.physical =
                    m.into_iter().filter(|m| !m.name.starts_with(OUTPUT_PREFIX) && !ours.contains(&m.name.as_str())).collect();
                inner.comp_failures = 0;
            }
            Err(e) => {
                inner.comp_failures += 1;
                tracing::warn!("compositor query failed: {e:#}");
            }
        }
    }

    fn free_output_name(&self, inner: &Inner) -> String {
        let used: Vec<String> = inner.screens.iter().filter_map(|s| s.output.clone()).collect();
        let existing: Vec<String> = self.comp.monitors().map(|m| m.into_iter().map(|m| m.name).collect()).unwrap_or_default();
        (1..)
            .map(|i| format!("{OUTPUT_PREFIX}{i}"))
            .find(|n| !used.contains(n) && !existing.contains(n))
            .unwrap()
    }

    /// Create the compositor output for screen `idx` if needed.
    fn ensure_output(&self, inner: &mut Inner, idx: usize) -> Result<()> {
        if inner.screens[idx].output.is_some() {
            return Ok(());
        }
        match inner.screens[idx].spec.clone() {
            ScreenSpec::Virtual { width, height } => {
                let name = self.free_output_name(inner);
                let real = self.comp.create_output(&name, width, height, self.cfg.refresh_hz)?;
                tracing::info!("created output {real} ({width}x{height}@{})", self.cfg.refresh_hz);
                let s = &mut inner.screens[idx];
                s.output = Some(real);
                s.width = width;
                s.height = height;
                s.refresh = self.cfg.refresh_hz;
            }
            ScreenSpec::Mirror { output } => {
                let m = self
                    .comp
                    .monitors()?
                    .into_iter()
                    .find(|m| m.name == output)
                    .with_context(|| format!("output {output} not connected"))?;
                let s = &mut inner.screens[idx];
                s.output = Some(m.name);
                s.width = m.width;
                s.height = m.height;
                s.refresh = m.refresh.round() as u32;
            }
        }
        Ok(())
    }

    fn ensure_outputs(&self, inner: &mut Inner) {
        if !inner.outputs_live {
            self.refresh_physical(inner);
            // Remember where the physical monitors were, persistently (crash safety).
            inner.store.original_positions = inner.physical.iter().map(|m| (m.name.clone(), m.x, m.y)).collect();
            inner.store.save();
        }
        inner.outputs_live = true;
        inner.grace_deadline = None;
        for i in 0..inner.screens.len() {
            if let Err(e) = self.ensure_output(inner, i) {
                tracing::warn!("screen {}: {e:#}", inner.screens[i].id);
            }
        }
        self.fill_layout(inner);
        self.refresh_physical(inner);
        self.sync_layout(inner);
    }

    /// Remove our outputs and restore the physical monitor positions.
    fn teardown(&self, inner: &mut Inner) {
        for s in &mut inner.screens {
            s.pipeline = None;
            if s.is_virtual()
                && let Some(o) = s.output.take()
            {
                tracing::info!("removing output {o}");
                if let Err(e) = self.comp.remove_output(&o) {
                    tracing::warn!("remove {o}: {e:#}");
                }
            }
            if !s.is_virtual() {
                s.output = None;
            }
        }
        if inner.outputs_live && !inner.store.original_positions.is_empty() {
            let _ = self.comp.set_positions(&inner.store.original_positions);
        }
        inner.store.original_positions.clear();
        // Virtual screens are gone for good; mirrors stay for the next session.
        let keep: Vec<bool> = inner.screens.iter().map(|s| !s.is_virtual()).collect();
        let mut i = 0;
        inner.store.layout.retain(|_| {
            let k = keep.get(i).copied().unwrap_or(false);
            i += 1;
            k
        });
        inner.screens.retain(|s| !s.is_virtual());
        self.fill_layout(inner);
        inner.store.save();
        inner.outputs_live = false;
        inner.grace_deadline = None;
        self.update_routes(inner);
        self.build_screens(inner);
    }

    /// Arrange compositor monitor positions after the 3D layout.
    fn sync_layout(&self, inner: &mut Inner) {
        if !inner.outputs_live {
            return;
        }
        // Every screen shown in VR (virtual or a mirrored monitor) is placed after its 3D
        // position, so moving the mouse across follows what the user sees. Physical monitors
        // not shown in VR keep their arrangement and go below.
        let mut panels = vec![];
        let mut in_vr: Vec<&str> = vec![];
        for (i, s) in inner.screens.iter().enumerate() {
            let Some(o) = &s.output else { continue };
            let (width, height) = if s.is_virtual() {
                (s.width as i32, s.height as i32)
            } else {
                match inner.physical.iter().find(|m| &m.name == o) {
                    Some(m) => m.logical_size(),
                    None => continue,
                }
            };
            in_vr.push(o);
            panels.push(SyncPanel { output: o.clone(), width, height, layout: inner.store.layout[i] });
        }
        let physical: Vec<SyncPhysical> = inner
            .physical
            .iter()
            .filter(|m| !in_vr.contains(&m.name.as_str()))
            .map(|m| {
                // Lay out physical monitors from their original arrangement.
                let (x, y) = inner
                    .store
                    .original_positions
                    .iter()
                    .find(|(n, ..)| *n == m.name)
                    .map(|(_, x, y)| (*x, *y))
                    .unwrap_or((m.x, m.y));
                SyncPhysical { output: m.name.clone(), x, y, width: m.logical_size().0 }
            })
            .collect();
        let pos = sync_positions(&panels, &physical);
        if let Err(e) = self.comp.set_positions(&pos) {
            tracing::warn!("layout sync: {e:#}");
        }
        self.refresh_physical(inner);
        self.update_routes(inner);
    }

    fn update_routes(&self, inner: &Inner) {
        let mons = self.comp.monitors().unwrap_or_default();
        let routes: Vec<Route> = inner
            .screens
            .iter()
            .filter_map(|s| {
                let o = s.output.as_ref()?;
                let m = mons.iter().find(|m| &m.name == o);
                Some(Route {
                    id: s.id,
                    output: o.clone(),
                    width: s.width,
                    height: s.height,
                    x: m.map_or(0, |m| m.x),
                    y: m.map_or(0, |m| m.y),
                    scale: m.map_or(1.0, |m| m.scale),
                })
            })
            .collect();
        if let Some(w) = self.cursor_watch.lock().unwrap().as_ref() {
            w.watch(if inner.session.is_some() { routes.iter().map(|r| r.output.clone()).collect() } else { vec![] });
        }
        *self.routes.write().unwrap() = routes;
    }

    fn start_pipelines(&self, inner: &mut Inner) {
        let Some(sess) = &inner.session else { return };
        let conn = sess.conn.clone();
        for s in &mut inner.screens {
            let Some(o) = &s.output else { continue };
            if s.pipeline.as_ref().is_some_and(|p| !p.is_stopped()) {
                continue;
            }
            s.pipeline = Some(pipeline::start(
                conn.clone(),
                &self.rt,
                s.id,
                o.clone(),
                s.width,
                s.height,
                self.hybrid.clone(),
                self.epoch,
            ));
        }
    }

    fn server_state(&self, inner: &Inner) -> ServerState {
        let mut screens = vec![];
        let mut layout = vec![];
        for (i, s) in inner.screens.iter().enumerate() {
            let Some(o) = &s.output else { continue };
            screens.push(ScreenInfo {
                id: s.id,
                output: o.clone(),
                kind: if s.is_virtual() { ScreenKind::Virtual } else { ScreenKind::Mirror },
                width: s.width,
                height: s.height,
                refresh_hz: s.refresh,
            });
            layout.push(inner.store.layout[i].with_id(s.id));
        }
        let mirrored: Vec<&str> = inner
            .screens
            .iter()
            .filter_map(|s| match &s.spec {
                ScreenSpec::Mirror { output } => Some(output.as_str()),
                _ => None,
            })
            .collect();
        ServerState {
            screens,
            layout,
            audio: inner.store.audio,
            settings: inner.store.settings.clone(),
            environments: self.environments(),
            mirrorable: inner.physical.iter().map(|m| m.name.clone()).filter(|n| !mirrored.contains(&n.as_str())).collect(),
        }
    }

    fn broadcast_state(&self, inner: &Inner) {
        if let Some(s) = &inner.session {
            let _ = s.tx.send(ServerMsg::State(self.server_state(inner)));
        }
        self.publish(inner);
    }

    fn publish(&self, inner: &Inner) {
        let screens = inner
            .screens
            .iter()
            .map(|s| ScreenStatus {
                id: s.id,
                output: s.output.clone(),
                kind: if s.is_virtual() { "virtual".into() } else { "mirror".into() },
                width: s.width,
                height: s.height,
                video: s.pipeline.as_ref().is_some_and(|p| p.stats.lock().unwrap().video_active),
                encode_ms: s.pipeline.as_ref().map_or(0.0, |p| (p.stats.lock().unwrap().encode_ms_avg * 10.0).round() / 10.0),
            })
            .collect();
        let st = Status {
            server_name: self.server_name.clone(),
            compositor: self.comp.kind().into(),
            encoder: self.encoder_name.clone(),
            port: self.cfg.port,
            fingerprint: short_fingerprint(&self.identity.fingerprint()),
            connected: inner.session.as_ref().map(|s| s.name.clone()),
            pairing_pin: inner.pairing.as_ref().map(|p| p.pin.clone()),
            screens,
            outputs_live: inner.outputs_live,
            grace_left_secs: inner.grace_deadline.map(|d| d.saturating_duration_since(Instant::now()).as_secs()),
            audio: match inner.store.audio {
                AudioTarget::Laptop => "laptop".into(),
                AudioTarget::Headset => "headset".into(),
            },
            bitrate_kbps: inner.bitrate_kbps,
            latency_ms: inner.session.as_ref().and_then(|s| s.stats.as_ref()).map(|s| s.latency_ms),
            paired_clients: inner.store.paired.iter().map(|c| format!("{} ({})", c.name, &c.fingerprint[..8.min(c.fingerprint.len())])).collect(),
        };
        self.status_tx.send_if_modified(|old| {
            if *old != st {
                *old = st;
                true
            } else {
                false
            }
        });
    }

    pub fn environments(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.cfg.environments_dir())
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| {
                        let l = n.to_ascii_lowercase();
                        l.ends_with(".jpg") || l.ends_with(".jpeg") || l.ends_with(".png")
                    })
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    /// Read an environment image by name (must be one of `environments()`).
    pub fn environment_data(&self, name: &str) -> Result<Vec<u8>> {
        if !self.environments().iter().any(|n| n == name) {
            bail!("unknown environment {name}");
        }
        let p = self.cfg.environments_dir().join(name);
        let len = std::fs::metadata(&p)?.len();
        if len > 48 * 1024 * 1024 {
            bail!("environment {name} is too large ({} MB, max 48)", len / 1024 / 1024);
        }
        Ok(std::fs::read(p)?)
    }

    // ---------------------------------------------------------------- session lifecycle

    pub fn is_paired(&self, fp: &Fingerprint) -> bool {
        self.inner.lock().unwrap().store.is_paired(fp)
    }

    /// Become the active headset. Replaces any previous session.
    pub fn attach(&self, conn: quinn::Connection, name: String, fingerprint: Fingerprint, tx: mpsc::UnboundedSender<ServerMsg>) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        if let Some(old) = inner.session.take() {
            tracing::info!("replacing session of {}", old.name);
            old.conn.close(2u32.into(), b"replaced by a new connection");
        }
        for s in &mut inner.screens {
            s.pipeline = None;
        }
        let id = inner.next_session;
        inner.next_session += 1;
        tracing::info!("headset {name} connected from {}", conn.remote_address());
        inner.session = Some(Session { id, conn: conn.clone(), name, fingerprint, tx: tx.clone(), stats: None });
        *self.active.write().unwrap() = Some(conn.clone());
        self.ensure_outputs(&mut inner);
        self.start_pipelines(&mut inner);
        if inner.store.audio == AudioTarget::Headset {
            *self.audio.target.lock().unwrap() = Some(conn);
            if let Err(e) = self.audio.to_headset() {
                tracing::warn!("audio to headset: {e:#}");
            }
        }
        let _ = tx.send(ServerMsg::State(self.server_state(&inner)));
        if let Some(shape) = self.cursor.lock().unwrap().shape.clone() {
            let _ = tx.send(shape);
        }
        self.update_routes(&inner);
        self.publish(&inner);
        id
    }

    /// `clean`: the headset said goodbye (app closed / disconnect); virtual outputs are removed
    /// right away. Otherwise they are kept for `grace_secs` in case it comes back.
    pub fn detach(&self, sid: u64, clean: bool) {
        let mut inner = self.inner.lock().unwrap();
        if inner.session.as_ref().map(|s| s.id) != Some(sid) {
            return;
        }
        let s = inner.session.take().unwrap();
        tracing::info!("headset {} disconnected ({})", s.name, if clean { "clean" } else { "lost" });
        *self.active.write().unwrap() = None;
        for s in &mut inner.screens {
            s.pipeline = None;
        }
        *self.audio.target.lock().unwrap() = None;
        if inner.store.audio == AudioTarget::Headset {
            // Don't leave the laptop silent while the headset is away; restored on reconnect.
            let _ = self.audio.to_laptop();
        }
        self.cursor.lock().unwrap().pos = None;
        if clean || self.cfg.grace_secs == 0 {
            self.teardown(&mut inner);
        } else {
            inner.grace_deadline = Some(Instant::now() + Duration::from_secs(self.cfg.grace_secs));
        }
        self.update_routes(&inner);
        self.publish(&inner);
    }

    pub fn set_client_stats(&self, sid: u64, stats: ClientStats) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(s) = inner.session.as_mut().filter(|s| s.id == sid) {
            s.stats = Some(stats);
        }
    }

    // ---------------------------------------------------------------- pairing

    /// Current PIN, creating (and announcing) one if needed.
    pub fn pairing_pin(&self) -> String {
        let mut inner = self.inner.lock().unwrap();
        let pin = self.ensure_pin(&mut inner);
        self.publish(&inner);
        pin
    }

    fn ensure_pin(&self, inner: &mut Inner) -> String {
        if let Some(p) = &inner.pairing
            && p.created.elapsed() < PIN_LIFETIME
            && p.attempts < MAX_ATTEMPTS
        {
            return p.pin.clone();
        }
        let pin = generate_pin();
        tracing::info!("pairing PIN: {pin}");
        notify("VR headset pairing", &format!("Enter PIN {pin} in the headset"));
        inner.pairing = Some(Pairing { pin: pin.clone(), attempts: 0, created: Instant::now() });
        pin
    }

    /// A wrong PIN was used. After `MAX_ATTEMPTS` the PIN is replaced.
    pub fn pairing_failed(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(p) = &mut inner.pairing {
            p.attempts += 1;
            if p.attempts >= MAX_ATTEMPTS {
                tracing::warn!("too many wrong PINs; generating a new one");
                inner.pairing = None;
                self.ensure_pin(&mut inner);
            }
        }
        self.publish(&inner);
    }

    pub fn pairing_succeeded(&self, name: &str, fp: &Fingerprint) {
        let mut inner = self.inner.lock().unwrap();
        inner.store.add_paired(name, fp);
        inner.store.save();
        inner.pairing = None;
        tracing::info!("paired with {name} ({})", short_fingerprint(fp));
        notify("VR headset paired", name);
        self.publish(&inner);
    }

    pub fn unpair(&self, which: &str) -> Result<usize> {
        let mut inner = self.inner.lock().unwrap();
        let before = inner.store.paired.len();
        let w = which.to_ascii_lowercase();
        inner.store.paired.retain(|c| !(w == "all" || c.name.to_ascii_lowercase() == w || (w.len() >= 4 && c.fingerprint.starts_with(&w))));
        let removed = before - inner.store.paired.len();
        inner.store.save();
        let fp_gone = inner.session.as_ref().is_some_and(|s| !inner.store.is_paired(&s.fingerprint));
        if fp_gone {
            let s = inner.session.as_ref().unwrap();
            s.conn.close(3u32.into(), b"unpaired");
        }
        self.publish(&inner);
        Ok(removed)
    }

    // ---------------------------------------------------------------- screen operations

    pub fn add_screen(&self, width: u32, height: u32) -> Result<ScreenId> {
        let (width, height) = config::clamp_size(width, height);
        self.add(ScreenSpec::Virtual { width, height })
    }

    pub fn add_mirror(&self, output: &str) -> Result<ScreenId> {
        {
            let mut inner = self.inner.lock().unwrap();
            self.refresh_physical(&mut inner);
            if !inner.physical.iter().any(|m| m.name == output) {
                bail!("{output} is not a physical output");
            }
            if inner.screens.iter().any(|s| matches!(&s.spec, ScreenSpec::Mirror { output: o } if o == output)) {
                bail!("{output} is already mirrored");
            }
        }
        self.add(ScreenSpec::Mirror { output: output.to_string() })
    }

    fn add(&self, spec: ScreenSpec) -> Result<ScreenId> {
        let mut inner = self.inner.lock().unwrap();
        if inner.screens.len() >= config::MAX_SCREENS {
            bail!("at most {} screens", config::MAX_SCREENS);
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let (width, height) = match &spec {
            ScreenSpec::Virtual { width, height } => (*width, *height),
            ScreenSpec::Mirror { .. } => (1920, 1080),
        };
        inner.screens.push(Screen { id, spec, output: None, width, height, refresh: self.cfg.refresh_hz, pipeline: None });
        let idx = inner.screens.len() - 1;
        if inner.outputs_live
            && let Err(e) = self.ensure_output(&mut inner, idx)
        {
            inner.screens.pop();
            return Err(e);
        }
        self.fill_layout(&mut inner);
        // Place the new panel with sizes known now.
        let widths: Vec<u32> = inner.screens.iter().map(|s| s.width).collect();
        let heights: Vec<u32> = inner.screens.iter().map(|s| s.height).collect();
        inner.store.layout[idx] = default_slot(idx, &widths, &heights);
        inner.store.save();
        self.refresh_physical(&mut inner);
        self.sync_layout(&mut inner);
        self.start_pipelines(&mut inner);
        self.broadcast_state(&inner);
        Ok(id)
    }

    pub fn remove_screen(&self, id: ScreenId) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let idx = inner.screens.iter().position(|s| s.id == id).context("no such screen")?;
        let mut s = inner.screens.remove(idx);
        inner.store.layout.remove(idx);
        s.pipeline = None;
        if s.is_virtual()
            && let Some(o) = &s.output
            && let Err(e) = self.comp.remove_output(o)
        {
            tracing::warn!("remove {o}: {e:#}");
        }
        self.fill_layout(&mut inner);
        inner.store.save();
        self.refresh_physical(&mut inner);
        self.sync_layout(&mut inner);
        self.broadcast_state(&inner);
        Ok(())
    }

    pub fn resize_screen(&self, id: ScreenId, width: u32, height: u32) -> Result<()> {
        let (width, height) = config::clamp_size(width, height);
        let mut inner = self.inner.lock().unwrap();
        let idx = inner.screens.iter().position(|s| s.id == id).context("no such screen")?;
        if !inner.screens[idx].is_virtual() {
            bail!("mirrored screens follow their monitor's resolution");
        }
        if let Some(o) = inner.screens[idx].output.clone() {
            self.comp.set_mode(&o, width, height, self.cfg.refresh_hz)?;
        }
        // Keep the panel's physical height roughly stable: scale width_m with the aspect.
        let s = &mut inner.screens[idx];
        let old_aspect = s.width as f32 / s.height.max(1) as f32;
        s.spec = ScreenSpec::Virtual { width, height };
        s.width = width;
        s.height = height;
        let new_aspect = width as f32 / height as f32;
        let l = &mut inner.store.layout[idx];
        l.width_m = (l.width_m * new_aspect / old_aspect).clamp(0.2, 8.0);
        self.fill_layout(&mut inner);
        inner.store.save();
        self.sync_layout(&mut inner);
        self.broadcast_state(&inner);
        Ok(())
    }

    pub fn set_layout(&self, panels: &[PanelLayout]) {
        let mut inner = self.inner.lock().unwrap();
        for p in panels {
            if !(p.width_m.is_finite() && p.pose.position.iter().chain(&p.pose.orientation).all(|v| v.is_finite())) {
                continue;
            }
            if let Some(i) = inner.screens.iter().position(|s| s.id == p.screen) {
                let mut l = SlotLayout::from_panel(p);
                l.width_m = l.width_m.clamp(0.1, 10.0);
                inner.store.layout[i] = l;
            }
        }
        inner.store.save();
        self.sync_layout(&mut inner);
        self.publish(&inner);
    }

    pub fn reset_layout(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.store.layout.clear();
        self.fill_layout(&mut inner);
        inner.store.save();
        self.sync_layout(&mut inner);
        self.broadcast_state(&inner);
    }

    pub fn set_settings(&self, mut s: Settings) -> Result<()> {
        if !s.dim.is_finite() {
            s.dim = 0.3;
        }
        s.dim = s.dim.clamp(0.0, 1.0);
        if let Environment::Image(n) = &s.environment
            && !self.environments().contains(n)
        {
            bail!("unknown environment {n}");
        }
        let mut inner = self.inner.lock().unwrap();
        inner.store.settings = s;
        inner.store.save();
        self.broadcast_state(&inner);
        Ok(())
    }

    pub fn set_audio(&self, target: Option<AudioTarget>) -> Result<AudioTarget> {
        let mut inner = self.inner.lock().unwrap();
        let t = target.unwrap_or(match inner.store.audio {
            AudioTarget::Laptop => AudioTarget::Headset,
            AudioTarget::Headset => AudioTarget::Laptop,
        });
        inner.store.audio = t;
        inner.store.save();
        let r = match (t, &inner.session) {
            (AudioTarget::Headset, Some(s)) => {
                *self.audio.target.lock().unwrap() = Some(s.conn.clone());
                self.audio.to_headset()
            }
            _ => {
                *self.audio.target.lock().unwrap() = None;
                self.audio.to_laptop()
            }
        };
        self.broadcast_state(&inner);
        r.map(|_| t)
    }

    pub fn request_refresh(&self, id: ScreenId) {
        let inner = self.inner.lock().unwrap();
        if let Some(p) = inner.screens.iter().find(|s| s.id == id).and_then(|s| s.pipeline.as_ref()) {
            p.request_refresh();
        }
    }

    // ---------------------------------------------------------------- input

    fn route(&self, id: ScreenId) -> Option<Route> {
        self.routes.read().unwrap().iter().find(|r| r.id == id).cloned()
    }

    pub fn pointer_move(&self, id: ScreenId, x: f32, y: f32) {
        let (Some(vp), Some(r)) = (&self.vpointer, self.route(id)) else { return };
        if !(x.is_finite() && y.is_finite()) {
            return;
        }
        let x = (x as f64).clamp(0.0, r.width.saturating_sub(1) as f64);
        let y = (y as f64).clamp(0.0, r.height.saturating_sub(1) as f64);
        vp.send(PointerCmd::Move { output: r.output, x, y, w: r.width, h: r.height });
    }

    pub fn pointer_button(&self, id: ScreenId, x: f32, y: f32, b: PointerButton, pressed: bool) {
        self.pointer_move(id, x, y);
        let (Some(vp), Some(r)) = (&self.vpointer, self.route(id)) else { return };
        let button = match b {
            PointerButton::Left => BTN_LEFT,
            PointerButton::Right => BTN_RIGHT,
            PointerButton::Middle => BTN_MIDDLE,
        };
        vp.send(PointerCmd::Button { output: r.output, button, pressed });
    }

    pub fn pointer_scroll(&self, id: ScreenId, dx: f32, dy: f32) {
        let (Some(vp), Some(r)) = (&self.vpointer, self.route(id)) else { return };
        if !(dx.is_finite() && dy.is_finite()) {
            return;
        }
        vp.send(PointerCmd::Scroll { output: r.output, dx: dx.clamp(-20.0, 20.0) as f64, dy: dy.clamp(-20.0, 20.0) as f64 });
    }

    // ---------------------------------------------------------------- cursor

    fn set_shape(&self, ev: CursorEvent) {
        let CursorEvent::Shape { width, height, hot_x, hot_y, rgba } = ev else { return };
        if width == 0 || height == 0 || width > 256 || height > 256 {
            return;
        }
        let mut c = self.cursor.lock().unwrap();
        c.serial = c.serial.wrapping_add(1);
        let msg = ServerMsg::CursorShape {
            serial: c.serial,
            width,
            height,
            hot_x: hot_x.clamp(0, width as i32 - 1) as u32,
            hot_y: hot_y.clamp(0, height as i32 - 1) as u32,
            rgba,
        };
        c.shape = Some(msg.clone());
        drop(c);
        if let Some(s) = &self.inner.lock().unwrap().session {
            let _ = s.tx.send(msg);
        }
    }

    fn send_cursor(&self, pos: Option<(ScreenId, f32, f32)>, force: bool) {
        let mut c = self.cursor.lock().unwrap();
        if !force && c.pos == pos {
            return;
        }
        c.pos = pos;
        c.seq = c.seq.wrapping_add(1);
        let d = Datagram::Cursor {
            seq: c.seq,
            screen: pos.map(|p| p.0),
            x: pos.map_or(0.0, |p| p.1),
            y: pos.map_or(0.0, |p| p.2),
            shape_serial: c.serial,
        };
        drop(c);
        if let Some(conn) = self.active.read().unwrap().as_ref() {
            let _ = conn.send_datagram(encode_datagram(&d).into());
        }
    }

    fn on_cursor_event(&self, ev: CursorEvent) {
        match ev {
            CursorEvent::Shape { .. } => self.set_shape(ev),
            CursorEvent::Position { output, x, y } => {
                let r = self.routes.read().unwrap().iter().find(|r| r.output == output).cloned();
                self.cursor.lock().unwrap().on_output = Some(output);
                if let Some(r) = r {
                    self.send_cursor(Some((r.id, x as f32, y as f32)), false);
                }
            }
            CursorEvent::Leave { output } => {
                let mut c = self.cursor.lock().unwrap();
                if c.on_output.as_deref() == Some(output.as_str()) {
                    c.on_output = None;
                    drop(c);
                    self.send_cursor(None, false);
                }
            }
        }
    }

    fn start_cursor(self: &Arc<Self>) {
        if self.cfg.cursor_capture {
            let weak = Arc::downgrade(self);
            match CursorWatcher::spawn(move |ev| {
                if let Some(d) = weak.upgrade() {
                    d.on_cursor_event(ev);
                }
            }) {
                Ok(w) => {
                    tracing::info!("cursor: ext-image-copy-capture cursor sessions");
                    *self.cursor_watch.lock().unwrap() = Some(w);
                }
                Err(e) => tracing::info!("cursor: {e:#}; using compositor position + built-in arrow"),
            }
        }
        let weak = Arc::downgrade(self);
        let polling = self.cursor_watch.lock().unwrap().is_none();
        std::thread::Builder::new()
            .name("cursor-pos".into())
            .spawn(move || {
                let mut last_send = Instant::now();
                loop {
                    let Some(d) = weak.upgrade() else { return };
                    let connected = d.active.read().unwrap().is_some();
                    if connected && polling {
                        match d.comp.cursor_pos() {
                            Ok(Some((gx, gy))) => {
                                let pos = d.routes.read().unwrap().iter().find_map(|r| {
                                    let lw = r.width as f64 / r.scale as f64;
                                    let lh = r.height as f64 / r.scale as f64;
                                    let (lx, ly) = (gx - r.x as f64, gy - r.y as f64);
                                    (lx >= 0.0 && ly >= 0.0 && lx < lw && ly < lh)
                                        .then_some((r.id, (lx * r.scale as f64) as f32, (ly * r.scale as f64) as f32))
                                });
                                d.send_cursor(pos, false);
                            }
                            Ok(None) => {}
                            Err(_) => {}
                        }
                    }
                    // Resend the latest state now and then: datagrams can be lost.
                    if connected && last_send.elapsed() > Duration::from_millis(100) {
                        let pos = d.cursor.lock().unwrap().pos;
                        d.send_cursor(pos, true);
                        last_send = Instant::now();
                    }
                    drop(d);
                    std::thread::sleep(if polling && connected { Duration::from_millis(8) } else { Duration::from_millis(50) });
                }
            })
            .expect("spawn cursor thread");
    }

    // ---------------------------------------------------------------- periodic work

    /// Called once per second.
    pub fn tick(&self) {
        let mut inner = self.inner.lock().unwrap();
        // Grace period over: remove the virtual outputs.
        if let Some(d) = inner.grace_deadline
            && Instant::now() >= d
        {
            tracing::info!("grace period over; removing virtual outputs");
            self.teardown(&mut inner);
        }
        // Expired PIN.
        if inner.pairing.as_ref().is_some_and(|p| p.created.elapsed() >= PIN_LIFETIME) {
            inner.pairing = None;
        }
        // Health: outputs that disappeared / dead pipelines.
        if inner.outputs_live {
            self.refresh_physical(&mut inner);
            if let Ok(mons) = self.comp.monitors() {
                let mut changed = false;
                for s in &mut inner.screens {
                    if let Some(o) = &s.output
                        && !mons.iter().any(|m| &m.name == o)
                    {
                        tracing::warn!("output {o} disappeared");
                        s.output = None;
                        s.pipeline = None;
                        changed = true;
                    }
                }
                if changed && inner.session.is_some() {
                    self.ensure_outputs(&mut inner);
                }
                if changed {
                    self.broadcast_state(&inner);
                }
            }
            if inner.session.is_some() {
                let dead = inner.screens.iter().any(|s| s.output.is_some() && s.pipeline.as_ref().is_none_or(|p| p.is_stopped()));
                if dead {
                    tracing::info!("restarting screen stream(s)");
                    self.start_pipelines(&mut inner);
                }
            }
        } else {
            self.refresh_physical(&mut inner);
        }
        // Bitrate.
        let mut total = 0u64;
        let mut seen = HashMap::new();
        for s in &inner.screens {
            if let Some(p) = &s.pipeline {
                let st = p.stats.lock().unwrap().enc;
                let bytes = st.tile_bytes + st.video_bytes;
                let prev = inner.last_bytes.get(&s.id).copied().unwrap_or(0);
                total += bytes.saturating_sub(prev);
                seen.insert(s.id, bytes);
            }
        }
        inner.last_bytes = seen;
        inner.bitrate_kbps = (total * 8 / 1000) as u32;
        if inner.comp_failures >= 10 {
            tracing::error!("compositor unreachable; exiting so the service restarts against the new instance");
            self.teardown(&mut inner);
            self.audio.shutdown();
            std::process::exit(1);
        }
        self.publish(&inner);
    }

    pub fn now_us(&self) -> u64 {
        now_us(self.epoch)
    }
}

/// The PC's "main" monitor: the built-in panel if there is one, else the one at the origin.
fn main_monitor(physical: &[Monitor]) -> Option<&str> {
    physical
        .iter()
        .find(|m| m.name.starts_with("eDP") || m.name.starts_with("LVDS") || m.name.starts_with("DSI"))
        .or_else(|| physical.iter().find(|m| (m.x, m.y) == (0, 0)))
        .or_else(|| physical.first())
        .map(|m| m.name.as_str())
}
