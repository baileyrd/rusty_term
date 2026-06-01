
mod backend;
mod core;

use backend::{Backend, UnixBackend, WindowsBackend, BackendHandle};
use core::{AnsiParser, TerminalBuffer};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

fn main() -> Result<(), std::io::Error> {
    let backend: Box<dyn Backend> = if cfg!(target_os = "windows") {
        Box::new(WindowsBackend)
    } else {
        Box::new(UnixBackend)
    };

    backend.set_raw_mode(true)?;

    let handle = backend.spawn_shell()?;
    // We wrap the handle in Arc<Mutex<>> so it can be shared between threads
    let shared_handle = Arc::new(Mutex::new(handle));
    let buffer = Arc::new(Mutex::new(TerminalBuffer::new(80, 24)));
    
    let reader_handle = Arc::clone(&shared_handle);
    let reader_buffer = Arc::clone(&buffer);
    
    // Thread 1: Read from Shell -> Update Buffer
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
    // Thread 2: Read from User -> Write to Shell
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

    // Main Thread: Render Loop
    loop {
        {
            let b = buffer.lock().unwrap();
            // Clear screen and reset cursor using ANSI codes
            print!("\x1b[2J\x1b[H");
            for y in 0..b.height {
                for x in 0..b.width {
                    print!("{}", b.cells[y * b.width + x].character);
                }
                print!("\r\n");
            }
            // Draw the cursor position for a little flair
            print!("\x1b[{};{}H", b.cursor_y + 1, b.cursor_x + 1);
            std::io::stdout().flush().unwrap();
        }
        thread::sleep(Duration::from_millis(33)); // ~30 FPS
    }
}
