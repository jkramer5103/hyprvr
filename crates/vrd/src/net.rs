//! QUIC server, LAN discovery responder and the per-connection protocol state machine.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hyprvr_proto::framing::{FrameError, read_msg, write_msg};
use hyprvr_proto::msg::{ClientMsg, Datagram, ServerMsg, StreamMsg, decode_datagram};
use hyprvr_proto::pairing::{PairState, Role, confirm_mac, verify_mac};
use hyprvr_proto::tls::{peer_fingerprint, server_config};
use hyprvr_proto::{DISCOVERY_PORT, PROTO_VERSION, discovery};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::daemon::Daemon;

/// Only answer discovery from local networks.
pub fn is_local(ip: IpAddr) -> bool {
    match ip {
        // Includes Tailscale/CGNAT 100.64.0.0/10.
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private() || v4.is_loopback() || v4.is_link_local() || (o[0] == 100 && (o[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_local(IpAddr::V4(v4));
            }
            v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00 || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

pub fn endpoint(d: &Daemon) -> Result<quinn::Endpoint> {
    let sc = server_config(&d.identity);
    // Dual-stack if IPv6 is available, else IPv4 only.
    match quinn::Endpoint::server(sc.clone(), SocketAddr::from(([0u16; 8], d.cfg.port))) {
        Ok(e) => Ok(e),
        Err(_) => quinn::Endpoint::server(sc, SocketAddr::from(([0, 0, 0, 0], d.cfg.port)))
            .with_context(|| format!("binding UDP port {}", d.cfg.port)),
    }
}

pub async fn serve(d: Arc<Daemon>, ep: quinn::Endpoint) {
    while let Some(inc) = ep.accept().await {
        if !is_local(inc.remote_address().ip()) {
            tracing::warn!("refusing connection from non-local {}", inc.remote_address());
            inc.refuse();
            continue;
        }
        let d = d.clone();
        tokio::spawn(async move {
            let addr = inc.remote_address();
            match inc.await {
                Ok(conn) => {
                    if let Err(e) = handle(d, conn.clone()).await {
                        tracing::info!("{addr}: {e:#}");
                        conn.close(1u32.into(), e.to_string().as_bytes());
                    }
                }
                Err(e) => tracing::debug!("{addr}: handshake: {e}"),
            }
        });
    }
}

pub async fn discovery_responder(d: Arc<Daemon>) {
    let sock = match tokio::net::UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("discovery disabled (UDP {DISCOVERY_PORT}: {e})");
            return;
        }
    };
    let reply = discovery::Reply::new(d.server_name.clone(), d.cfg.port, d.identity.fingerprint()).encode();
    let mut buf = [0u8; 64];
    loop {
        let Ok((n, from)) = sock.recv_from(&mut buf).await else { continue };
        if &buf[..n] == discovery::REQUEST && is_local(from.ip()) {
            let _ = sock.send_to(&reply, from).await;
        }
    }
}

async fn send(s: &mut quinn::SendStream, m: &ServerMsg) -> Result<()> {
    write_msg(s, m).await.context("control stream write")
}

async fn recv(r: &mut quinn::RecvStream) -> Result<ClientMsg> {
    match read_msg(r).await {
        Ok(m) => Ok(m),
        Err(FrameError::Closed) => bail!("control stream closed"),
        Err(e) => Err(e).context("control stream read"),
    }
}

fn clean_name(n: &str) -> String {
    let s: String = n.chars().filter(|c| !c.is_control()).take(48).collect();
    if s.trim().is_empty() { "headset".into() } else { s.trim().to_string() }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.expect("blocking task panicked")
}

async fn handle(d: Arc<Daemon>, conn: quinn::Connection) -> Result<()> {
    let client_fp = peer_fingerprint(&conn).context("client sent no certificate")?;
    let server_fp = d.identity.fingerprint();
    let (mut tx, mut rx) = timeout(Duration::from_secs(10), conn.accept_bi()).await.context("no control stream")??;
    let (version, device) = match timeout(Duration::from_secs(10), recv(&mut rx)).await.context("no Hello")?? {
        ClientMsg::Hello { version, device_name } => (version, clean_name(&device_name)),
        _ => bail!("expected Hello"),
    };
    if version != PROTO_VERSION {
        send(&mut tx, &ServerMsg::Error { message: format!("protocol version {version} unsupported (server: {PROTO_VERSION})") })
            .await?;
        bail!("client protocol version {version}");
    }
    let paired = {
        let d = d.clone();
        blocking(move || d.is_paired(&client_fp)).await
    };
    send(&mut tx, &ServerMsg::Welcome { version: PROTO_VERSION, server_name: d.server_name.clone(), paired }).await?;

    if !paired {
        let pin = {
            let d = d.clone();
            blocking(move || d.pairing_pin()).await
        };
        let res = timeout(Duration::from_secs(180), async {
            let ClientMsg::PairStart { spake } = recv(&mut rx).await? else { bail!("expected PairStart") };
            let (st, ours) = PairState::server(&pin);
            send(&mut tx, &ServerMsg::PairStart { spake: ours }).await?;
            let key = st.finish(&spake).ok();
            let ClientMsg::PairConfirm { mac } = recv(&mut rx).await? else { bail!("expected PairConfirm") };
            match key {
                Some(k) if verify_mac(&k, Role::Client, &server_fp, &client_fp, &mac).is_ok() => Ok(Some(k)),
                _ => Ok(None),
            }
        })
        .await
        .context("pairing timed out")??;
        match res {
            Some(key) => {
                {
                    let (d, name) = (d.clone(), device.clone());
                    blocking(move || d.pairing_succeeded(&name, &client_fp)).await;
                }
                send(&mut tx, &ServerMsg::PairOk { mac: confirm_mac(&key, Role::Server, &server_fp, &client_fp) }).await?;
            }
            None => {
                {
                    let d = d.clone();
                    blocking(move || d.pairing_failed()).await;
                }
                send(&mut tx, &ServerMsg::PairFailed { reason: "wrong PIN".into() }).await?;
                let _ = tx.finish();
                // Slow down guessing, and let the message reach the client.
                tokio::time::sleep(Duration::from_secs(1)).await;
                bail!("pairing failed (wrong PIN) for {device}");
            }
        }
    }

    // Authenticated from here on.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerMsg>();
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if write_msg(&mut tx, &m).await.is_err() {
                break;
            }
        }
        let _ = tx.finish();
    });
    let sid = {
        let (d, c, name, otx) = (d.clone(), conn.clone(), device.clone(), out_tx.clone());
        blocking(move || d.attach(c, name, client_fp, otx)).await
    };

    let dgram = {
        let (d, conn) = (d.clone(), conn.clone());
        tokio::spawn(async move {
            let mut last_seq: Option<u32> = None;
            while let Ok(b) = conn.read_datagram().await {
                if let Ok(Datagram::PointerMove { seq, screen, x, y }) = decode_datagram(&b) {
                    // Drop reordered (older) moves; tolerate wraparound.
                    if let Some(l) = last_seq
                        && seq.wrapping_sub(l) > u32::MAX / 2
                    {
                        continue;
                    }
                    last_seq = Some(seq);
                    d.pointer_move(screen, x, y);
                }
            }
        })
    };

    let result = control_loop(&d, &conn, sid, &mut rx, &out_tx).await;
    dgram.abort();
    {
        let d = d.clone();
        // A goodbye close code means the headset left on purpose.
        let reason = match conn.close_reason() {
            Some(r) => Some(r),
            None => timeout(Duration::from_millis(300), conn.closed()).await.ok(),
        };
        let clean = matches!(
            reason,
            Some(quinn::ConnectionError::ApplicationClosed(ref c)) if c.error_code == quinn::VarInt::from_u32(hyprvr_proto::CLOSE_GOODBYE)
        );
        blocking(move || d.detach(sid, clean)).await;
    }
    drop(out_tx);
    let _ = timeout(Duration::from_secs(1), writer).await;
    result
}

async fn control_loop(
    d: &Arc<Daemon>,
    conn: &quinn::Connection,
    sid: u64,
    rx: &mut quinn::RecvStream,
    out: &mpsc::UnboundedSender<ServerMsg>,
) -> Result<()> {
    loop {
        let m = tokio::select! {
            m = recv(rx) => m,
            e = conn.closed() => return match e {
                quinn::ConnectionError::ApplicationClosed(_) | quinn::ConnectionError::LocallyClosed => Ok(()),
                e => Err(e.into()),
            },
        };
        let m = match m {
            Ok(m) => m,
            Err(e) => {
                if conn.close_reason().is_some() {
                    return Ok(());
                }
                return Err(e);
            }
        };
        let err = |e: anyhow::Error| {
            let _ = out.send(ServerMsg::Error { message: format!("{e:#}") });
        };
        let dd = d.clone();
        match m {
            ClientMsg::Ping { client_time_us } => {
                let _ = out.send(ServerMsg::Pong { client_time_us, server_time_us: d.now_us() });
            }
            ClientMsg::Stats(s) => d.set_client_stats(sid, s),
            ClientMsg::Button { screen, x, y, button, pressed } => d.pointer_button(screen, x, y, button, pressed),
            ClientMsg::Scroll { screen, dx, dy } => d.pointer_scroll(screen, dx, dy),
            ClientMsg::RequestRefresh { screen } => d.request_refresh(screen),
            ClientMsg::SetLayout(l) => blocking(move || dd.set_layout(&l)).await,
            ClientMsg::SetSettings(s) => blocking(move || dd.set_settings(s)).await.unwrap_or_else(err),
            ClientMsg::AddScreen { width, height } => blocking(move || dd.add_screen(width, height)).await.map(|_| ()).unwrap_or_else(err),
            ClientMsg::AddMirror { output } => blocking(move || dd.add_mirror(&output)).await.map(|_| ()).unwrap_or_else(err),
            ClientMsg::RemoveScreen { id } => blocking(move || dd.remove_screen(id)).await.unwrap_or_else(err),
            ClientMsg::ResizeScreen { id, width, height } => {
                blocking(move || dd.resize_screen(id, width, height)).await.unwrap_or_else(err)
            }
            ClientMsg::SetAudio(t) => blocking(move || dd.set_audio(Some(t))).await.map(|_| ()).unwrap_or_else(err),
            ClientMsg::GetEnvironment { name } => {
                let (conn, out) = (conn.clone(), out.clone());
                tokio::spawn(async move {
                    let r = async {
                        let data = blocking({
                            let (dd, name) = (dd.clone(), name.clone());
                            move || dd.environment_data(&name)
                        })
                        .await?;
                        let mut s = conn.open_uni().await?;
                        // Below screen streams: an environment must not stall the desktop.
                        let _ = s.set_priority(-1);
                        write_msg(&mut s, &StreamMsg::EnvironmentImage { name, data }).await?;
                        s.finish()?;
                        anyhow::Ok(())
                    }
                    .await;
                    if let Err(e) = r {
                        let _ = out.send(ServerMsg::Error { message: format!("environment: {e:#}") });
                    }
                });
            }
            ClientMsg::GetEnvironmentThumbs => {
                let (conn, out) = (conn.clone(), out.clone());
                tokio::spawn(async move {
                    let r = async {
                        let mut s = conn.open_uni().await?;
                        let _ = s.set_priority(-2);
                        let names = dd.environments();
                        for name in names {
                            let thumb = blocking({
                                let (dd, name) = (dd.clone(), name.clone());
                                move || dd.environment_thumb(&name)
                            })
                            .await;
                            match thumb {
                                Ok(data) => write_msg(&mut s, &StreamMsg::EnvironmentThumb { name, data }).await?,
                                Err(e) => tracing::warn!("thumbnail {name}: {e:#}"),
                            }
                        }
                        s.finish()?;
                        anyhow::Ok(())
                    }
                    .await;
                    if let Err(e) = r {
                        let _ = out.send(ServerMsg::Error { message: format!("environment previews: {e:#}") });
                    }
                });
            }
            ClientMsg::Hello { .. } | ClientMsg::PairStart { .. } | ClientMsg::PairConfirm { .. } => {
                bail!("unexpected handshake message")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_addresses() {
        assert!(is_local("192.168.1.5".parse().unwrap()));
        assert!(is_local("10.0.0.1".parse().unwrap()));
        assert!(is_local("::ffff:172.16.0.9".parse().unwrap()));
        assert!(is_local("fe80::1".parse().unwrap()));
        assert!(is_local("fd00::1".parse().unwrap()));
        assert!(is_local("100.101.102.103".parse().unwrap()));
        assert!(!is_local("8.8.8.8".parse().unwrap()));
        assert!(!is_local("100.128.0.1".parse().unwrap()));
        assert!(!is_local("2001:db8::1".parse().unwrap()));
    }
}
