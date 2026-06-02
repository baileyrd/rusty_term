mod backend;
mod core;

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::backend::{Backend, UnixBackend, WindowsBackend};
use crate::core::{AnsiParser, DirtyFrame, Grid};

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
            if last != Some((cell.fg, cell.bg)) {
                line_buf.push_str(&sgr_for(cell.fg, cell.bg));
                last = Some((cell.fg, cell.bg));
            }
            line_buf.push(cell.ch);
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
    const COLS: usize = 80;
    const ROWS: usize = 24;

    let grid = Arc::new(Mutex::new(Grid::new(COLS, ROWS)));
    let running = Arc::new(AtomicBool::new(true));

    let backend: Box<dyn Backend> = if cfg!(target_os = "windows") {
        Box::new(WindowsBackend)
    } else {
        Box::new(UnixBackend)
    };

    // Raw mode stays enabled for the lifetime of this guard; it is restored on
    // any exit path, including a panic.
    let _raw_guard = RawModeGuard::enable(backend.as_ref())?;

    // The reader owns the child (and reaps it on drop); the writer is an
    // independent handle to the same PTY, so read and write never contend on a
    // shared lock.
    let mut reader = backend.spawn_shell()?;
    let mut writer = reader.try_clone()?;

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
                // Non-empty data: fold it into the grid.
                Ok(data) if !data.is_empty() => {
                    let mut g = grid_parser.lock().unwrap();
                    parser.advance(&mut g, &data);
                    g.epoch += 1;
                }
                // Empty read == EOF: the shell exited.
                Ok(_) => break,
                Err(_) => break,
            }
        }
        running_parser.store(false, Ordering::Relaxed);
        // `reader` drops here, reaping the child shell.
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

    while running.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(16));

        let frame = {
            let mut g = grid.lock().unwrap();
            let frame = g.snapshot_dirty();
            g.clear_dirty();
            frame
        };

        if !frame.rows.is_empty() {
            draw(&frame);
        }
    }

    // `_raw_guard` drops here, restoring cooked mode.
    Ok(())
}
