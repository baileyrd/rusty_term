//! Core terminal logic for rusty_term.
//!
//! This module is the platform-independent heart of the emulator. It defines
//! the [`Grid`] (the authoritative screen state), the [`Cell`] that fills it,
//! the [`DirtyFrame`] handed to the renderer, and the [`AnsiParser`] that
//! drives the grid from a byte stream produced by the child shell.
//!
//! The parser intentionally implements a pragmatic subset of the VT100/ECMA-48
//! escape repertoire (SGR colors, cursor positioning, erase line/display).

use unicode_width::UnicodeWidthChar;

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

/// [`Cell::flags`] bit marking the trailing (second) cell of a double-width
/// character. The renderer skips these so the wide glyph occupies two columns.
pub const WIDE_TRAILER: u16 = 0b0000_0001;

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

/// Maximum number of trailing combining marks stored per cell.
pub const MAX_COMBINING: usize = 2;

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
}

impl Cell {
    /// Construct a blank cell (space glyph, default colors).
    fn blank() -> Self {
        Cell {
            ch: ' ',
            combining: ['\0'; MAX_COMBINING],
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            flags: 0,
        }
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
    /// Top row of the scrolling region (inclusive, 0-based).
    pub scroll_top: usize,
    /// Bottom row of the scrolling region (inclusive, 0-based).
    pub scroll_bottom: usize,
    /// When `Some`, the alternate screen is active and this holds the primary
    /// screen to restore on exit. Kept the same dimensions as the live buffer.
    primary: Option<SavedScreen>,
    /// Monotonic counter bumped on every parsed batch; lets the renderer
    /// reason about frame freshness.
    pub epoch: u64,
    /// Window title last set by the child via OSC 0/2. The renderer forwards
    /// changes to the host terminal's title bar; empty until the child sets one.
    pub title: String,
    /// Working directory last reported by the child via OSC 7 (typically a
    /// `file://host/path` URI). Captured for future use (e.g. "open new tab in
    /// the same directory"); empty until reported.
    pub cwd: String,
}

/// Which DEC private mode selected the alternate screen, which determines the
/// cursor save/restore behaviour on exit (per xterm: only `1049` does it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AltMode {
    /// `?47` — bare buffer swap, no cursor save/restore.
    Dec47,
    /// `?1047` — buffer swap, no cursor save/restore.
    Dec1047,
    /// `?1049` — save cursor on entry (DECSC-style), restore on exit.
    Dec1049,
}

/// Map a DEC private parameter to its alternate-screen mode, if any.
fn alt_mode(param: usize) -> Option<AltMode> {
    match param {
        47 => Some(AltMode::Dec47),
        1047 => Some(AltMode::Dec1047),
        1049 => Some(AltMode::Dec1049),
        _ => None,
    }
}

/// A stashed screen buffer plus its cursor, used to swap the primary screen out
/// while the alternate screen is active.
struct SavedScreen {
    cells: Vec<Cell>,
    cursor: (usize, usize),
    /// The DECSC/`CSI s` register at the time of the switch, kept separate from
    /// the alternate screen's so the two buffers don't share one save slot.
    saved_cursor: (usize, usize),
    /// The mode that activated the alternate screen, governing exit behaviour.
    mode: AltMode,
}

/// Reflow `old` (sized `old_cols`×`old_rows`) into a fresh `cols`×`rows` buffer,
/// preserving the top-left overlap and blank-filling any new area.
fn reflow(old: &[Cell], old_cols: usize, old_rows: usize, cols: usize, rows: usize) -> Vec<Cell> {
    let mut new = vec![Cell::blank(); cols * rows];
    let copy_rows = rows.min(old_rows);
    let copy_cols = cols.min(old_cols);
    for y in 0..copy_rows {
        let src = y * old_cols;
        let dst = y * cols;
        new[dst..dst + copy_cols].copy_from_slice(&old[src..src + copy_cols]);
    }
    new
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
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            primary: None,
            epoch: 0,
            title: String::new(),
            cwd: String::new(),
        }
    }

    /// Write `cell` at `(x, y)`, marking the row dirty. Out-of-bounds writes
    /// are silently ignored (the caller is responsible for clamping).
    ///
    /// If the write lands on one half of an existing double-width glyph, the
    /// orphaned partner cell is blanked so no stale head/trailer is left behind.
    pub fn set_cell(&mut self, x: usize, y: usize, cell: Cell) {
        if x >= self.cols || y >= self.rows {
            return;
        }
        let idx = y * self.cols + x;
        if self.cells[idx].flags & WIDE_TRAILER != 0 {
            // Overwriting the trailing half: blank the head to its left.
            if x >= 1 {
                self.cells[idx - 1] = Cell::blank();
            }
        } else if x + 1 < self.cols && self.cells[idx + 1].flags & WIDE_TRAILER != 0 {
            // Overwriting the leading half: blank the orphaned trailer to its right.
            self.cells[idx + 1] = Cell::blank();
        }
        self.cells[idx] = cell;
        self.dirty[y] = true;
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

    /// Advance the cursor one row. At the bottom of the scrolling region this
    /// scrolls the region up instead of moving the cursor past it.
    pub fn newline(&mut self) {
        if self.cursor.1 == self.scroll_bottom {
            self.scroll_up();
        } else if self.cursor.1 + 1 < self.rows {
            self.cursor.1 += 1;
        }
    }

    /// Scroll the current scrolling region up by one row, blanking the freed
    /// bottom row of the region. Only the region's rows are marked dirty.
    pub fn scroll_up(&mut self) {
        let (top, bottom) = (self.scroll_top, self.scroll_bottom);
        if bottom <= top || bottom >= self.rows {
            return;
        }
        let src = (top + 1) * self.cols;
        let dst = top * self.cols;
        let count = (bottom - top) * self.cols;
        self.cells.copy_within(src..src + count, dst);
        let last = bottom * self.cols;
        for c in &mut self.cells[last..last + self.cols] {
            *c = Cell::blank();
        }
        for d in &mut self.dirty[top..=bottom] {
            *d = true;
        }
    }

    /// Set the scrolling region to rows `top..=bottom` (0-based, inclusive) and
    /// home the cursor. An invalid range resets the region to the full screen.
    fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let bottom = bottom.min(self.rows.saturating_sub(1));
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        } else {
            self.scroll_top = 0;
            self.scroll_bottom = self.rows.saturating_sub(1);
        }
        self.cursor = (0, 0);
    }

    /// Reset the scrolling region to span the full screen.
    fn reset_scroll_region(&mut self) {
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
    }

    /// Write `ch` at the cursor with the given colors, wrapping to the next
    /// line if it would not fit, then advancing the cursor by the glyph's
    /// display width. A double-width glyph also writes a flagged trailing cell.
    pub fn put_char(&mut self, ch: char, fg: u32, bg: u32) {
        let w = char_width(ch);
        if w == 0 {
            // Zero-width combining mark: attach it to the preceding glyph.
            self.add_combining(ch);
            return;
        }
        if self.cursor.0 + w > self.cols {
            self.carriage_return();
            self.newline();
        }
        let (x, y) = self.cursor;
        self.set_cell(x, y, Cell { ch, combining: ['\0'; MAX_COMBINING], fg, bg, flags: 0 });
        if w == 2 && x + 1 < self.cols {
            // Trailing half: a flagged placeholder the renderer skips.
            self.set_cell(
                x + 1,
                y,
                Cell { ch: ' ', combining: ['\0'; MAX_COMBINING], fg, bg, flags: WIDE_TRAILER },
            );
        }
        self.cursor.0 += w;
    }

    /// Attach a zero-width combining mark to the most recently written glyph
    /// (the cell to the left of the cursor, stepping back over a wide-glyph
    /// trailer to its head). Dropped at the start of a line, or once a cell's
    /// combining slots are full.
    fn add_combining(&mut self, mark: char) {
        let (cx, cy) = self.cursor;
        if cx == 0 || cy >= self.rows {
            return;
        }
        let mut bx = cx - 1;
        if self.cells[cy * self.cols + bx].flags & WIDE_TRAILER != 0 && bx >= 1 {
            bx -= 1; // land on the wide glyph's head, not its trailer
        }
        let cell = &mut self.cells[cy * self.cols + bx];
        if let Some(slot) = cell.combining.iter_mut().find(|s| **s == '\0') {
            *slot = mark;
            self.dirty[cy] = true;
        }
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
        let clamp = |(x, y): (usize, usize)| (x.min(cols - 1), y.min(rows - 1));
        let new_cells = reflow(&self.cells, self.cols, self.rows, cols, rows);
        // Keep the stashed primary screen the same size as the live buffer so
        // it can be restored without a size mismatch.
        if let Some(saved) = &mut self.primary {
            saved.cells = reflow(&saved.cells, self.cols, self.rows, cols, rows);
            saved.cursor = clamp(saved.cursor);
            saved.saved_cursor = clamp(saved.saved_cursor);
        }
        self.cells = new_cells;
        self.cols = cols;
        self.rows = rows;
        self.dirty = vec![true; rows];
        self.cursor = clamp(self.cursor);
        self.saved_cursor = clamp(self.saved_cursor);
        self.reset_scroll_region();
    }

    /// Switch to the alternate screen, stashing the primary buffer. Only
    /// `?1049` saves the cursor and homes it; `?47`/`?1047` swap the buffer and
    /// leave the cursor where it is. No-op if already on the alternate screen.
    fn enter_alt_screen(&mut self, mode: AltMode) {
        if self.primary.is_some() {
            return;
        }
        self.primary = Some(SavedScreen {
            cells: std::mem::replace(&mut self.cells, vec![Cell::blank(); self.cols * self.rows]),
            cursor: self.cursor,
            saved_cursor: self.saved_cursor,
            mode,
        });
        if mode == AltMode::Dec1049 {
            self.cursor = (0, 0);
        }
        self.reset_scroll_region();
        self.dirty.iter_mut().for_each(|d| *d = true);
    }

    /// Switch back to the primary screen, restoring its buffer. For `?1049` the
    /// cursor (and DECSC register) saved on entry are restored; `?47`/`?1047`
    /// leave the cursor as the alternate session left it. No-op if not on the
    /// alternate screen.
    fn leave_alt_screen(&mut self) {
        if let Some(saved) = self.primary.take() {
            self.cells = saved.cells;
            if saved.mode == AltMode::Dec1049 {
                self.cursor = (
                    saved.cursor.0.min(self.cols.saturating_sub(1)),
                    saved.cursor.1.min(self.rows.saturating_sub(1)),
                );
                self.saved_cursor = (
                    saved.saved_cursor.0.min(self.cols.saturating_sub(1)),
                    saved.saved_cursor.1.min(self.rows.saturating_sub(1)),
                );
            }
            self.reset_scroll_region();
            self.dirty.iter_mut().for_each(|d| *d = true);
        }
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
    /// Inside a string-type control sequence — DCS (`ESC P`), APC (`ESC _`),
    /// PM (`ESC ^`), or SOS (`ESC X`). Like [`ParserState::Osc`], the body is
    /// consumed opaquely (we don't act on Sixel, DECRQSS, Kitty graphics, …)
    /// so it never leaks onto the screen. Terminated by `ST` (`ESC \`).
    StrSink,
    /// Saw `ESC` while inside a [`ParserState::StrSink`] string; awaiting the
    /// `\` of an `ST` terminator.
    StrSinkEsc,
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
    /// The actual private-marker byte (`?`/`<`/`=`/`>`) of the CSI in flight, or
    /// `0` if none. Lets the dispatcher tell DA2 (`CSI > c`) from DEC private
    /// modes (`CSI ? … h/l`). Reset at the start of each CSI.
    csi_marker: u8,
    /// Code point accumulated so far while in [`ParserState::Utf8`].
    utf8_acc: u32,
    /// Number of UTF-8 continuation bytes still expected.
    utf8_remaining: usize,
    /// Bytes the parser owes the host in reply to a query (DA1/DA2/DSR). The
    /// driver drains these via [`AnsiParser::take_responses`] after each
    /// `advance` and writes them back to the PTY master, where the child reads
    /// them as terminal input.
    responses: Vec<u8>,
    /// Raw bytes of the OSC string currently being collected (between `ESC ]`
    /// and its `BEL`/`ST` terminator). Decoded as UTF-8 at dispatch. Capped at
    /// [`OSC_MAX`] so a pathological unterminated OSC can't grow without bound.
    osc_buffer: Vec<u8>,
}

/// Upper bound on the bytes buffered for a single OSC string. Real titles and
/// cwd URIs are far shorter; past this we keep consuming the string but stop
/// storing it.
const OSC_MAX: usize = 4096;

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
            csi_marker: 0,
            utf8_acc: 0,
            utf8_remaining: 0,
            responses: Vec::new(),
            osc_buffer: Vec::new(),
        }
    }

    /// Drain the bytes the parser owes the host in reply to queries (DA1/DA2/
    /// DSR). Returns an empty vector when there is nothing to send. The driver
    /// calls this after `advance` and writes the result to the PTY master.
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.responses)
    }

    /// Act on the OSC string just collected in `osc_buffer`. The payload is
    /// `<code> ; <text>`; we handle window title (0/2) and working-directory
    /// (7) reports, recording them on the grid. Other OSC codes (4, 8, 52,
    /// 133, …) are recognized as well-formed and ignored for now.
    fn dispatch_osc(&mut self, g: &mut Grid) {
        let payload = String::from_utf8_lossy(&self.osc_buffer);
        let Some((code, text)) = payload.split_once(';') else {
            return; // no separator — nothing actionable
        };
        match code {
            // 0 sets icon name *and* window title; 2 sets the window title.
            "0" | "2" => g.title = text.to_string(),
            // 7 reports the working directory (usually a file:// URI).
            "7" => g.cwd = text.to_string(),
            _ => {}
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
                        self.csi_marker = 0;
                        self.state = ParserState::Csi;
                    }
                    b']' => {
                        self.osc_buffer.clear();
                        self.state = ParserState::Osc;
                    }
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
                    // String-type introducers — DCS (`P`), SOS (`X`), PM (`^`),
                    // APC (`_`). Their bodies are consumed opaquely until ST so
                    // they don't leak as printed text (cf. tmux DCS passthrough,
                    // DECRQSS replies, Kitty graphics, Sixel).
                    b'P' | b'X' | b'^' | b'_' => self.state = ParserState::StrSink,
                    // Any other ESC X sequence is a single byte we don't model;
                    // consuming b returns us to ground without leaking it.
                    _ => self.state = ParserState::Ground,
                },
                ParserState::Csi => match b {
                    // Parameter bytes.
                    b'0'..=b'9' | b';' => self.param_buffer.push(b as char),
                    // Private markers (`<`, `=`, `>`, `?`): flag, remember which,
                    // and keep collecting.
                    0x3c..=0x3f => {
                        self.csi_private = true;
                        self.csi_marker = b;
                    }
                    // Intermediate bytes (space..`/`): ignored but part of the sequence.
                    0x20..=0x2f => {}
                    // Final byte: dispatch and reset. Private sequences (with a
                    // `?`/`<`/`=`/`>` marker) go to their own handler.
                    0x40..=0x7e => {
                        if self.csi_private {
                            self.handle_private_csi(b, g);
                        } else {
                            self.handle_csi(b, g);
                        }
                        self.state = ParserState::Ground;
                        self.param_buffer.clear();
                        self.csi_private = false;
                    }
                    // CAN / SUB cancel the sequence.
                    0x18 | 0x1a => {
                        self.state = ParserState::Ground;
                        self.param_buffer.clear();
                        self.csi_private = false;
                    }
                    // ESC starts a fresh escape sequence.
                    0x1b => {
                        self.state = ParserState::Esc;
                        self.param_buffer.clear();
                        self.csi_private = false;
                    }
                    // Other C0 controls execute in place; the CSI continues
                    // (VT500 parser semantics) so its parameters are preserved.
                    0x00..=0x17 | 0x19 | 0x1c..=0x1f => {
                        self.ground_byte(b, g);
                        self.state = ParserState::Csi;
                    }
                    // DEL is ignored inside a CSI; any other byte aborts.
                    _ => {
                        if b != 0x7f {
                            self.state = ParserState::Ground;
                            self.param_buffer.clear();
                            self.csi_private = false;
                        }
                    }
                },
                ParserState::Osc => match b {
                    0x07 => {
                        // BEL terminator: act on the collected string.
                        self.dispatch_osc(g);
                        self.state = ParserState::Ground;
                    }
                    0x1b => self.state = ParserState::OscEsc, // possible ST (ESC \)
                    _ => {
                        // Accumulate the payload byte, bounded.
                        if self.osc_buffer.len() < OSC_MAX {
                            self.osc_buffer.push(b);
                        }
                    }
                },
                ParserState::OscEsc => {
                    // Whether or not this is the `\` of an ST, the OSC string is
                    // over; act on it and return to ground.
                    self.dispatch_osc(g);
                    self.state = ParserState::Ground;
                }
                ParserState::EscCharset => self.state = ParserState::Ground,
                ParserState::StrSink => match b {
                    0x1b => self.state = ParserState::StrSinkEsc, // possible ST (ESC \)
                    0x18 | 0x1a => self.state = ParserState::Ground, // CAN / SUB abort
                    _ => {}                                          // consume body byte
                },
                ParserState::StrSinkEsc => {
                    // Whether or not this is the `\` of an ST, the string is
                    // over; drop the byte and return to ground.
                    self.state = ParserState::Ground;
                }
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

    /// Handle a private CSI sequence (one carrying a `?`/`<`/`=`/`>` marker).
    ///
    /// Only the alternate-screen DEC modes are acted upon; other private modes
    /// (bracketed paste `2004`, cursor visibility `25`, …) are consumed and
    /// ignored so they never leak as text.
    fn handle_private_csi(&mut self, cmd: u8, g: &mut Grid) {
        // DA2 (Secondary Device Attributes): `CSI > c`. Reply with a terminal
        // type (0), a firmware "version", and a ROM cartridge field (0) — the
        // values are conventional; programs care that an answer arrives.
        if self.csi_marker == b'>' && cmd == b'c' {
            self.responses.extend_from_slice(b"\x1b[>0;1;0c");
            return;
        }
        let params = self.parse_params();
        // 47 / 1047 / 1049 select the alternate screen buffer; the mode governs
        // cursor save/restore (only 1049). The leave path uses the mode stashed
        // on entry, so the reset parameter's exact number doesn't matter.
        let mode = params.iter().flatten().copied().find_map(alt_mode);
        match (cmd, mode) {
            (b'h', Some(mode)) => g.enter_alt_screen(mode),
            (b'l', Some(_)) => g.leave_alt_screen(),
            _ => {}
        }
    }

    /// Parse `param_buffer` into positional parameters. An empty slot (e.g. the
    /// leading field of `CSI ;5H`) becomes `None` so callers can apply the
    /// per-command default in the correct position, per ECMA-48 §5.4.2.
    fn parse_params(&self) -> Vec<Option<usize>> {
        if self.param_buffer.is_empty() {
            return Vec::new();
        }
        self.param_buffer
            .split(';')
            .map(|s| if s.is_empty() { None } else { s.parse().ok() })
            .collect()
    }

    /// Dispatch a completed CSI sequence given its final command byte.
    fn handle_csi(&mut self, cmd: u8, g: &mut Grid) {
        let params = self.parse_params();
        // Positional parameter `i` with `default` applied to an absent/empty slot.
        let p = |i: usize, default: usize| params.get(i).copied().flatten().unwrap_or(default);
        // Most cursor-motion commands take a single count that defaults to (and
        // treats 0 as) 1.
        let count = p(0, 1).max(1);

        match cmd {
            b'm' => {
                // SGR: an empty list resets; otherwise an empty slot means 0.
                let sgr: Vec<usize> = params.iter().map(|o| o.unwrap_or(0)).collect();
                self.apply_sgr(&sgr);
            }
            b'H' | b'f' => {
                // CSI row ; col H — both 1-based; default to 1.
                g.set_cursor(p(1, 1).saturating_sub(1), p(0, 1).saturating_sub(1));
            }
            b'A' => g.set_cursor(g.cursor.0, g.cursor.1.saturating_sub(count)), // CUU
            b'B' => g.set_cursor(g.cursor.0, g.cursor.1.saturating_add(count)), // CUD
            b'C' => g.set_cursor(g.cursor.0.saturating_add(count), g.cursor.1), // CUF
            b'D' => g.set_cursor(g.cursor.0.saturating_sub(count), g.cursor.1), // CUB
            b'G' => g.set_cursor(p(0, 1).saturating_sub(1), g.cursor.1),        // CHA
            b'd' => g.set_cursor(g.cursor.0, p(0, 1).saturating_sub(1)),        // VPA
            b'@' => g.insert_chars(count), // ICH
            b'P' => g.delete_chars(count), // DCH
            b'X' => g.erase_chars(count),  // ECH
            b's' => g.save_cursor(),       // SCP
            b'u' => g.restore_cursor(),    // RCP
            b'r' => {
                // DECSTBM — set top/bottom scrolling margins (1-based).
                let top = p(0, 1).saturating_sub(1);
                let bottom = p(1, g.rows).saturating_sub(1);
                g.set_scroll_region(top, bottom);
            }
            b'J' => {
                // ED — erase in display.
                let (cx, cy) = g.cursor;
                match p(0, 0) {
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
                match p(0, 0) {
                    0 => g.clear_row_range(cy, cx, g.cols),
                    1 => g.clear_row_range(cy, 0, cx + 1),
                    2 => g.clear_row_range(cy, 0, g.cols),
                    _ => {}
                }
            }
            b'c' => {
                // DA1 (Primary Device Attributes). Only the default/`0` form is
                // a query; reply that we're a VT100 with Advanced Video Option,
                // a level apps widely accept. A program that sent this would
                // otherwise block waiting for the answer.
                if p(0, 0) == 0 {
                    self.responses.extend_from_slice(b"\x1b[?1;2c");
                }
            }
            b'n' => match p(0, 0) {
                // DSR — Device Status Report.
                5 => self.responses.extend_from_slice(b"\x1b[0n"), // terminal OK
                6 => {
                    // CPR — report the cursor position, 1-based row;col.
                    let (cx, cy) = g.cursor;
                    self.responses
                        .extend_from_slice(format!("\x1b[{};{}R", cy + 1, cx + 1).as_bytes());
                }
                _ => {}
            },
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
    fn alt_screen_saves_and_restores_primary() {
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"primary text");
        assert_eq!(&row_text(&g, 0)[..12], "primary text");

        // Enter alt screen (DEC private 1049): cleared, cursor home.
        p.advance(&mut g, b"\x1b[?1049h");
        assert_eq!(g.cells[0].ch, ' ');
        assert_eq!(g.cursor, (0, 0));
        p.advance(&mut g, b"ALT");
        assert_eq!(&row_text(&g, 0)[..3], "ALT");

        // Leave alt screen: primary content and cursor come back.
        p.advance(&mut g, b"\x1b[?1049l");
        assert_eq!(&row_text(&g, 0)[..12], "primary text");
        assert_eq!(g.cursor, (12, 0));
    }

    #[test]
    fn alt_screen_47_does_not_save_or_restore_cursor() {
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"primary");
        g.cursor = (5, 5);

        // ?47 swaps the buffer but must not home or save the cursor.
        p.advance(&mut g, b"\x1b[?47h");
        assert_eq!(g.cursor, (5, 5)); // not homed (unlike 1049)
        g.cursor = (10, 3);

        // ?47l swaps back without restoring the cursor.
        p.advance(&mut g, b"\x1b[?47l");
        assert_eq!(g.cursor, (10, 3)); // not restored
        assert_eq!(&row_text(&g, 0)[..7], "primary"); // primary buffer back
    }

    #[test]
    fn alt_screen_survives_resize() {
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"keep me");
        p.advance(&mut g, b"\x1b[?1049h"); // to alt
        g.resize(100, 30); // resize while on alt screen
        p.advance(&mut g, b"\x1b[?1049l"); // back to primary
        assert_eq!(g.cols, 100);
        assert_eq!(&row_text(&g, 0)[..7], "keep me");
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
    fn scroll_region_limits_scrolling_and_dirtying() {
        // 1-row grid would be degenerate; use 5 rows, region = rows 2..=3 (1-based 3;4).
        let mut g = Grid::new(4, 5);
        let mut p = AnsiParser::new();
        // Fill rows 2 and 3 with markers.
        g.cursor = (0, 2);
        p.advance(&mut g, b"AAAA");
        g.cursor = (0, 3);
        p.advance(&mut g, b"BBBB");
        // Set region to rows 3..4 (1-based) = 2..=3 (0-based) and clear dirty.
        p.advance(&mut g, b"\x1b[3;4r");
        assert_eq!((g.scroll_top, g.scroll_bottom), (2, 3));
        assert_eq!(g.cursor, (0, 0)); // DECSTBM homes the cursor
        g.clear_dirty();

        // Put cursor at region bottom and newline -> scroll only the region.
        g.cursor = (0, 3);
        p.advance(&mut g, b"\n");
        assert_eq!(row_text(&g, 2).trim_end(), "BBBB"); // row 3 shifted up to row 2
        assert_eq!(row_text(&g, 3).trim_end(), "");      // region bottom blanked
        // Only region rows are dirty.
        assert_eq!(g.dirty, vec![false, false, true, true, false]);
    }

    #[test]
    fn scroll_region_resets_on_full_screen_request() {
        let mut g = Grid::new(4, 5);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[2;4r");
        assert_eq!((g.scroll_top, g.scroll_bottom), (1, 3));
        p.advance(&mut g, b"\x1b[r"); // no params -> full screen
        assert_eq!((g.scroll_top, g.scroll_bottom), (0, 4));
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
    fn csi_empty_leading_param_keeps_position() {
        // CSI ;5H -> row defaults to 1, column = 5 -> (col 4, row 0).
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        g.cursor = (10, 10);
        p.advance(&mut g, b"\x1b[;5H");
        assert_eq!(g.cursor, (4, 0));
        // CSI ;10r -> top defaults to 1 (row 0), bottom = 10 (row 9).
        p.advance(&mut g, b"\x1b[;10r");
        assert_eq!((g.scroll_top, g.scroll_bottom), (0, 9));
    }

    #[test]
    fn csi_huge_count_does_not_overflow() {
        // CUD/CUF with a near-usize::MAX count must saturate, not panic/wrap.
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        g.cursor = (5, 5);
        p.advance(&mut g, b"\x1b[18446744073709551610B"); // CUD
        assert_eq!(g.cursor.1, 23); // clamped to last row
        p.advance(&mut g, b"\x1b[18446744073709551610C"); // CUF
        assert_eq!(g.cursor.0, 79); // clamped to last column
    }

    #[test]
    fn c0_control_inside_csi_executes_and_continues() {
        // CSI 5 \r ; 10 H: the CR executes mid-sequence, the CSI continues, and
        // nothing leaks as printed text.
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[5\r;10H");
        assert_eq!(g.cursor, (9, 4)); // CUP row 5, col 10 applied
        assert_eq!(g.cells[0].ch, ' '); // ";10H" not printed
    }

    #[test]
    fn alt_screen_does_not_leak_saved_cursor() {
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        g.cursor = (5, 5);
        p.advance(&mut g, b"\x1b7"); // DECSC on primary -> saved (5,5)
        p.advance(&mut g, b"\x1b[?1049h"); // to alt
        g.cursor = (10, 10);
        p.advance(&mut g, b"\x1b7"); // DECSC on alt -> alt's saved (10,10)
        p.advance(&mut g, b"\x1b[?1049l"); // back to primary
        p.advance(&mut g, b"\x1b8"); // DECRC on primary
        assert_eq!(g.cursor, (5, 5)); // primary's saved cursor intact
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
    fn char_width_classifies_common_cases() {
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('é'), 1);
        assert_eq!(char_width('世'), 2); // CJK
        assert_eq!(char_width('😀'), 2); // emoji
        assert_eq!(char_width('\u{0301}'), 0); // combining acute accent
    }

    #[test]
    fn char_width_covers_cases_the_old_table_missed() {
        // Zero-width characters the hand-rolled table didn't list. Getting any
        // of these wrong shifts the rest of the line (cursor desync).
        assert_eq!(char_width('\u{200D}'), 0); // ZWJ (emoji sequence glue)
        assert_eq!(char_width('\u{200C}'), 0); // ZWNJ
        assert_eq!(char_width('\u{FE0F}'), 0); // VS16 (emoji presentation selector)
        assert_eq!(char_width('\u{064B}'), 0); // Arabic fathatan
        assert_eq!(char_width('\u{094D}'), 0); // Devanagari virama
        assert_eq!(char_width('\u{1160}'), 0); // Hangul conjoining jungseong filler

        // Default-emoji-presentation symbols below the old 0x2E80 wide cutoff;
        // these render double-width and were previously reported as 1.
        assert_eq!(char_width('\u{231A}'), 2); // ⌚ WATCH
        assert_eq!(char_width('\u{26A1}'), 2); // ⚡ HIGH VOLTAGE
        assert_eq!(char_width('\u{2705}'), 2); // ✅ WHITE HEAVY CHECK MARK

        // Text-presentation-by-default symbol stays width 1 (no VS16 follows).
        assert_eq!(char_width('\u{2764}'), 1); // ❤ HEAVY BLACK HEART
    }

    #[test]
    fn wide_char_occupies_two_cells() {
        let g = parse("世x".as_bytes(), 80, 24);
        assert_eq!(g.cells[0].ch, '世');
        assert_eq!(g.cells[0].flags & WIDE_TRAILER, 0);
        assert_ne!(g.cells[1].flags & WIDE_TRAILER, 0); // trailer flagged
        assert_eq!(g.cells[2].ch, 'x'); // next glyph after the wide pair
        assert_eq!(g.cursor, (3, 0));
    }

    #[test]
    fn overwriting_wide_head_clears_orphan_trailer() {
        let mut g = Grid::new(80, 24);
        g.put_char('世', DEFAULT_FG, DEFAULT_BG); // head col 0, trailer col 1
        g.cursor = (0, 0);
        g.put_char('a', DEFAULT_FG, DEFAULT_BG); // overwrite the head
        assert_eq!(g.cells[0].ch, 'a');
        assert_eq!(g.cells[1].ch, ' '); // orphaned trailer blanked
        assert_eq!(g.cells[1].flags & WIDE_TRAILER, 0);
    }

    #[test]
    fn overwriting_wide_trailer_clears_orphan_head() {
        let mut g = Grid::new(80, 24);
        g.put_char('世', DEFAULT_FG, DEFAULT_BG); // head col 0, trailer col 1
        g.cursor = (1, 0);
        g.put_char('b', DEFAULT_FG, DEFAULT_BG); // overwrite the trailer
        assert_eq!(g.cells[1].ch, 'b');
        assert_eq!(g.cells[0].ch, ' '); // orphaned head blanked
    }

    #[test]
    fn wide_char_wraps_when_it_would_not_fit() {
        // Width-3 grid: 'a' at col 0, wide '世' needs cols 1-2 -> fits at 1..3.
        let g = parse("a世".as_bytes(), 3, 24);
        assert_eq!(g.cells[0].ch, 'a');
        assert_eq!(g.cells[1].ch, '世');
        // Now only 1 column free; a second wide char must wrap to the next row.
        let g2 = parse("ab世".as_bytes(), 3, 24);
        assert_eq!(row_text(&g2, 0), "ab ");
        assert_eq!(g2.cells[3].ch, '世'); // wrapped to row 1, col 0
    }

    #[test]
    fn combining_mark_attaches_to_preceding_glyph() {
        // 'a' + U+0301 (combining acute) + 'b'.
        let g = parse("a\u{0301}b".as_bytes(), 80, 24);
        assert_eq!(g.cells[0].ch, 'a');
        assert_eq!(g.cells[0].combining[0], '\u{0301}'); // mark composed onto 'a'
        assert_eq!(g.cells[1].ch, 'b'); // mark consumed no cell
        assert_eq!(g.cursor, (2, 0));
    }

    #[test]
    fn multiple_combining_marks_and_overflow() {
        let mut g = Grid::new(80, 24);
        g.put_char('e', DEFAULT_FG, DEFAULT_BG);
        // Two marks fill both slots; a third is dropped (bounded).
        g.put_char('\u{0301}', DEFAULT_FG, DEFAULT_BG);
        g.put_char('\u{0323}', DEFAULT_FG, DEFAULT_BG);
        g.put_char('\u{0308}', DEFAULT_FG, DEFAULT_BG);
        assert_eq!(g.cells[0].combining, ['\u{0301}', '\u{0323}']);
        assert_eq!(g.cursor, (1, 0));
    }

    #[test]
    fn combining_mark_at_line_start_is_dropped() {
        // No preceding glyph -> nothing to attach to.
        let g = parse("\u{0301}x".as_bytes(), 80, 24);
        assert_eq!(g.cells[0].ch, 'x');
        assert_eq!(g.cursor, (1, 0));
    }

    #[test]
    fn combining_mark_attaches_to_wide_glyph_head() {
        let g = parse("世\u{0301}".as_bytes(), 80, 24);
        assert_eq!(g.cells[0].ch, '世');
        assert_eq!(g.cells[0].combining[0], '\u{0301}'); // on the head, not the trailer
        assert_ne!(g.cells[1].flags & WIDE_TRAILER, 0);
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

    #[test]
    fn dcs_string_is_consumed_not_printed() {
        // DCS (ESC P) … ST (ESC \) — e.g. a DECRQSS status reply. The body must
        // not leak onto the screen.
        let g = parse(b"\x1bP1$r0m\x1b\\X", 80, 24);
        assert_eq!(g.cells[0].ch, 'X');
        assert_eq!(g.cursor, (1, 0));
    }

    #[test]
    fn apc_string_is_consumed_not_printed() {
        // APC (ESC _) … ST — e.g. the Kitty graphics protocol introducer.
        let g = parse(b"\x1b_Gf=100,a=T;base64data\x1b\\X", 80, 24);
        assert_eq!(g.cells[0].ch, 'X');
        assert_eq!(g.cursor, (1, 0));
    }

    #[test]
    fn pm_string_is_consumed_not_printed() {
        // PM (ESC ^) … ST.
        let g = parse(b"\x1b^private message\x1b\\X", 80, 24);
        assert_eq!(g.cells[0].ch, 'X');
        assert_eq!(g.cursor, (1, 0));
    }

    #[test]
    fn sos_string_is_consumed_not_printed() {
        // SOS (ESC X) … ST.
        let g = parse(b"\x1bXstart of string\x1b\\Y", 80, 24);
        assert_eq!(g.cells[0].ch, 'Y');
        assert_eq!(g.cursor, (1, 0));
    }

    #[test]
    fn dcs_string_split_across_chunks_is_consumed() {
        // The string sink state must persist across read boundaries.
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1bP1$r");
        p.advance(&mut g, b"sixel-ish body");
        p.advance(&mut g, b"\x1b\\Z");
        assert_eq!(g.cells[0].ch, 'Z');
        assert_eq!(g.cursor, (1, 0));
    }

    #[test]
    fn dcs_string_aborted_by_can() {
        // CAN (0x18) cancels the string; subsequent bytes render normally.
        let g = parse(b"\x1bPbody\x18X", 80, 24);
        assert_eq!(g.cells[0].ch, 'X');
        assert_eq!(g.cursor, (1, 0));
    }

    #[test]
    fn da1_query_is_answered_and_not_printed() {
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        // Both the bare and explicit-0 forms are queries.
        p.advance(&mut g, b"\x1b[c");
        assert_eq!(p.take_responses(), b"\x1b[?1;2c");
        p.advance(&mut g, b"\x1b[0c");
        assert_eq!(p.take_responses(), b"\x1b[?1;2c");
        // Nothing leaked onto the grid.
        assert_eq!(g.cells[0].ch, ' ');
        assert_eq!(g.cursor, (0, 0));
    }

    #[test]
    fn da2_query_is_answered() {
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[>c");
        assert_eq!(p.take_responses(), b"\x1b[>0;1;0c");
        // The `>` marker must not be confused with a DEC private mode and must
        // not disturb the alt screen.
        assert!(g.cells[0].ch == ' ');
    }

    #[test]
    fn dsr_status_report_is_answered() {
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[5n");
        assert_eq!(p.take_responses(), b"\x1b[0n");
    }

    #[test]
    fn dsr_cursor_position_report_uses_one_based_coords() {
        let mut g = Grid::new(80, 24);
        g.cursor = (4, 9); // col 4, row 9 (0-based)
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[6n");
        assert_eq!(p.take_responses(), b"\x1b[10;5R"); // row 10, col 5 (1-based)
    }

    #[test]
    fn no_query_means_no_response() {
        // A normal print run owes the host nothing.
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"hello\x1b[31mworld");
        assert!(p.take_responses().is_empty());
    }

    #[test]
    fn osc_2_sets_window_title() {
        let mut g = parse(b"\x1b]2;My Title\x07", 80, 24);
        assert_eq!(g.title, "My Title");
        // OSC 0 also sets the title.
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b]0;Another\x07");
        assert_eq!(g.title, "Another");
    }

    #[test]
    fn osc_7_sets_working_directory() {
        let g = parse(b"\x1b]7;file://host/home/user\x1b\\", 80, 24);
        assert_eq!(g.cwd, "file://host/home/user");
    }

    #[test]
    fn osc_title_decodes_utf8_and_does_not_print() {
        // Multi-byte payload must round-trip, and the trailing 'X' still renders.
        let g = parse("\x1b]2;café 世\x07X".as_bytes(), 80, 24);
        assert_eq!(g.title, "café 世");
        assert_eq!(g.cells[0].ch, 'X');
        assert_eq!(g.cursor, (1, 0));
    }

    #[test]
    fn osc_split_across_chunks_is_captured() {
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b]2;split ");
        p.advance(&mut g, b"title\x07");
        assert_eq!(g.title, "split title");
    }

    #[test]
    fn osc_without_separator_is_ignored() {
        // No ';' — not actionable, and must not panic or print.
        let g = parse(b"\x1b]999\x07Z", 80, 24);
        assert_eq!(g.title, "");
        assert_eq!(g.cwd, "");
        assert_eq!(g.cells[0].ch, 'Z');
    }

    #[test]
    fn osc_unknown_code_is_ignored_but_consumed() {
        // OSC 52 (clipboard) isn't acted on yet, but must not leak or set title.
        let g = parse(b"\x1b]52;c;SGVsbG8=\x07W", 80, 24);
        assert_eq!(g.title, "");
        assert_eq!(g.cells[0].ch, 'W');
    }
}
