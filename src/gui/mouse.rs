//! Thin helper for SGR/1006 encoded mouse input generation.

use crate::core::grid::MouseModes;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MousePoint {
    pub col: usize,
    pub row: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseEventKind {
    Press,
    Release,
    Move,
    Scroll { lines: isize },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MouseEvent {
    pub point: MousePoint,
    pub kind: MouseEventKind,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl MouseEvent {
    pub fn new_point(col: usize, row: usize) -> Self {
        Self {
            point: MousePoint { col, row },
            kind: MouseEventKind::Move,
            shift: false,
            alt: false,
            ctrl: false,
        }
    }

    pub fn with_button(mut self, pressed: bool) -> Self {
        self.kind = if pressed { MouseEventKind::Press } else { MouseEventKind::Release };
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
        let MouseEvent { point, kind, shift, alt, ctrl, .. } = e;
        let col = point.col.saturating_add(1);
        let row = point.row.saturating_add(1);

        let (cb, command) = match (self.base, kind) {
            (1000 | 1002, MouseEventKind::Press) => (0, b'M'),
            (1000 | 1002, MouseEventKind::Release) => (3, b'm'),
            (1000 | 1002, MouseEventKind::Move) => (32, b'M'),
            (1003, MouseEventKind::Press) => (0, b'M'),
            (1003, MouseEventKind::Release) => (3, b'm'),
            (1003, MouseEventKind::Move) => (32, b'M'),
            (_, MouseEventKind::Scroll { lines: 0 }) => return,
            (_, MouseEventKind::Scroll { lines }) if lines > 0 => (64, b'M'),
            (_, MouseEventKind::Scroll { lines }) if lines < 0 => (65, b'M'),
            _ => return,
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
