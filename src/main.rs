mod backend;
mod core;

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::backend::{Backend, UnixBackend, WindowsBackend, BackendHandle};
use crate::core::{AnsiParser, Grid, DirtyFrame};

fn hex_to_ansi_color(hex: u32, is_bg: bool) -> String {
    let r = (hex >> 16) & 0xFF;
    let g = (hex >> 8) & 0xFF;
    let b = hex & 0xFF;
    if is_bg {
        format!("\x1b[48;2;{};{};{}m", r, g, b)
    } else {
        format!("\x1b[38;2;{};{};{}m", r, g, b)
    }
}

fn draw(frame: &DirtyFrame) {
    if frame.rows.is_empty() {
        return;
    }
    let out = std::io::stdout();
    let mut out = out.lock();
    
    for (y, cells) in &frame.rows {
        // Move cursor to start of line (1-indexed)
        let _ = write!(out, "\x1b[{};0H", y + 1);
        
        let mut line_buf = String::with_capacity(cells.len() * 10);
        for cell in cells {
            line_buf.push_str(&hex_to_ansi_color(cell.fg, false));
            line_buf.push_str(&hex_to_ansi_color(cell.bg, true));
            line_buf.push(cell.ch);
            line_buf.push_str("\x1b[0m"); 
        }
        let _ = write!(out, "{}", line_buf);
    }
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

    backend.set_raw_mode(true)?;

    let handle = backend.spawn_shell()?;
    let shared_handle = Arc::new(Mutex::new(handle));
    
    // --- Thread 1: The Parser (PTY -> Grid) ---
    let grid_parser = Arc::clone(&grid);
    let running_parser = Arc::clone(&running);
    let handle_parser = Arc::clone(&shared_handle);
    
    thread::spawn(move || {
        let mut parser = AnsiParser::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            if !running_parser.load(Ordering::Relaxed) { break; }
            
            let n = {
                let mut h = handle_parser.lock().unwrap();
                match h.read() {
                    Ok(data) if !data.is_empty() => {
                        let len = data.len().min(buf.len());
                        buf[..len].copy_from_slice(&data[..len]);
                        len
                    },
                    _ => 0,
                }
            };

            if n > 0 {
                let mut g = grid_parser.lock().unwrap();
                parser.advance(&mut g, &buf[..n]);
                g.epoch += 1;
            } else {
                thread::sleep(Duration::from_millis(1));
            }
        }
        running_parser.store(false, Ordering::Relaxed);
    });

    // --- Thread 2: The Input Pump (Stdin -> PTY) ---
    let running_input = Arc::clone(&running);
    let handle_input = Arc::clone(&shared_handle);
    
    thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut stdin_handle = stdin.lock();
        let mut input_buf = [0u8; 1024];
        
        loop {
            if !running_input.load(Ordering::Relaxed) { break; }
            
            match stdin_handle.read(&mut input_buf) {
                Ok(0) => break,
                Ok(n) => {
                    let mut h = handle_input.lock().unwrap();
                    if h.write(&input_buf[..n]).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        running_input.store(false, Ordering::Relaxed);
    });

    // --- Thread 3: The Renderer (Main Loop) ---
    print!("\x1b[2J"); 
    
    while running.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(16));
        
        let frame = {
            let mut g = grid.lock().unwrap();
            let frame = g.snapshot_dirty();
            g.clear_dirty();
            frame
        };
        
        draw(&frame);
    }

    backend.set_raw_mode(false)?;
    Ok(())
}
