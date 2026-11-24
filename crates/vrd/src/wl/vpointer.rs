//! Headset pointer input via wlr-virtual-pointer-unstable-v1 (one pointer per output).

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use wayland_client::protocol::{wl_output, wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1 as mgr_v1, zwlr_virtual_pointer_v1 as ptr_v1,
};

use super::dispatch_timeout;

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

/// Wheel scroll distance per notch (matches libinput's default 15 for a wheel click).
const AXIS_PER_NOTCH: f64 = 15.0;

#[derive(Debug, Clone)]
pub enum PointerCmd {
    /// Absolute position in output pixels (0..w, 0..h).
    Move { output: String, x: f64, y: f64, w: u32, h: u32 },
    Button { output: String, button: u32, pressed: bool },
    /// Notches; +dy scrolls down.
    Scroll { output: String, dx: f64, dy: f64 },
}

#[derive(Clone)]
pub struct VirtualPointer {
    tx: mpsc::Sender<PointerCmd>,
}

impl VirtualPointer {
    pub fn send(&self, c: PointerCmd) {
        let _ = self.tx.send(c);
    }

    /// Start the input thread. Fails if the compositor has no virtual pointer support.
    pub fn spawn() -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (ok_tx, ok_rx) = mpsc::channel();
        std::thread::Builder::new().name("vpointer".into()).spawn(move || {
            if let Err(e) = run(rx, ok_tx.clone()) {
                tracing::error!("virtual pointer thread: {e:#}");
                let _ = ok_tx.send(Err(e.to_string()));
            }
        })?;
        match ok_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(VirtualPointer { tx }),
            Ok(Err(e)) => bail!(e),
            Err(_) => bail!("virtual pointer thread did not start"),
        }
    }
}

struct State {
    seat: Option<wl_seat::WlSeat>,
    mgr: Option<(mgr_v1::ZwlrVirtualPointerManagerV1, u32)>,
    outputs: Vec<(wl_output::WlOutput, u32, Option<String>)>,
}

struct Ptr {
    p: ptr_v1::ZwlrVirtualPointerV1,
    acc: [f64; 2],
}

fn run(rx: mpsc::Receiver<PointerCmd>, ok: mpsc::Sender<Result<(), String>>) -> Result<()> {
    let conn = Connection::connect_to_env().context("connect to Wayland")?;
    let mut q = conn.new_event_queue::<State>();
    let qh = q.handle();
    let mut st = State { seat: None, mgr: None, outputs: vec![] };
    conn.display().get_registry(&qh, ());
    q.roundtrip(&mut st)?;
    q.roundtrip(&mut st)?;
    let Some((mgr, ver)) = st.mgr.clone() else { bail!("compositor lacks zwlr_virtual_pointer_manager_v1") };
    let seat = st.seat.clone().context("no wl_seat")?;
    let _ = ok.send(Ok(()));
    let start = Instant::now();
    let mut ptrs: HashMap<String, Ptr> = HashMap::new();
    loop {
        let cmd = match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(c) => Some(c),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };
        dispatch_timeout(&conn, &mut q, &mut st, Duration::ZERO)?;
        // Drop pointers of outputs that went away.
        ptrs.retain(|name, p| {
            let alive = st.outputs.iter().any(|o| o.2.as_deref() == Some(name.as_str()));
            if !alive {
                p.p.destroy();
            }
            alive
        });
        let Some(cmd) = cmd else { continue };
        let name = match &cmd {
            PointerCmd::Move { output, .. } | PointerCmd::Button { output, .. } | PointerCmd::Scroll { output, .. } => output.clone(),
        };
        if !ptrs.contains_key(&name) {
            let Some(out) = st.outputs.iter().find(|o| o.2.as_deref() == Some(name.as_str())) else {
                tracing::debug!("pointer event for unknown output {name}");
                continue;
            };
            let p = if ver >= 2 {
                mgr.create_virtual_pointer_with_output(Some(&seat), Some(&out.0), &qh, ())
            } else {
                mgr.create_virtual_pointer(Some(&seat), &qh, ())
            };
            // Let the compositor finish adding the device before we use it.
            q.roundtrip(&mut st)?;
            ptrs.insert(name.clone(), Ptr { p, acc: [0.0; 2] });
        }
        let ptr = ptrs.get_mut(&name).unwrap();
        let t = start.elapsed().as_millis() as u32;
        match cmd {
            PointerCmd::Move { x, y, w, h, .. } => {
                // 1/16 px precision.
                let (ew, eh) = (w.max(1) * 16, h.max(1) * 16);
                let px = ((x * 16.0).round().max(0.0) as u32).min(ew - 1);
                let py = ((y * 16.0).round().max(0.0) as u32).min(eh - 1);
                ptr.p.motion_absolute(t, px, py, ew, eh);
            }
            PointerCmd::Button { button, pressed, .. } => {
                let s = if pressed { wl_pointer::ButtonState::Pressed } else { wl_pointer::ButtonState::Released };
                ptr.p.button(t, button, s);
            }
            PointerCmd::Scroll { dx, dy, .. } => {
                ptr.p.axis_source(wl_pointer::AxisSource::Wheel);
                for (i, (v, axis)) in [(dx, wl_pointer::Axis::HorizontalScroll), (dy, wl_pointer::Axis::VerticalScroll)].into_iter().enumerate() {
                    if v == 0.0 {
                        continue;
                    }
                    ptr.acc[i] += v;
                    let discrete = ptr.acc[i].trunc();
                    ptr.acc[i] -= discrete;
                    if discrete != 0.0 {
                        ptr.p.axis_discrete(t, axis, v * AXIS_PER_NOTCH, discrete as i32);
                    } else {
                        ptr.p.axis(t, axis, v * AXIS_PER_NOTCH);
                    }
                }
            }
        }
        ptr.p.frame();
        conn.flush()?;
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(s: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match ev {
            wl_registry::Event::Global { name, interface, version } => match interface.as_str() {
                "wl_seat" if s.seat.is_none() => s.seat = Some(reg.bind(name, version.min(7), qh, ())),
                "zwlr_virtual_pointer_manager_v1" => {
                    let v = version.min(2);
                    s.mgr = Some((reg.bind(name, v, qh, ()), v));
                }
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

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore mgr_v1::ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore ptr_v1::ZwlrVirtualPointerV1);
