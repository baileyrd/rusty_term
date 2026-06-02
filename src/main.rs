mod backend;
mod core;

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::backend::Backend;
#[cfg(unix)]
use crate::backend::UnixBackend;
#[cfg(windows)]
use crate::backend::WindowsBackend;
use crate::core::{
    ATTR_BLINK, ATTR_BOLD, ATTR_DIM, ATTR_HIDDEN, ATTR_ITALIC, ATTR_MASK, ATTR_REVERSE,
    ATTR_STRIKE, ATTR_UNDERLINE, AnsiParser, DirtyFrame, Grid, WIDE_TRAILER,
};

/// Set by the `SIGWINCH` handler; the render loop drains it to resize the grid
/// and the PTY in step with the host terminal. A plain atomic store is the only
/// async-signal-safe work the handler does.
static RESIZE_PENDING: AtomicBool = AtomicBool::new(false);

/// Wakes the render thread when there is something new to paint, so it can park
/// on a condvar instead of spinning on a fixed timer. The boolean is the
/// "frame pending" predicate: producers (the parser on new output, the input
/// pump on a scrollback move) set it and signal; the renderer clears it on each
/// wake. Many producer events between two repaints collapse into one frame via
/// the renderer's frame budget, so `notify` is deliberately cheap and idempotent.
struct FrameSignal {
    pending: Mutex<bool>,
    cv: Condvar,
}

impl FrameSignal {
    fn new() -> Self {
        Self { pending: Mutex::new(false), cv: Condvar::new() }
    }

    /// Mark a frame pending and wake the renderer if it is parked.
    fn notify(&self) {
        *self.pending.lock().unwrap() = true;
        self.cv.notify_one();
    }

    /// Park until a frame is pending or `timeout` elapses, then clear the
    /// predicate. Returns `true` when woken by a `notify` (real work), `false`
    /// on a bare timeout — the periodic tick that lets a `SIGWINCH`-driven
    /// resize be noticed even when no output is flowing. The timeout is the only
    /// reason the renderer wakes while idle.
    fn wait(&self, timeout: Duration) -> bool {
        let mut pending = self.pending.lock().unwrap();
        if !*pending {
            let (guard, res) = self.cv.wait_timeout(pending, timeout).unwrap();
            pending = guard;
            // A notify landing right at the timeout boundary still counts as work.
            if res.timed_out() && !*pending {
                return false;
            }
        }
        *pending = false;
        true
    }
}

/// Minimum wall-clock spacing between repaints. Bursts of output coalesce into
/// at most one frame per budget (~60 Hz), so a flood (`cat bigfile`) repaints
/// smoothly instead of once per PTY read.
const FRAME_BUDGET: Duration = Duration::from_millis(16);

/// How long the renderer parks when idle before waking to re-check for a
/// pending resize. Output and scrollback moves wake it immediately; this bare
/// tick exists only because the async-signal-safe `SIGWINCH` handler can set a
/// flag but cannot touch the condvar, so it bounds resize latency while quiet.
const IDLE_TICK: Duration = Duration::from_millis(100);

#[cfg(unix)]
extern "C" fn handle_sigwinch(_: libc::c_int) {
    RESIZE_PENDING.store(true, Ordering::Relaxed);
}

/// Restores the host terminal out of raw mode when dropped, so an early return
/// or a panic can never leave the user's shell with echo/line-editing disabled.
struct RawModeGuard<'a> {
    backend: &'a dyn Backend,
}

impl<'a> RawModeGuard<'a> {
    fn enable(backend: &'a dyn Backend) -> Result<Self, std::io::Error> {
        backend.set_raw_mode(true)?;
        Ok(Self { backend })
    }
}

impl Drop for RawModeGuard<'_> {
    fn drop(&mut self) {
        let _ = self.backend.set_raw_mode(false);
    }
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
fn draw(frame: &DirtyFrame, position_cursor: bool) {
    let out = std::io::stdout();
    let mut out = out.lock();

    for (y, cells) in &frame.rows {
        // Move to column 1 (1-indexed) of this row.
        let _ = write!(out, "\x1b[{};1H", y + 1);

        let mut line_buf = String::with_capacity(cells.len() + 32);
        let mut last: Option<(u32, u32, u16)> = None;
        // Active hyperlink id while painting this row; reset per row so a link
        // is reopened at the start of each line it covers and closed at row end.
        let mut cur_link: u16 = 0;
        for cell in cells {
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
            // Emit any combining marks so they render over the base glyph.
            for &m in &cell.combining {
                if m != '\0' {
                    line_buf.push(m);
                }
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

fn main() -> Result<(), std::io::Error> {
    #[cfg(unix)]
    let backend: Box<dyn Backend> = Box::new(UnixBackend);
    #[cfg(windows)]
    let backend: Box<dyn Backend> = Box::new(WindowsBackend);

    // Start at the host terminal's actual size, falling back to 80x24.
    let (init_cols, init_rows) = backend.terminal_size().unwrap_or((80, 24));
    let grid = Arc::new(Mutex::new(Grid::new(init_cols as usize, init_rows as usize)));
    let running = Arc::new(AtomicBool::new(true));
    let signal = Arc::new(FrameSignal::new());

    // Raw mode stays enabled for the lifetime of this guard; it is restored on
    // any exit path, including a panic.
    let _raw_guard = RawModeGuard::enable(backend.as_ref())?;

    // Install the SIGWINCH handler before spawning, so a resize during startup
    // isn't lost to the default (ignore) disposition. sigaction with SA_RESTART
    // keeps the parser's blocking read from being interrupted.
    #[cfg(unix)]
    unsafe {
        let handler: extern "C" fn(libc::c_int) = handle_sigwinch;
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGWINCH, &sa, std::ptr::null_mut());
    }

    // The child renders through rusty_term, not the host terminal, so it should
    // see *our* identity and capabilities: a widely-compatible 256-color xterm
    // plus truecolor (the renderer always emits 24-bit SGR). Set before spawning
    // so the child inherits it. We're still single-threaded here, so this
    // process-wide env mutation races with nothing.
    unsafe {
        std::env::set_var("TERM", "xterm-256color");
        std::env::set_var("COLORTERM", "truecolor");
    }

    // The reader owns the child (and reaps it on drop); the writer and resizer
    // are independent handles to the same PTY, so read, write, and winsize
    // updates never contend on a shared lock.
    let mut reader = backend.spawn_shell(init_cols, init_rows)?;
    let mut writer = reader.try_clone()?;
    let mut resizer = reader.try_clone()?;

    // --- Thread 1: Parser (PTY -> Grid) ---
    let grid_parser = Arc::clone(&grid);
    let running_parser = Arc::clone(&running);
    let signal_parser = Arc::clone(&signal);

    thread::spawn(move || {
        let mut parser = AnsiParser::new();
        loop {
            if !running_parser.load(Ordering::Relaxed) {
                break;
            }
            match reader.read() {
                // Non-empty data: fold it into the grid, then send any query
                // replies (DA/DSR) back to the child via the master fd.
                Ok(data) if !data.is_empty() => {
                    let responses = {
                        let mut g = grid_parser.lock().unwrap();
                        parser.advance(&mut g, &data);
                        g.epoch += 1;
                        parser.take_responses()
                    };
                    // New output landed in the grid — ask for a repaint.
                    signal_parser.notify();
                    if !responses.is_empty() && reader.write(&responses).is_err() {
                        break;
                    }
                }
                // Empty read == EOF: the shell exited.
                Ok(_) => break,
                Err(_) => break,
            }
        }
        running_parser.store(false, Ordering::Relaxed);
        // Wake the renderer so it observes `running == false` and exits its park.
        signal_parser.notify();
        // On this thread's own exit path (shell EOF/error) `reader` drops here
        // and reaps the child. On the stdin-EOF path the process exits while
        // this thread is blocked in read(), so Drop may not run — but the child
        // now has a controlling tty, so the kernel sends it SIGHUP when the
        // master closes, and init reaps the orphan.
    });

    // --- Thread 2: Input pump (stdin -> PTY) ---
    let running_input = Arc::clone(&running);
    let grid_input = Arc::clone(&grid);
    let signal_input = Arc::clone(&signal);

    thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut stdin_handle = stdin.lock();
        let mut input_buf = [0u8; 1024];

        // Shift+PageUp / Shift+PageDown browse scrollback instead of reaching
        // the child; everything else is forwarded and snaps the view to the
        // live bottom. (A scroll key split across reads is forwarded verbatim —
        // harmless, just won't scroll; keypresses arrive atomically in raw mode.)
        const SCROLL_UP_KEY: &[u8] = b"\x1b[5;2~";
        const SCROLL_DN_KEY: &[u8] = b"\x1b[6;2~";

        loop {
            if !running_input.load(Ordering::Relaxed) {
                break;
            }
            match stdin_handle.read(&mut input_buf) {
                Ok(0) => break,
                Ok(n) => {
                    let buf = &input_buf[..n];
                    let mut forward: Vec<u8> = Vec::with_capacity(n);
                    let mut i = 0;
                    while i < buf.len() {
                        if buf[i..].starts_with(SCROLL_UP_KEY) {
                            let moved = {
                                let mut g = grid_input.lock().unwrap();
                                let page = g.rows.saturating_sub(1).max(1);
                                g.scroll_view_up(page)
                            };
                            if moved {
                                signal_input.notify();
                            }
                            i += SCROLL_UP_KEY.len();
                        } else if buf[i..].starts_with(SCROLL_DN_KEY) {
                            let moved = {
                                let mut g = grid_input.lock().unwrap();
                                let page = g.rows.saturating_sub(1).max(1);
                                g.scroll_view_down(page)
                            };
                            if moved {
                                signal_input.notify();
                            }
                            i += SCROLL_DN_KEY.len();
                        } else {
                            forward.push(buf[i]);
                            i += 1;
                        }
                    }
                    if !forward.is_empty() {
                        // Real input snaps the view to the live bottom; only wake
                        // the renderer when that actually moved the viewport.
                        let moved = grid_input.lock().unwrap().reset_view();
                        if moved {
                            signal_input.notify();
                        }
                        if writer.write(&forward).is_err() {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        running_input.store(false, Ordering::Relaxed);
        // Wake the renderer so it observes `running == false` and exits its park.
        signal_input.notify();
    });

    // --- Thread 3: Renderer (main loop) ---
    print!("\x1b[2J");

    // Last cursor position we positioned the host cursor at; used to repaint on
    // cursor-only motion (arrows, Ctrl-A/E, backspace) that dirties no rows.
    let mut last_cursor: Option<(usize, usize)> = None;

    // Last window title forwarded to the host, so OSC 0/2 updates are passed
    // through to the host terminal's title bar only when they actually change.
    let mut last_title: Option<String> = None;

    // Current visibility of the host cursor (starts shown). The cursor is shown
    // only in the live view and only when the child wants it (DECTCEM); it is
    // hidden while browsing scrollback or when the child issued `?25l`.
    let mut cursor_shown = true;
    // Timestamp of the last actual paint, so the frame budget spaces real
    // repaints rather than no-op wakes.
    let mut last_frame = Instant::now();

    loop {
        // Park until a producer flags a pending frame, or the idle tick fires so
        // a SIGWINCH-driven resize is still picked up while output is quiet.
        let woke_for_work = signal.wait(IDLE_TICK);
        if !running.load(Ordering::Relaxed) {
            break;
        }
        // Coalesce bursts: when woken by output, hold off until the frame budget
        // since the last paint has elapsed, so a flood repaints at ~60 Hz rather
        // than once per read. Idle ticks are already older than the budget.
        if woke_for_work {
            let since = last_frame.elapsed();
            if since < FRAME_BUDGET {
                thread::sleep(FRAME_BUDGET - since);
            }
        }

        // Apply a pending resize before snapshotting: reflow the grid, tell the
        // child its new size, and clear the screen so the full repaint is clean.
        // Only act when the size actually changed — a spurious SIGWINCH must not
        // clear the screen (the clear would flush ahead of a dirty-rows-only
        // repaint and blank everything else).
        if RESIZE_PENDING.swap(false, Ordering::Relaxed)
            && let Some((cols, rows)) = backend.terminal_size()
        {
            let changed = {
                let mut g = grid.lock().unwrap();
                let changed = g.cols != cols as usize || g.rows != rows as usize;
                if changed {
                    g.resize(cols as usize, rows as usize);
                }
                changed
            };
            if changed {
                let _ = resizer.set_winsize(cols, rows);
                let mut out = std::io::stdout();
                let _ = out.write_all(b"\x1b[2J");
                let _ = out.flush();
            }
        }

        let (frame, title, viewing, dirty_any, host_out, app_cursor_visible) = {
            let mut g = grid.lock().unwrap();
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
            (frame, g.title.clone(), viewing, dirty_any, g.take_host_out(), g.cursor_visible)
        };

        // Forward any clipboard (OSC 52) bytes to the host terminal verbatim.
        if !host_out.is_empty() {
            let mut out = std::io::stdout();
            let _ = out.write_all(&host_out);
            let _ = out.flush();
        }

        // Forward a changed, non-empty window title to the host terminal so its
        // title bar tracks what the child set via OSC 0/2.
        if !title.is_empty() && last_title.as_deref() != Some(title.as_str()) {
            let mut out = std::io::stdout();
            let _ = write!(out, "\x1b]0;{}\x07", title);
            let _ = out.flush();
            last_title = Some(title);
        }

        // The host cursor is shown only in the live view and only when the child
        // wants it visible. Sync the host's state on any change.
        let want_cursor = !viewing && app_cursor_visible;
        if want_cursor != cursor_shown {
            let mut out = std::io::stdout();
            let _ = out.write_all(if want_cursor { b"\x1b[?25h" } else { b"\x1b[?25l" });
            let _ = out.flush();
            cursor_shown = want_cursor;
        }

        if viewing {
            // Repaint the whole viewport only when something changed (a scroll,
            // or new output arriving underneath).
            if dirty_any {
                draw(&frame, false);
                last_frame = Instant::now();
            }
            // Force a cursor reposition on the first live frame after we return.
            last_cursor = None;
        } else {
            // Draw when cells changed, or when only the cursor moved — `draw`
            // emits the final cursor-positioning escape, so a pure motion still
            // needs it.
            if !frame.rows.is_empty() || last_cursor != Some(frame.cursor) {
                last_cursor = Some(frame.cursor);
                draw(&frame, true);
                last_frame = Instant::now();
            }
        }
    }

    // Reset any host-terminal input modes we may have relayed on the child's
    // behalf (mouse, focus, bracketed paste) and ensure the cursor is visible,
    // so a child that exited without disabling them can't leave the host stuck
    // emitting mouse escapes on every click.
    {
        let mut out = std::io::stdout();
        let _ = out.write_all(
            b"\x1b[?1l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?1015l\x1b[?1016l\x1b[?2004l\x1b[?25h",
        );
        let _ = out.flush();
    }

    // `_raw_guard` drops here, restoring cooked mode.
    Ok(())
}

#[cfg(test)]
mod frame_signal_tests {
    use super::FrameSignal;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    // A notify issued before the wait must not be lost: the wait observes the
    // pending predicate and returns `true` without parking for the timeout.
    #[test]
    fn notify_before_wait_returns_immediately() {
        let signal = FrameSignal::new();
        signal.notify();
        let start = Instant::now();
        let woke = signal.wait(Duration::from_secs(5));
        assert!(woke, "a pre-set notify must report work");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "wait blocked despite a pending frame",
        );
    }

    // A notify from another thread must wake a parked wait and report work, well
    // before the (generous) timeout would fire.
    #[test]
    fn parked_wait_is_woken_by_cross_thread_notify() {
        let signal = Arc::new(FrameSignal::new());
        let notifier = Arc::clone(&signal);
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            notifier.notify();
        });
        let start = Instant::now();
        let woke = signal.wait(Duration::from_secs(5));
        let elapsed = start.elapsed();
        handle.join().unwrap();
        assert!(woke, "cross-thread notify must report work");
        assert!(
            elapsed < Duration::from_secs(2),
            "wait was not woken by the notify",
        );
    }

    // With no notify, wait must eventually report a bare timeout (`false`) — the
    // periodic tick the renderer relies on to poll for a pending resize. Retried
    // to absorb the rare spurious condvar wakeup, which the contract reports as
    // `true`.
    #[test]
    fn wait_reports_bare_timeout_without_notify() {
        let signal = FrameSignal::new();
        let mut timed_out = false;
        for _ in 0..40 {
            if !signal.wait(Duration::from_millis(25)) {
                timed_out = true;
                break;
            }
        }
        assert!(timed_out, "wait never reported a bare timeout");
    }
}
