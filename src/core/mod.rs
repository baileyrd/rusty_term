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
}

/// Incremental parser that turns a shell's output byte stream into [`Grid`]
/// mutations. Tracks the current SGR colors across `advance` calls.
pub struct AnsiParser {
    state: ParserState,
    current_fg: u32,
    current_bg: u32,
    param_buffer: String,
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
        }
    }

    /// Feed a chunk of bytes, applying their effects to `g`. Parser state
    /// persists across calls, so escape sequences may straddle chunk boundaries.
    pub fn advance(&mut self, g: &mut Grid, bytes: &[u8]) {
        for &b in bytes {
            match self.state {
                ParserState::Ground => match b {
                    0x1b => self.state = ParserState::Esc,
                    b'\n' => {
                        g.carriage_return();
                        g.newline();
                    }
                    b'\r' => g.carriage_return(),
                    b'\t' => {
                        // Advance to the next 8-column tab stop, clamped at the
                        // right margin so we never wrap/scroll on a tab.
                        let next_stop = (g.cursor.0 / 8 + 1) * 8;
                        let target = next_stop.min(g.cols.saturating_sub(1));
                        while g.cursor.0 < target {
                            g.put_char(' ', self.current_fg, self.current_bg);
                        }
                    }
                    0x20..=0x7e => g.put_char(b as char, self.current_fg, self.current_bg),
                    _ => {}
                },
                ParserState::Esc => {
                    self.state = if b == b'[' { ParserState::Csi } else { ParserState::Ground };
                    if self.state == ParserState::Csi {
                        self.param_buffer.clear();
                    }
                }
                ParserState::Csi => {
                    // Collect parameter and intermediate bytes; any other byte
                    // is the final command byte that terminates the sequence.
                    if b.is_ascii_digit() || b == b';' {
                        self.param_buffer.push(b as char);
                    } else {
                        self.handle_csi(b, g);
                        self.state = ParserState::Ground;
                        self.param_buffer.clear();
                    }
                }
            }
        }
    }

    /// Dispatch a completed CSI sequence given its final command byte.
    fn handle_csi(&mut self, cmd: u8, g: &mut Grid) {
        let params: Vec<usize> = self.param_buffer
            .split(';')
            .filter_map(|s| s.parse().ok())
            .collect();

        match cmd {
            b'm' => self.apply_sgr(&params),
            b'H' | b'f' => {
                // CSI row ; col H — both 1-based; default to 1.
                let y = params.first().copied().unwrap_or(1);
                let x = params.get(1).copied().unwrap_or(1);
                g.set_cursor(x.saturating_sub(1), y.saturating_sub(1));
            }
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
    fn escape_sequence_split_across_chunks() {
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[3");
        p.advance(&mut g, b"1mX");
        assert_eq!(g.cells[0].fg, PALETTE_16[1]); // SGR 31
    }
}
