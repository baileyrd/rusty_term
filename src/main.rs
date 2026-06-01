
mod backend;
mod core;

use backend::{Backend, UnixBackend, WindowsBackend, BackendHandle};
use core::{AnsiParser, TerminalBuffer};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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

fn main() -> Result<(), std::io::Error> {
    let backend: Box<dyn Backend> = if cfg!(target_os = "windows") {
        Box::new(WindowsBackend)
    } else {
        Box::new(UnixBackend)
    };

    backend.set_raw_mode(true)?;

    let handle = backend.spawn_shell()?;
    let shared_handle = Arc::new(Mutex::new(handle));
    let buffer = Arc::new(Mutex::new(TerminalBuffer::new(80, 24)));
    
    let reader_handle = Arc::clone(&shared_handle);
    let reader_buffer = Arc::clone(&buffer);
    
    thread::spawn(move || {
        let mut parser = AnsiParser::new();
        loop {
            let mut h = reader_handle.lock().unwrap();
            if let Ok(data) = h.read() {
                if data.is_empty() { break; }
                let mut b = reader_buffer.lock().unwrap();
                parser.parse(&data, &mut b);
            }
            drop(h);
            thread::sleep(Duration::from_millis(10));
        }
    });

    let writer_handle = Arc::clone(&shared_handle);
    thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 1];
        loop {
            if stdin.read(&mut buf).is_ok() {
                let mut h = writer_handle.lock().unwrap();
                if h.write(&buf).is_err() { break; }
            }
        }
    });

    loop {
        {
            let b = buffer.lock().unwrap();
            // Use a string to buffer the frame to reduce flicker
            let mut frame = String::with_capacity(80 * 24 * 20);
            frame.push_str("\x1b[H"); // Cursor home
            
            for y in 0..b.height {
                for x in 0..b.width {
                    let cell = b.cells[y * b.width + x];
                    frame.push_str(&hex_to_ansi_color(cell.fg_color, false));
                    frame.push_str(&hex_to_ansi_color(cell.bg_color, true));
                    frame.push(cell.character);
                }
                frame.push_str("\x1b[0m\r\n");
            }
            
            print!("{}", frame);
            // Restore cursor a bit more cleanly
            print!("\x1b[{} ; {} H", b.cursor_y + 1, b.cursor_x + 1);
            std::io::stdout().flush().unwrap();
        }
        thread::sleep(Duration::from_millis(33));
    }
}
