//! Core terminal logic for rusty_term.
//!
//! This module is the platform-independent heart of the emulator, split by
//! standards layer into focused submodules:
//!
//! - [`cell`] — the [`Cell`] atom and Unicode width classification (L05)
//! - [`grid`] — the [`Grid`] screen buffer: scrollback, alt screen, scrolling
//!   region, cursor (L06 state)
//! - [`parser`] — the [`AnsiParser`], a VT100/ECMA-48 escape-sequence state
//!   machine driving the grid (L06)
//! - [`color`] — the ANSI palette and SGR color resolution (L06)
//! - [`osc`] — OSC dispatch: window title, cwd, hyperlinks, clipboard (L08)
//!
//! The parser drives the grid through its semantic API; the grid hands the
//! renderer a [`DirtyFrame`] snapshot. The parser intentionally implements a
//! pragmatic subset of the VT100/ECMA-48 escape repertoire.

mod cell;
mod color;
mod grid;
mod osc;
mod parser;

pub use cell::{
    ATTR_BLINK, ATTR_BOLD, ATTR_DIM, ATTR_HIDDEN, ATTR_ITALIC, ATTR_MASK, ATTR_REVERSE,
    ATTR_STRIKE, ATTR_UNDERLINE, WIDE_TRAILER,
};
pub use grid::{DirtyFrame, Grid};
pub use parser::AnsiParser;

#[cfg(test)]
mod tests;
