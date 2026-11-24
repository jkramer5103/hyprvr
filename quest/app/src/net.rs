//! Networking: discovery, QUIC connection, pairing, streams, datagrams. Runs on a tokio
//! runtime in its own threads and talks to the render thread through channels.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use hyprvr_proto::discovery;
use hyprvr_proto::framing::{FrameError, read_msg, write_msg};
use hyprvr_proto::msg::{ClientMsg, Datagram, Rect, ScreenId, ServerMsg, ServerState, StreamMsg, VideoCodec};
use hyprvr_proto::pairing::{self, Fingerprint, PairState, Role};
use hyprvr_proto::tls::{self, Identity};
use hyprvr_quest_core::misc::ClockSync;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc as tmpsc;

/// Microseconds on a process-wide monotonic clock (shared by net and render threads).
pub fn now_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

#[derive(Debug)]
pub enum NetCmd {
    Send(ClientMsg),
    PointerMove { screen: ScreenId, x: f32, y: f32 },
    Pin(String),
    ForgetServer,
    /// The app is closing: say goodbye (server drops virtual screens at once) and stop.
    Quit,
}

#[derive(Debug)]
pub enum NetEvent {
    Status(String),
    NeedPin { server: String },
    PairFailed(String),
    Connected { server: String },
    Disconnected(String),
    State(ServerState),
    CursorShape { serial: u32, width: u32, height: u32, hot_x: u32, hot_y: u32, rgba: Vec<u8> },
    Cursor { seq: u32, screen: Option<ScreenId>, x: f32, y: f32, serial: u32 },
    ScreenOpen { screen: ScreenId, width: u32, height: u32 },
    ScreenClosed { screen: ScreenId },
    Resized { screen: ScreenId, width: u32, height: u32 },
    /// `data` holds `rect.w*rect.h*4` RGBA bytes per rect, concatenated in order.
    Tiles { screen: ScreenId, capture_us: u64, rects: Vec<Rect>, data: Vec<u8> },
    VideoStart { screen: ScreenId, generation: u64, rect: Rect },
    VideoStop { screen: ScreenId },
    Environment { name: String, width: u32, height: u32, rgba: Vec<u8> },
}

/// Hardware video decoding, implemented by the platform layer.
pub trait VideoBackend: Send + Sync {
    /// Start a decoder for a new video region (replacing any previous one for `screen`).
    fn start(&self, screen: ScreenId, generation: u64, codec: VideoCodec, rect: Rect);
    fn frame(&self, screen: ScreenId, generation: u64, data: Vec<u8>, pts_us: u64, keyframe: bool);
    fn stop(&self, screen: ScreenId);
}

/// Audio playback, implemented by the platform layer.
pub trait AudioBackend: Send + Sync {
    fn push(&self, seq: u32, pcm: &[u8]);
    fn reset(&self);
}

pub struct NullVideo;
impl VideoBackend for NullVideo {
    fn start(&self, _: ScreenId, _: u64, _: VideoCodec, _: Rect) {}
    fn frame(&self, _: ScreenId, _: u64, _: Vec<u8>, _: u64, _: bool) {}
    fn stop(&self, _: ScreenId) {}
}

/// State shared with the render thread.
#[derive(Default)]
pub struct Shared {
    pub clock: Mutex<ClockSync>,
    pub rx_bytes: AtomicU64,
    pub connected: AtomicBool,
    /// Set once `NetCmd::Quit` was handled; the client stops reconnecting.
    pub quitting: AtomicBool,
}

pub struct NetConfig {
    pub files_dir: PathBuf,
    pub device_name: String,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
struct PinnedServer {
    name: String,
    fingerprint: String,
    addr: Option<String>,
}

/// Optional `files/config.toml` (push with adb): `host = "192.168.1.20"`, `port = 47800`.
#[derive(Deserialize, Default, Debug)]
struct UserConfig {
    host: Option<String>,
    port: Option<u16>,
}

pub struct NetHandle {
    pub cmd: tmpsc::UnboundedSender<NetCmd>,
    pub events: mpsc::Receiver<NetEvent>,
    pub shared: Arc<Shared>,
}

pub fn spawn(cfg: NetConfig, video: Arc<dyn VideoBackend>, audio: Arc<dyn AudioBackend>) -> NetHandle {
    let (cmd_tx, cmd_rx) = tmpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::channel();
    let shared = Arc::new(Shared::default());
    let sh = shared.clone();
    std::thread::Builder::new()
        .name("hyprvr-net".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(3)
                .enable_all()
                .thread_name("hyprvr-tokio")
                .build()
                .expect("tokio runtime");
            rt.block_on(run(cfg, video, audio, sh, ev_tx, cmd_rx));
        })
        .expect("spawn net thread");
    NetHandle { cmd: cmd_tx, events: ev_rx, shared }
}

struct Ctx {
    cfg: NetConfig,
    video: Arc<dyn VideoBackend>,
    audio: Arc<dyn AudioBackend>,
    shared: Arc<Shared>,
    ev: mpsc::Sender<NetEvent>,
    identity: Identity,
}

impl Ctx {
    fn emit(&self, e: NetEvent) {
        let _ = self.ev.send(e);
    }
    fn status(&self, s: impl Into<String>) {
        let s = s.into();
        info!("status: {s}");
        self.emit(NetEvent::Status(s));
    }
    fn pinned(&self) -> Option<PinnedServer> {
        let s = std::fs::read_to_string(self.cfg.files_dir.join("server.toml")).ok()?;
        toml::from_str(&s).ok()
    }
    fn save_pinned(&self, p: &PinnedServer) {
        match toml::to_string(p) {
            Ok(s) => {
                if let Err(e) = std::fs::write(self.cfg.files_dir.join("server.toml"), s) {
                    error!("saving server.toml: {e}");
                }
            }
            Err(e) => error!("serializing pinned server: {e}"),
        }
    }
    fn forget(&self) {
        let _ = std::fs::remove_file(self.cfg.files_dir.join("server.toml"));
        info!("forgot pinned server");
    }
    fn user_config(&self) -> UserConfig {
        std::fs::read_to_string(self.cfg.files_dir.join("config.toml"))
            .ok()
            .and_then(|s| toml::from_str(&s).map_err(|e| warn!("config.toml: {e}")).ok())
            .unwrap_or_default()
    }
}

async fn run(
    cfg: NetConfig,
    video: Arc<dyn VideoBackend>,
    audio: Arc<dyn AudioBackend>,
    shared: Arc<Shared>,
    ev: mpsc::Sender<NetEvent>,
    mut cmd_rx: tmpsc::UnboundedReceiver<NetCmd>,
) {
    let identity = match Identity::load_or_generate(&cfg.files_dir, "client") {
        Ok(i) => i,
        Err(e) => {
            error!("client identity: {e}; using an ephemeral one");
            Identity::generate("hyprvr")
        }
    };
    info!("client fingerprint {}", tls::short_fingerprint(&identity.fingerprint()));
    let ctx = Arc::new(Ctx { cfg, video, audio, shared, ev, identity });
    let mut endpoint = match quinn::Endpoint::client(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)) {
        Ok(e) => e,
        Err(e) => {
            ctx.status(format!("Network unavailable: {e}"));
            return;
        }
    };
    endpoint.set_default_client_config(tls::client_config(&ctx.identity));
    let mut backoff = Duration::from_millis(500);
    loop {
        match session(&ctx, &endpoint, &mut cmd_rx).await {
            Ok(()) => backoff = Duration::from_millis(500),
            Err(e) => {
                warn!("session ended: {e:#}");
                ctx.emit(NetEvent::Disconnected(format!("{e:#}")));
            }
        }
        ctx.shared.connected.store(false, Ordering::Relaxed);
        ctx.audio.reset();
        // Drain commands that piled up while disconnected (except Forget / Quit).
        while let Ok(c) = cmd_rx.try_recv() {
            match c {
                NetCmd::ForgetServer => ctx.forget(),
                NetCmd::Quit => ctx.shared.quitting.store(true, Ordering::Relaxed),
                _ => {}
            }
        }
        if ctx.shared.quitting.load(Ordering::Relaxed) {
            // Let the CONNECTION_CLOSE go out before the process dies.
            let _ = tokio::time::timeout(Duration::from_millis(400), endpoint.wait_idle()).await;
            info!("network stopped (quit)");
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(4));
    }
}

/// Candidate server addresses in priority order.
async fn candidates(ctx: &Ctx) -> Vec<(SocketAddr, Option<Fingerprint>, String)> {
    let mut out = Vec::new();
    let uc = ctx.user_config();
    let port = uc.port.unwrap_or(hyprvr_proto::DEFAULT_PORT);
    if let Some(h) = &uc.host {
        match tokio::net::lookup_host((h.as_str(), port)).await {
            Ok(addrs) => out.extend(addrs.filter(|a| a.is_ipv4()).map(|a| (a, None, h.clone()))),
            Err(e) => warn!("config host {h}: {e}"),
        }
    }
    let pinned = ctx.pinned();
    let pinned_fp = pinned.as_ref().and_then(|p| hex_fp(&p.fingerprint));
    match discover().await {
        Ok(replies) => {
            for (ip, r) in replies {
                if pinned_fp.is_some_and(|fp| fp != r.fingerprint) {
                    info!("ignoring unpaired server {} at {ip}", r.name);
                    continue;
                }
                out.push((SocketAddr::new(ip, r.port), Some(r.fingerprint), r.name));
            }
        }
        Err(e) => warn!("discovery: {e:#}"),
    }
    if let Some(p) = &pinned
        && let Some(a) = p.addr.as_ref().and_then(|a| a.parse().ok())
        && !out.iter().any(|(x, _, _)| *x == a)
    {
        out.push((a, pinned_fp, p.name.clone()));
    }
    out
}

fn hex_fp(s: &str) -> Option<Fingerprint> {
    let v = hex::decode(s).ok()?;
    v.try_into().ok()
}

async fn discover() -> Result<Vec<(IpAddr, discovery::Reply)>> {
    let sock = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    sock.set_broadcast(true)?;
    let mut targets = vec![Ipv4Addr::BROADCAST];
    targets.extend(directed_broadcasts());
    for t in &targets {
        if let Err(e) = sock.send_to(discovery::REQUEST, (*t, hyprvr_proto::DISCOVERY_PORT)).await {
            warn!("discovery send to {t}: {e}");
        }
    }
    let mut out: Vec<(IpAddr, discovery::Reply)> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(700);
    let mut buf = [0u8; 1500];
    loop {
        match tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await {
            Err(_) => break,
            Ok(Err(e)) => return Err(e.into()),
            Ok(Ok((n, from))) => {
                if let Some(r) = discovery::Reply::decode(&buf[..n])
                    && !out.iter().any(|(ip, x)| *ip == from.ip() && x.fingerprint == r.fingerprint) {
                        info!("discovered {} at {}", r.name, from.ip());
                        out.push((from.ip(), r));
                    }
            }
        }
    }
    Ok(out)
}

/// Directed broadcast addresses of all up IPv4 interfaces.
fn directed_broadcasts() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    unsafe {
        let mut ifa: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifa) != 0 {
            return out;
        }
        let mut cur = ifa;
        while !cur.is_null() {
            let i = &*cur;
            if !i.ifa_addr.is_null()
                && !i.ifa_netmask.is_null()
                && (*i.ifa_addr).sa_family as i32 == libc::AF_INET
                && i.ifa_flags & libc::IFF_LOOPBACK as u32 == 0
                && i.ifa_flags & libc::IFF_UP as u32 != 0
            {
                let a = &*(i.ifa_addr as *const libc::sockaddr_in);
                let m = &*(i.ifa_netmask as *const libc::sockaddr_in);
                let ip = u32::from_be(a.sin_addr.s_addr);
                let mask = u32::from_be(m.sin_addr.s_addr);
                out.push(Ipv4Addr::from(ip | !mask));
            }
            cur = i.ifa_next;
        }
        libc::freeifaddrs(ifa);
    }
    out
}

async fn connect_any(ctx: &Ctx, endpoint: &quinn::Endpoint) -> Result<(quinn::Connection, String, SocketAddr)> {
    let cands = candidates(ctx).await;
    if cands.is_empty() {
        bail!("no hyprvr server found on the network (is vrd running?)");
    }
    let mut last = anyhow!("no candidates");
    for (addr, _fp, name) in cands {
        ctx.status(format!("Connecting to {name} ({addr})..."));
        let attempt = async {
            let c = endpoint.connect(addr, tls::SERVER_NAME)?;
            Ok::<_, anyhow::Error>(c.await?)
        };
        match tokio::time::timeout(Duration::from_secs(4), attempt).await {
            Ok(Ok(conn)) => return Ok((conn, name, addr)),
            Ok(Err(e)) => last = e.context(format!("connect {addr}")),
            Err(_) => last = anyhow!("connect {addr}: timeout"),
        }
    }
    Err(last)
}

async fn session(ctx: &Arc<Ctx>, endpoint: &quinn::Endpoint, cmd_rx: &mut tmpsc::UnboundedReceiver<NetCmd>) -> Result<()> {
    ctx.status("Searching for your PC...");
    let (conn, name, addr) = connect_any(ctx, endpoint).await?;
    let server_fp = tls::peer_fingerprint(&conn).context("server sent no certificate")?;
    let pinned = ctx.pinned();
    if let Some(p) = &pinned
        && hex_fp(&p.fingerprint).is_some_and(|fp| fp != server_fp)
    {
        conn.close(0u32.into(), b"identity mismatch");
        ctx.status("This PC's identity changed. Press Forget on the pairing pad to re-pair.");
        bail!("server fingerprint mismatch");
    }
    let (mut send, mut recv) = conn.open_bi().await?;
    write_msg(&mut send, &ClientMsg::Hello { version: hyprvr_proto::PROTO_VERSION, device_name: ctx.cfg.device_name.clone() }).await?;
    let welcome: ServerMsg = tokio::time::timeout(Duration::from_secs(5), read_msg(&mut recv)).await.context("no Welcome")??;
    let (server_name, paired) = match welcome {
        ServerMsg::Welcome { version, server_name, paired } => {
            if version != hyprvr_proto::PROTO_VERSION {
                ctx.status(format!("Version mismatch: PC speaks v{version}, headset v{}. Update both.", hyprvr_proto::PROTO_VERSION));
                bail!("protocol version mismatch");
            }
            (server_name, paired)
        }
        other => bail!("expected Welcome, got {other:?}"),
    };
    let display_name = if server_name.is_empty() { name } else { server_name };
    if !paired {
        pair(ctx, &conn, &mut send, &mut recv, cmd_rx, &display_name, server_fp).await?;
    }
    if paired && pinned.is_none() {
        info!("server already knows us; pinning {}", tls::short_fingerprint(&server_fp));
    }
    ctx.save_pinned(&PinnedServer { name: display_name.clone(), fingerprint: hex::encode(server_fp), addr: Some(addr.to_string()) });
    ctx.shared.connected.store(true, Ordering::Relaxed);
    ctx.emit(NetEvent::Connected { server: display_name.clone() });
    info!("connected to {display_name} at {addr}");

    // Control reader.
    let c2 = ctx.clone();
    let mut control_reader = tokio::spawn(async move {
        loop {
            let m: ServerMsg = match read_msg(&mut recv).await {
                Ok(m) => m,
                Err(FrameError::Closed) => return anyhow!("control stream closed"),
                Err(e) => return anyhow!("control stream: {e}"),
            };
            match m {
                ServerMsg::State(s) => c2.emit(NetEvent::State(s)),
                ServerMsg::CursorShape { serial, width, height, hot_x, hot_y, rgba } => {
                    c2.emit(NetEvent::CursorShape { serial, width, height, hot_x, hot_y, rgba })
                }
                ServerMsg::Pong { client_time_us, server_time_us } => {
                    c2.shared.clock.lock().unwrap().add(client_time_us, server_time_us, now_us());
                }
                ServerMsg::Error { message } => c2.status(format!("PC: {message}")),
                other => warn!("unexpected control message {other:?}"),
            }
        }
    });
    // Uni streams (screens, environments).
    let c3 = ctx.clone();
    let conn3 = conn.clone();
    let streams = tokio::spawn(async move {
        loop {
            match conn3.accept_uni().await {
                Ok(s) => {
                    let c = c3.clone();
                    tokio::spawn(async move {
                        if let Err(e) = uni_stream(&c, s).await {
                            warn!("stream: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    info!("accept_uni ended: {e}");
                    break;
                }
            }
        }
    });
    // Datagrams.
    let c4 = ctx.clone();
    let conn4 = conn.clone();
    let datagrams = tokio::spawn(async move {
        while let Ok(b) = conn4.read_datagram().await {
            match hyprvr_proto::msg::decode_datagram(&b) {
                Ok(Datagram::Cursor { seq, screen, x, y, shape_serial }) => {
                    c4.emit(NetEvent::Cursor { seq, screen, x, y, serial: shape_serial })
                }
                Ok(Datagram::Audio { seq, pcm }) => c4.audio.push(seq, &pcm),
                Ok(other) => warn!("unexpected datagram {other:?}"),
                Err(e) => warn!("bad datagram: {e}"),
            }
        }
    });
    let mut ping = tokio::time::interval(Duration::from_millis(250));
    let mut pings_sent = 0u32;
    let mut move_seq = 0u32;
    let result: Result<()> = loop {
        tokio::select! {
            e = &mut control_reader => break Err(e.unwrap_or_else(|j| anyhow!("control task: {j}"))),
            reason = conn.closed() => break Err(anyhow!("connection closed: {reason}")),
            _ = ping.tick() => {
                pings_sent += 1;
                // Fast pings first for quick clock convergence, then every 2 s.
                if (pings_sent <= 12 || pings_sent.is_multiple_of(8))
                    && let Err(e) = write_msg(&mut send, &ClientMsg::Ping { client_time_us: now_us() }).await {
                        break Err(e.into());
                    }
            }
            c = cmd_rx.recv() => match c {
                None => break Ok(()),
                Some(NetCmd::Send(m)) => {
                    if let Err(e) = write_msg(&mut send, &m).await { break Err(e.into()); }
                }
                Some(NetCmd::PointerMove { screen, x, y }) => {
                    move_seq = move_seq.wrapping_add(1);
                    let d = hyprvr_proto::msg::encode_datagram(&Datagram::PointerMove { seq: move_seq, screen, x, y });
                    if let Err(e) = conn.send_datagram(d.into()) { warn!("send datagram: {e}"); }
                }
                Some(NetCmd::Pin(_)) => {}
                Some(NetCmd::Quit) => {
                    ctx.shared.quitting.store(true, Ordering::Relaxed);
                    conn.close(hyprvr_proto::CLOSE_GOODBYE.into(), b"quit");
                    break Ok(());
                }
                Some(NetCmd::ForgetServer) => {
                    ctx.forget();
                    conn.close(hyprvr_proto::CLOSE_GOODBYE.into(), b"forget");
                    break Ok(());
                }
            },
        }
    };
    control_reader.abort();
    streams.abort();
    datagrams.abort();
    conn.close(0u32.into(), b"bye");
    result
}

async fn pair(
    ctx: &Ctx,
    conn: &quinn::Connection,
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    cmd_rx: &mut tmpsc::UnboundedReceiver<NetCmd>,
    server: &str,
    server_fp: Fingerprint,
) -> Result<()> {
    ctx.emit(NetEvent::NeedPin { server: server.to_string() });
    let pin = loop {
        tokio::select! {
            reason = conn.closed() => bail!("closed while waiting for PIN: {reason}"),
            c = cmd_rx.recv() => match c {
                None => bail!("shutting down"),
                Some(NetCmd::Pin(p)) => break p,
                Some(NetCmd::ForgetServer) => ctx.forget(),
                Some(NetCmd::Quit) => {
                    ctx.shared.quitting.store(true, Ordering::Relaxed);
                    conn.close(hyprvr_proto::CLOSE_GOODBYE.into(), b"quit");
                    bail!("quitting");
                }
                Some(_) => {}
            },
        }
    };
    let client_fp = ctx.identity.fingerprint();
    let (state, msg) = PairState::client(&pin);
    write_msg(send, &ClientMsg::PairStart { spake: msg }).await?;
    let reply: ServerMsg = tokio::time::timeout(Duration::from_secs(10), read_msg(recv)).await.context("pairing timeout")??;
    let key = match reply {
        ServerMsg::PairStart { spake } => state.finish(&spake)?,
        ServerMsg::PairFailed { reason } => {
            ctx.emit(NetEvent::PairFailed(reason.clone()));
            bail!("pairing refused: {reason}");
        }
        other => bail!("expected PairStart, got {other:?}"),
    };
    let mac = pairing::confirm_mac(&key, Role::Client, &server_fp, &client_fp);
    write_msg(send, &ClientMsg::PairConfirm { mac }).await?;
    let res: ServerMsg = tokio::time::timeout(Duration::from_secs(10), read_msg(recv)).await.context("pairing timeout")??;
    match res {
        ServerMsg::PairOk { mac } => {
            if pairing::verify_mac(&key, Role::Server, &server_fp, &client_fp, &mac).is_err() {
                ctx.emit(NetEvent::PairFailed("PC could not prove the PIN (possible interception)".into()));
                bail!("server confirmation MAC mismatch");
            }
            info!("paired with {server}");
            Ok(())
        }
        ServerMsg::PairFailed { reason } => {
            ctx.emit(NetEvent::PairFailed(reason.clone()));
            bail!("pairing failed: {reason}")
        }
        other => bail!("expected PairOk, got {other:?}"),
    }
}

async fn uni_stream(ctx: &Ctx, mut s: quinn::RecvStream) -> Result<()> {
    let first: StreamMsg = read_msg(&mut s).await?;
    let (screen, mut width, mut height) = match first {
        StreamMsg::ScreenHeader { screen, width, height } => (screen, width, height),
        StreamMsg::EnvironmentImage { name, data } => {
            ctx.shared.rx_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
            let decoded = tokio::task::spawn_blocking(move || decode_env(&data)).await?;
            match decoded {
                Ok((w, h, rgba)) => ctx.emit(NetEvent::Environment { name, width: w, height: h, rgba }),
                Err(e) => ctx.status(format!("Environment {name}: {e:#}")),
            }
            return Ok(());
        }
        other => bail!("unexpected first stream message {other:?}"),
    };
    info!("screen {screen} stream open {width}x{height}");
    ctx.emit(NetEvent::ScreenOpen { screen, width, height });
    let mut generation = 0u64;
    let mut video_on = false;
    let res: Result<()> = async {
        loop {
            let m: StreamMsg = match read_msg(&mut s).await {
                Ok(m) => m,
                Err(FrameError::Closed) => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            match m {
                StreamMsg::Tiles { capture_us, payload, .. } => {
                    ctx.shared.rx_bytes.fetch_add(payload.len() as u64, Ordering::Relaxed);
                    let mut rects = Vec::new();
                    let mut data = Vec::new();
                    let (w, h) = (width, height);
                    let r = hyprvr_proto::tiles::decode_tiles(&payload, |r, rgba| {
                        if r.x + r.w <= w && r.y + r.h <= h {
                            rects.push(r);
                            data.extend_from_slice(rgba);
                        }
                    });
                    if let Err(e) = r {
                        warn!("screen {screen}: bad tiles: {e}");
                        continue;
                    }
                    ctx.emit(NetEvent::Tiles { screen, capture_us, rects, data });
                }
                StreamMsg::VideoStart { codec, rect, .. } => {
                    generation += 1;
                    video_on = true;
                    ctx.video.start(screen, generation, codec, rect);
                    ctx.emit(NetEvent::VideoStart { screen, generation, rect });
                }
                StreamMsg::VideoFrame { capture_us, keyframe, data, .. } => {
                    ctx.shared.rx_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                    if video_on {
                        ctx.video.frame(screen, generation, data, capture_us, keyframe);
                    }
                }
                StreamMsg::VideoStop { .. } => {
                    video_on = false;
                    ctx.video.stop(screen);
                    ctx.emit(NetEvent::VideoStop { screen });
                }
                StreamMsg::Resized { width: w, height: h } => {
                    width = w;
                    height = h;
                    if video_on {
                        video_on = false;
                        ctx.video.stop(screen);
                        ctx.emit(NetEvent::VideoStop { screen });
                    }
                    ctx.emit(NetEvent::Resized { screen, width, height });
                }
                StreamMsg::ScreenHeader { .. } | StreamMsg::EnvironmentImage { .. } => {
                    warn!("screen {screen}: unexpected message in stream")
                }
            }
        }
    }
    .await;
    if video_on {
        ctx.video.stop(screen);
        ctx.emit(NetEvent::VideoStop { screen });
    }
    ctx.emit(NetEvent::ScreenClosed { screen });
    info!("screen {screen} stream closed");
    res
}

fn decode_env(data: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let img = image::load_from_memory(data).context("decode image")?.to_rgba8();
    let img = if img.width() > 4096 {
        let h = (img.height() as u64 * 4096 / img.width() as u64) as u32;
        image::imageops::resize(&img, 4096, h.max(1), image::imageops::FilterType::Triangle)
    } else {
        img
    };
    Ok((img.width(), img.height(), img.into_raw()))
}
