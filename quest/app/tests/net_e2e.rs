//! End-to-end test of the headset networking against a minimal fake daemon on localhost:
//! PIN pairing, fingerprint pinning, State, a screen stream with tiles, video events,
//! cursor datagrams and Ping/Pong clock sync.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hyprvr_proto::framing::{read_msg, write_msg};
use hyprvr_proto::msg::*;
use hyprvr_proto::pairing::{self, PairState, Role};
use hyprvr_proto::tiles::{Frame, PixelFormat, encode_tiles};
use hyprvr_proto::tls::{self, Identity};
use hyprvr_quest::net::{self, AudioBackend, NetCmd, NetConfig, NetEvent, VideoBackend};

const PIN: &str = "424242";

#[derive(Default)]
struct RecVideo(Mutex<Vec<String>>);
impl VideoBackend for RecVideo {
    fn start(&self, screen: ScreenId, generation: u64, codec: VideoCodec, rect: Rect) {
        self.0.lock().unwrap().push(format!("start {screen} {generation} {codec:?} {}x{}", rect.w, rect.h));
    }
    fn frame(&self, screen: ScreenId, generation: u64, data: Vec<u8>, pts_us: u64, keyframe: bool) {
        self.0.lock().unwrap().push(format!("frame {screen} {generation} {} {pts_us} {keyframe}", data.len()));
    }
    fn stop(&self, screen: ScreenId) {
        self.0.lock().unwrap().push(format!("stop {screen}"));
    }
}

#[derive(Default)]
struct RecAudio(Mutex<Vec<u32>>);
impl AudioBackend for RecAudio {
    fn push(&self, seq: u32, _pcm: &[u8]) {
        self.0.lock().unwrap().push(seq);
    }
    fn reset(&self) {}
}

fn state() -> ServerState {
    ServerState {
        screens: vec![ScreenInfo { id: 1, output: "HYPRVR-1".into(), kind: ScreenKind::Virtual, width: 128, height: 64, refresh_hz: 60 }],
        layout: vec![PanelLayout {
            screen: 1,
            pose: Pose { position: [0.0, 1.5, -1.0], orientation: [0.0, 0.0, 0.0, 1.0] },
            width_m: 1.0,
            curvature: Curvature::Flat,
        }],
        audio: AudioTarget::Headset,
        settings: Settings::default(),
        environments: vec![],
        mirrorable: vec![],
    }
}

/// Fake daemon: one connection; pairs with PIN unless the client is already known.
async fn serve(endpoint: quinn::Endpoint, known: Arc<Mutex<Option<[u8; 32]>>>, server_fp: [u8; 32]) {
    while let Some(inc) = endpoint.accept().await {
        let conn = inc.await.unwrap();
        let client_fp = tls::peer_fingerprint(&conn).expect("client cert");
        let (mut send, mut recv) = conn.accept_bi().await.unwrap();
        let hello: ClientMsg = read_msg(&mut recv).await.unwrap();
        assert!(matches!(hello, ClientMsg::Hello { version: hyprvr_proto::PROTO_VERSION, .. }));
        let paired = *known.lock().unwrap() == Some(client_fp);
        write_msg(&mut send, &ServerMsg::Welcome { version: hyprvr_proto::PROTO_VERSION, server_name: "testbox".into(), paired }).await.unwrap();
        if !paired {
            let ClientMsg::PairStart { spake } = read_msg(&mut recv).await.unwrap() else { panic!("expected PairStart") };
            let (st, msg) = PairState::server(PIN);
            write_msg(&mut send, &ServerMsg::PairStart { spake: msg }).await.unwrap();
            let key = st.finish(&spake).unwrap();
            let ClientMsg::PairConfirm { mac } = read_msg(&mut recv).await.unwrap() else { panic!("expected PairConfirm") };
            if pairing::verify_mac(&key, Role::Client, &server_fp, &client_fp, &mac).is_err() {
                write_msg(&mut send, &ServerMsg::PairFailed { reason: "wrong PIN".into() }).await.unwrap();
                let _ = send.finish();
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            let ok = pairing::confirm_mac(&key, Role::Server, &server_fp, &client_fp);
            write_msg(&mut send, &ServerMsg::PairOk { mac: ok }).await.unwrap();
            *known.lock().unwrap() = Some(client_fp);
        }
        write_msg(&mut send, &ServerMsg::State(state())).await.unwrap();

        // Screen stream: header, tiles, a short video region, stop.
        let mut uni = conn.open_uni().await.unwrap();
        write_msg(&mut uni, &StreamMsg::ScreenHeader { screen: 1, width: 128, height: 64 }).await.unwrap();
        let px: Vec<u8> = (0..128 * 64).flat_map(|i| [(i % 256) as u8, 7, 9, 0]).collect();
        let frame = Frame { data: &px, stride: 128 * 4, width: 128, height: 64, format: PixelFormat::Rgbx };
        let payload = encode_tiles(&frame, &[Rect { x: 0, y: 0, w: 64, h: 64 }], 1);
        write_msg(&mut uni, &StreamMsg::Tiles { seq: 1, capture_us: 1000, payload }).await.unwrap();
        let rect = Rect { x: 0, y: 0, w: 64, h: 32 };
        write_msg(&mut uni, &StreamMsg::VideoStart { seq: 2, codec: VideoCodec::H264, rect }).await.unwrap();
        write_msg(&mut uni, &StreamMsg::VideoFrame { seq: 3, capture_us: 2000, keyframe: true, data: vec![0, 0, 0, 1, 0x67] }).await.unwrap();
        write_msg(&mut uni, &StreamMsg::VideoStop { seq: 4 }).await.unwrap();

        conn.send_datagram(encode_datagram(&Datagram::Cursor { seq: 5, screen: Some(1), x: 10.0, y: 20.0, shape_serial: 3 }).into()).unwrap();
        conn.send_datagram(encode_datagram(&Datagram::Audio { seq: 9, pcm: vec![0; 960] }).into()).unwrap();

        // Answer pings, record pointer moves, until the client goes away.
        let c2 = conn.clone();
        tokio::spawn(async move {
            while let Ok(d) = c2.read_datagram().await {
                if let Ok(Datagram::PointerMove { screen, x, y, .. }) = decode_datagram(&d) {
                    assert_eq!((screen, x, y), (1, 5.0, 6.0));
                    let _ = c2.send_datagram(encode_datagram(&Datagram::Cursor { seq: 6, screen: Some(1), x, y, shape_serial: 3 }).into());
                }
            }
        });
        loop {
            match read_msg::<_, ClientMsg>(&mut recv).await {
                Ok(ClientMsg::Ping { client_time_us }) => {
                    write_msg(&mut send, &ServerMsg::Pong { client_time_us, server_time_us: client_time_us + 5_000_000 }).await.unwrap();
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
}

fn wait<F: FnMut(&NetEvent) -> bool>(rx: &std::sync::mpsc::Receiver<NetEvent>, what: &str, mut f: F) -> NetEvent {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(e) if f(&e) => return e,
            Ok(_) => {}
            Err(_) => panic!("timed out waiting for {what}"),
        }
    }
}

#[test]
fn pairing_state_tiles_video_cursor_clock() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let server_id = Identity::generate("hyprvr");
    let server_fp = server_id.fingerprint();
    let endpoint = rt.block_on(async {
        quinn::Endpoint::server(tls::server_config(&server_id), SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap()
    });
    let port = endpoint.local_addr().unwrap().port();
    let known = Arc::new(Mutex::new(None));
    rt.spawn(serve(endpoint, known.clone(), server_fp));

    let dir = std::env::temp_dir().join(format!("hyprvr-net-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), format!("host = \"127.0.0.1\"\nport = {port}\n")).unwrap();

    let video = Arc::new(RecVideo::default());
    let audio = Arc::new(RecAudio::default());
    let h = net::spawn(NetConfig { files_dir: dir.clone(), device_name: "test".into() }, video.clone(), audio.clone());

    // Wrong PIN first: PairFailed, then the client reconnects and asks again.
    wait(&h.events, "NeedPin", |e| matches!(e, NetEvent::NeedPin { .. }));
    h.cmd.send(NetCmd::Pin("000000".into())).unwrap();
    wait(&h.events, "PairFailed", |e| matches!(e, NetEvent::PairFailed(_)));
    let NetEvent::NeedPin { server } = wait(&h.events, "NeedPin again", |e| matches!(e, NetEvent::NeedPin { .. })) else { unreachable!() };
    assert_eq!(server, "testbox");
    h.cmd.send(NetCmd::Pin(PIN.into())).unwrap();
    wait(&h.events, "Connected", |e| matches!(e, NetEvent::Connected { .. }));

    // Pinned server saved.
    let pinned = std::fs::read_to_string(dir.join("server.toml")).unwrap();
    assert!(pinned.contains(&hex::encode(server_fp)), "{pinned}");

    // Stream messages arrive in order; the cursor datagram may overtake any of them.
    let mut order: Vec<&str> = Vec::new();
    let mut cursor = false;
    wait(&h.events, "State/ScreenOpen/Tiles/Video/Cursor", |e| {
        match e {
            NetEvent::State(s) => {
                assert_eq!(s.screens[0].output, "HYPRVR-1");
                order.push("state");
            }
            NetEvent::ScreenOpen { screen: 1, width: 128, height: 64 } => order.push("open"),
            NetEvent::Tiles { rects, data, capture_us, .. } => {
                assert_eq!(*capture_us, 1000);
                let total: u32 = rects.iter().map(|r| r.w * r.h).sum();
                assert_eq!(data.len(), total as usize * 4);
                // Rgbx input -> RGBA with opaque alpha; red channel = (y*128 + x) % 256.
                let r0 = rects[0];
                assert_eq!(&data[0..4], &[((r0.y * 128 + r0.x) % 256) as u8, 7, 9, 255]);
                order.push("tiles");
            }
            NetEvent::VideoStart { screen: 1, generation: 1, .. } => order.push("vstart"),
            NetEvent::VideoStop { screen: 1 } => order.push("vstop"),
            NetEvent::Cursor { screen: Some(1), serial: 3, .. } => cursor = true,
            _ => {}
        }
        cursor && order.len() == 5
    });
    // State (control stream) and the screen stream are independent; within the screen stream order holds.
    order.retain(|x| *x != "state");
    assert_eq!(order, ["open", "tiles", "vstart", "vstop"]);

    // Pointer move datagram is echoed back as a cursor update.
    h.cmd.send(NetCmd::PointerMove { screen: 1, x: 5.0, y: 6.0 }).unwrap();
    wait(&h.events, "echoed Cursor", |e| matches!(e, NetEvent::Cursor { x, y, .. } if *x == 5.0 && *y == 6.0));

    let log = video.0.lock().unwrap().clone();
    assert_eq!(log, vec!["start 1 1 H264 64x32".to_string(), "frame 1 1 5 2000 true".into(), "stop 1".into()]);
    assert_eq!(*audio.0.lock().unwrap(), vec![9]);

    // Clock sync converged to the fake +5 s offset.
    std::thread::sleep(Duration::from_millis(800));
    let off = h.shared.clock.lock().unwrap().offset_us().expect("clock offset");
    assert!((off - 5_000_000).abs() < 50_000, "offset {off}");

    let _ = std::fs::remove_dir_all(&dir);
}
