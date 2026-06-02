//! The screen buffer (L06 state): the authoritative grid of cells, the cursor,
//! scrolling region, alternate screen, and scrollback history.
//!
//! The [`Grid`] exposes a semantic API (`put_char`, `set_cursor`, `scroll_up`,
//! …) that the [`AnsiParser`](super::parser::AnsiParser) drives, and produces a
//! [`DirtyFrame`] snapshot for the renderer.

use std::collections::VecDeque;

use super::cell::{char_width, Cell, Pen, MAX_COMBINING, WIDE_TRAILER};

/// Maximum number of lines retained in the scrollback history. Older lines are
/// evicted from the front once this is exceeded.
pub const SCROLLBACK_MAX: usize = 10_000;

/// Maximum number of distinct hyperlink URIs interned from OSC 8. Past this,
/// further links are dropped (rendered as plain text) rather than growing the
/// table without bound.
const LINK_MAX: usize = 4096;

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
    /// Lines that have scrolled off the top of the primary screen, oldest at the
    /// front. Bounded by [`SCROLLBACK_MAX`]. Each line is stored at the width it
    /// had when evicted and padded/truncated to the live width when viewed.
    pub(crate) scrollback: VecDeque<Vec<Cell>>,
    /// How many lines the viewport is scrolled up into [`Grid::scrollback`].
    /// `0` is the live view (bottom); the renderer composites history above the
    /// live grid when this is non-zero.
    pub view_offset: usize,
    /// Bytes destined for the *host* terminal (not the grid): OSC 52 clipboard
    /// requests forwarded verbatim. The renderer drains these via
    /// [`Grid::take_host_out`] each frame and writes them to its stdout.
    pub(crate) host_out: Vec<u8>,
    /// Interned hyperlink URIs from OSC 8; a [`Cell::link`] of `n` refers to
    /// `links[n - 1]` (`0` means no link). Append-only and bounded by
    /// [`LINK_MAX`], so ids stay stable for cells held in scrollback.
    pub(crate) links: Vec<String>,
    /// The hyperlink id stamped onto cells written while an OSC 8 link is open
    /// (`0` when none). Set by the parser via [`Grid::set_link`].
    current_link: u16,
    /// Columns at which a horizontal tab stops; `len() == cols`. Defaults to
    /// every 8th column and is modified by `HTS` / `TBC`. A resize preserves
    /// stops within the surviving width and defaults the new columns.
    tab_stops: Vec<bool>,
}

/// Build the default tab-stop table for a `cols`-wide grid: a stop at every
/// 8th column (0, 8, 16, …), matching the classic 8-column default.
fn default_tab_stops(cols: usize) -> Vec<bool> {
    (0..cols).map(|i| i % 8 == 0).collect()
}

/// Which DEC private mode selected the alternate screen, which determines the
/// cursor save/restore behaviour on exit (per xterm: only `1049` does it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AltMode {
    /// `?47` — bare buffer swap, no cursor save/restore.
    Dec47,
    /// `?1047` — buffer swap, no cursor save/restore.
    Dec1047,
    /// `?1049` — save cursor on entry (DECSC-style), restore on exit.
    Dec1049,
}

/// Map a DEC private parameter to its alternate-screen mode, if any.
pub(crate) fn alt_mode(param: usize) -> Option<AltMode> {
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
            scrollback: VecDeque::new(),
            view_offset: 0,
            host_out: Vec::new(),
            links: Vec::new(),
            current_link: 0,
            tab_stops: default_tab_stops(cols),
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
        // Capture the line leaving the top into scrollback, but only for a
        // full-screen scroll on the primary buffer: partial-region scrolls
        // (TUI apps using DECSTBM) and the alternate screen don't form history.
        if top == 0 && bottom == self.rows - 1 && self.primary.is_none() {
            self.scrollback.push_back(self.cells[0..self.cols].to_vec());
            if self.scrollback.len() > SCROLLBACK_MAX {
                self.scrollback.pop_front();
            }
            // If the user is browsing history, advance the offset in step with
            // the incoming line so the viewed region stays put under new output.
            if self.view_offset > 0 {
                self.view_offset = (self.view_offset + 1).min(self.scrollback.len());
                self.dirty.iter_mut().for_each(|d| *d = true);
            }
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
    pub(crate) fn set_scroll_region(&mut self, top: usize, bottom: usize) {
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

    /// Write `ch` at the cursor with the given [`Pen`], wrapping to the next
    /// line if it would not fit, then advancing the cursor by the glyph's
    /// display width. A double-width glyph also writes a flagged trailing cell.
    pub fn put_char(&mut self, ch: char, pen: Pen) {
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
        let link = self.current_link;
        self.set_cell(
            x,
            y,
            Cell { ch, combining: ['\0'; MAX_COMBINING], fg: pen.fg, bg: pen.bg, flags: pen.attrs, link },
        );
        if w == 2 && x + 1 < self.cols {
            // Trailing half: a flagged placeholder the renderer skips. It keeps
            // the pen's colors but only the WIDE_TRAILER layout bit.
            self.set_cell(
                x + 1,
                y,
                Cell {
                    ch: ' ',
                    combining: ['\0'; MAX_COMBINING],
                    fg: pen.fg,
                    bg: pen.bg,
                    flags: WIDE_TRAILER,
                    link,
                },
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
    pub(crate) fn clear_row_range(&mut self, y: usize, from: usize, to: usize) {
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
    pub(crate) fn clear_all(&mut self) {
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
        // Preserve tab stops within the surviving width; default new columns.
        let mut stops = default_tab_stops(cols);
        let keep = cols.min(self.cols);
        stops[..keep].copy_from_slice(&self.tab_stops[..keep]);
        self.tab_stops = stops;
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
    pub(crate) fn enter_alt_screen(&mut self, mode: AltMode) {
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
        // History isn't browsable under a full-screen app; snap to the live view.
        self.view_offset = 0;
        self.reset_scroll_region();
        self.dirty.iter_mut().for_each(|d| *d = true);
    }

    /// Switch back to the primary screen, restoring its buffer. For `?1049` the
    /// cursor (and DECSC register) saved on entry are restored; `?47`/`?1047`
    /// leave the cursor as the alternate session left it. No-op if not on the
    /// alternate screen.
    pub(crate) fn leave_alt_screen(&mut self) {
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
    pub(crate) fn save_cursor(&mut self) {
        self.saved_cursor = self.cursor;
    }

    /// Restore the saved cursor position (`DECRC` / `CSI u`), clamped.
    pub(crate) fn restore_cursor(&mut self) {
        let (x, y) = self.saved_cursor;
        self.set_cursor(x, y);
    }

    /// Delete `n` characters at the cursor, shifting the remainder of the row
    /// left and blanking the freed cells at the right (`DCH`).
    pub(crate) fn delete_chars(&mut self, n: usize) {
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
    pub(crate) fn insert_chars(&mut self, n: usize) {
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
    pub(crate) fn erase_chars(&mut self, n: usize) {
        let (x, y) = self.cursor;
        let n = n.min(self.cols.saturating_sub(x));
        self.clear_row_range(y, x, x + n);
    }

    /// Insert `n` blank lines at the cursor row, pushing the rows below it down
    /// within the scrolling region; rows pushed past the region bottom are lost
    /// (`IL`). A no-op when the cursor is outside the scrolling region.
    pub(crate) fn insert_lines(&mut self, n: usize) {
        let cy = self.cursor.1;
        if cy < self.scroll_top || cy > self.scroll_bottom {
            return;
        }
        let n = n.min(self.scroll_bottom + 1 - cy);
        let cols = self.cols;
        // Shift rows [cy, scroll_bottom - n] down by n. copy_within is a memmove,
        // so the forward (overlapping) copy is well-defined.
        let count = (self.scroll_bottom + 1 - cy - n) * cols;
        if count > 0 {
            let src = cy * cols;
            let dst = (cy + n) * cols;
            self.cells.copy_within(src..src + count, dst);
        }
        // Blank the n freed rows at the cursor.
        let blank_end = (cy + n) * cols;
        for c in &mut self.cells[cy * cols..blank_end] {
            *c = Cell::blank();
        }
        for d in &mut self.dirty[cy..=self.scroll_bottom] {
            *d = true;
        }
    }

    /// Delete `n` lines at the cursor row, pulling the rows below it up within
    /// the scrolling region and blanking the freed rows at the region bottom
    /// (`DL`). A no-op when the cursor is outside the scrolling region.
    pub(crate) fn delete_lines(&mut self, n: usize) {
        let cy = self.cursor.1;
        if cy < self.scroll_top || cy > self.scroll_bottom {
            return;
        }
        let n = n.min(self.scroll_bottom + 1 - cy);
        let cols = self.cols;
        // Shift rows [cy + n, scroll_bottom] up by n.
        let count = (self.scroll_bottom + 1 - cy - n) * cols;
        if count > 0 {
            let src = (cy + n) * cols;
            let dst = cy * cols;
            self.cells.copy_within(src..src + count, dst);
        }
        // Blank the n rows freed at the region bottom.
        let first_blank = (self.scroll_bottom + 1 - n) * cols;
        let region_end = (self.scroll_bottom + 1) * cols;
        for c in &mut self.cells[first_blank..region_end] {
            *c = Cell::blank();
        }
        for d in &mut self.dirty[cy..=self.scroll_bottom] {
            *d = true;
        }
    }

    /// Scroll the current scrolling region up by `n` rows (`SU`). Reuses the
    /// single-row [`Grid::scroll_up`] per line, so a full-screen scroll on the
    /// primary buffer captures the displaced lines into scrollback exactly as a
    /// line feed would.
    pub(crate) fn scroll_up_n(&mut self, n: usize) {
        for _ in 0..n {
            self.scroll_up();
        }
    }

    /// Scroll the current scrolling region down by `n` rows (`SD`): shift the
    /// region's rows down and blank the `n` freed rows at the top. Displaced
    /// bottom rows are lost (scrollback is never un-scrolled).
    pub(crate) fn scroll_down_n(&mut self, n: usize) {
        let (top, bottom) = (self.scroll_top, self.scroll_bottom);
        if bottom <= top || bottom >= self.rows {
            return;
        }
        let n = n.min(bottom + 1 - top);
        let cols = self.cols;
        // Shift rows [top, bottom - n] down by n.
        let count = (bottom + 1 - top - n) * cols;
        if count > 0 {
            let src = top * cols;
            let dst = (top + n) * cols;
            self.cells.copy_within(src..src + count, dst);
        }
        // Blank the n freed rows at the region top.
        let blank_end = (top + n) * cols;
        for c in &mut self.cells[top * cols..blank_end] {
            *c = Cell::blank();
        }
        for d in &mut self.dirty[top..=bottom] {
            *d = true;
        }
    }

    /// Move the cursor up one row, scrolling the region down when already at its
    /// top (`RI`, reverse index). The mirror of a line feed at the region bottom.
    pub(crate) fn reverse_index(&mut self) {
        if self.cursor.1 == self.scroll_top {
            self.scroll_down_n(1);
        } else if self.cursor.1 > 0 {
            self.cursor.1 -= 1;
        }
    }

    /// Move the cursor forward `n` tab stops (`HT` / `CHT`), without writing
    /// over the cells it passes. Stops at the right margin when no further tab
    /// stop exists.
    pub(crate) fn tab_forward(&mut self, n: usize) {
        let last = self.cols.saturating_sub(1);
        let mut x = self.cursor.0;
        for _ in 0..n {
            if x >= last {
                x = last;
                break;
            }
            let mut nx = x + 1;
            while nx < last && !self.tab_stops[nx] {
                nx += 1;
            }
            x = nx;
        }
        self.cursor.0 = x.min(last);
    }

    /// Move the cursor back `n` tab stops (`CBT`). Stops at column 0.
    pub(crate) fn tab_backward(&mut self, n: usize) {
        let mut x = self.cursor.0.min(self.cols.saturating_sub(1));
        for _ in 0..n {
            if x == 0 {
                break;
            }
            let mut nx = x - 1;
            while nx > 0 && !self.tab_stops[nx] {
                nx -= 1;
            }
            x = nx;
        }
        self.cursor.0 = x;
    }

    /// Set a tab stop at the current cursor column (`HTS`).
    pub(crate) fn set_tab_stop(&mut self) {
        let x = self.cursor.0;
        if x < self.tab_stops.len() {
            self.tab_stops[x] = true;
        }
    }

    /// Clear the tab stop at the current cursor column (`TBC 0`).
    pub(crate) fn clear_tab_stop(&mut self) {
        let x = self.cursor.0;
        if x < self.tab_stops.len() {
            self.tab_stops[x] = false;
        }
    }

    /// Clear every tab stop (`TBC 3`).
    pub(crate) fn clear_all_tab_stops(&mut self) {
        self.tab_stops.iter_mut().for_each(|s| *s = false);
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
        DirtyFrame { cursor: self.cursor, rows, links: self.links.clone() }
    }

    /// Move the viewport up into history by up to `n` lines, clamped to the
    /// available scrollback. No-op on the alternate screen (no history there).
    /// Returns `true` if the view actually moved.
    pub fn scroll_view_up(&mut self, n: usize) -> bool {
        if self.primary.is_some() {
            return false;
        }
        let target = (self.view_offset + n).min(self.scrollback.len());
        self.set_view_offset(target)
    }

    /// Move the viewport back down toward the live bottom by up to `n` lines.
    /// Returns `true` if the view actually moved.
    pub fn scroll_view_down(&mut self, n: usize) -> bool {
        let target = self.view_offset.saturating_sub(n);
        self.set_view_offset(target)
    }

    /// Snap the viewport back to the live bottom. Returns `true` if it moved.
    pub fn reset_view(&mut self) -> bool {
        self.set_view_offset(0)
    }

    /// Set the scrollback view offset, marking every row dirty so the renderer
    /// repaints the whole viewport. Returns `true` if the offset changed.
    fn set_view_offset(&mut self, offset: usize) -> bool {
        if offset == self.view_offset {
            return false;
        }
        self.view_offset = offset;
        self.dirty.iter_mut().for_each(|d| *d = true);
        true
    }

    /// Snapshot the entire visible viewport, compositing scrollback history
    /// above the live grid according to [`Grid::view_offset`]. Every row is
    /// included. History lines are padded/truncated to the current width.
    /// Used by the renderer whenever the view is scrolled up.
    pub fn snapshot_viewport(&self) -> DirtyFrame {
        let history = self.scrollback.len();
        let off = self.view_offset.min(history);
        let mut rows = Vec::with_capacity(self.rows);
        for y in 0..self.rows {
            let cells = if y < off {
                // Top `off` viewport rows show the tail of history.
                let line = &self.scrollback[history - off + y];
                let mut row = vec![Cell::blank(); self.cols];
                let n = line.len().min(self.cols);
                row[..n].copy_from_slice(&line[..n]);
                row
            } else {
                // The rest shows the live grid, shifted down by `off`.
                let gy = y - off;
                let start = gy * self.cols;
                self.cells[start..start + self.cols].to_vec()
            };
            rows.push((y, cells));
        }
        DirtyFrame { cursor: self.cursor, rows, links: self.links.clone() }
    }

    /// Drain bytes queued for the host terminal (forwarded OSC 52 clipboard
    /// requests). Empty when there's nothing to send.
    pub fn take_host_out(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.host_out)
    }

    /// Set the active hyperlink (OSC 8). `None` (or an empty URI) closes the
    /// link; a URI is interned and its id stamped onto subsequently written
    /// cells.
    pub(crate) fn set_link(&mut self, uri: Option<&str>) {
        self.current_link = match uri {
            None | Some("") => 0,
            Some(u) => self.intern_link(u),
        };
    }

    /// Return the id for `uri`, interning it on first use. Ids are `index + 1`
    /// so `0` can mean "no link". Returns `0` once the table is full.
    fn intern_link(&mut self, uri: &str) -> u16 {
        if let Some(i) = self.links.iter().position(|l| l == uri) {
            return (i + 1) as u16;
        }
        if self.links.len() >= LINK_MAX {
            return 0;
        }
        self.links.push(uri.to_string());
        self.links.len() as u16
    }
}

/// A snapshot of the dirty rows of a [`Grid`], plus the cursor position so the
/// renderer can place the hardware cursor.
pub struct DirtyFrame {
    /// Cursor position `(col, row)` at snapshot time.
    pub cursor: (usize, usize),
    /// Dirty rows as `(row_index, cells)` pairs.
    pub rows: Vec<(usize, Vec<Cell>)>,
    /// Interned hyperlink URIs (OSC 8); a [`Cell::link`] of `n` indexes
    /// `links[n - 1]`. Cloned so the renderer can resolve links without the lock.
    pub links: Vec<String>,
}
