//! Thin helper for SGR/1006 encoded mouse input generation.

use crate::core::MouseModes;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MousePoint {
    pub col: usize,
    pub row: usize,
}

/// Which physical button an event concerns. Only the three SGR mouse
/// protocol models (xterm's button field): left/middle/right — side buttons
/// and anything else have no encoding and are simply not reported.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MouseButtonKind {
    #[default]
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseEventKind {
    Press,
    Release,
    /// Pointer motion. `dragging` is whether a button is currently held —
    /// distinguishes a `?1002` (button-event) drag from a `?1003` (any-event)
    /// idle hover, and picks which button number a drag reports.
    Move { dragging: bool },
    Scroll { lines: isize },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MouseEvent {
    pub point: MousePoint,
    pub kind: MouseEventKind,
    pub button: MouseButtonKind,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl MouseEvent {
    pub fn new_point(col: usize, row: usize) -> Self {
        Self {
            point: MousePoint { col, row },
            kind: MouseEventKind::Move { dragging: false },
            button: MouseButtonKind::Left,
            shift: false,
            alt: false,
            ctrl: false,
        }
    }

    pub fn with_button(mut self, pressed: bool) -> Self {
        self.kind = if pressed { MouseEventKind::Press } else { MouseEventKind::Release };
        self
    }

    /// Set which physical button this event concerns (default `Left`).
    pub fn with_button_kind(mut self, button: MouseButtonKind) -> Self {
        self.button = button;
        self
    }

    /// Mark this as pointer motion; `dragging` is whether a button is
    /// currently held (see [`MouseEventKind::Move`]).
    pub fn with_move(mut self, dragging: bool) -> Self {
        self.kind = MouseEventKind::Move { dragging };
        self
    }

    pub fn with_modifiers(mut self, shift: bool, alt: bool, ctrl: bool) -> Self {
        self.shift = shift;
        self.alt = alt;
        self.ctrl = ctrl;
        self
    }

    pub fn with_scroll(mut self, lines: isize) -> Self {
        self.kind = MouseEventKind::Scroll { lines };
        self
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SgrEncoder {
    base: usize,
}

impl SgrEncoder {
    pub(crate) fn new(modes: MouseModes) -> Self {
        Self { base: modes.base }
    }

    pub(crate) fn write(&self, e: MouseEvent, out: &mut Vec<u8>) {
        if self.base == 0 {
            return;
        }
        let MouseEvent { point, kind, button, shift, alt, ctrl } = e;
        let col = point.col.saturating_add(1);
        let row = point.row.saturating_add(1);
        let btn = match button {
            MouseButtonKind::Left => 0,
            MouseButtonKind::Middle => 1,
            MouseButtonKind::Right => 2,
        };

        let (cb, command) = match kind {
            MouseEventKind::Press => (btn, b'M'),
            MouseEventKind::Release => (3, b'm'),
            MouseEventKind::Move { dragging } => {
                // `?1000` (click-only) never reports motion; `?1002`
                // (button-event) only while a button is held; `?1003`
                // (any-event) reports every motion, idle or dragging. A
                // dragging motion reports the held button (xterm's "add 32
                // to the button number"); an idle `?1003` hover reports the
                // release code 3 + 32 = 35 (xterm's "no button" motion code).
                if self.base == 1000 || (self.base == 1002 && !dragging) {
                    return;
                }
                (32 + if dragging { btn } else { 3 }, b'M')
            }
            MouseEventKind::Scroll { lines: 0 } => return,
            MouseEventKind::Scroll { lines } if lines > 0 => (64, b'M'),
            MouseEventKind::Scroll { lines } if lines < 0 => (65, b'M'),
            MouseEventKind::Scroll { .. } => return,
        };

        let mut flags = 0u8;
        if shift { flags |= 4; }
        if alt { flags |= 8; }
        if ctrl { flags |= 16; }

        out.extend_from_slice(b"\x1b[<");
        out.extend_from_slice((cb | flags as usize).to_string().as_bytes());
        out.push(b';');
        out.extend_from_slice(col.to_string().as_bytes());
        out.push(b';');
        out.extend_from_slice(row.to_string().as_bytes());
        out.push(command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::MouseModes;

    fn enc(base: usize) -> SgrEncoder {
        SgrEncoder::new(MouseModes { base, extended: 0 })
    }

    #[test]
    fn press_encodes_sgr_at_one_based_coords() {
        let mut out = Vec::new();
        enc(1000).write(MouseEvent::new_point(0, 0).with_button(true), &mut out);
        assert_eq!(out, b"\x1b[<0;1;1M");
    }

    #[test]
    fn release_uses_lowercase_m_and_button_3() {
        let mut out = Vec::new();
        enc(1000).write(MouseEvent::new_point(4, 2).with_button(false), &mut out);
        assert_eq!(out, b"\x1b[<3;5;3m");
    }

    #[test]
    fn modifiers_or_into_button_field() {
        let mut out = Vec::new();
        let e = MouseEvent::new_point(0, 0).with_button(true).with_modifiers(true, false, true);
        enc(1002).write(e, &mut out);
        // button 0 | shift(4) | ctrl(16) = 20
        assert_eq!(out, b"\x1b[<20;1;1M");
    }

    #[test]
    fn scroll_up_and_down_use_buttons_64_65() {
        let mut up = Vec::new();
        enc(1000).write(MouseEvent::new_point(0, 0).with_scroll(3), &mut up);
        assert_eq!(up, b"\x1b[<64;1;1M");
        let mut down = Vec::new();
        enc(1000).write(MouseEvent::new_point(0, 0).with_scroll(-3), &mut down);
        assert_eq!(down, b"\x1b[<65;1;1M");
    }

    #[test]
    fn inactive_modes_emit_nothing() {
        let mut out = Vec::new();
        enc(0).write(MouseEvent::new_point(0, 0).with_button(true), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn right_and_middle_buttons_encode_their_own_number() {
        let mut right = Vec::new();
        enc(1000)
            .write(MouseEvent::new_point(0, 0).with_button(true).with_button_kind(MouseButtonKind::Right), &mut right);
        assert_eq!(right, b"\x1b[<2;1;1M");
        let mut middle = Vec::new();
        enc(1000)
            .write(MouseEvent::new_point(0, 0).with_button(true).with_button_kind(MouseButtonKind::Middle), &mut middle);
        assert_eq!(middle, b"\x1b[<1;1;1M");
    }

    #[test]
    fn mode_1000_never_reports_motion() {
        let mut out = Vec::new();
        enc(1000).write(MouseEvent::new_point(0, 0).with_move(true), &mut out);
        assert!(out.is_empty());
        let mut out2 = Vec::new();
        enc(1000).write(MouseEvent::new_point(0, 0).with_move(false), &mut out2);
        assert!(out2.is_empty());
    }

    #[test]
    fn mode_1002_reports_motion_only_while_dragging() {
        let mut idle = Vec::new();
        enc(1002).write(MouseEvent::new_point(0, 0).with_move(false), &mut idle);
        assert!(idle.is_empty(), "no button held: ?1002 stays quiet");

        let mut dragging = Vec::new();
        enc(1002).write(
            MouseEvent::new_point(4, 2).with_move(true).with_button_kind(MouseButtonKind::Left),
            &mut dragging,
        );
        // 32 (motion) + 0 (left) = 32.
        assert_eq!(dragging, b"\x1b[<32;5;3M");
    }

    #[test]
    fn mode_1003_reports_idle_hover_and_drag_motion() {
        let mut idle = Vec::new();
        enc(1003).write(MouseEvent::new_point(0, 0).with_move(false), &mut idle);
        // 32 (motion) + 3 (no button) = 35.
        assert_eq!(idle, b"\x1b[<35;1;1M");

        let mut dragging = Vec::new();
        enc(1003).write(
            MouseEvent::new_point(0, 0).with_move(true).with_button_kind(MouseButtonKind::Right),
            &mut dragging,
        );
        // 32 (motion) + 2 (right) = 34.
        assert_eq!(dragging, b"\x1b[<34;1;1M");
    }
}
