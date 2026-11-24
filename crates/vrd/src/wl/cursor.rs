//! Cursor image + position via ext-image-copy-capture cursor sessions (Hyprland >= 0.50,
//! wlroots >= 0.19). One session per watched output. Falls back to nothing; the daemon
//! then polls the compositor for the position and uses a built-in arrow.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use wayland_client::protocol::{wl_buffer, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1 as src_v1, ext_output_image_capture_source_manager_v1 as osrc_mgr,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_cursor_session_v1 as csess, ext_image_copy_capture_frame_v1 as cframe,
    ext_image_copy_capture_manager_v1 as cmgr, ext_image_copy_capture_session_v1 as sess,
};

use super::{ShmFile, dispatch_timeout};

#[derive(Debug, Clone, PartialEq)]
pub enum CursorEvent {
    /// Hotspot position in output buffer pixels.
    Position { output: String, x: i32, y: i32 },
    Leave { output: String },
    /// Straight-alpha RGBA image.
    Shape { width: u32, height: u32, hot_x: i32, hot_y: i32, rgba: Vec<u8> },
}

pub struct CursorWatcher {
    tx: mpsc::Sender<Vec<String>>,
}

impl CursorWatcher {
    /// Set the outputs to watch.
    pub fn watch(&self, outputs: Vec<String>) {
        let _ = self.tx.send(outputs);
    }

    /// Start the watcher. Fails quickly if the compositor lacks the protocols.
    pub fn spawn(events: impl Fn(CursorEvent) + Send + 'static) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (ok_tx, ok_rx) = mpsc::channel();
        std::thread::Builder::new().name("cursor".into()).spawn(move || {
            if let Err(e) = run(rx, &ok_tx, events) {
                tracing::warn!("cursor capture: {e:#}");
                let _ = ok_tx.send(Err(e.to_string()));
            }
        })?;
        match ok_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(CursorWatcher { tx }),
            Ok(Err(e)) => bail!(e),
            Err(_) => bail!("cursor thread did not start"),
        }
    }
}

#[derive(Default)]
struct SessionState {
    size: Option<(u32, u32)>,
    format: Option<wl_shm::Format>,
    constraints_done: bool,
    stopped: bool,
    frame_ready: bool,
    frame_failed: bool,
    pending_hotspot: Option<(i32, i32)>,
    hotspot: (i32, i32),
}

struct Watched {
    cursor: csess::ExtImageCopyCaptureCursorSessionV1,
    session: sess::ExtImageCopyCaptureSessionV1,
    source: src_v1::ExtImageCaptureSourceV1,
    frame: Option<cframe::ExtImageCopyCaptureFrameV1>,
    buf: Option<(ShmFile, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, u32, u32, wl_shm::Format)>,
}

impl Drop for Watched {
    fn drop(&mut self) {
        if let Some(f) = self.frame.take() {
            f.destroy();
        }
        if let Some((_, pool, b, ..)) = self.buf.take() {
            b.destroy();
            pool.destroy();
        }
        self.session.destroy();
        self.cursor.destroy();
        self.source.destroy();
    }
}

struct State {
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    has_pointer: bool,
    osrc: Option<osrc_mgr::ExtOutputImageCaptureSourceManagerV1>,
    cmgr: Option<cmgr::ExtImageCopyCaptureManagerV1>,
    outputs: Vec<(wl_output::WlOutput, u32, Option<String>)>,
    sessions: HashMap<String, SessionState>,
    events: Vec<CursorEvent>,
}

fn run(rx: mpsc::Receiver<Vec<String>>, ok: &mpsc::Sender<Result<(), String>>, emit: impl Fn(CursorEvent)) -> Result<()> {
    let conn = Connection::connect_to_env().context("connect to Wayland")?;
    let mut q = conn.new_event_queue::<State>();
    let qh = q.handle();
    let mut st = State {
        shm: None,
        seat: None,
        has_pointer: false,
        osrc: None,
        cmgr: None,
        outputs: vec![],
        sessions: HashMap::new(),
        events: vec![],
    };
    conn.display().get_registry(&qh, ());
    q.roundtrip(&mut st)?;
    q.roundtrip(&mut st)?;
    let (Some(osrc), Some(mgr), Some(shm), Some(seat)) = (st.osrc.clone(), st.cmgr.clone(), st.shm.clone(), st.seat.clone()) else {
        bail!("compositor lacks ext-image-copy-capture cursor sessions");
    };
    let _ = ok.send(Ok(()));
    let mut pointer: Option<wl_pointer::WlPointer> = None;
    let mut want: Vec<String> = vec![];
    let mut watched: HashMap<String, Watched> = HashMap::new();
    let mut last_shape: Option<CursorEvent> = None;

    loop {
        loop {
            match rx.try_recv() {
                Ok(w) => want = w,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }
        if pointer.is_none() && st.has_pointer {
            pointer = Some(seat.get_pointer(&qh, ()));
        }
        // Reconcile sessions.
        watched.retain(|name, _| want.contains(name) && st.outputs.iter().any(|o| o.2.as_deref() == Some(name)));
        st.sessions.retain(|name, s| watched.contains_key(name) && !s.stopped);
        watched.retain(|name, _| st.sessions.contains_key(name));
        if let Some(ptr) = &pointer {
            for name in &want {
                if watched.contains_key(name) {
                    continue;
                }
                let Some(out) = st.outputs.iter().find(|o| o.2.as_deref() == Some(name.as_str())) else { continue };
                let source = osrc.create_source(&out.0, &qh, ());
                let cursor = mgr.create_pointer_cursor_session(&source, ptr, &qh, name.clone());
                let session = cursor.get_capture_session(&qh, name.clone());
                st.sessions.insert(name.clone(), SessionState::default());
                watched.insert(name.clone(), Watched { cursor, session, source, frame: None, buf: None });
            }
        }
        // Drive frames.
        for (name, w) in watched.iter_mut() {
            let s = st.sessions.get_mut(name).unwrap();
            if s.frame_ready || s.frame_failed {
                if let Some(f) = w.frame.take() {
                    f.destroy();
                }
                if s.frame_ready
                    && let Some((shm_file, _, _, bw, bh, fmt)) = &w.buf
                {
                    if let Some(h) = s.pending_hotspot.take() {
                        s.hotspot = h;
                    }
                    let rgba = to_rgba(shm_file.as_slice(), *bw, *bh, *fmt);
                    let ev = CursorEvent::Shape { width: *bw, height: *bh, hot_x: s.hotspot.0, hot_y: s.hotspot.1, rgba };
                    if last_shape.as_ref() != Some(&ev) {
                        last_shape = Some(ev.clone());
                        emit(ev);
                    }
                }
                s.frame_ready = false;
                s.frame_failed = false;
            }
            if w.frame.is_none()
                && s.constraints_done
                && let (Some((bw, bh)), Some(fmt)) = (s.size, s.format)
                && bw > 0
                && bh > 0
            {
                if w.buf.as_ref().is_none_or(|b| (b.3, b.4, b.5) != (bw, bh, fmt)) {
                    if let Some((_, pool, b, ..)) = w.buf.take() {
                        b.destroy();
                        pool.destroy();
                    }
                    let size = (bw * bh * 4) as usize;
                    let file = ShmFile::new(size)?;
                    let pool = shm.create_pool(std::os::fd::AsFd::as_fd(&file.fd), size as i32, &qh, ());
                    let b = pool.create_buffer(0, bw as i32, bh as i32, (bw * 4) as i32, fmt, &qh, ());
                    w.buf = Some((file, pool, b, bw, bh, fmt));
                }
                let f = w.session.create_frame(&qh, name.clone());
                f.attach_buffer(&w.buf.as_ref().unwrap().2);
                f.damage_buffer(0, 0, bw as i32, bh as i32);
                f.capture();
                w.frame = Some(f);
            }
        }
        for ev in st.events.drain(..) {
            emit(ev);
        }
        dispatch_timeout(&conn, &mut q, &mut st, Duration::from_millis(100))?;
    }
}

/// wl_shm ARGB8888/XRGB8888 (premultiplied, little endian BGRA bytes) -> straight RGBA.
fn to_rgba(src: &[u8], w: u32, h: u32, fmt: wl_shm::Format) -> Vec<u8> {
    let n = (w * h) as usize;
    let mut out = Vec::with_capacity(n * 4);
    let opaque = fmt == wl_shm::Format::Xrgb8888;
    for p in src[..n * 4].as_chunks::<4>().0 {
        let (b, g, r, a) = (p[0], p[1], p[2], if opaque { 255 } else { p[3] });
        if a == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
        } else if a == 255 {
            out.extend_from_slice(&[r, g, b, 255]);
        } else {
            let un = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
            out.extend_from_slice(&[un(r), un(g), un(b), a]);
        }
    }
    out
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(s: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match ev {
            wl_registry::Event::Global { name, interface, version } => match interface.as_str() {
                "wl_shm" => s.shm = Some(reg.bind(name, 1, qh, ())),
                "wl_seat" if s.seat.is_none() => s.seat = Some(reg.bind(name, version.min(7), qh, ())),
                "ext_output_image_capture_source_manager_v1" => s.osrc = Some(reg.bind(name, 1, qh, ())),
                "ext_image_copy_capture_manager_v1" => s.cmgr = Some(reg.bind(name, 1, qh, ())),
                "wl_output" if version >= 4 => s.outputs.push((reg.bind(name, 4, qh, name), name, None)),
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => s.outputs.retain(|o| o.1 != name),
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(s: &mut Self, _: &wl_output::WlOutput, ev: wl_output::Event, id: &u32, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = ev
            && let Some(o) = s.outputs.iter_mut().find(|o| o.1 == *id)
        {
            o.2 = Some(name);
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(s: &mut Self, _: &wl_seat::WlSeat, ev: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(c) } = ev {
            s.has_pointer = c.contains(wl_seat::Capability::Pointer);
        }
    }
}

impl Dispatch<csess::ExtImageCopyCaptureCursorSessionV1, String> for State {
    fn event(s: &mut Self, _: &csess::ExtImageCopyCaptureCursorSessionV1, ev: csess::Event, name: &String, _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            csess::Event::Position { x, y } => s.events.push(CursorEvent::Position { output: name.clone(), x, y }),
            csess::Event::Leave => s.events.push(CursorEvent::Leave { output: name.clone() }),
            csess::Event::Hotspot { x, y } => {
                if let Some(ss) = s.sessions.get_mut(name) {
                    ss.pending_hotspot = Some((x, y));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<sess::ExtImageCopyCaptureSessionV1, String> for State {
    fn event(s: &mut Self, _: &sess::ExtImageCopyCaptureSessionV1, ev: sess::Event, name: &String, _: &Connection, _: &QueueHandle<Self>) {
        let Some(ss) = s.sessions.get_mut(name) else { return };
        match ev {
            sess::Event::BufferSize { width, height } => {
                ss.size = Some((width, height));
                ss.constraints_done = false;
            }
            sess::Event::ShmFormat { format: WEnum::Value(f) } => {
                // Prefer ARGB8888 (cursors need alpha).
                if f == wl_shm::Format::Argb8888 || (ss.format.is_none() && f == wl_shm::Format::Xrgb8888) {
                    ss.format = Some(f);
                }
            }
            sess::Event::Done => ss.constraints_done = true,
            sess::Event::Stopped => ss.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<cframe::ExtImageCopyCaptureFrameV1, String> for State {
    fn event(s: &mut Self, _: &cframe::ExtImageCopyCaptureFrameV1, ev: cframe::Event, name: &String, _: &Connection, _: &QueueHandle<Self>) {
        let Some(ss) = s.sessions.get_mut(name) else { return };
        match ev {
            cframe::Event::Ready => ss.frame_ready = true,
            cframe::Event::Failed { .. } => ss.frame_failed = true,
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_pointer::WlPointer);
delegate_noop!(State: ignore osrc_mgr::ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ignore src_v1::ExtImageCaptureSourceV1);
delegate_noop!(State: ignore cmgr::ExtImageCopyCaptureManagerV1);

/// Built-in 16x24 arrow (fallback when the compositor can't give us the real cursor).
pub fn arrow_sprite() -> CursorEvent {
    const ART: [&str; 24] = [
        "X...............",
        "XX..............",
        "XOX.............",
        "XOOX............",
        "XOOOX...........",
        "XOOOOX..........",
        "XOOOOOX.........",
        "XOOOOOOX........",
        "XOOOOOOOX.......",
        "XOOOOOOOOX......",
        "XOOOOOOOOOX.....",
        "XOOOOOOOOOOX....",
        "XOOOOOOOOOOOX...",
        "XOOOOOOOOOOOOX..",
        "XOOOOOOXXXXXXXX.",
        "XOOOXOOX........",
        "XOOXXOOOX.......",
        "XOX..XOOX.......",
        "XX...XOOOX......",
        "X.....XOOX......",
        "......XOOOX.....",
        ".......XOOX.....",
        ".......XXX......",
        "................",
    ];
    let mut rgba = Vec::with_capacity(16 * 24 * 4);
    for row in ART {
        for c in row.bytes() {
            rgba.extend_from_slice(match c {
                b'X' => &[0, 0, 0, 255],
                b'O' => &[255, 255, 255, 255],
                _ => &[0, 0, 0, 0],
            });
        }
    }
    CursorEvent::Shape { width: 16, height: 24, hot_x: 0, hot_y: 0, rgba }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpremultiply() {
        let px = [64u8, 32, 16, 128]; // BGRA premultiplied
        let out = to_rgba(&px, 1, 1, wl_shm::Format::Argb8888);
        assert_eq!(out, vec![32, 64, 128, 128]);
    }

    #[test]
    fn arrow_dims() {
        let CursorEvent::Shape { width, height, rgba, .. } = arrow_sprite() else { panic!() };
        assert_eq!(rgba.len(), (width * height * 4) as usize);
    }
}
