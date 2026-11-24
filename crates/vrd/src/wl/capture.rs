//! Output capture with wlr-screencopy-unstable-v1 (`copy_with_damage`, SHM, no cursor).

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hyprvr_proto::msg::Rect;
use hyprvr_proto::tiles::{Frame, PixelFormat};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::{zwlr_screencopy_frame_v1 as frame_v1, zwlr_screencopy_manager_v1 as mgr_v1};

use super::{ShmFile, dispatch_timeout};

#[derive(Default)]
struct FrameState {
    info: Option<(wl_shm::Format, u32, u32, u32)>,
    buffer_done: bool,
    copied: bool,
    damage: Vec<Rect>,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

pub(crate) struct Globals {
    pub shm: Option<wl_shm::WlShm>,
    pub screencopy: Option<(mgr_v1::ZwlrScreencopyManagerV1, u32)>,
    /// (output, registry name, output name)
    pub outputs: Vec<(wl_output::WlOutput, u32, Option<String>)>,
}

struct State {
    g: Globals,
    frame: FrameState,
}

struct Buf {
    shm: ShmFile,
    _pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

impl Drop for Buf {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

pub struct Captured<'a> {
    pub frame: Frame<'a>,
    /// Damage reported by the compositor; `None` = treat everything as damaged.
    pub damage: Option<Vec<Rect>>,
    pub captured_at: Instant,
}

pub struct Capturer {
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    output: wl_output::WlOutput,
    pub output_name: String,
    buf: Option<Buf>,
    pending: Option<frame_v1::ZwlrScreencopyFrameV1>,
    flip: Vec<u8>,
    last_size: (u32, u32),
    first: bool,
}

/// Bind registry globals common to our Wayland clients and wait for output names.
pub(crate) fn connect<S>(conn: &Connection) -> Result<(EventQueue<S>, QueueHandle<S>)>
where
    S: 'static,
{
    let q = conn.new_event_queue::<S>();
    let qh = q.handle();
    Ok((q, qh))
}

impl Capturer {
    pub fn new(output_name: &str) -> Result<Self> {
        let conn = Connection::connect_to_env().context("connect to Wayland")?;
        let (mut queue, qh) = connect::<State>(&conn)?;
        let mut state = State { g: Globals { shm: None, screencopy: None, outputs: vec![] }, frame: FrameState::default() };
        conn.display().get_registry(&qh, ());
        queue.roundtrip(&mut state)?; // globals
        queue.roundtrip(&mut state)?; // output names
        if state.g.screencopy.is_none() {
            bail!("compositor lacks zwlr_screencopy_manager_v1");
        }
        if state.g.shm.is_none() {
            bail!("compositor lacks wl_shm");
        }
        let output = state
            .g
            .outputs
            .iter()
            .find(|(_, _, n)| n.as_deref() == Some(output_name))
            .map(|(o, _, _)| o.clone())
            .with_context(|| format!("output {output_name} not found"))?;
        Ok(Capturer {
            conn,
            queue,
            qh,
            state,
            output,
            output_name: output_name.to_string(),
            buf: None,
            pending: None,
            flip: vec![],
            last_size: (0, 0),
            first: true,
        })
    }

    fn ensure_buffer(&mut self, format: wl_shm::Format, w: u32, h: u32, stride: u32) -> Result<()> {
        if let Some(b) = &self.buf
            && (b.format, b.width, b.height, b.stride) == (format, w, h, stride)
        {
            return Ok(());
        }
        self.buf = None;
        let size = (stride * h) as usize;
        let shm = ShmFile::new(size)?;
        let pool = self.state.g.shm.as_ref().unwrap().create_pool(std::os::fd::AsFd::as_fd(&shm.fd), size as i32, &self.qh, ());
        let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, &self.qh, ());
        self.buf = Some(Buf { shm, _pool: pool, buffer, format, width: w, height: h, stride });
        Ok(())
    }

    /// Wait up to `timeout` for the next frame with damage. `Ok(None)` on timeout (the
    /// request stays pending). Errors mean the output is gone or the compositor failed.
    pub fn next_frame(&mut self, timeout: Duration) -> Result<Option<Captured<'_>>> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.pending.is_none() {
                self.state.frame = FrameState::default();
                let (mgr, _) = self.state.g.screencopy.as_ref().unwrap();
                self.pending = Some(mgr.capture_output(0, &self.output, &self.qh, ()));
            }
            let version = self.state.g.screencopy.as_ref().unwrap().1;
            let f = &self.state.frame;
            if f.failed {
                self.pending = None;
                bail!("screencopy failed for {}", self.output_name);
            }
            if f.ready {
                break;
            }
            if !f.copied
                && let Some((fmt, w, h, stride)) = f.info
                && (f.buffer_done || version < 3)
            {
                self.ensure_buffer(fmt, w, h, stride)?;
                let frame = self.pending.as_ref().unwrap();
                let buf = &self.buf.as_ref().unwrap().buffer;
                if self.first {
                    frame.copy(buf);
                } else {
                    frame.copy_with_damage(buf);
                }
                self.state.frame.copied = true;
                continue;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            dispatch_timeout(&self.conn, &mut self.queue, &mut self.state, left)?;
        }
        // Frame ready.
        if let Some(f) = self.pending.take() {
            f.destroy();
        }
        let captured_at = Instant::now();
        let b = self.buf.as_ref().unwrap();
        let format = match b.format {
            wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888 => PixelFormat::Bgrx,
            wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888 => PixelFormat::Rgbx,
            other => bail!("unsupported screencopy format {other:?}"),
        };
        let (w, h, stride) = (b.width, b.height, b.stride as usize);
        let resized = self.last_size != (w, h);
        self.last_size = (w, h);
        let full = self.first || resized;
        self.first = false;
        let mut damage = std::mem::take(&mut self.state.frame.damage);
        let data: &[u8] = if self.state.frame.y_invert {
            let src = b.shm.as_slice();
            self.flip.resize(src.len(), 0);
            for y in 0..h as usize {
                let s = &src[y * stride..(y + 1) * stride];
                let d = (h as usize - 1 - y) * stride;
                self.flip[d..d + stride].copy_from_slice(s);
            }
            for r in &mut damage {
                r.y = h.saturating_sub(r.y + r.h);
            }
            &self.flip
        } else {
            b.shm.as_slice()
        };
        let damage = if full || damage.is_empty() { None } else { Some(damage) };
        Ok(Some(Captured { frame: Frame { data, stride, width: w, height: h, format }, damage, captured_at }))
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(s: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        handle_registry(&mut s.g, reg, ev, qh);
    }
}

pub(crate) fn handle_registry<S>(g: &mut Globals, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, qh: &QueueHandle<S>)
where
    S: Dispatch<wl_shm::WlShm, ()> + Dispatch<mgr_v1::ZwlrScreencopyManagerV1, ()> + Dispatch<wl_output::WlOutput, u32> + 'static,
{
    match ev {
        wl_registry::Event::Global { name, interface, version } => match interface.as_str() {
            "wl_shm" => g.shm = Some(reg.bind(name, 1, qh, ())),
            "zwlr_screencopy_manager_v1" => {
                let v = version.min(3);
                g.screencopy = Some((reg.bind(name, v, qh, ()), v));
            }
            "wl_output" if version >= 4 => g.outputs.push((reg.bind(name, 4, qh, name), name, None)),
            _ => {}
        },
        wl_registry::Event::GlobalRemove { name } => g.outputs.retain(|(_, n, _)| *n != name),
        _ => {}
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(s: &mut Self, _: &wl_output::WlOutput, ev: wl_output::Event, id: &u32, _: &Connection, _: &QueueHandle<Self>) {
        output_event(&mut s.g, ev, *id);
    }
}

pub(crate) fn output_event(g: &mut Globals, ev: wl_output::Event, id: u32) {
    if let wl_output::Event::Name { name } = ev
        && let Some(o) = g.outputs.iter_mut().find(|o| o.1 == id)
    {
        o.2 = Some(name);
    }
}

impl Dispatch<frame_v1::ZwlrScreencopyFrameV1, ()> for State {
    fn event(s: &mut Self, _: &frame_v1::ZwlrScreencopyFrameV1, ev: frame_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let f = &mut s.frame;
        match ev {
            frame_v1::Event::Buffer { format: WEnum::Value(fmt), width, height, stride } => {
                f.info = Some((fmt, width, height, stride));
            }
            frame_v1::Event::BufferDone => f.buffer_done = true,
            frame_v1::Event::Flags { flags: WEnum::Value(fl) } => f.y_invert = fl.contains(frame_v1::Flags::YInvert),
            frame_v1::Event::Damage { x, y, width, height } => f.damage.push(Rect { x, y, w: width, h: height }),
            frame_v1::Event::Ready { .. } => f.ready = true,
            frame_v1::Event::Failed => f.failed = true,
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore mgr_v1::ZwlrScreencopyManagerV1);
