//! rusty_term — runnable 3-thread skeleton around Variant A (std-only).
//!
//! Topology (matches a real terminal):
//!   input thread   : reads OUR stdin  -> writes child's stdin   (keystrokes)
//!   parser thread  : reads child stdout -> mutates Arc<Mutex<Grid>>
//!   render thread  : main loop; short lock -> snapshot dirty rows -> "draw"
//!
//! std gives no PTY, so we spawn `sh` and read its piped stdout. In the real
//! build you replace that ONE handle with a PTY master fd; nothing else moves.
//!
//! Try it:
//!   rustc --edition 2021 rusty_term_skeleton.rs -o rusty_term && \
//!     printf 'ls\nuname -a\nexit\n' | ./rusty_term

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Grid — the single source of truth (Variant A).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct Cell {
    ch: char,
    fg: u32,
    bg: u32,
    flags: u16,
}

struct Grid {
    cols: usize,
    rows: usize,
    cells: Vec<Cell>,       // row-major, len == cols * rows
    dirty: Vec<bool>,       // per-row damage flag
    cursor: (usize, usize), // (col, row)
    epoch: u64,
}

impl Grid {
    fn new(cols: usize, rows: usize) -> Self {
        let blank = Cell { ch: ' ', ..Default::default() };
        Self {
            cols,
            rows,
            cells: vec![blank; cols * rows],
            dirty: vec![false; rows],
            cursor: (0, 0),
            epoch: 0,
        }
    }

    fn set_cell(&mut self, x: usize, y: usize, cell: Cell) {
        self.cells[y * self.cols + x] = cell;
        self.dirty[y] = true;
    }

    fn carriage_return(&mut self) {
        self.cursor.0 = 0;
    }

    fn newline(&mut self) {
        self.cursor.1 += 1;
        if self.cursor.1 >= self.rows {
            self.scroll_up();
            self.cursor.1 = self.rows - 1;
        }
    }

    fn scroll_up(&mut self) {
        // Shift every row up by one; clear the last row.
        self.cells.copy_within(self.cols.., 0);
        let last = (self.rows - 1) * self.cols;
        for c in &mut self.cells[last..] {
            *c = Cell { ch: ' ', ..Default::default() };
        }
        self.dirty.iter_mut().for_each(|d| *d = true);
    }

    fn put_char(&mut self, ch: char) {
        if self.cursor.0 >= self.cols {
            self.carriage_return();
            self.newline();
        }
        let (x, y) = self.cursor;
        self.set_cell(x, y, Cell { ch, ..Default::default() });
        self.cursor.0 += 1;
    }

    fn clear_dirty(&mut self) {
        self.dirty.iter_mut().for_each(|d| *d = false);
    }

    /// Clone out only damaged rows. Cost is O(dirty cells), not O(grid).
    fn snapshot_dirty(&self) -> DirtyFrame {
        let rows = self
            .dirty
            .iter()
            .enumerate()
            .filter(|(_, &d)| d)
            .map(|(y, _)| {
                let start = y * self.cols;
                (y, self.cells[start..start + self.cols].to_vec())
            })
            .collect();
        DirtyFrame { epoch: self.epoch, rows }
    }
}

struct DirtyFrame {
    epoch: u64,
    rows: Vec<(usize, Vec<Cell>)>,
}

// ---------------------------------------------------------------------------
// Toy stateful parser — REAL enough to demo, sequential by design.
// State persists across read() batches (escapes straddle chunk boundaries).
// Swap this whole struct for the `vte` state machine later.
// ---------------------------------------------------------------------------

enum PState {
    Ground,
    Esc,
    Csi,
}

struct Parser {
    state: PState,
}

impl Parser {
    fn new() -> Self {
        Self { state: PState::Ground }
    }

    fn advance(&mut self, g: &mut Grid, bytes: &[u8]) {
        for &b in bytes {
            match self.state {
                PState::Ground => match b {
                    0x1b => self.state = PState::Esc,
                    // Bare pipe gives no ONLCR; a real PTY translates \n -> \r\n.
                    // Emulate that here so the demo returns to col 0 on newline.
                    b'\n' => {
                        g.carriage_return();
                        g.newline();
                    }
                    b'\r' => g.carriage_return(),
                    b'\t' => {
                        let pad = 8 - (g.cursor.0 % 8);
                        for _ in 0..pad {
                            g.put_char(' ');
                        }
                    }
                    0x20..=0x7e => g.put_char(b as char),
                    _ => {} // ignore other controls in the toy parser
                },
                PState::Esc => {
                    self.state = if b == b'[' { PState::Csi } else { PState::Ground };
                }
                PState::Csi => {
                    // consume params/intermediates until a final byte (0x40..=0x7e)
                    if (0x40..=0x7e).contains(&b) {
                        self.state = PState::Ground; // effect ignored (toy)
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// main — three threads around the shared grid.
// ---------------------------------------------------------------------------

fn main() {
    const COLS: usize = 80;
    const ROWS: usize = 24;

    let grid = Arc::new(Mutex::new(Grid::new(COLS, ROWS)));
    let running = Arc::new(AtomicBool::new(true));

    let mut child = Command::new("sh")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn shell");

    let child_stdout = child.stdout.take().expect("no child stdout");
    let child_stdin = child.stdin.take().expect("no child stdin");

    // --- parser thread: child stdout -> grid ---
    let parser_handle = {
        let grid = Arc::clone(&grid);
        let running = Arc::clone(&running);
        thread::spawn(move || {
            let mut parser = Parser::new();
            let mut reader = child_stdout;
            let mut buf = [0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break, // EOF: shell closed
                    Ok(n) => {
                        // Lock held ONLY for the apply — never across a draw.
                        let mut g = grid.lock().unwrap();
                        parser.advance(&mut g, &buf[..n]);
                        g.epoch += 1;
                    }
                }
            }
            running.store(false, Ordering::Relaxed);
        })
    };

    // --- input thread: our stdin -> child stdin (keystrokes) ---
    {
        let running = Arc::clone(&running);
        thread::spawn(move || {
            let mut child_stdin = child_stdin;
            let stdin = std::io::stdin();
            for line in BufReader::new(stdin).lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                if line.trim() == "quit" {
                    break;
                }
                if writeln!(child_stdin, "{}", line).is_err() {
                    break;
                }
            }
            running.store(false, Ordering::Relaxed);
            // child_stdin dropped here -> shell sees EOF -> exits
        });
    }

    // --- render loop: main thread (Variant A draw path) ---
    while running.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(16)); // ~60 fps vsync stand-in

        // SHORT critical section: copy + clear, then release before drawing.
        let frame = {
            let mut g = grid.lock().unwrap();
            let frame = g.snapshot_dirty();
            g.clear_dirty();
            frame
        }; // <-- lock released HERE

        draw(&frame); // no lock held during "GPU" work
    }

    // one final flush of whatever landed in the last partial frame
    {
        let mut g = grid.lock().unwrap();
        let frame = g.snapshot_dirty();
        g.clear_dirty();
        drop(g);
        draw(&frame);
    }

    let _ = parser_handle.join();
    let _ = child.wait();
    // input thread may be parked on stdin; process exit reaps it.
}

/// Stand-in for the renderer: prints only the dirty rows.
/// Real impl uploads an instance buffer to the GPU instead.
fn draw(frame: &DirtyFrame) {
    if frame.rows.is_empty() {
        return;
    }
    let out = std::io::stdout();
    let mut out = out.lock();
    for (y, cells) in &frame.rows {
        let text: String = cells
            .iter()
            .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
            .collect();
        let _ = writeln!(out, "[e{:>4} r{:>2}] {}", frame.epoch, y, text.trim_end());
    }
}
