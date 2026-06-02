//! Core terminal logic for rusty_term.
//!
//! This module is the platform-independent heart of the emulator. It defines
//! the [`Grid`] (the authoritative screen state), the [`Cell`] that fills it,
//! the [`DirtyFrame`] handed to the renderer, and the [`AnsiParser`] that
//! drives the grid from a byte stream produced by the child shell.
//!
//! The parser intentionally implements a pragmatic subset of the VT100/ECMA-48
//! escape repertoire (SGR colors, cursor positioning, erase line/display).

/// Default foreground color (white) used on reset and for blank cells.
pub const DEFAULT_FG: u32 = 0xFFFFFF;
/// Default background color (black) used on reset and for blank cells.
pub const DEFAULT_BG: u32 = 0x000000;

/// Standard 16-color ANSI palette (indices 0-7 normal, 8-15 bright), in
/// `0xRRGGBB` form. Roughly matches the classic xterm palette.
const PALETTE_16: [u32; 16] = [
    0x000000, 0x800000, 0x008000, 0x808000, 0x000080, 0x800080, 0x008080, 0xC0C0C0,
    0x808080, 0xFF0000, 0x00FF00, 0xFFFF00, 0x0000FF, 0xFF00FF, 0x00FFFF, 0xFFFFFF,
];

/// A single character cell: its glyph plus truecolor attributes.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Cell {
    /// The displayed character.
    pub ch: char,
    /// Foreground color as `0xRRGGBB`.
    pub fg: u32,
    /// Background color as `0xRRGGBB`.
    pub bg: u32,
    /// Attribute bitset (bold, italic, …). Reserved for future use.
    pub flags: u16,
}

impl Cell {
    /// Construct a blank cell (space glyph, default colors).
    fn blank() -> Self {
        Cell { ch: ' ', fg: DEFAULT_FG, bg: DEFAULT_BG, flags: 0 }
    }
}

/// The authoritative screen buffer: a row-major grid of [`Cell`]s plus a
/// per-row damage (`dirty`) tracker and a cursor position.
pub struct Grid {
    /// Number of columns.
    pub cols: usize,
    /// Number of rows.
    pub rows: usize,
    /// Row-major cell storage; `len() == cols * rows`.
    pub cells: Vec<Cell>,
    /// Per-row damage flags; `len() == rows`.
    pub dirty: Vec<bool>,
    /// Cursor position as `(col, row)`, both zero-based.
    pub cursor: (usize, usize),
    /// Cursor position stashed by a save (`DECSC` / `CSI s`) and restored by
    /// `DECRC` / `CSI u`.
    pub saved_cursor: (usize, usize),
    /// Monotonic counter bumped on every parsed batch; lets the renderer
    /// reason about frame freshness.
    pub epoch: u64,
}

impl Grid {
    /// Create a `cols`×`rows` grid filled with blank cells.
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            cols,
            rows,
            cells: vec![Cell::blank(); cols * rows],
            dirty: vec![false; rows],
            cursor: (0, 0),
            saved_cursor: (0, 0),
            epoch: 0,
        }
    }

    /// Write `cell` at `(x, y)`, marking the row dirty. Out-of-bounds writes
    /// are silently ignored (the caller is responsible for clamping).
    pub fn set_cell(&mut self, x: usize, y: usize, cell: Cell) {
        if x < self.cols && y < self.rows {
            self.cells[y * self.cols + x] = cell;
            self.dirty[y] = true;
        }
    }

    /// Move the cursor to `(x, y)`, clamping into the grid so a malformed or
    /// oversized positioning sequence can never park the cursor off-screen.
    pub fn set_cursor(&mut self, x: usize, y: usize) {
        self.cursor = (
            x.min(self.cols.saturating_sub(1)),
            y.min(self.rows.saturating_sub(1)),
        );
    }

    /// Move the cursor to column 0 of the current row.
    pub fn carriage_return(&mut self) {
        self.cursor.0 = 0;
    }

    /// Advance the cursor one row, scrolling the viewport up if it would leave
    /// the bottom of the grid.
    pub fn newline(&mut self) {
        self.cursor.1 += 1;
        if self.cursor.1 >= self.rows {
            self.scroll_up();
            self.cursor.1 = self.rows - 1;
        }
    }

    /// Scroll every row up by one, blanking the freed bottom row. Marks the
    /// whole grid dirty.
    pub fn scroll_up(&mut self) {
        self.cells.copy_within(self.cols.., 0);
        let last_row_start = (self.rows - 1) * self.cols;
        for c in &mut self.cells[last_row_start..] {
            *c = Cell::blank();
        }
        self.dirty.iter_mut().for_each(|d| *d = true);
    }

    /// Write `ch` at the cursor with the given colors, wrapping to the next
    /// line at the right margin, then advancing the cursor one column.
    pub fn put_char(&mut self, ch: char, fg: u32, bg: u32) {
        if self.cursor.0 >= self.cols {
            self.carriage_return();
            self.newline();
        }
        let (x, y) = self.cursor;
        self.set_cell(x, y, Cell { ch, fg, bg, flags: 0 });
        self.cursor.0 += 1;
    }

    /// Blank columns `[from, to)` of row `y`, marking it dirty.
    fn clear_row_range(&mut self, y: usize, from: usize, to: usize) {
        if y >= self.rows {
            return;
        }
        let to = to.min(self.cols);
        if from >= to {
            return;
        }
        let base = y * self.cols;
        for c in &mut self.cells[base + from..base + to] {
            *c = Cell::blank();
        }
        self.dirty[y] = true;
    }

    /// Blank every cell and home the cursor (used by `CSI 2 J`).
    fn clear_all(&mut self) {
        self.cells.fill(Cell::blank());
        self.dirty.iter_mut().for_each(|d| *d = true);
        self.cursor = (0, 0);
    }

    /// Resize the grid to `cols`×`rows`, preserving the top-left overlap of the
    /// existing content. The cursor (and saved cursor) are clamped into the new
    /// bounds and every row is marked dirty so the next frame repaints fully.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == 0 || rows == 0 || (cols == self.cols && rows == self.rows) {
            return;
        }
        let mut new_cells = vec![Cell::blank(); cols * rows];
        let copy_rows = rows.min(self.rows);
        let copy_cols = cols.min(self.cols);
        for y in 0..copy_rows {
            let src = y * self.cols;
            let dst = y * cols;
            new_cells[dst..dst + copy_cols].copy_from_slice(&self.cells[src..src + copy_cols]);
        }
        self.cells = new_cells;
        self.cols = cols;
        self.rows = rows;
        self.dirty = vec![true; rows];
        let clamp = |(x, y): (usize, usize)| (x.min(cols - 1), y.min(rows - 1));
        self.cursor = clamp(self.cursor);
        self.saved_cursor = clamp(self.saved_cursor);
    }

    /// Save the current cursor position (`DECSC` / `CSI s`).
    fn save_cursor(&mut self) {
        self.saved_cursor = self.cursor;
    }

    /// Restore the saved cursor position (`DECRC` / `CSI u`), clamped.
    fn restore_cursor(&mut self) {
        let (x, y) = self.saved_cursor;
        self.set_cursor(x, y);
    }

    /// Delete `n` characters at the cursor, shifting the remainder of the row
    /// left and blanking the freed cells at the right (`DCH`).
    fn delete_chars(&mut self, n: usize) {
        let (x, y) = self.cursor;
        if y >= self.rows || x >= self.cols || n == 0 {
            return;
        }
        let base = y * self.cols;
        let row_end = base + self.cols;
        let from = base + x;
        let n = n.min(self.cols - x);
        self.cells.copy_within(from + n..row_end, from);
        for c in &mut self.cells[row_end - n..row_end] {
            *c = Cell::blank();
        }
        self.dirty[y] = true;
    }

    /// Insert `n` blank characters at the cursor, shifting the rest of the row
    /// right and dropping cells that fall off the right margin (`ICH`).
    fn insert_chars(&mut self, n: usize) {
        let (x, y) = self.cursor;
        if y >= self.rows || x >= self.cols || n == 0 {
            return;
        }
        let base = y * self.cols;
        let row_end = base + self.cols;
        let from = base + x;
        let n = n.min(self.cols - x);
        self.cells.copy_within(from..row_end - n, from + n);
        for c in &mut self.cells[from..from + n] {
            *c = Cell::blank();
        }
        self.dirty[y] = true;
    }

    /// Blank `n` characters starting at the cursor without shifting (`ECH`).
    fn erase_chars(&mut self, n: usize) {
        let (x, y) = self.cursor;
        let n = n.min(self.cols.saturating_sub(x));
        self.clear_row_range(y, x, x + n);
    }

    /// Clear all per-row dirty flags. Call after handing a frame to the renderer.
    pub fn clear_dirty(&mut self) {
        self.dirty.iter_mut().for_each(|d| *d = false);
    }

    /// Snapshot only the rows currently marked dirty, cloning their cells into
    /// a [`DirtyFrame`]. This is the high-locality handoff the renderer consumes.
    pub fn snapshot_dirty(&self) -> DirtyFrame {
        let rows = self.dirty.iter().enumerate()
            .filter(|&(_, d)| *d)
            .map(|(y, _)| {
                let start = y * self.cols;
                (y, self.cells[start..start + self.cols].to_vec())
            })
            .collect();
        DirtyFrame { cursor: self.cursor, rows }
    }
}

/// A snapshot of the dirty rows of a [`Grid`], plus the cursor position so the
/// renderer can place the hardware cursor.
pub struct DirtyFrame {
    /// Cursor position `(col, row)` at snapshot time.
    pub cursor: (usize, usize),
    /// Dirty rows as `(row_index, cells)` pairs.
    pub rows: Vec<(usize, Vec<Cell>)>,
}

/// States of the escape-sequence recognizer.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ParserState {
    /// Printable / control bytes go straight to the grid.
    Ground,
    /// Saw `ESC`; awaiting the sequence introducer.
    Esc,
    /// Inside a `CSI` (`ESC [`) sequence; accumulating parameter bytes.
    Csi,
    /// Mid-way through a multibyte UTF-8 code point; awaiting continuation bytes.
    Utf8,
    /// Inside an `OSC` (`ESC ]`) string; bytes are consumed until a `BEL` or
    /// `ST` terminator. We don't act on OSC payloads (titles, cwd reports), we
    /// just keep them from leaking onto the screen.
    Osc,
    /// Saw `ESC` while inside an OSC string; awaiting the `\` of an `ST`
    /// (`ESC \`) terminator.
    OscEsc,
    /// A charset-designation escape (`ESC ( B`, etc.); consume one more byte.
    EscCharset,
}

/// Incremental parser that turns a shell's output byte stream into [`Grid`]
/// mutations. Tracks the current SGR colors across `advance` calls.
pub struct AnsiParser {
    state: ParserState,
    current_fg: u32,
    current_bg: u32,
    param_buffer: String,
    /// Set when a CSI sequence carries a private marker (`?`, `<`, `=`, `>`).
    /// Such sequences (e.g. DEC private mode set/reset) are consumed but not
    /// acted upon.
    csi_private: bool,
    /// Code point accumulated so far while in [`ParserState::Utf8`].
    utf8_acc: u32,
    /// Number of UTF-8 continuation bytes still expected.
    utf8_remaining: usize,
}

impl Default for AnsiParser {
    fn default() -> Self {
        Self::new()
    }
}

impl AnsiParser {
    /// Create a parser in the ground state with default colors.
    pub fn new() -> Self {
        Self {
            state: ParserState::Ground,
            current_fg: DEFAULT_FG,
            current_bg: DEFAULT_BG,
            param_buffer: String::new(),
            csi_private: false,
            utf8_acc: 0,
            utf8_remaining: 0,
        }
    }

    /// Feed a chunk of bytes, applying their effects to `g`. Parser state
    /// persists across calls, so escape sequences may straddle chunk boundaries.
    pub fn advance(&mut self, g: &mut Grid, bytes: &[u8]) {
        for &b in bytes {
            match self.state {
                ParserState::Ground => self.ground_byte(b, g),
                ParserState::Utf8 => {
                    if (0x80..=0xbf).contains(&b) {
                        // Valid continuation byte: fold in its 6 payload bits.
                        self.utf8_acc = (self.utf8_acc << 6) | (b as u32 & 0x3f);
                        self.utf8_remaining -= 1;
                        if self.utf8_remaining == 0 {
                            let ch = char::from_u32(self.utf8_acc).unwrap_or('\u{FFFD}');
                            g.put_char(ch, self.current_fg, self.current_bg);
                            self.state = ParserState::Ground;
                        }
                    } else {
                        // Truncated sequence: emit a replacement char, then
                        // reprocess this byte from the ground state.
                        g.put_char('\u{FFFD}', self.current_fg, self.current_bg);
                        self.state = ParserState::Ground;
                        self.ground_byte(b, g);
                    }
                }
                ParserState::Esc => match b {
                    b'[' => {
                        self.param_buffer.clear();
                        self.csi_private = false;
                        self.state = ParserState::Csi;
                    }
                    b']' => self.state = ParserState::Osc,
                    // DECSC / DECRC: save and restore the cursor.
                    b'7' => {
                        g.save_cursor();
                        self.state = ParserState::Ground;
                    }
                    b'8' => {
                        g.restore_cursor();
                        self.state = ParserState::Ground;
                    }
                    // Charset designation (`ESC ( B`, etc.): one more byte follows.
                    b'(' | b')' | b'*' | b'+' => self.state = ParserState::EscCharset,
                    // Any other ESC X sequence is a single byte we don't model;
                    // consuming b returns us to ground without leaking it.
                    _ => self.state = ParserState::Ground,
                },
                ParserState::Csi => match b {
                    // Parameter bytes.
                    b'0'..=b'9' | b';' => self.param_buffer.push(b as char),
                    // Private markers (`<`, `=`, `>`, `?`): flag and keep collecting.
                    0x3c..=0x3f => self.csi_private = true,
                    // Intermediate bytes (space..`/`): ignored but part of the sequence.
                    0x20..=0x2f => {}
                    // Final byte: dispatch (unless this is a private sequence) and reset.
                    0x40..=0x7e => {
                        if !self.csi_private {
                            self.handle_csi(b, g);
                        }
                        self.state = ParserState::Ground;
                        self.param_buffer.clear();
                        self.csi_private = false;
                    }
                    // Anything else aborts the malformed sequence.
                    _ => {
                        self.state = ParserState::Ground;
                        self.param_buffer.clear();
                        self.csi_private = false;
                    }
                },
                ParserState::Osc => match b {
                    0x07 => self.state = ParserState::Ground, // BEL terminator
                    0x1b => self.state = ParserState::OscEsc, // possible ST (ESC \)
                    _ => {}                                   // consume payload byte
                },
                ParserState::OscEsc => {
                    // Whether or not this is the `\` of an ST, the OSC string is
                    // over; drop the byte and return to ground.
                    self.state = ParserState::Ground;
                }
                ParserState::EscCharset => self.state = ParserState::Ground,
            }
        }
    }

    /// Handle a single byte while in the ground state: C0 controls, printable
    /// ASCII, and the lead byte of a UTF-8 code point (which transitions into
    /// [`ParserState::Utf8`]).
    fn ground_byte(&mut self, b: u8, g: &mut Grid) {
        match b {
            0x1b => self.state = ParserState::Esc,
            0x08 => g.cursor.0 = g.cursor.0.saturating_sub(1), // backspace
            b'\n' => {
                g.carriage_return();
                g.newline();
            }
            b'\r' => g.carriage_return(),
            b'\t' => {
                // Advance to the next 8-column tab stop, clamped at the right
                // margin so we never wrap/scroll on a tab.
                let next_stop = (g.cursor.0 / 8 + 1) * 8;
                let target = next_stop.min(g.cols.saturating_sub(1));
                while g.cursor.0 < target {
                    g.put_char(' ', self.current_fg, self.current_bg);
                }
            }
            0x20..=0x7e => g.put_char(b as char, self.current_fg, self.current_bg),
            // UTF-8 lead bytes: stash the payload bits and how many continuation
            // bytes to expect. (0xC0/0xC1 are always overlong, hence excluded.)
            0xc2..=0xdf => {
                self.utf8_acc = (b as u32) & 0x1f;
                self.utf8_remaining = 1;
                self.state = ParserState::Utf8;
            }
            0xe0..=0xef => {
                self.utf8_acc = (b as u32) & 0x0f;
                self.utf8_remaining = 2;
                self.state = ParserState::Utf8;
            }
            0xf0..=0xf4 => {
                self.utf8_acc = (b as u32) & 0x07;
                self.utf8_remaining = 3;
                self.state = ParserState::Utf8;
            }
            // Stray continuation or otherwise invalid lead byte.
            0x80..=0xbf | 0xc0..=0xc1 | 0xf5..=0xff => {
                g.put_char('\u{FFFD}', self.current_fg, self.current_bg);
            }
            // Other C0 controls are ignored.
            _ => {}
        }
    }

    /// Dispatch a completed CSI sequence given its final command byte.
    fn handle_csi(&mut self, cmd: u8, g: &mut Grid) {
        let params: Vec<usize> = self.param_buffer
            .split(';')
            .filter_map(|s| s.parse().ok())
            .collect();

        // Most cursor-motion commands take a single optional count that
        // defaults to (and treats 0 as) 1.
        let count = params.first().copied().unwrap_or(1).max(1);

        match cmd {
            b'm' => self.apply_sgr(&params),
            b'H' | b'f' => {
                // CSI row ; col H — both 1-based; default to 1.
                let y = params.first().copied().unwrap_or(1);
                let x = params.get(1).copied().unwrap_or(1);
                g.set_cursor(x.saturating_sub(1), y.saturating_sub(1));
            }
            b'A' => g.set_cursor(g.cursor.0, g.cursor.1.saturating_sub(count)), // CUU
            b'B' => g.set_cursor(g.cursor.0, g.cursor.1 + count),               // CUD
            b'C' => g.set_cursor(g.cursor.0 + count, g.cursor.1),               // CUF
            b'D' => g.set_cursor(g.cursor.0.saturating_sub(count), g.cursor.1), // CUB
            b'G' => {
                // CHA — cursor to absolute column (1-based).
                let col = params.first().copied().unwrap_or(1);
                g.set_cursor(col.saturating_sub(1), g.cursor.1);
            }
            b'd' => {
                // VPA — cursor to absolute row (1-based).
                let row = params.first().copied().unwrap_or(1);
                g.set_cursor(g.cursor.0, row.saturating_sub(1));
            }
            b'@' => g.insert_chars(count), // ICH
            b'P' => g.delete_chars(count), // DCH
            b'X' => g.erase_chars(count),  // ECH
            b's' => g.save_cursor(),       // SCP
            b'u' => g.restore_cursor(),    // RCP
            b'J' => {
                // ED — erase in display.
                let (cx, cy) = g.cursor;
                match params.first().copied().unwrap_or(0) {
                    0 => {
                        // Cursor to end of screen.
                        g.clear_row_range(cy, cx, g.cols);
                        for y in (cy + 1)..g.rows {
                            g.clear_row_range(y, 0, g.cols);
                        }
                    }
                    1 => {
                        // Start of screen to cursor (inclusive).
                        for y in 0..cy {
                            g.clear_row_range(y, 0, g.cols);
                        }
                        g.clear_row_range(cy, 0, cx + 1);
                    }
                    2 | 3 => g.clear_all(),
                    _ => {}
                }
            }
            b'K' => {
                // EL — erase in line.
                let (cx, cy) = g.cursor;
                match params.first().copied().unwrap_or(0) {
                    0 => g.clear_row_range(cy, cx, g.cols),
                    1 => g.clear_row_range(cy, 0, cx + 1),
                    2 => g.clear_row_range(cy, 0, g.cols),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Reset SGR state to the default colors.
    fn reset_sgr(&mut self) {
        self.current_fg = DEFAULT_FG;
        self.current_bg = DEFAULT_BG;
    }

    /// Apply an SGR (`CSI … m`) parameter list. Supports reset, the 16-color
    /// palette (normal + bright), and the extended `38/48;5;n` (256-color) and
    /// `38/48;2;r;g;b` (truecolor) forms. An empty list means reset.
    fn apply_sgr(&mut self, params: &[usize]) {
        if params.is_empty() {
            self.reset_sgr();
            return;
        }
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => self.reset_sgr(),
                30..=37 => self.current_fg = PALETTE_16[params[i] - 30],
                38 => {
                    if let Some((color, consumed)) = parse_extended_color(&params[i + 1..]) {
                        self.current_fg = color;
                        i += consumed;
                    }
                }
                39 => self.current_fg = DEFAULT_FG,
                40..=47 => self.current_bg = PALETTE_16[params[i] - 40],
                48 => {
                    if let Some((color, consumed)) = parse_extended_color(&params[i + 1..]) {
                        self.current_bg = color;
                        i += consumed;
                    }
                }
                49 => self.current_bg = DEFAULT_BG,
                90..=97 => self.current_fg = PALETTE_16[8 + (params[i] - 90)],
                100..=107 => self.current_bg = PALETTE_16[8 + (params[i] - 100)],
                _ => {}
            }
            i += 1;
        }
    }
}

/// Parse the tail of an extended SGR color selector (the part after `38`/`48`).
///
/// Returns the resolved `0xRRGGBB` color and how many *additional* params were
/// consumed, or `None` if the form is unrecognized.
///
/// * `5; n`        → 256-color palette index `n` (3 params consumed)
/// * `2; r; g; b`  → truecolor (4 params consumed)
fn parse_extended_color(rest: &[usize]) -> Option<(u32, usize)> {
    match rest.first().copied() {
        Some(5) => {
            let n = *rest.get(1)?;
            Some((xterm_256_to_rgb(n), 2))
        }
        Some(2) => {
            let r = (*rest.get(1)? & 0xFF) as u32;
            let g = (*rest.get(2)? & 0xFF) as u32;
            let b = (*rest.get(3)? & 0xFF) as u32;
            Some(((r << 16) | (g << 8) | b, 4))
        }
        _ => None,
    }
}

/// Convert an xterm 256-color index to `0xRRGGBB`.
///
/// 0-15 map to the base palette, 16-231 form a 6×6×6 color cube, and 232-255
/// are a 24-step grayscale ramp.
fn xterm_256_to_rgb(n: usize) -> u32 {
    match n {
        0..=15 => PALETTE_16[n],
        16..=231 => {
            let n = n - 16;
            let steps = [0u32, 95, 135, 175, 215, 255];
            let r = steps[(n / 36) % 6];
            let g = steps[(n / 6) % 6];
            let b = steps[n % 6];
            (r << 16) | (g << 8) | b
        }
        232..=255 => {
            let level = 8 + (n - 232) as u32 * 10;
            (level << 16) | (level << 8) | level
        }
        _ => DEFAULT_FG,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &[u8], cols: usize, rows: usize) -> Grid {
        let mut g = Grid::new(cols, rows);
        let mut p = AnsiParser::new();
        p.advance(&mut g, input);
        g
    }

    fn row_text(g: &Grid, y: usize) -> String {
        let base = y * g.cols;
        g.cells[base..base + g.cols].iter().map(|c| c.ch).collect()
    }

    #[test]
    fn writes_plain_text() {
        let g = parse(b"hi", 80, 24);
        assert_eq!(g.cells[0].ch, 'h');
        assert_eq!(g.cells[1].ch, 'i');
        assert_eq!(g.cursor, (2, 0));
        assert!(g.dirty[0]);
    }

    #[test]
    fn newline_and_carriage_return() {
        let g = parse(b"ab\r\nc", 80, 24);
        assert_eq!(g.cells[0].ch, 'a');
        assert_eq!(row_text(&g, 1).trim_end(), "c");
        assert_eq!(g.cursor, (1, 1));
    }

    #[test]
    fn put_char_wraps_at_right_margin() {
        let g = parse(b"abc", 2, 24);
        assert_eq!(row_text(&g, 0), "ab");
        assert_eq!(g.cells[2].ch, 'c'); // wrapped to row 1, col 0 -> index y*cols+x = 1*2+0
        assert_eq!(g.cursor, (1, 1));
    }

    #[test]
    fn tab_stops_at_eight_and_clamps_at_margin() {
        let g = parse(b"a\tb", 80, 24);
        assert_eq!(g.cursor.0, 9); // 'a' at 0, tab to 8, 'b' at 8 -> cursor 9
        assert_eq!(g.cells[8].ch, 'b');

        // A tab on a narrow grid must not wrap/scroll.
        let g2 = parse(b"\t", 4, 4);
        assert_eq!(g2.cursor.1, 0);
        assert!(g2.cursor.0 <= 3);
    }

    #[test]
    fn sgr_sets_basic_colors() {
        let g = parse(b"\x1b[31mX", 80, 24);
        assert_eq!(g.cells[0].fg, PALETTE_16[1]); // SGR 31 = dim red 0x800000
    }

    #[test]
    fn sgr_reset_restores_defaults() {
        let g = parse(b"\x1b[31mA\x1b[0mB", 80, 24);
        assert_eq!(g.cells[0].fg, PALETTE_16[1]);
        assert_eq!(g.cells[1].fg, DEFAULT_FG);
    }

    #[test]
    fn sgr_empty_param_is_reset() {
        let g = parse(b"\x1b[31mA\x1b[mB", 80, 24);
        assert_eq!(g.cells[1].fg, DEFAULT_FG);
    }

    #[test]
    fn sgr_truecolor_and_256() {
        let g = parse(b"\x1b[38;2;10;20;30mX", 80, 24);
        assert_eq!(g.cells[0].fg, 0x0A141E);

        let g2 = parse(b"\x1b[48;5;15mY", 80, 24);
        assert_eq!(g2.cells[0].bg, 0xFFFFFF); // palette index 15
    }

    #[test]
    fn cursor_position_is_clamped() {
        let g = parse(b"\x1b[999;999H", 80, 24);
        assert_eq!(g.cursor, (79, 23));
    }

    #[test]
    fn cursor_position_default_is_home() {
        let g = parse(b"X\x1b[HY", 80, 24);
        assert_eq!(g.cells[0].ch, 'Y'); // overwrote 'X' at home
        assert_eq!(g.cursor, (1, 0));
    }

    #[test]
    fn erase_line_to_end() {
        let mut g = parse(b"abcdef", 80, 24);
        g.cursor = (3, 0);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[K");
        assert_eq!(&row_text(&g, 0)[..6], "abc   ");
    }

    #[test]
    fn erase_line_to_start() {
        let mut g = parse(b"abcdef", 80, 24);
        g.cursor = (2, 0);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[1K");
        assert_eq!(&row_text(&g, 0)[..6], "   def");
    }

    #[test]
    fn erase_display_full() {
        let mut g = parse(b"hello", 80, 24);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[2J");
        assert_eq!(g.cells[0].ch, ' ');
        assert_eq!(g.cursor, (0, 0));
    }

    #[test]
    fn scroll_up_shifts_rows() {
        let mut g = Grid::new(4, 2);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"top\r\nbot");
        // Force one more newline to scroll.
        p.advance(&mut g, b"\r\nnew");
        assert_eq!(row_text(&g, 0).trim_end(), "bot");
        assert_eq!(row_text(&g, 1).trim_end(), "new");
    }

    #[test]
    fn resize_preserves_content_and_clamps_cursor() {
        let mut g = parse(b"hello", 80, 24);
        g.cursor = (40, 20);
        g.resize(10, 5);
        assert_eq!(g.cols, 10);
        assert_eq!(g.rows, 5);
        assert_eq!(&row_text(&g, 0)[..5], "hello"); // top-left content kept
        assert_eq!(g.cursor, (9, 4)); // clamped into new bounds
        assert!(g.dirty.iter().all(|&d| d)); // full repaint queued
        // Growing back keeps the surviving content and blanks new area.
        g.resize(80, 24);
        assert_eq!(&row_text(&g, 0)[..5], "hello");
        assert_eq!(g.cells[79].ch, ' ');
    }

    #[test]
    fn snapshot_only_dirty_rows() {
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"x");
        let frame = g.snapshot_dirty();
        assert_eq!(frame.rows.len(), 1);
        assert_eq!(frame.rows[0].0, 0);
        assert_eq!(frame.cursor, (1, 0));
        g.clear_dirty();
        assert!(g.snapshot_dirty().rows.is_empty());
    }

    #[test]
    fn cursor_motion_relative() {
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        g.cursor = (10, 10);
        p.advance(&mut g, b"\x1b[3C"); // forward 3
        assert_eq!(g.cursor, (13, 10));
        p.advance(&mut g, b"\x1b[5D"); // back 5
        assert_eq!(g.cursor, (8, 10));
        p.advance(&mut g, b"\x1b[2A"); // up 2
        assert_eq!(g.cursor, (8, 8));
        p.advance(&mut g, b"\x1b[B"); // down 1 (default)
        assert_eq!(g.cursor, (8, 9));
    }

    #[test]
    fn cursor_absolute_column_and_row() {
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        g.cursor = (10, 10);
        p.advance(&mut g, b"\x1b[1G"); // column 1 (0-based 0)
        assert_eq!(g.cursor, (0, 10));
        p.advance(&mut g, b"\x1b[5d"); // row 5 (0-based 4)
        assert_eq!(g.cursor, (0, 4));
    }

    #[test]
    fn backspace_moves_left() {
        let mut g = parse(b"abc", 80, 24);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x08"); // cursor 3 -> 2
        assert_eq!(g.cursor, (2, 0));
        p.advance(&mut g, b"X"); // overwrites 'c'
        assert_eq!(row_text(&g, 0).trim_end(), "abX");
    }

    #[test]
    fn delete_chars_shifts_left() {
        let mut g = parse(b"abcdef", 80, 24);
        g.cursor = (1, 0);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[2P"); // delete "bc"
        assert_eq!(&row_text(&g, 0)[..6], "adef  ");
    }

    #[test]
    fn insert_chars_shifts_right() {
        let mut g = parse(b"abcdef", 80, 24);
        g.cursor = (1, 0);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[2@"); // insert 2 blanks at col 1
        assert_eq!(&row_text(&g, 0)[..6], "a  bcd");
    }

    #[test]
    fn erase_chars_blanks_without_shift() {
        let mut g = parse(b"abcdef", 80, 24);
        g.cursor = (2, 0);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[2X"); // blank "cd"
        assert_eq!(&row_text(&g, 0)[..6], "ab  ef");
    }

    #[test]
    fn save_and_restore_cursor() {
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        g.cursor = (5, 5);
        p.advance(&mut g, b"\x1b[s"); // save
        g.cursor = (20, 20);
        p.advance(&mut g, b"\x1b[u"); // restore
        assert_eq!(g.cursor, (5, 5));
        // DECSC/DECRC (ESC 7 / ESC 8) variant.
        g.cursor = (1, 2);
        p.advance(&mut g, b"\x1b7");
        g.cursor = (9, 9);
        p.advance(&mut g, b"\x1b8");
        assert_eq!(g.cursor, (1, 2));
    }

    #[test]
    fn osc_title_is_consumed_not_printed() {
        // OSC 2 (set window title) terminated by BEL, then real text.
        let g = parse(b"\x1b]2;my title\x07X", 80, 24);
        assert_eq!(g.cells[0].ch, 'X');
        assert_eq!(g.cursor, (1, 0));
    }

    #[test]
    fn osc_terminated_by_st_is_consumed() {
        // OSC 7 (cwd) terminated by ST (ESC \).
        let g = parse(b"\x1b]7;file://host/path\x1b\\Y", 80, 24);
        assert_eq!(g.cells[0].ch, 'Y');
    }

    #[test]
    fn csi_private_mode_is_consumed_not_printed() {
        // Bracketed-paste enable/disable must not leak "2004h"/"2004l".
        let g = parse(b"\x1b[?2004hA\x1b[?2004lB", 80, 24);
        assert_eq!(g.cells[0].ch, 'A');
        assert_eq!(g.cells[1].ch, 'B');
        assert_eq!(g.cursor, (2, 0));
    }

    #[test]
    fn charset_designation_is_consumed() {
        // ESC ( B (designate ASCII) must not leak the 'B'.
        let g = parse(b"\x1b(BZ", 80, 24);
        assert_eq!(g.cells[0].ch, 'Z');
        assert_eq!(g.cursor, (1, 0));
    }

    #[test]
    fn utf8_two_byte_decodes() {
        // U+00E9 'é' = C3 A9
        let g = parse(b"\xc3\xa9", 80, 24);
        assert_eq!(g.cells[0].ch, 'é');
        assert_eq!(g.cursor, (1, 0));
    }

    #[test]
    fn utf8_three_byte_decodes() {
        // U+2794 '➔'-family arrow = E2 9E 94; the prompt arrow '➜' is E2 9E 9C.
        let g = parse("➜".as_bytes(), 80, 24);
        assert_eq!(g.cells[0].ch, '➜');
    }

    #[test]
    fn utf8_four_byte_emoji_decodes() {
        // U+1F600 😀 = F0 9F 98 80
        let g = parse("😀".as_bytes(), 80, 24);
        assert_eq!(g.cells[0].ch, '😀');
    }

    #[test]
    fn utf8_split_across_chunks() {
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        let bytes = "é".as_bytes(); // C3 A9
        p.advance(&mut g, &bytes[..1]); // lead byte only
        assert_eq!(g.cells[0].ch, ' '); // nothing emitted yet
        p.advance(&mut g, &bytes[1..]); // continuation
        assert_eq!(g.cells[0].ch, 'é');
    }

    #[test]
    fn utf8_invalid_yields_replacement() {
        // Stray continuation byte.
        let g = parse(b"\x80X", 80, 24);
        assert_eq!(g.cells[0].ch, '\u{FFFD}');
        assert_eq!(g.cells[1].ch, 'X');
    }

    #[test]
    fn utf8_truncated_then_ascii_recovers() {
        // Lead byte expecting a continuation, interrupted by an ASCII byte:
        // emit replacement for the truncated char, then render the ASCII byte.
        let g = parse(b"\xc3A", 80, 24);
        assert_eq!(g.cells[0].ch, '\u{FFFD}');
        assert_eq!(g.cells[1].ch, 'A');
    }

    #[test]
    fn escape_sequence_split_across_chunks() {
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[3");
        p.advance(&mut g, b"1mX");
        assert_eq!(g.cells[0].fg, PALETTE_16[1]); // SGR 31
    }
}
