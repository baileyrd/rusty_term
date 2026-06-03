//! The screen buffer (L06 state): the authoritative grid of cells, the cursor,
//! scrolling region, alternate screen, and scrollback history.
//!
//! The [`Grid`] exposes a semantic API (`put_char`, `set_cursor`, `scroll_up`,
//! …) that the [`AnsiParser`](super::parser::AnsiParser) drives, and produces a
//! [`DirtyFrame`] snapshot for the renderer.

use std::collections::VecDeque;

use super::cell::{Cell, DEFAULT_BG, DEFAULT_FG, Pen, WIDE_TRAILER, char_width};
use unicode_segmentation::UnicodeSegmentation;

use super::sixel::SixelImage;

/// Maximum number of lines retained in the scrollback history. Older lines are
/// evicted from the front once this is exceeded.
pub const SCROLLBACK_MAX: usize = 10_000;

/// Maximum number of prompt marks (OSC 133;A) retained for navigation. Older
/// marks are dropped once exceeded — far more than any real session needs.
const PROMPT_MARKS_MAX: usize = 1024;

/// Maximum number of distinct hyperlink URIs interned from OSC 8. Past this,
/// further links are dropped (rendered as plain text) rather than growing the
/// table without bound.
const LINK_MAX: usize = 4096;

/// Maximum number of distinct grapheme-continuation strings interned from
/// multi-scalar glyphs. Past this, further continuations are dropped (the base
/// glyph still renders) rather than growing the table without bound.
const CLUSTER_MAX: usize = 8192;

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
    /// Interned grapheme-continuation strings; a [`Cell::cluster`] of `n` refers
    /// to `clusters[n - 1]` (`0` means a lone `ch`). Append-only and bounded by
    /// [`CLUSTER_MAX`], so ids stay stable for cells held in scrollback.
    pub(crate) clusters: Vec<String>,
    /// The hyperlink id stamped onto cells written while an OSC 8 link is open
    /// (`0` when none). Set by the parser via [`Grid::set_link`].
    current_link: u16,
    /// Columns at which a horizontal tab stops; `len() == cols`. Defaults to
    /// every 8th column and is modified by `HTS` / `TBC`. A resize preserves
    /// stops within the surviving width and defaults the new columns.
    tab_stops: Vec<bool>,
    /// Whether the text cursor is visible (DECTCEM `?25`, default on). The
    /// renderer shows/hides the host cursor accordingly — independent of the
    /// separate hide it applies while browsing scrollback.
    pub cursor_visible: bool,
    /// Whether autowrap (DECAWM `?7`, default on) is enabled. When off, a glyph
    /// printed at the right margin overwrites the last column instead of
    /// wrapping to the next line.
    pub(crate) autowrap: bool,
    /// Whether origin mode (DECOM `?6`, default off) is enabled. When on,
    /// absolute cursor positioning (`CUP`/`HVP`/`VPA`) is relative to the scroll
    /// region top and the cursor is confined to the region.
    pub(crate) origin_mode: bool,
    /// Whether insert mode (IRM, ANSI mode `4`, default off) is enabled. When
    /// on, a printed glyph shifts the rest of the row right instead of
    /// overwriting.
    pub(crate) insert_mode: bool,
    /// Default foreground/background colors (OSC 10/11), mirrored from the
    /// parser's palette. The background doubles as the erase-fill color, so a
    /// theme set via OSC 11 colors cleared regions, not just freshly written text.
    default_fg: u32,
    default_bg: u32,
    /// Logical line indices (counting from the oldest retained scrollback line)
    /// of shell prompt starts reported via OSC 133;A, kept sorted. Powers
    /// prompt-to-prompt scrollback navigation. Bounded by [`PROMPT_MARKS_MAX`].
    prompt_marks: Vec<usize>,
    /// Per-row line size attributes (DECDWL/DECDHL); `len() == rows`. The
    /// renderer relays each to the host so double-width/height lines display
    /// correctly, and they shift with the rows they label as the screen scrolls.
    line_attrs: Vec<LineAttr>,
}

/// Build the default tab-stop table for a `cols`-wide grid: a stop at every
/// 8th column (0, 8, 16, …), matching the classic 8-column default.
fn default_tab_stops(cols: usize) -> Vec<bool> {
    (0..cols).map(|i| i % 8 == 0).collect()
}

/// Per-row size attribute set by `ESC # 3/4/5/6` (DECDHL/DECDWL/DECSWL). A
/// double-width or double-height line renders at twice the cell width, so only
/// the left half of the columns is displayed; the renderer relays the attribute
/// to the host terminal, which does the scaling.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LineAttr {
    /// Normal single-width, single-height (`ESC # 5`, the default).
    #[default]
    Single,
    /// Double-width, single-height (`ESC # 6`).
    DoubleWidth,
    /// Top half of a double-width, double-height line (`ESC # 3`).
    DoubleTop,
    /// Bottom half of a double-width, double-height line (`ESC # 4`).
    DoubleBottom,
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
    /// Line size attributes of the stashed screen, swapped back on exit.
    line_attrs: Vec<LineAttr>,
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
            clusters: Vec::new(),
            current_link: 0,
            tab_stops: default_tab_stops(cols),
            cursor_visible: true,
            autowrap: true,
            origin_mode: false,
            insert_mode: false,
            default_fg: DEFAULT_FG,
            default_bg: DEFAULT_BG,
            line_attrs: vec![LineAttr::Single; rows],
            prompt_marks: Vec::new(),
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

    /// Absolute cursor positioning that honors origin mode (DECOM). `col`/`row`
    /// are 0-based. With origin mode on, `row` is relative to the scroll region
    /// top and the cursor is confined to the region; otherwise it is screen-
    /// absolute. Used by `CUP`/`HVP` and `VPA`.
    pub(crate) fn set_cursor_abs(&mut self, col: usize, row: usize) {
        let y = if self.origin_mode {
            (self.scroll_top + row).min(self.scroll_bottom)
        } else {
            row.min(self.rows.saturating_sub(1))
        };
        self.cursor = (col.min(self.cols.saturating_sub(1)), y);
    }

    /// Move the cursor up `n` rows (`CUU`). The scroll region's top margin is a
    /// floor when the cursor starts at or below it, so cursor-up can't escape
    /// the region; above the region it floors at row 0.
    pub(crate) fn cursor_up(&mut self, n: usize) {
        let floor = if self.cursor.1 >= self.scroll_top {
            self.scroll_top
        } else {
            0
        };
        self.cursor.1 = self.cursor.1.saturating_sub(n).max(floor);
    }

    /// Move the cursor down `n` rows (`CUD`). The scroll region's bottom margin
    /// is a ceiling when the cursor starts at or above it; below the region it
    /// ceilings at the last row.
    pub(crate) fn cursor_down(&mut self, n: usize) {
        let ceil = if self.cursor.1 <= self.scroll_bottom {
            self.scroll_bottom
        } else {
            self.rows.saturating_sub(1)
        };
        self.cursor.1 = self.cursor.1.saturating_add(n).min(ceil);
    }

    /// The cursor home position: the top-left of the screen, or of the scroll
    /// region when origin mode is on.
    fn home_position(&self) -> (usize, usize) {
        (0, if self.origin_mode { self.scroll_top } else { 0 })
    }

    /// Enable or disable origin mode (DECOM), moving the cursor to the (now
    /// possibly origin-relative) home position as the spec requires.
    pub(crate) fn set_origin_mode(&mut self, on: bool) {
        self.origin_mode = on;
        self.cursor = self.home_position();
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

    /// Render an RGB(A) image at the cursor as truecolor half-block glyphs
    /// (`▀`/`▄`) — one cell per pixel column, two pixel rows per cell. `pixels`
    /// is row-major `width × height`, `None` meaning transparent. The image is
    /// shrunk to fit the columns remaining from the cursor (aspect-preserving,
    /// never enlarged), placed top-left at the cursor, and scrolled like printed
    /// lines if it runs past the bottom; the cursor ends at column 0 of the row
    /// below it (xterm "sixel scrolling"). Shared by the Sixel and Kitty paths.
    pub(crate) fn render_image(&mut self, width: usize, height: usize, pixels: &[Option<u32>]) {
        if width == 0 || height == 0 || pixels.len() < width * height {
            return;
        }
        // Nearest-neighbor source sample for target pixel `(tx, ty)`.
        let sample = |tx: usize, ty: usize, tw: usize, th: usize| -> Option<u32> {
            if ty >= th {
                return None;
            }
            pixels[(ty * height / th) * width + (tx * width / tw)]
        };
        // Combine the two pixels of one cell into a half-block glyph; an unset
        // half takes the default background, both unset leaves the cell alone.
        let half_block = |top: Option<u32>, bottom: Option<u32>, def_bg: u32| {
            let mk = |ch, fg, bg| Cell {
                ch,
                cluster: 0,
                fg,
                bg,
                flags: 0,
                link: 0,
            };
            match (top, bottom) {
                (None, None) => None,
                (Some(t), Some(b)) => Some(mk('\u{2580}', t, b)),
                (Some(t), None) => Some(mk('\u{2580}', t, def_bg)),
                (None, Some(b)) => Some(mk('\u{2584}', b, def_bg)),
            }
        };

        let origin = self.cursor.0;
        let avail = self.cols.saturating_sub(origin).max(1);
        // Fit to the available width (shrink only), preserving aspect.
        let tw = width.min(avail);
        let th = (height * tw / width).max(1);
        let cell_rows = th.div_ceil(2);

        for cr in 0..cell_rows {
            let y = self.cursor.1;
            for cc in 0..tw {
                let col = origin + cc;
                if col >= self.cols {
                    break;
                }
                let top = sample(cc, cr * 2, tw, th);
                let bottom = sample(cc, cr * 2 + 1, tw, th);
                if let Some(cell) = half_block(top, bottom, self.default_bg) {
                    self.set_cell(col, y, cell);
                }
            }
            self.newline();
        }
        self.carriage_return();
    }

    /// Render a decoded Sixel image (delegates to [`Grid::render_image`]).
    pub(crate) fn render_sixel(&mut self, img: &SixelImage) {
        self.render_image(img.width, img.height, &img.pixels);
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
                self.evict_prompt_mark();
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
        let blank = self.erase_cell();
        for c in &mut self.cells[last..last + self.cols] {
            *c = blank;
        }
        self.shift_line_attrs(top + 1, top, bottom - top, bottom..bottom + 1);
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
        self.cursor = self.home_position();
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
        // A non-ASCII scalar may continue the grapheme in the cell to the left
        // (combining mark, ZWJ join, skin tone, variation selector). ASCII is
        // always its own grapheme, so the common path skips the check entirely.
        if !ch.is_ascii() {
            if let Some((bx, by)) = self.left_base()
                && self.continues_grapheme(bx, by, ch)
            {
                self.append_to_glyph(bx, by, ch);
                return;
            }
            // Not a continuation: a zero-width scalar has no cell of its own.
            if char_width(ch) == 0 {
                return;
            }
        }
        let w = char_width(ch); // >= 1 here (zero-width non-continuations dropped above)
        if self.cursor.0 + w > self.cols {
            if self.autowrap {
                self.carriage_return();
                self.newline();
            } else {
                // Autowrap off: keep the glyph in the last cell(s) of this row.
                self.cursor.0 = self.cols.saturating_sub(w);
            }
        }
        // Insert mode (IRM): make room by shifting the rest of the row right.
        if self.insert_mode {
            self.insert_chars(w);
        }
        let (x, y) = self.cursor;
        let link = self.current_link;
        self.set_cell(
            x,
            y,
            Cell {
                ch,
                cluster: 0,
                fg: pen.fg,
                bg: pen.bg,
                flags: pen.attrs,
                link,
            },
        );
        if w == 2 && x + 1 < self.cols {
            // Trailing half: a flagged placeholder the renderer skips. It keeps
            // the pen's colors but only the WIDE_TRAILER layout bit.
            self.set_cell(
                x + 1,
                y,
                Cell {
                    ch: ' ',
                    cluster: 0,
                    fg: pen.fg,
                    bg: pen.bg,
                    flags: WIDE_TRAILER,
                    link,
                },
            );
        }
        self.cursor.0 += w;
    }

    /// The base cell of the grapheme immediately left of the cursor, stepping
    /// back over a wide glyph's trailer to its head. `None` at column 0.
    fn left_base(&self) -> Option<(usize, usize)> {
        let (cx, cy) = self.cursor;
        if cy >= self.rows || cx == 0 {
            return None;
        }
        let left = cx - 1;
        if self.cells[cy * self.cols + left].flags & WIDE_TRAILER != 0 && left >= 1 {
            Some((left - 1, cy)) // land on the wide glyph's head, not its trailer
        } else {
            Some((left, cy))
        }
    }

    /// Whether appending `next` to the glyph at `(x, y)` keeps it a single
    /// grapheme cluster (UAX #29) — i.e. `next` continues that glyph rather than
    /// starting a new one.
    fn continues_grapheme(&self, x: usize, y: usize, next: char) -> bool {
        let mut s = self.glyph_text(x, y);
        s.push(next);
        s.graphemes(true).count() == 1
    }

    /// The full glyph text at `(x, y)`: the base scalar plus any interned
    /// grapheme continuation.
    fn glyph_text(&self, x: usize, y: usize) -> String {
        let cell = self.cells[y * self.cols + x];
        let mut s = String::new();
        s.push(cell.ch);
        if cell.cluster != 0
            && let Some(suffix) = self.clusters.get((cell.cluster - 1) as usize)
        {
            s.push_str(suffix);
        }
        s
    }

    /// Append `ch` to the grapheme continuation of the cell at `(x, y)`,
    /// re-interning the grown suffix and marking the row dirty.
    fn append_to_glyph(&mut self, x: usize, y: usize, ch: char) {
        let idx = y * self.cols + x;
        let mut suffix = match self.cells[idx].cluster {
            0 => String::new(),
            id => self
                .clusters
                .get((id - 1) as usize)
                .cloned()
                .unwrap_or_default(),
        };
        suffix.push(ch);
        let id = self.intern_cluster(suffix);
        self.cells[idx].cluster = id;
        self.dirty[y] = true;
    }

    /// Return the id for grapheme continuation `suffix`, interning it on first
    /// use. Ids are `index + 1` so `0` means "no continuation". Returns `0` once
    /// the table is full (the continuation is dropped; the base glyph remains).
    fn intern_cluster(&mut self, suffix: String) -> u16 {
        if let Some(i) = self.clusters.iter().position(|c| *c == suffix) {
            return (i + 1) as u16;
        }
        if self.clusters.len() >= CLUSTER_MAX {
            return 0;
        }
        self.clusters.push(suffix);
        self.clusters.len() as u16
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
        let blank = self.erase_cell();
        for c in &mut self.cells[base + from..base + to] {
            *c = blank;
        }
        self.dirty[y] = true;
    }

    /// Blank every cell and home the cursor (used by `CSI 2 J`).
    pub(crate) fn clear_all(&mut self) {
        let blank = self.erase_cell();
        self.cells.fill(blank);
        self.dirty.iter_mut().for_each(|d| *d = true);
        self.cursor = (0, 0);
    }

    /// A blank cell painted in the current default colors — the fill used by
    /// every erase / scroll-clear path, so a default background set via OSC 11
    /// applies to cleared regions, not only to text.
    fn erase_cell(&self) -> Cell {
        let mut c = Cell::blank();
        c.fg = self.default_fg;
        c.bg = self.default_bg;
        c
    }

    /// Update the default foreground/background colors (OSC 10/11). Mirrors the
    /// parser's palette so subsequent erases fill with the new background.
    pub(crate) fn set_default_colors(&mut self, fg: u32, bg: u32) {
        self.default_fg = fg;
        self.default_bg = bg;
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
            saved.line_attrs = vec![LineAttr::Single; rows];
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
        self.line_attrs = vec![LineAttr::Single; rows];
        self.dirty = vec![true; rows];
        self.cursor = clamp(self.cursor);
        self.saved_cursor = clamp(self.saved_cursor);
        self.prompt_marks.clear();
        self.reset_scroll_region();
    }

    /// Switch to the alternate screen, stashing the primary buffer. Only
    /// `?1049` saves the cursor and homes it; `?47`/`?1047` swap the buffer and
    /// leave the cursor where it is. No-op if already on the alternate screen.
    pub(crate) fn enter_alt_screen(&mut self, mode: AltMode) {
        if self.primary.is_some() {
            return;
        }
        let blank = self.erase_cell();
        self.primary = Some(SavedScreen {
            cells: std::mem::replace(&mut self.cells, vec![blank; self.cols * self.rows]),
            cursor: self.cursor,
            saved_cursor: self.saved_cursor,
            mode,
            line_attrs: std::mem::replace(&mut self.line_attrs, vec![LineAttr::Single; self.rows]),
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
            self.line_attrs = saved.line_attrs;
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
        let blank = self.erase_cell();
        for c in &mut self.cells[row_end - n..row_end] {
            *c = blank;
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
        let blank = self.erase_cell();
        for c in &mut self.cells[from..from + n] {
            *c = blank;
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
        let blank = self.erase_cell();
        for c in &mut self.cells[cy * cols..blank_end] {
            *c = blank;
        }
        self.shift_line_attrs(cy, cy + n, count / cols, cy..cy + n);
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
        let blank = self.erase_cell();
        for c in &mut self.cells[first_blank..region_end] {
            *c = blank;
        }
        self.shift_line_attrs(
            cy + n,
            cy,
            count / cols,
            (self.scroll_bottom + 1 - n)..(self.scroll_bottom + 1),
        );
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
        let blank = self.erase_cell();
        for c in &mut self.cells[top * cols..blank_end] {
            *c = blank;
        }
        self.shift_line_attrs(top, top + n, count / cols, top..top + n);
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

    /// Set the current cursor row's line size attribute (`ESC # 3/4/5/6`).
    pub(crate) fn set_line_attr(&mut self, attr: LineAttr) {
        let y = self.cursor.1;
        if y < self.line_attrs.len() {
            self.line_attrs[y] = attr;
            self.dirty[y] = true;
        }
    }

    /// Mirror a region scroll on the line-attribute table: move `count` rows
    /// from `src_row` to begin at `dst_row`, then reset the rows in `blank` to
    /// single width. Keeps line size glued to its content as the screen scrolls.
    fn shift_line_attrs(
        &mut self,
        src_row: usize,
        dst_row: usize,
        count: usize,
        blank: std::ops::Range<usize>,
    ) {
        if count > 0 {
            self.line_attrs
                .copy_within(src_row..src_row + count, dst_row);
        }
        for a in &mut self.line_attrs[blank] {
            *a = LineAttr::Single;
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

    /// Full reset (`RIS`): return the grid to its power-on state — blank primary
    /// screen, home cursor, full-screen scroll region, default tab stops,
    /// cleared scrollback, cursor visible, autowrap on. The window title and cwd
    /// are intentionally left alone (a hardware reset doesn't relabel the tab).
    /// The parser separately resets its pen.
    pub(crate) fn reset(&mut self) {
        self.primary = None; // leave the alternate screen if active
        self.cells = vec![Cell::blank(); self.cols * self.rows];
        self.dirty = vec![true; self.rows];
        self.line_attrs = vec![LineAttr::Single; self.rows];
        self.cursor = (0, 0);
        self.saved_cursor = (0, 0);
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
        self.scrollback.clear();
        self.prompt_marks.clear();
        self.view_offset = 0;
        self.tab_stops = default_tab_stops(self.cols);
        self.cursor_visible = true;
        self.autowrap = true;
        self.origin_mode = false;
        self.insert_mode = false;
        self.default_fg = DEFAULT_FG;
        self.default_bg = DEFAULT_BG;
        self.current_link = 0;
    }

    /// Soft reset (`DECSTR`): reset terminal modes without clearing the screen
    /// or moving the active cursor — full-screen scroll region, saved cursor to
    /// home, cursor visible, autowrap on. The parser separately resets its pen.
    pub(crate) fn soft_reset(&mut self) {
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
        self.saved_cursor = (0, 0);
        self.cursor_visible = true;
        self.autowrap = true;
        self.origin_mode = false;
        self.insert_mode = false;
        self.default_fg = DEFAULT_FG;
        self.default_bg = DEFAULT_BG;
        self.current_link = 0;
        for a in &mut self.line_attrs {
            *a = LineAttr::Single;
        }
    }

    /// Screen-alignment test (`DECALN`, `ESC # 8`): fill every cell with `E` and
    /// home the cursor. Used to check character positioning.
    pub(crate) fn fill_alignment(&mut self) {
        let mut e = Cell::blank();
        e.ch = 'E';
        self.cells.fill(e);
        for a in &mut self.line_attrs {
            *a = LineAttr::Single;
        }
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
        let rows = self
            .dirty
            .iter()
            .enumerate()
            .filter(|&(_, d)| *d)
            .map(|(y, _)| {
                let start = y * self.cols;
                (y, self.cells[start..start + self.cols].to_vec())
            })
            .collect();
        DirtyFrame {
            cursor: self.cursor,
            rows,
            links: self.links.clone(),
            clusters: self.clusters.clone(),
            line_attrs: self.line_attrs.clone(),
        }
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

    /// Record a shell prompt start (OSC 133;A) at the current cursor row, for
    /// prompt-to-prompt scrollback navigation. No-op on the alternate screen,
    /// which has no history. Marks are logical line indices (0 = oldest retained
    /// scrollback line), kept sorted and deduplicated.
    pub(crate) fn mark_prompt(&mut self) {
        if self.primary.is_some() {
            return;
        }
        let line = self.scrollback.len() + self.cursor.1;
        if let Err(pos) = self.prompt_marks.binary_search(&line) {
            self.prompt_marks.insert(pos, line);
            if self.prompt_marks.len() > PROMPT_MARKS_MAX {
                self.prompt_marks.remove(0);
            }
        }
    }

    /// One scrollback line was evicted from the front: every logical index drops
    /// by one, and a mark on the evicted line (index 0) is discarded.
    fn evict_prompt_mark(&mut self) {
        self.prompt_marks.retain(|&l| l != 0);
        for l in &mut self.prompt_marks {
            *l -= 1;
        }
    }

    /// Scroll the viewport up to the nearest prompt mark above the current top
    /// visible line (OSC 133 navigation). Returns `true` if it moved.
    pub fn scroll_to_prev_prompt(&mut self) -> bool {
        if self.primary.is_some() {
            return false;
        }
        let h = self.scrollback.len();
        let top_visible = h - self.view_offset.min(h);
        match self
            .prompt_marks
            .iter()
            .copied()
            .filter(|&l| l < top_visible)
            .max()
        {
            Some(l) => self.set_view_offset(h - l),
            None => false,
        }
    }

    /// Scroll the viewport down to the nearest prompt mark below the current top
    /// visible line, snapping to the live bottom when the next mark is on the
    /// live screen or there is none. Returns `true` if it moved.
    pub fn scroll_to_next_prompt(&mut self) -> bool {
        if self.primary.is_some() {
            return false;
        }
        let h = self.scrollback.len();
        let top_visible = h - self.view_offset.min(h);
        match self
            .prompt_marks
            .iter()
            .copied()
            .filter(|&l| l > top_visible)
            .min()
        {
            Some(l) if l < h => self.set_view_offset(h - l),
            _ => self.reset_view(),
        }
    }

    /// Snapshot the entire visible viewport, compositing scrollback history
    /// above the live grid according to [`Grid::view_offset`]. Every row is
    /// included. History lines are padded/truncated to the current width.
    /// Used by the renderer whenever the view is scrolled up.
    pub fn snapshot_viewport(&self) -> DirtyFrame {
        let history = self.scrollback.len();
        let off = self.view_offset.min(history);
        let mut rows = Vec::with_capacity(self.rows);
        let mut attrs = Vec::with_capacity(self.rows);
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
            attrs.push(if y < off {
                LineAttr::Single // history lines are always single width
            } else {
                self.line_attrs[y - off]
            });
        }
        DirtyFrame {
            cursor: self.cursor,
            rows,
            links: self.links.clone(),
            clusters: self.clusters.clone(),
            line_attrs: attrs,
        }
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
    /// Interned grapheme continuations (see [`Grid::clusters`]); a
    /// [`Cell::cluster`] of `n` indexes `clusters[n - 1]`. Cloned so the renderer
    /// can resolve glyphs without holding the grid lock.
    pub clusters: Vec<String>,
    /// Per-row line size attributes (DECDWL/DECDHL), indexed by each row's `y`
    /// in [`rows`](Self::rows). Cloned so the renderer can relay them to the host.
    pub line_attrs: Vec<LineAttr>,
}
