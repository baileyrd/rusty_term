mod backend;
mod core;

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::backend::{Backend, UnixBackend, WindowsBackend};
use crate::core::{AnsiParser, DirtyFrame, Grid, WIDE_TRAILER};

/// Set by the `SIGWINCH` handler; the render loop drains it to resize the grid
/// and the PTY in step with the host terminal. A plain atomic store is the only
/// async-signal-safe work the handler does.
static RESIZE_PENDING: AtomicBool = AtomicBool::new(false);

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

/// Build the combined truecolor SGR introducer for a foreground/background pair.
fn sgr_for(fg: u32, bg: u32) -> String {
    let (fr, fg_, fb) = ((fg >> 16) & 0xFF, (fg >> 8) & 0xFF, fg & 0xFF);
    let (br, bg_, bb) = ((bg >> 16) & 0xFF, (bg >> 8) & 0xFF, bg & 0xFF);
    format!("\x1b[38;2;{};{};{};48;2;{};{};{}m", fr, fg_, fb, br, bg_, bb)
}

/// Paint the dirty rows of `frame` to stdout, then position the hardware cursor
/// where the shell's cursor is. SGR sequences are emitted only when the color
/// changes within a row, so a run of same-colored cells costs one introducer
/// instead of one per cell.
fn draw(frame: &DirtyFrame) {
    let out = std::io::stdout();
    let mut out = out.lock();

    for (y, cells) in &frame.rows {
        // Move to column 1 (1-indexed) of this row.
        let _ = write!(out, "\x1b[{};1H", y + 1);

        let mut line_buf = String::with_capacity(cells.len() + 32);
        let mut last: Option<(u32, u32)> = None;
        for cell in cells {
            // The trailing half of a wide glyph is not emitted; the glyph
            // itself already advances the host cursor by two columns.
            if cell.flags & WIDE_TRAILER != 0 {
                continue;
            }
            if last != Some((cell.fg, cell.bg)) {
                line_buf.push_str(&sgr_for(cell.fg, cell.bg));
                last = Some((cell.fg, cell.bg));
            }
            line_buf.push(cell.ch);
            // Emit any combining marks so they render over the base glyph.
            for &m in &cell.combining {
                if m != '\0' {
                    line_buf.push(m);
                }
            }
        }
        line_buf.push_str("\x1b[0m");
        let _ = write!(out, "{}", line_buf);
    }

    // Place the visible cursor where the shell expects it (1-indexed).
    let (cx, cy) = frame.cursor;
    let _ = write!(out, "\x1b[{};{}H", cy + 1, cx + 1);
    let _ = out.flush();
}

fn main() -> Result<(), std::io::Error> {
    let backend: Box<dyn Backend> = if cfg!(target_os = "windows") {
        Box::new(WindowsBackend)
    } else {
        Box::new(UnixBackend)
    };

    // Start at the host terminal's actual size, falling back to 80x24.
    let (init_cols, init_rows) = backend.terminal_size().unwrap_or((80, 24));
    let grid = Arc::new(Mutex::new(Grid::new(init_cols as usize, init_rows as usize)));
    let running = Arc::new(AtomicBool::new(true));

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

    // The reader owns the child (and reaps it on drop); the writer and resizer
    // are independent handles to the same PTY, so read, write, and winsize
    // updates never contend on a shared lock.
    let mut reader = backend.spawn_shell(init_cols, init_rows)?;
    let mut writer = reader.try_clone()?;
    let mut resizer = reader.try_clone()?;

    // --- Thread 1: Parser (PTY -> Grid) ---
    let grid_parser = Arc::clone(&grid);
    let running_parser = Arc::clone(&running);

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
        // On this thread's own exit path (shell EOF/error) `reader` drops here
        // and reaps the child. On the stdin-EOF path the process exits while
        // this thread is blocked in read(), so Drop may not run — but the child
        // now has a controlling tty, so the kernel sends it SIGHUP when the
        // master closes, and init reaps the orphan.
    });

    // --- Thread 2: Input pump (stdin -> PTY) ---
    let running_input = Arc::clone(&running);

    thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut stdin_handle = stdin.lock();
        let mut input_buf = [0u8; 1024];

        loop {
            if !running_input.load(Ordering::Relaxed) {
                break;
            }
            match stdin_handle.read(&mut input_buf) {
                Ok(0) => break,
                Ok(n) => {
                    if writer.write(&input_buf[..n]).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        running_input.store(false, Ordering::Relaxed);
    });

    // --- Thread 3: Renderer (main loop) ---
    print!("\x1b[2J");

    // Last cursor position we positioned the host cursor at; used to repaint on
    // cursor-only motion (arrows, Ctrl-A/E, backspace) that dirties no rows.
    let mut last_cursor: Option<(usize, usize)> = None;

    while running.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(16));

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

        let frame = {
            let mut g = grid.lock().unwrap();
            let frame = g.snapshot_dirty();
            g.clear_dirty();
            frame
        };

        // Draw when cells changed, or when only the cursor moved — `draw` emits
        // the final cursor-positioning escape, so a pure motion still needs it.
        if !frame.rows.is_empty() || last_cursor != Some(frame.cursor) {
            last_cursor = Some(frame.cursor);
            draw(&frame);
        }
    }

    // `_raw_guard` drops here, restoring cooked mode.
    Ok(())
}
