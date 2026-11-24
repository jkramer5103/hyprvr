//! `vrd`: hyprvr PC daemon + CLI.

mod audio;
mod compositor;
mod config;
mod daemon;
mod encode;
mod envfetch;
mod ipc;
mod layout;
mod net;
mod pipeline;
mod store;
mod wl;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use hyprvr_proto::msg::VideoCodec;
use hyprvr_proto::tls::Identity;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use crate::config::{CodecChoice, Config};
use crate::daemon::Daemon;
use crate::encode::HybridConfig;
use crate::ipc::{Request, Response, Status};

#[derive(Parser)]
#[command(name = "vrd", version, about = "hyprvr: multi-screen VR desktop daemon for Hyprland + Quest 3")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon (normally started by the systemd user service).
    Daemon,
    /// Show daemon status.
    Status {
        /// JSON output. With --follow this is the bar-module format (same as `vrd waybar`).
        #[arg(long)]
        json: bool,
        /// Keep printing on every change.
        #[arg(long, short)]
        follow: bool,
    },
    /// Show (and if needed create) the pairing PIN.
    Pin,
    /// Manage screens.
    Screen {
        #[command(subcommand)]
        cmd: ScreenCmd,
    },
    /// Mirror a physical output (e.g. eDP-1) into VR.
    Mirror { output: String },
    /// Route audio: laptop | headset | toggle.
    Audio { target: String },
    /// Layout commands.
    Layout {
        #[command(subcommand)]
        cmd: LayoutCmd,
    },
    /// Forget a paired headset: "all", its name, or a fingerprint prefix.
    Unpair {
        #[arg(default_value = "all")]
        which: String,
    },
    /// Environments (360° backgrounds in the headset).
    Env {
        #[command(subcommand)]
        cmd: EnvCmd,
    },
    /// Continuous JSON for a bar module (Waybar `custom` / quickshell). Same as `status --follow --json`.
    Waybar,
}

#[derive(Subcommand)]
enum ScreenCmd {
    /// List screens.
    List,
    /// Add a virtual screen, e.g. `vrd screen add 2560x1440`.
    Add {
        #[arg(default_value = "1920x1080")]
        size: String,
    },
    /// Remove a screen by id.
    Remove { id: u32 },
    /// Resize a virtual screen, e.g. `vrd screen resize 2 2560x1440`.
    Resize { id: u32, size: String },
}

#[derive(Subcommand)]
enum EnvCmd {
    /// Download the free recommended environments (~250 MB from Poly Haven, Wikimedia
    /// Commons and ESO) and convert them into the environments folder.
    Fetch,
    /// Show who made the recommended environments, and their licenses.
    Credits,
    /// Print the environments folder (drop your own equirectangular .jpg/.png files here).
    Dir,
}

#[derive(Subcommand)]
enum LayoutCmd {
    /// Put all panels back to the default arc.
    Reset,
}

fn parse_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s.split_once(['x', 'X']).context("size must look like 1920x1080")?;
    Ok((w.trim().parse()?, h.trim().parse()?))
}

fn main() {
    let cli = Cli::parse();
    let r = match cli.cmd {
        Cmd::Daemon => run_daemon(),
        Cmd::Status { json, follow } => cmd_status(json, follow),
        Cmd::Waybar => cmd_status(true, true),
        Cmd::Pin => request(&Request::Pin).map(|r| println!("{}", r.message)),
        Cmd::Screen { cmd } => match cmd {
            ScreenCmd::List => cmd_status(false, false),
            ScreenCmd::Add { size } => parse_size(&size).and_then(|(width, height)| simple(&Request::ScreenAdd { width, height })),
            ScreenCmd::Remove { id } => simple(&Request::ScreenRemove { id }),
            ScreenCmd::Resize { id, size } => {
                parse_size(&size).and_then(|(width, height)| simple(&Request::ScreenResize { id, width, height }))
            }
        },
        Cmd::Mirror { output } => simple(&Request::Mirror { output }),
        Cmd::Audio { target } => simple(&Request::Audio { target }),
        Cmd::Layout { cmd: LayoutCmd::Reset } => simple(&Request::LayoutReset),
        Cmd::Unpair { which } => simple(&Request::Unpair { which }),
        Cmd::Env { cmd } => match cmd {
            EnvCmd::Fetch => cmd_env_fetch(),
            EnvCmd::Credits => {
                cmd_env_credits();
                Ok(())
            }
            EnvCmd::Dir => Config::load().map(|c| println!("{}", c.environments_dir().display())),
        },
    };
    if let Err(e) = r {
        eprintln!("vrd: {e:#}");
        std::process::exit(1);
    }
}

// ------------------------------------------------------------------------- CLI client

fn cmd_env_fetch() -> Result<()> {
    use hyprvr_proto::environments::{RECOMMENDED, total_bytes};
    use std::io::IsTerminal;
    let dir = Config::load()?.environments_dir();
    let todo = envfetch::missing(&dir);
    if todo.is_empty() {
        envfetch::write_credits(&dir)?;
        println!("All {} recommended environments are already in {}", RECOMMENDED.len(), dir.display());
        return Ok(());
    }
    let mb: u64 = todo.iter().map(|r| r.bytes).sum::<u64>() / 1_000_000;
    println!(
        "Downloading {} of {} environments ({mb} MB of {} MB) into {}",
        todo.len(),
        RECOMMENDED.len(),
        total_bytes() / 1_000_000,
        dir.display()
    );
    let tty = std::io::stdout().is_terminal();
    let mut line = String::new();
    let mut out = std::io::stdout();
    let r = envfetch::fetch_missing(&dir, &mut |ev| match ev {
        envfetch::Event::Start { index, total, item } => {
            line = format!("[{}/{total}] {} ({}, {})", index + 1, item.title, item.author, item.license);
            if tty {
                let _ = write!(out, "\r\x1b[K{line}");
            } else {
                println!("{line}");
            }
            let _ = out.flush();
        }
        envfetch::Event::Progress { percent } if tty => {
            let _ = write!(out, "\r\x1b[K{line}  {percent}%");
            let _ = out.flush();
        }
        envfetch::Event::Converting if tty => {
            let _ = write!(out, "\r\x1b[K{line}  converting");
            let _ = out.flush();
        }
        envfetch::Event::Done if tty => println!("\r\x1b[K{line}  done"),
        envfetch::Event::Failed { error, .. } => {
            if tty {
                println!("\r\x1b[K{line}  FAILED");
            }
            eprintln!("    {error}");
        }
        _ => {}
    });
    // Let a running daemon tell the headset; fine if it isn't running.
    let _ = request(&Request::EnvironmentsChanged);
    let n = r?;
    println!("Added {n} environment(s). Credits: {}", dir.join("CREDITS.md").display());
    Ok(())
}

fn cmd_env_credits() {
    use hyprvr_proto::environments::RECOMMENDED;
    println!("Recommended environments (downloaded from the source by `vrd env fetch`):\n");
    for r in RECOMMENDED {
        println!("{}\n    {}, {}\n    {}\n", r.title, r.author, r.license, r.source);
    }
}

fn connect() -> Result<UnixStream> {
    UnixStream::connect(ipc::socket_path()).context("hyprvr daemon is not running (systemctl --user start vrd)")
}

fn request(req: &Request) -> Result<Response> {
    let mut s = connect()?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    s.write_all(line.as_bytes())?;
    let mut resp = String::new();
    BufReader::new(s).read_line(&mut resp)?;
    let r: Response = serde_json::from_str(&resp).context("bad reply from daemon")?;
    if !r.ok {
        bail!("{}", r.message);
    }
    Ok(r)
}

fn simple(req: &Request) -> Result<()> {
    let r = request(req)?;
    if !r.message.is_empty() {
        println!("{}", r.message);
    }
    Ok(())
}

fn print_status(s: &Status) {
    println!("hyprvr on {} — {} / encoder {} / UDP {} / id {}", s.server_name, s.compositor, s.encoder, s.port, s.fingerprint);
    match &s.connected {
        Some(c) => println!(
            "headset: {c}{}  {:.1} Mbit/s",
            s.latency_ms.map(|l| format!("  latency {l:.0} ms")).unwrap_or_default(),
            s.bitrate_kbps as f32 / 1000.0
        ),
        None => println!("headset: not connected"),
    }
    if let Some(pin) = &s.pairing_pin {
        println!("pairing PIN: {pin}");
    }
    if let Some(g) = s.grace_left_secs {
        println!("virtual screens are removed in {g}s");
    }
    println!("audio: {}", s.audio);
    println!("screens{}:", if s.outputs_live { "" } else { " (created when the headset connects)" });
    for sc in &s.screens {
        println!(
            "  #{:<2} {:<10} {:>4}x{:<4} {}{}{}",
            sc.id,
            sc.output.as_deref().unwrap_or("-"),
            sc.width,
            sc.height,
            sc.kind,
            if sc.video { "  [video]" } else { "" },
            if sc.encode_ms > 0.0 { format!("  encode {:.1} ms/frame", sc.encode_ms) } else { String::new() }
        );
    }
    if !s.paired_clients.is_empty() {
        println!("paired: {}", s.paired_clients.join(", "));
    }
}

fn cmd_status(json: bool, follow: bool) -> Result<()> {
    if !follow {
        let s = request(&Request::Status)?.status.context("no status")?;
        if json {
            println!("{}", serde_json::to_string_pretty(&s)?);
        } else {
            print_status(&s);
        }
        return Ok(());
    }
    // Follow mode never exits: it survives daemon restarts (bar modules expect that).
    let out = std::io::stdout();
    let emit = |s: Option<&Status>| {
        let mut o = out.lock();
        if json {
            let _ = writeln!(o, "{}", ipc::waybar_json(s));
        } else if let Some(s) = s {
            print_status(s);
            let _ = writeln!(o);
        } else {
            let _ = writeln!(o, "daemon not running");
        }
        let _ = o.flush();
    };
    loop {
        if let Ok(mut s) = connect() {
            let mut line = serde_json::to_string(&Request::Subscribe)?;
            line.push('\n');
            if s.write_all(line.as_bytes()).is_ok() {
                for l in BufReader::new(s).lines() {
                    let Ok(l) = l else { break };
                    if let Ok(st) = serde_json::from_str::<Status>(&l) {
                        emit(Some(&st));
                    }
                }
            }
        }
        emit(None);
        std::thread::sleep(Duration::from_secs(2));
    }
}

// ------------------------------------------------------------------------- daemon

/// Over SSH or from a bare systemd unit, the session variables may be missing: find the
/// running Hyprland instance and its Wayland socket in `$XDG_RUNTIME_DIR`.
fn discover_env() {
    let rt = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", rustix::process::getuid().as_raw())));
    // SAFETY: called at startup before any other thread exists.
    let set = |k: &str, v: &std::ffi::OsStr| unsafe { std::env::set_var(k, v) };
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        set("XDG_RUNTIME_DIR", rt.as_os_str());
    }
    let mut wayland_hint = None;
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none()
        && std::env::var_os("SWAYSOCK").is_none()
        && let Some((sig, dir)) = newest_hyprland(&rt.join("hypr"))
    {
        {
            tracing::info!("found Hyprland instance {sig}");
            set("HYPRLAND_INSTANCE_SIGNATURE", std::ffi::OsStr::new(&sig));
            // hyprland.lock: line 1 = pid, line 2 = wayland socket name.
            wayland_hint = std::fs::read_to_string(dir.join("hyprland.lock"))
                .ok()
                .and_then(|t| t.lines().nth(1).map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty());
        }
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        let candidates = wayland_hint.into_iter().chain(["wayland-1".to_string(), "wayland-0".to_string()]);
        for c in candidates {
            if UnixStream::connect(rt.join(&c)).is_ok() {
                set("WAYLAND_DISPLAY", std::ffi::OsStr::new(&c));
                break;
            }
        }
    }
}

fn newest_hyprland(dir: &Path) -> Option<(String, PathBuf)> {
    let mut best: Option<(std::time::SystemTime, String, PathBuf)> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        let sock = p.join(".socket.sock");
        if UnixStream::connect(&sock).is_err() {
            continue;
        }
        let t = std::fs::metadata(&sock).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().is_none_or(|b| t > b.0) {
            best = Some((t, e.file_name().to_string_lossy().into_owned(), p));
        }
    }
    best.map(|b| (b.1, b.2))
}

fn run_daemon() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("VRD_LOG").unwrap_or_else(|_| "info".into()))
        .init();
    discover_env();
    // SAFETY: plain global setter. Keeps libx264/VAAPI chatter out of the journal.
    #[allow(clippy::unnecessary_cast)] // the constant's type differs between FFmpeg versions
    let level = ffmpeg_sys_next::AV_LOG_WARNING as i32;
    unsafe { ffmpeg_sys_next::av_log_set_level(level) };
    let cfg = Config::load()?;
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(4).build()?;
    let handle = rt.handle().clone();
    rt.block_on(async move {
        let comp = compositor::detect()?;
        let identity = Identity::load_or_generate(&store::data_dir(), "server").context("loading server identity")?;
        let codec = match cfg.codec {
            CodecChoice::H264 => VideoCodec::H264,
            CodecChoice::Hevc => VideoCodec::Hevc,
        };
        let backend = {
            let (choice, node) = (cfg.encoder, cfg.render_node.clone());
            tokio::task::spawn_blocking(move || encode::video::probe_backend(choice, &node, codec)).await?
        };
        let (backend, encoder_name) = match backend {
            Ok(b) => {
                let n = format!("{} {}", b.name(), if codec == VideoCodec::H264 { "h264" } else { "hevc" });
                tracing::info!("video encoder: {n}");
                (Some(b), n)
            }
            Err(e) => {
                tracing::warn!("no video encoder ({e:#}); lossless tiles only");
                (None, "lossless-only".to_string())
            }
        };
        let kcfg = cfg.clone();
        let hybrid = HybridConfig {
            backend,
            codec,
            fps: cfg.refresh_hz,
            zstd_level: cfg.tile_zstd_level,
            tile_kbps_budget: cfg.tile_kbps_budget,
            kbps_for: Arc::new(move |w, h| kcfg.video_kbps_for(w, h)),
        };
        let d = {
            let h = handle.clone();
            tokio::task::spawn_blocking(move || Daemon::new(cfg, identity, comp, hybrid, encoder_name, h)).await?
        };
        tracing::info!(
            "hyprvr daemon '{}' on {} (id {}), UDP {}",
            d.server_name,
            d.kind(),
            hyprvr_proto::tls::short_fingerprint(&d.identity.fingerprint()),
            d.cfg.port
        );

        let ep = net::endpoint(&d)?;
        tokio::spawn(net::serve(d.clone(), ep.clone()));
        tokio::spawn(net::discovery_responder(d.clone()));
        let listener = ipc_listener()?;
        tokio::spawn(ipc_serve(d.clone(), listener));
        {
            let d = d.clone();
            tokio::spawn(async move {
                let mut iv = tokio::time::interval(Duration::from_secs(1));
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    iv.tick().await;
                    let d = d.clone();
                    let _ = tokio::task::spawn_blocking(move || d.tick()).await;
                }
            });
        }

        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        let mut int = signal(SignalKind::interrupt())?;
        tokio::select! {
            _ = term.recv() => {},
            _ = int.recv() => {},
        }
        tracing::info!("shutting down");
        ep.close(0u32.into(), b"daemon stopping");
        let d2 = d.clone();
        tokio::task::spawn_blocking(move || d2.shutdown()).await?;
        let _ = std::fs::remove_file(ipc::socket_path());
        anyhow::Ok(())
    })?;
    rt.shutdown_timeout(Duration::from_secs(1));
    Ok(())
}

fn ipc_listener() -> Result<tokio::net::UnixListener> {
    let path = ipc::socket_path();
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            bail!("another vrd daemon is already running ({})", path.display());
        }
        std::fs::remove_file(&path)?;
    }
    let l = tokio::net::UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

async fn ipc_serve(d: Arc<Daemon>, l: tokio::net::UnixListener) {
    while let Ok((s, _)) = l.accept().await {
        let d = d.clone();
        tokio::spawn(async move {
            if let Err(e) = ipc_client(d, s).await {
                tracing::debug!("ipc: {e:#}");
            }
        });
    }
}

async fn ipc_client(d: Arc<Daemon>, s: tokio::net::UnixStream) -> Result<()> {
    let (r, mut w) = s.into_split();
    let mut r = tokio::io::BufReader::new(r).take_line_limit();
    let mut line = String::new();
    r.read_line(&mut line).await?;
    let req: Request = serde_json::from_str(line.trim()).context("bad request")?;
    if req == Request::Subscribe {
        let mut rx = d.subscribe();
        loop {
            let st = rx.borrow_and_update().clone();
            let mut l = serde_json::to_string(&st)?;
            l.push('\n');
            w.write_all(l.as_bytes()).await?;
            if rx.changed().await.is_err() {
                return Ok(());
            }
        }
    }
    let resp = handle_request(&d, req).await;
    let mut l = serde_json::to_string(&resp)?;
    l.push('\n');
    w.write_all(l.as_bytes()).await?;
    Ok(())
}

/// Bound request lines (the socket is user-only, this is just hygiene).
trait LineLimit: Sized {
    fn take_line_limit(self) -> tokio::io::Take<Self>;
}
impl<R: tokio::io::AsyncRead> LineLimit for R {
    fn take_line_limit(self) -> tokio::io::Take<Self> {
        tokio::io::AsyncReadExt::take(self, 64 * 1024)
    }
}

async fn handle_request(d: &Arc<Daemon>, req: Request) -> Response {
    let d = d.clone();
    tokio::task::spawn_blocking(move || -> Response {
        let r: Result<String> = match req {
            Request::Status => return Response { ok: true, message: String::new(), status: Some(d.status()) },
            Request::Pin => Ok(d.pairing_pin()),
            Request::ScreenAdd { width, height } => d.add_screen(width, height).map(|id| format!("added screen #{id}")),
            Request::ScreenRemove { id } => d.remove_screen(id).map(|_| format!("removed screen #{id}")),
            Request::ScreenResize { id, width, height } => {
                d.resize_screen(id, width, height).map(|_| format!("screen #{id} resized"))
            }
            Request::Mirror { output } => d.add_mirror(&output).map(|id| format!("mirroring {output} as screen #{id}")),
            Request::Audio { target } => {
                let t = match target.as_str() {
                    "laptop" => Ok(Some(hyprvr_proto::msg::AudioTarget::Laptop)),
                    "headset" => Ok(Some(hyprvr_proto::msg::AudioTarget::Headset)),
                    "toggle" => Ok(None),
                    o => Err(anyhow::anyhow!("unknown audio target {o} (laptop|headset|toggle)")),
                };
                t.and_then(|t| d.set_audio(t)).map(|t| format!("audio: {}", format!("{t:?}").to_lowercase()))
            }
            Request::LayoutReset => {
                d.reset_layout();
                Ok("layout reset".into())
            }
            Request::Unpair { which } => d.unpair(&which).map(|n| format!("removed {n} paired headset(s)")),
            Request::EnvironmentsChanged => {
                d.environments_changed();
                Ok(String::new())
            }
            Request::Subscribe => unreachable!(),
        };
        match r {
            Ok(m) => Response::ok(m),
            Err(e) => Response::err(format!("{e:#}")),
        }
    })
    .await
    .unwrap_or_else(|e| Response::err(format!("internal error: {e}")))
}
