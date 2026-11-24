//! Touchscreen-like pinch semantics for hand tracking on a desktop panel:
//! a quick pinch clicks, pinch-and-move scrolls (content follows the hand),
//! pinch-hold-then-move presses the button and drags (selecting text, moving windows).

/// Hold this long without moving to start a button drag.
pub const HOLD_S: f32 = 0.35;
/// Hand travel per wheel notch = panel height / this (a notch scrolls ~3 lines in most apps,
/// so this is much slower than 1:1 on purpose).
pub const PX_PER_NOTCH_DIV: f32 = 5.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// Not decided yet; nothing sent.
    Pending,
    Scroll,
    /// Button is down.
    Drag,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Out {
    None,
    /// Press the button at (x, y) (start of a drag).
    Press { x: f32, y: f32 },
    /// Scroll by this many wheel notches (+y = down).
    Scroll { dx: f32, dy: f32 },
}

#[derive(Clone, Copy, Debug)]
pub struct PinchTouch {
    pub start: (f32, f32),
    last: (f32, f32),
    held: f32,
    slop_px: f32,
    px_per_notch: f32,
    pub mode: Mode,
}

impl PinchTouch {
    /// `panel_h_px`: panel height in pixels; thresholds scale with it.
    pub fn new(x: f32, y: f32, panel_h_px: u32) -> Self {
        let h = panel_h_px.max(240) as f32;
        PinchTouch { start: (x, y), last: (x, y), held: 0.0, slop_px: (h * 0.03).max(18.0), px_per_notch: h / PX_PER_NOTCH_DIV, mode: Mode::Pending }
    }

    /// `pos`: current hit point on the same panel (None when the ray left it).
    pub fn update(&mut self, pos: Option<(f32, f32)>, dt: f32) -> Out {
        match self.mode {
            Mode::Pending => {
                self.held += dt;
                if let Some(p) = pos {
                    let (dx, dy) = (p.0 - self.start.0, p.1 - self.start.1);
                    if (dx * dx + dy * dy).sqrt() > self.slop_px {
                        self.mode = Mode::Scroll;
                        // Begin scrolling from the slop circle, so there is no jump.
                        self.last = p;
                        return Out::None;
                    }
                }
                if self.held >= HOLD_S {
                    self.mode = Mode::Drag;
                    return Out::Press { x: self.start.0, y: self.start.1 };
                }
                Out::None
            }
            Mode::Scroll => {
                let Some(p) = pos else { return Out::None };
                let (dx, dy) = (p.0 - self.last.0, p.1 - self.last.1);
                self.last = p;
                // Content follows the hand: hand moves up (y decreases) => scroll down (+).
                Out::Scroll { dx: -dx / self.px_per_notch, dy: -dy / self.px_per_notch }
            }
            Mode::Drag => Out::None,
        }
    }

    /// True if releasing now should produce a click at `start`.
    pub fn is_click(&self) -> bool {
        self.mode == Mode::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_pinch_is_click() {
        let mut t = PinchTouch::new(100.0, 100.0, 1080);
        assert_eq!(t.update(Some((103.0, 101.0)), 0.1), Out::None);
        assert!(t.is_click());
    }

    #[test]
    fn hold_then_drag() {
        let mut t = PinchTouch::new(100.0, 100.0, 1080);
        assert_eq!(t.update(Some((101.0, 100.0)), 0.2), Out::None);
        assert_eq!(t.update(Some((101.0, 100.0)), 0.2), Out::Press { x: 100.0, y: 100.0 });
        assert_eq!(t.mode, Mode::Drag);
        assert!(!t.is_click());
        assert_eq!(t.update(Some((300.0, 100.0)), 0.016), Out::None);
    }

    #[test]
    fn move_scrolls_with_content_following_hand() {
        let mut t = PinchTouch::new(500.0, 500.0, 1080);
        assert_eq!(t.update(Some((500.0, 460.0)), 0.05), Out::None); // past slop: scroll mode
        assert_eq!(t.mode, Mode::Scroll);
        match t.update(Some((500.0, 28.0)), 0.016) {
            // Hand moved up 432 px = 2 notches at 216 px/notch => scroll down.
            Out::Scroll { dx, dy } => {
                assert!(dx.abs() < 1e-4);
                assert!((dy - 2.0).abs() < 1e-3, "{dy}");
            }
            o => panic!("{o:?}"),
        }
        // Scrolling never turns into a press, however long it's held.
        assert!(matches!(t.update(Some((500.0, 28.0)), 2.0), Out::Scroll { .. }));
        assert!(!t.is_click());
    }
}
