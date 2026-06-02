//! Cell model and character-width classification (L05).
//!
//! The [`Cell`] is the atom of the screen buffer — a base glyph, any trailing
//! combining marks, and truecolor attributes — and [`char_width`] classifies a
//! code point's display width per Unicode UAX #11.

use unicode_width::UnicodeWidthChar;

/// Default foreground color (white) used on reset and for blank cells.
pub const DEFAULT_FG: u32 = 0xFFFFFF;
/// Default background color (black) used on reset and for blank cells.
pub const DEFAULT_BG: u32 = 0x000000;

/// [`Cell::flags`] bit marking the trailing (second) cell of a double-width
/// character. The renderer skips these so the wide glyph occupies two columns.
pub const WIDE_TRAILER: u16 = 0b0000_0001;

/// Maximum number of trailing combining marks stored per cell.
pub const MAX_COMBINING: usize = 2;

/// Display width of `ch` in terminal cells: `0` for zero-width (combining
/// marks, joiners, variation selectors, …), `2` for wide East Asian / emoji
/// code points, and `1` otherwise.
///
/// Backed by the [`unicode-width`] crate, which implements the full Unicode
/// East Asian Width (UAX #11) and emoji-presentation property tables. Control
/// characters (for which the crate reports no width) collapse to `0`; the
/// parser handles C0/C1 controls before they ever reach this function.
///
/// [`unicode-width`]: https://docs.rs/unicode-width
pub fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// A single character cell: its base glyph, any trailing combining marks, and
/// truecolor attributes. Kept `Copy` (combining marks live in a fixed inline
/// array) so the grid can shift cells with `copy_within`.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Cell {
    /// The base (spacing) character.
    pub ch: char,
    /// Zero-width combining marks applied to `ch`; unused slots are `'\0'`.
    pub combining: [char; MAX_COMBINING],
    /// Foreground color as `0xRRGGBB`.
    pub fg: u32,
    /// Background color as `0xRRGGBB`.
    pub bg: u32,
    /// Attribute bitset (bold, italic, …). Reserved for future use.
    pub flags: u16,
    /// Hyperlink id (OSC 8): `0` for none, else an index+1 into `Grid::links`.
    pub link: u16,
}

impl Cell {
    /// Construct a blank cell (space glyph, default colors).
    pub(crate) fn blank() -> Self {
        Cell {
            ch: ' ',
            combining: ['\0'; MAX_COMBINING],
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            flags: 0,
            link: 0,
        }
    }
}
