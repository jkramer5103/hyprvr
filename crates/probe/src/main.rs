//! `hyprvr-probe`: headless test client for vrd. Discovers/connects, pairs with a PIN,
//! keeps an RGBA canvas per screen (lossless tiles + software-decoded video), writes PNG
//! snapshots, prints latency / fps / bitrate and can inject pointer input.

mod decode;

use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use hyprvr_proto::framing::{FrameError, read_msg, write_msg};
use hyprvr_proto::msg::{
    AudioTarget, ClientMsg, ClientStats, Datagram, PointerButton, Rect, ScreenId, ServerMsg, ServerState, StreamMsg,
    VideoCodec, decode_datagram, encode_datagram,
};
use hyprvr_proto::pairing::{PairState, Role, confirm_mac, verify_mac};
use hyprvr_proto::tiles::apply_tiles;
use hyprvr_proto::tls::{Identity, SERVER_NAME, client_config, peer_fingerprint, short_fingerprint};
use hyprvr_proto::{DEFAULT_PORT, DISCOVERY_PORT, PROTO_VERSION, discovery};

#[derive(Parser)]
#[command(name = "hyprvr-probe", about = "Headless hyprvr test client")]
struct Args {
    /// Server address (host[:port]). Default: LAN discovery.
    #[arg(long)]
    server: Option<String>,
    /// PIN for pairing (only needed the first time).
    #[arg(long)]
    pin: Option<String>,
    /// Where the client certificate lives.
    #[arg(long, default_value = "/tmp/hyprvr-probe")]
    identity: PathBuf,
    #[arg(long, default_value = "hyprvr-probe")]
    name: String,
    /// Run for this many seconds (0 = forever).
    #[arg(long, default_value_t = 10.0)]
    duration: f64,
    /// Write `screen-<id>.png` here at the end (and every --snapshot-every seconds).
    #[arg(long)]
    snapshots: Option<PathBuf>,
    #[arg(long, default_value_t = 0.0)]
    snapshot_every: f64,
    /// Actions, `;`-separated, run in order: `wait S`, `move ID X Y`, `click ID X Y [left|right|middle]`,
    /// `scroll ID DX DY`, `add WxH`, `remove ID`, `resize ID WxH`, `audio laptop|headset`,
    /// `refresh ID`, `env NAME`, `snap`.
    #[arg(long, default_value = "")]
    actions: String,
    /// Print a stats line every second.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    stats: bool,
    /// Exit after pairing (don't stream).
    #[arg(long)]
    pair_only: bool,
}

#[derive(Default)]
struct Canvas {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    video: Option<(Rect, decode::Decoder)>,
    frames: u64,
    bytes: u64,
    video_frames: u64,
    latency_sum_ms: f64,
    latency_n: u64,
    video_active: bool,
}

#[derive(Default)]
struct Shared {
    canvases: HashMap<ScreenId, Canvas>,
    state: Option<ServerState>,
    /// server_us - client_us
    clock_offset_us: Option<i64>,
    rtt_us: Option<u64>,
    cursor: Option<(Option<ScreenId>, f32, f32, u32)>,
    cursor_shape: Option<(u32, u32, u32)>,
    audio_bytes: u64,
    audio_peak: i16,
    errors: Vec<String>,
}

type S = Arc<Mutex<Shared>>;

fn client_us(epoch: Instant) -> u64 {
    epoch.elapsed().as_micros() as u64
}

async fn discover() -> Result<SocketAddr> {
    let sock = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    sock.set_broadcast(true)?;
    let mut buf = [0u8; 512];
    for _ in 0..6 {
        for target in ["255.255.255.255", "127.0.0.1"] {
            let _ = sock.send_to(discovery::REQUEST, (target, DISCOVERY_PORT)).await;
        }
        if let Ok(Ok((n, from))) = tokio::time::timeout(Duration::from_millis(500), sock.recv_from(&mut buf)).await
            && let Some(r) = discovery::Reply::decode(&buf[..n])
        {
            println!("discovered '{}' at {} (id {})", r.name, from.ip(), short_fingerprint(&r.fingerprint));
            return Ok(SocketAddr::new(from.ip(), r.port));
        }
    }
    bail!("no hyprvr server answered discovery")
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    match run(args).await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("probe: {e:#}");
            std::process::exit(1);
        }
    }
}

async fn run(args: Args) -> Result<i32> {
    let addr = match &args.server {
        Some(s) => {
            let s = if s.contains(':') && !s.starts_with('[') && s.matches(':').count() == 1 { s.clone() } else { format!("{s}:{DEFAULT_PORT}") };
            s.to_socket_addrs()?.next().context("resolving server")?
        }
        None => discover().await?,
    };
    let id = Identity::load_or_generate(&args.identity, "probe")?;
    let mut ep = quinn::Endpoint::client(if addr.is_ipv6() { "[::]:0".parse()? } else { "0.0.0.0:0".parse()? })?;
    ep.set_default_client_config(client_config(&id));
    let conn = ep.connect(addr, SERVER_NAME)?.await.context("QUIC connect")?;
    let server_fp = peer_fingerprint(&conn).context("server certificate")?;
    let client_fp = id.fingerprint();
    println!("connected to {addr} (server id {})", short_fingerprint(&server_fp));
    let (mut tx, mut rx) = conn.open_bi().await?;
    write_msg(&mut tx, &ClientMsg::Hello { version: PROTO_VERSION, device_name: args.name.clone() }).await?;
    let paired = match read_msg::<_, ServerMsg>(&mut rx).await? {
        ServerMsg::Welcome { server_name, paired, .. } => {
            println!("welcome from '{server_name}', paired={paired}");
            paired
        }
        ServerMsg::Error { message } => bail!("server error: {message}"),
        m => bail!("unexpected {m:?}"),
    };
    if !paired {
        let Some(pin) = &args.pin else {
            println!("not paired: run again with --pin (see `vrd pin` / notification)");
            return Ok(3);
        };
        let (st, msg) = PairState::client(pin);
        write_msg(&mut tx, &ClientMsg::PairStart { spake: msg }).await?;
        let ServerMsg::PairStart { spake } = read_msg(&mut rx).await? else { bail!("expected PairStart") };
        let key = st.finish(&spake).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        write_msg(&mut tx, &ClientMsg::PairConfirm { mac: confirm_mac(&key, Role::Client, &server_fp, &client_fp) }).await?;
        match read_msg::<_, ServerMsg>(&mut rx).await {
            Ok(ServerMsg::PairOk { mac }) => {
                verify_mac(&key, Role::Server, &server_fp, &client_fp, &mac).map_err(|_| anyhow::anyhow!("server MAC invalid"))?;
                println!("PAIRING OK");
            }
            Ok(ServerMsg::PairFailed { reason }) => {
                println!("PAIRING FAILED: {reason}");
                return Ok(2);
            }
            Ok(m) => bail!("unexpected {m:?}"),
            Err(e) => {
                println!("PAIRING FAILED: connection ended ({e})");
                return Ok(2);
            }
        }
    }
    if args.pair_only {
        conn.close(0u32.into(), b"bye");
        ep.wait_idle().await;
        return Ok(0);
    }

    let epoch = Instant::now();
    let shared: S = Arc::new(Mutex::new(Shared::default()));
    let (ctl_tx, mut ctl_rx) = tokio::sync::mpsc::unbounded_channel::<ClientMsg>();
    tokio::spawn(async move {
        while let Some(m) = ctl_rx.recv().await {
            if write_msg(&mut tx, &m).await.is_err() {
                break;
            }
        }
    });
    // Control reader.
    {
        let sh = shared.clone();
        tokio::spawn(async move {
            loop {
                match read_msg::<_, ServerMsg>(&mut rx).await {
                    Ok(ServerMsg::State(s)) => {
                        println!(
                            "STATE screens={:?} audio={:?} env={:?} mirrorable={:?}",
                            s.screens.iter().map(|s| format!("#{} {} {}x{}", s.id, s.output, s.width, s.height)).collect::<Vec<_>>(),
                            s.audio,
                            s.settings.environment,
                            s.mirrorable
                        );
                        sh.lock().unwrap().state = Some(s);
                    }
                    Ok(ServerMsg::CursorShape { serial, width, height, .. }) => {
                        sh.lock().unwrap().cursor_shape = Some((serial, width, height));
                    }
                    Ok(ServerMsg::Pong { client_time_us, server_time_us }) => {
                        let now = client_us(epoch);
                        let rtt = now.saturating_sub(client_time_us);
                        let mid = client_time_us + rtt / 2;
                        let mut s = sh.lock().unwrap();
                        // Keep the sample with the lowest RTT (best offset estimate).
                        if s.rtt_us.is_none_or(|r| rtt <= r + 200) {
                            s.rtt_us = Some(rtt);
                            s.clock_offset_us = Some(server_time_us as i64 - mid as i64);
                        }
                    }
                    Ok(ServerMsg::Error { message }) => {
                        println!("SERVER ERROR: {message}");
                        sh.lock().unwrap().errors.push(message);
                    }
                    Ok(m) => println!("control: {m:?}"),
                    Err(FrameError::Closed) => break,
                    Err(e) => {
                        println!("control closed: {e}");
                        break;
                    }
                }
            }
        });
    }
    // Screen / environment streams.
    {
        let (sh, conn) = (shared.clone(), conn.clone());
        let snaps = args.snapshots.clone();
        tokio::spawn(async move {
            while let Ok(mut s) = conn.accept_uni().await {
                let (sh, snaps) = (sh.clone(), snaps.clone());
                tokio::spawn(async move {
                    if let Err(e) = screen_stream(&mut s, &sh, epoch, snaps.as_deref()).await {
                        println!("stream: {e:#}");
                    }
                });
            }
        });
    }
    // Datagrams.
    {
        let (sh, conn) = (shared.clone(), conn.clone());
        tokio::spawn(async move {
            while let Ok(b) = conn.read_datagram().await {
                match decode_datagram(&b) {
                    Ok(Datagram::Cursor { screen, x, y, shape_serial, .. }) => {
                        sh.lock().unwrap().cursor = Some((screen, x, y, shape_serial));
                    }
                    Ok(Datagram::Audio { pcm, .. }) => {
                        let mut s = sh.lock().unwrap();
                        s.audio_bytes += pcm.len() as u64;
                        for c in pcm.as_chunks::<2>().0 {
                            s.audio_peak = s.audio_peak.max(i16::from_le_bytes([c[0], c[1]]).saturating_abs());
                        }
                    }
                    _ => {}
                }
            }
        });
    }
    // Pings + stats.
    {
        let (sh, ctl) = (shared.clone(), ctl_tx.clone());
        let show = args.stats;
        tokio::spawn(async move {
            let mut i = 0u64;
            loop {
                for _ in 0..4 {
                    let _ = ctl.send(ClientMsg::Ping { client_time_us: client_us(epoch) });
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                i += 1;
                let mut s = sh.lock().unwrap();
                let mut lat_all = (0.0, 0u64);
                let mut parts = vec![];
                let mut ids: Vec<_> = s.canvases.keys().copied().collect();
                ids.sort();
                for id in ids {
                    let c = s.canvases.get_mut(&id).unwrap();
                    let lat = if c.latency_n > 0 { c.latency_sum_ms / c.latency_n as f64 } else { f64::NAN };
                    lat_all.0 += c.latency_sum_ms;
                    lat_all.1 += c.latency_n;
                    parts.push(format!(
                        "#{id} {}x{} {}fps ({} video) {:.2}Mbit/s lat {:.1}ms{}",
                        c.width,
                        c.height,
                        c.frames,
                        c.video_frames,
                        c.bytes as f64 * 8.0 / 1e6,
                        lat,
                        if c.video_active { " [video]" } else { "" }
                    ));
                    c.frames = 0;
                    c.video_frames = 0;
                    c.bytes = 0;
                    c.latency_sum_ms = 0.0;
                    c.latency_n = 0;
                }
                if show {
                    println!(
                        "t={i}s rtt {:.1}ms | {} | cursor {:?} shape {:?} | audio {:.0}kbit/s peak {}",
                        s.rtt_us.unwrap_or(0) as f64 / 1000.0,
                        parts.join(" | "),
                        s.cursor,
                        s.cursor_shape,
                        s.audio_bytes as f64 * 8.0 / 1000.0,
                        s.audio_peak
                    );
                }
                s.audio_bytes = 0;
                s.audio_peak = 0;
                let stats = ClientStats {
                    latency_ms: if lat_all.1 > 0 { (lat_all.0 / lat_all.1 as f64) as f32 } else { 0.0 },
                    decoded_fps: 0.0,
                    dropped_frames: 0,
                    render_fps: 0.0,
                };
                drop(s);
                let _ = ctl.send(ClientMsg::Stats(stats));
            }
        });
    }
    // Periodic snapshots.
    if let (Some(dir), true) = (&args.snapshots, args.snapshot_every > 0.0) {
        let (sh, dir, every) = (shared.clone(), dir.clone(), args.snapshot_every);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs_f64(every)).await;
                let _ = snapshot(&sh, &dir);
            }
        });
    }

    // Actions run once the server has attached us (first State), like a real client.
    let start = Instant::now();
    while shared.lock().unwrap().state.is_none() && start.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut seq = 0u32;
    for a in args.actions.split(';').map(str::trim).filter(|a| !a.is_empty()) {
        let w: Vec<&str> = a.split_whitespace().collect();
        let num = |i: usize| -> Result<f32> { w.get(i).context("missing argument")?.parse::<f32>().map_err(Into::into) };
        let size = |i: usize| -> Result<(u32, u32)> {
            let (a, b) = w.get(i).context("missing size")?.split_once('x').context("WxH")?;
            Ok((a.parse()?, b.parse()?))
        };
        println!("ACTION {a}");
        match w[0] {
            "wait" => tokio::time::sleep(Duration::from_secs_f32(num(1)?)).await,
            "move" => {
                seq += 1;
                let d = Datagram::PointerMove { seq, screen: num(1)? as u32, x: num(2)?, y: num(3)? };
                conn.send_datagram(encode_datagram(&d).into())?;
            }
            "click" => {
                let (screen, x, y) = (num(1)? as u32, num(2)?, num(3)?);
                let button = match w.get(4).copied().unwrap_or("left") {
                    "right" => PointerButton::Right,
                    "middle" => PointerButton::Middle,
                    _ => PointerButton::Left,
                };
                for pressed in [true, false] {
                    ctl_tx.send(ClientMsg::Button { screen, x, y, button, pressed })?;
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
            }
            "scroll" => ctl_tx.send(ClientMsg::Scroll { screen: num(1)? as u32, dx: num(2)?, dy: num(3)? })?,
            "add" => {
                let (width, height) = size(1)?;
                ctl_tx.send(ClientMsg::AddScreen { width, height })?
            }
            "remove" => ctl_tx.send(ClientMsg::RemoveScreen { id: num(1)? as u32 })?,
            "resize" => {
                let (width, height) = size(2)?;
                ctl_tx.send(ClientMsg::ResizeScreen { id: num(1)? as u32, width, height })?
            }
            "audio" => ctl_tx.send(ClientMsg::SetAudio(if w.get(1) == Some(&"headset") { AudioTarget::Headset } else { AudioTarget::Laptop }))?,
            "refresh" => ctl_tx.send(ClientMsg::RequestRefresh { screen: num(1)? as u32 })?,
            "env" => ctl_tx.send(ClientMsg::GetEnvironment { name: w.get(1).context("name")?.to_string() })?,
            "snap" => {
                if let Some(d) = &args.snapshots {
                    snapshot(&shared, d)?;
                }
            }
            other => bail!("unknown action {other}"),
        }
    }
    if args.duration > 0.0 {
        let left = Duration::from_secs_f64(args.duration).saturating_sub(start.elapsed());
        tokio::select! {
            _ = tokio::time::sleep(left) => {},
            e = conn.closed() => println!("connection closed: {e}"),
        }
    } else {
        println!("connection closed: {}", conn.closed().await);
    }
    if let Some(d) = &args.snapshots {
        snapshot(&shared, d)?;
    }
    conn.close(0u32.into(), b"bye");
    ep.wait_idle().await;
    Ok(0)
}

async fn screen_stream(s: &mut quinn::RecvStream, sh: &S, epoch: Instant, snaps: Option<&Path>) -> Result<()> {
    let id = match read_msg::<_, StreamMsg>(s).await? {
        StreamMsg::ScreenHeader { screen, width, height } => {
            println!("screen stream #{screen} {width}x{height}");
            let mut g = sh.lock().unwrap();
            let c = g.canvases.entry(screen).or_default();
            if (c.width, c.height) != (width, height) {
                *c = Canvas { width, height, rgba: vec![0; (width * height * 4) as usize], ..Default::default() };
            }
            screen
        }
        StreamMsg::EnvironmentImage { name, data } => {
            println!("ENVIRONMENT {name}: {} bytes", data.len());
            if let Some(d) = snaps {
                std::fs::create_dir_all(d)?;
                std::fs::write(d.join(format!("env-{}", name.replace('/', "_"))), &data)?;
            }
            return Ok(());
        }
        m => bail!("unexpected first stream message {m:?}"),
    };
    loop {
        let m = match read_msg::<_, StreamMsg>(s).await {
            Ok(m) => m,
            Err(FrameError::Closed) => {
                println!("screen stream #{id} ended");
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        let now = client_us(epoch);
        let mut g = sh.lock().unwrap();
        let offset = g.clock_offset_us;
        let c = g.canvases.get_mut(&id).unwrap();
        let lat = |capture_us: u64, c: &mut Canvas| {
            if let Some(o) = offset {
                let server_now = now as i64 + o;
                c.latency_sum_ms += (server_now - capture_us as i64) as f64 / 1000.0;
                c.latency_n += 1;
            }
        };
        match m {
            StreamMsg::Tiles { capture_us, payload, .. } => {
                c.bytes += payload.len() as u64;
                c.frames += 1;
                apply_tiles(&payload, &mut c.rgba, c.width, c.height)?;
                lat(capture_us, c);
            }
            StreamMsg::VideoStart { codec, rect, .. } => {
                println!("VIDEO START #{id} {codec:?} {}x{}+{}+{}", rect.w, rect.h, rect.x, rect.y);
                c.video = Some((rect, decode::Decoder::new(codec == VideoCodec::Hevc)?));
                c.video_active = true;
            }
            StreamMsg::VideoFrame { capture_us, data, keyframe, .. } => {
                c.bytes += data.len() as u64;
                c.frames += 1;
                c.video_frames += 1;
                let (w, h) = (c.width, c.height);
                if let Some((rect, dec)) = &mut c.video {
                    let rect = *rect;
                    for (fw, fh, rgba) in dec.decode(&data).with_context(|| format!("decoding (keyframe={keyframe})"))? {
                        // Blit the decoded picture (cropped to rect) into the canvas.
                        let cw = rect.w.min(fw).min(w - rect.x);
                        let ch = rect.h.min(fh).min(h - rect.y);
                        for y in 0..ch {
                            let src = (y * fw * 4) as usize;
                            let dst = (((rect.y + y) * w + rect.x) * 4) as usize;
                            c.rgba[dst..dst + cw as usize * 4].copy_from_slice(&rgba[src..src + cw as usize * 4]);
                        }
                    }
                }
                lat(capture_us, c);
            }
            StreamMsg::VideoStop { .. } => {
                println!("VIDEO STOP #{id}");
                c.video = None;
                c.video_active = false;
            }
            StreamMsg::Resized { width, height } => {
                println!("RESIZED #{id} {width}x{height}");
                *c = Canvas { width, height, rgba: vec![0; (width * height * 4) as usize], ..Default::default() };
            }
            m => println!("stream #{id}: unexpected {m:?}"),
        }
    }
}

fn snapshot(sh: &S, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let g = sh.lock().unwrap();
    for (id, c) in &g.canvases {
        let p = dir.join(format!("screen-{id}.png"));
        let f = std::io::BufWriter::new(std::fs::File::create(&p)?);
        let mut e = png::Encoder::new(f, c.width, c.height);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        e.write_header()?.write_image_data(&c.rgba)?;
        println!("SNAPSHOT {}", p.display());
    }
    Ok(())
}
