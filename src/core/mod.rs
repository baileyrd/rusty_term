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

#[cfg(any(test, feature = "gui"))]
mod arabic;
#[cfg(any(test, feature = "gui"))]
mod arabic_tables;
mod base64;
#[cfg(any(test, feature = "gui"))]
mod canon_tables;
#[cfg(any(test, feature = "gui"))]
mod bidi;
#[cfg(any(test, feature = "gui"))]
mod bidi_tables;
mod cell;
mod charset;
mod color;
mod gif;
mod grid;
mod inflate;
mod iterm;
mod jpeg;
mod kitty;
#[cfg(any(test, feature = "gui"))]
mod kitty_diacritics;
mod osc;
mod parser;
mod png;
mod sixel;
mod webp;

pub use cell::{
    ATTR_BLINK, ATTR_BOLD, ATTR_DIM, ATTR_HIDDEN, ATTR_ITALIC, ATTR_MASK, ATTR_REVERSE,
    ATTR_STRIKE, ATTR_UNDERLINE, ATTR_UNDERLINE_COLOR, UnderlineStyle, WIDE_TRAILER,
};
pub use color::Theme;
#[cfg(feature = "gui")]
pub(crate) use color::ensure_contrast;
pub use grid::{CursorShape, DirtyFrame, Grid, LineAttr, SCROLLBACK_MAX};
#[cfg(any(test, feature = "gui"))]
pub use grid::BlockMark;
#[cfg(any(test, feature = "gui"))]
pub use grid::BidiRow;
#[cfg(feature = "gui")]
pub(crate) use bidi::mirrored as bidi_mirrored;
#[cfg(feature = "gui")]
pub use cell::{Cell, char_width};
#[cfg(feature = "gui")]
pub use grid::{MouseModes, Selection};
#[cfg(feature = "gui")]
pub(crate) use base64::encode as base64_encode;
#[cfg(feature = "gui")]
pub(crate) use png::decode as png_decode;
pub use parser::AnsiParser;

#[cfg(test)]
mod tests;
