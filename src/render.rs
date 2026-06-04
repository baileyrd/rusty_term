//! Shared rendering support used by both runtimes: SGR/glyph output to the host
//! terminal, the one-shot `render_once` snapshot-and-paint step, the host
//! raw-mode guard, and the host-input-mode reset performed on exit.
//!
//! Keeping the paint path here means the runtime's wake mechanism (`AsyncFd` on
//! Unix, channel + poll bridge on Windows) never changes *what* gets drawn.

use std::io::Write;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::backend::Backend;
use crate::core::{
    ATTR_BLINK, ATTR_BOLD, ATTR_DIM, ATTR_HIDDEN, ATTR_ITALIC, ATTR_MASK, ATTR_REVERSE,
    ATTR_STRIKE, ATTR_UNDERLINE, DirtyFrame, Grid, LineAttr, WIDE_TRAILER,
};

/// Minimum wall-clock spacing between repaints. Bursts of output coalesce into
/// at most one frame per budget (~60 Hz), so a flood (`cat bigfile`) repaints
/// smoothly instead of once per PTY read.
pub(crate) const FRAME_BUDGET: Duration = Duration::from_millis(16);

/// Restores the host terminal out of raw mode when dropped, so an early return
/// or a panic can never leave the user's shell with echo/line-editing disabled.
pub(crate) struct RawModeGuard<'a> {
    backend: &'a dyn Backend,
}

impl<'a> RawModeGuard<'a> {
    pub(crate) fn enable(backend: &'a dyn Backend) -> Result<Self, std::io::Error> {
        backend.set_raw_mode(true)?;
        Ok(Self { backend })
    }
}

impl Drop for RawModeGuard<'_> {
    fn drop(&mut self) {
        let _ = self.backend.set_raw_mode(false);
    }
}

/// Reset any host-terminal input modes a runtime may have relayed on the
/// child's behalf (mouse, focus, bracketed paste) and ensure the cursor is
/// visible, so a child that exited without disabling them can't leave the host
/// stuck emitting mouse escapes on every click. Called once on shutdown.
pub(crate) fn restore_host_modes() {
    let mut out = std::io::stdout();
    let _ = out.write_all(
        b"\x1b[?1l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?1015l\x1b[?1016l\x1b[?2004l\x1b[?25h",
    );
    let _ = out.flush();
}

/// Build the combined SGR introducer for a foreground/background/attribute
/// triple. Starts with a reset (`0`) so attributes left active by the previous
/// run are cleared, then re-states the active attributes and truecolor pair.
fn sgr_for(fg: u32, bg: u32, attrs: u16) -> String {
    let mut s = String::from("\x1b[0");
    if attrs & ATTR_BOLD != 0 {
        s.push_str(";1");
    }
    if attrs & ATTR_DIM != 0 {
        s.push_str(";2");
    }
    if attrs & ATTR_ITALIC != 0 {
        s.push_str(";3");
    }
    if attrs & ATTR_UNDERLINE != 0 {
        s.push_str(";4");
    }
    if attrs & ATTR_BLINK != 0 {
        s.push_str(";5");
    }
    if attrs & ATTR_REVERSE != 0 {
        s.push_str(";7");
    }
    if attrs & ATTR_HIDDEN != 0 {
        s.push_str(";8");
    }
    if attrs & ATTR_STRIKE != 0 {
        s.push_str(";9");
    }
    let (fr, fg_, fb) = ((fg >> 16) & 0xFF, (fg >> 8) & 0xFF, fg & 0xFF);
    let (br, bg_, bb) = ((bg >> 16) & 0xFF, (bg >> 8) & 0xFF, bg & 0xFF);
    use std::fmt::Write as _;
    let _ = write!(s, ";38;2;{};{};{};48;2;{};{};{}m", fr, fg_, fb, br, bg_, bb);
    s
}

/// Paint the dirty rows of `frame` to stdout, then position the hardware cursor
/// where the shell's cursor is. SGR sequences are emitted only when the color
/// changes within a row, so a run of same-colored cells costs one introducer
/// instead of one per cell.
pub(crate) fn draw(frame: &DirtyFrame, position_cursor: bool) {
    let out = std::io::stdout();
    let mut out = out.lock();

    for (y, cells) in &frame.rows {
        // Move to column 1 (1-indexed) of this row.
        let _ = write!(out, "\x1b[{};1H", y + 1);

        // Relay this row's line size (DECDWL/DECDHL) to the host, which scales the
        // glyphs. A double-width/height row displays only the left half of the
        // columns, so cap emission to avoid spilling into the next line.
        let (dec_seq, max_cells) = match frame
            .line_attrs
            .get(*y)
            .copied()
            .unwrap_or(LineAttr::Single)
        {
            LineAttr::Single => ("\x1b#5", cells.len()),
            LineAttr::DoubleWidth => ("\x1b#6", cells.len() / 2),
            LineAttr::DoubleTop => ("\x1b#3", cells.len() / 2),
            LineAttr::DoubleBottom => ("\x1b#4", cells.len() / 2),
        };
        let mut line_buf = String::with_capacity(cells.len() + 32);
        line_buf.push_str(dec_seq);
        let mut last: Option<(u32, u32, u16)> = None;
        // Active hyperlink id while painting this row; reset per row so a link
        // is reopened at the start of each line it covers and closed at row end.
        let mut cur_link: u16 = 0;
        for cell in cells.iter().take(max_cells) {
            // The trailing half of a wide glyph is not emitted; the glyph
            // itself already advances the host cursor by two columns.
            if cell.flags & WIDE_TRAILER != 0 {
                continue;
            }
            // Open/close an OSC 8 hyperlink when the cell's link changes.
            if cell.link != cur_link {
                match frame.links.get(cell.link.wrapping_sub(1) as usize) {
                    Some(uri) if cell.link != 0 => {
                        line_buf.push_str("\x1b]8;;");
                        line_buf.push_str(uri);
                        line_buf.push_str("\x1b\\");
                    }
                    // link == 0, or an unknown id: close any open link.
                    _ => line_buf.push_str("\x1b]8;;\x1b\\"),
                }
                cur_link = cell.link;
            }
            // Style key excludes the WIDE_TRAILER layout bit (trailers are
            // skipped above, so only rendition attributes reach here).
            let attrs = cell.flags & ATTR_MASK;
            if last != Some((cell.fg, cell.bg, attrs)) {
                line_buf.push_str(&sgr_for(cell.fg, cell.bg, attrs));
                last = Some((cell.fg, cell.bg, attrs));
            }
            line_buf.push(cell.ch);
            // Emit the grapheme continuation (combining marks, ZWJ joins, …) so
            // the full cluster renders as one glyph.
            if cell.cluster != 0
                && let Some(suffix) = frame.clusters.get((cell.cluster - 1) as usize)
            {
                line_buf.push_str(suffix);
            }
        }
        // Close a still-open hyperlink before ending the row.
        if cur_link != 0 {
            line_buf.push_str("\x1b]8;;\x1b\\");
        }
        line_buf.push_str("\x1b[0m");
        let _ = write!(out, "{}", line_buf);
    }

    // Place the visible cursor where the shell expects it (1-indexed). Skipped
    // while browsing scrollback, where the live cursor position is meaningless.
    if position_cursor {
        let (cx, cy) = frame.cursor;
        let _ = write!(out, "\x1b[{};{}H", cy + 1, cx + 1);
    }
    let _ = out.flush();
}

/// Mutable per-frame renderer bookkeeping carried across repaints, so a paint
/// happens only when something actually changed (cells, cursor, title, or
/// cursor visibility).
pub(crate) struct RenderState {
    /// Last cursor position the host cursor was placed at; lets a cursor-only
    /// move (arrows, Ctrl-A/E, backspace) repaint even when no row is dirty.
    pub(crate) last_cursor: Option<(usize, usize)>,
    /// Last window title forwarded to the host, so OSC 0/2 updates pass through
    /// only when they actually change.
    pub(crate) last_title: Option<String>,
    /// Current visibility of the host cursor (starts shown).
    pub(crate) cursor_shown: bool,
    /// Timestamp of the last actual paint, so the frame budget spaces real
    /// repaints rather than no-op wakes.
    pub(crate) last_frame: Instant,
}

impl RenderState {
    pub(crate) fn new() -> Self {
        Self {
            last_cursor: None,
            last_title: None,
            cursor_shown: true,
            last_frame: Instant::now(),
        }
    }
}

/// Snapshot the grid and paint one frame if warranted, forwarding any clipboard
/// (OSC 52) bytes, title (OSC 0/2) changes, and cursor-visibility (DECTCEM)
/// changes to the host terminal. Shared verbatim by both runtimes.
pub(crate) fn render_once(grid: &Mutex<Grid>, st: &mut RenderState) {
    let (frame, title, viewing, dirty_any, host_out, app_cursor_visible) = {
        let mut g = grid.lock();
        let viewing = g.view_offset > 0;
        let dirty_any = g.dirty.iter().any(|&d| d);
        // While scrolled back, composite history over the live grid; that
        // snapshot covers every row, so it's only worth painting on a change.
        let frame = if viewing {
            g.snapshot_viewport()
        } else {
            g.snapshot_dirty()
        };
        g.clear_dirty();
        (
            frame,
            g.title.clone(),
            viewing,
            dirty_any,
            g.take_host_out(),
            g.cursor_visible,
        )
    };

    // Forward any clipboard (OSC 52) bytes to the host terminal verbatim.
    if !host_out.is_empty() {
        let mut out = std::io::stdout();
        let _ = out.write_all(&host_out);
        let _ = out.flush();
    }

    // Forward a changed, non-empty window title to the host so its title bar
    // tracks what the child set via OSC 0/2.
    if !title.is_empty() && st.last_title.as_deref() != Some(title.as_str()) {
        let mut out = std::io::stdout();
        let _ = write!(out, "\x1b]0;{}\x07", title);
        let _ = out.flush();
        st.last_title = Some(title);
    }

    // The host cursor is shown only in the live view and only when the child
    // wants it visible. Sync the host's state on any change.
    let want_cursor = !viewing && app_cursor_visible;
    if want_cursor != st.cursor_shown {
        let mut out = std::io::stdout();
        let _ = out.write_all(if want_cursor {
            b"\x1b[?25h"
        } else {
            b"\x1b[?25l"
        });
        let _ = out.flush();
        st.cursor_shown = want_cursor;
    }

    if viewing {
        // Repaint the whole viewport only when something changed (a scroll, or
        // new output arriving underneath).
        if dirty_any {
            draw(&frame, false);
            st.last_frame = Instant::now();
        }
        // Force a cursor reposition on the first live frame after we return.
        st.last_cursor = None;
    } else if !frame.rows.is_empty() || st.last_cursor != Some(frame.cursor) {
        // Draw when cells changed, or when only the cursor moved — `draw` emits
        // the final cursor-positioning escape, so a pure motion still needs it.
        st.last_cursor = Some(frame.cursor);
        draw(&frame, true);
        st.last_frame = Instant::now();
    }
}
