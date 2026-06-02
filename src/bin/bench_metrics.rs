//! Performance benchmark for rusty_term Grid handoff strategies.
//! Compares "Full Grid Copy" vs "Dirty Row Snapshot".

use std::time::{Instant, Duration};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Default)]
struct Cell {
    ch: char,
    fg: u32,
    bg: u32,
}

struct Grid {
    cols: usize,
    rows: usize,
    cells: Vec<Cell>,
    dirty: Vec<bool>,
}

impl Grid {
    fn new(cols: usize, rows: usize) -> Self {
        Self {
            cols,
            rows,
            cells: vec![Cell::default(); cols * rows],
            dirty: vec![false; rows],
        }
    }

    fn mark_dirty(&mut self, row: usize) {
        self.dirty[row] = true;
    }

    // Strategy 1: Full Copy (The baseline we replaced)
    fn snapshot_full(&self) -> Vec<Cell> {
        self.cells.clone()
    }

    // Strategy 2: Dirty Snapshot (The current implementation)
    fn snapshot_dirty(&self) -> Vec<(usize, Vec<Cell>)> {
        self.dirty.iter().enumerate()
            .filter(|&(_, d)| *d)
            .map(|(y, _)| {
                let start = y * self.cols;
                (y, self.cells[start..start + self.cols].to_vec())
            })
            .collect()
    }

    fn clear_dirty(&mut self) {
        self.dirty.fill(false);
    }
}

fn main() {
    const COLS: usize = 120;
    const ROWS: usize = 48;
    const ITERATIONS: u32 = 10_000;

    let grid = Grid::new(COLS, ROWS);
    
    println!("--- Performance Metrics: Grid Handoff ---");
    println!("Config: {}x{} Grid | {} Iterations", COLS, ROWS, ITERATIONS);

    // Scenario A: High Locality (Only 2 rows changing)
    // This is the common case (cursor moving on one line, or a few lines of output)
    {
        let mut g = Grid::new(COLS, ROWS);
        g.mark_dirty(5);
        g.mark_dirty(6);

        let start_full = Instant::now();
        for _ in 0..ITERATIONS {
            let _ = g.snapshot_full();
        }
        let duration_full = start_full.elapsed();

        let start_dirty = Instant::now();
        for _ in 0..ITERATIONS {
            let _ = g.snapshot_dirty();
        }
        let duration_dirty = start_dirty.elapsed();

        println!("\n[Scenario: High Locality (2/{} rows dirty)]", ROWS);
        println!("Full Copy:    {:?} (avg {:?} per frame)", duration_full, duration_full / ITERATIONS);
        println!("Dirty Snap:   {:?} (avg {:?} per frame)", duration_dirty, duration_dirty / ITERATIONS);
        println!("Gain:         {:.2}x faster", duration_full.as_nanos() as f64 / duration_dirty.as_nanos() as f64);
    }

    // Scenario B: Full Screen Flush (All rows changing)
    // This happens during a clear-screen or a massive `cat` of a file.
    {
        let mut g = Grid::new(COLS, ROWS);
        for r in 0..ROWS { g.mark_dirty(r); }

        let start_full = Instant::now();
        for _ in 0..ITERATIONS {
            let _ = g.snapshot_full();
        }
        let duration_full = start_full.elapsed();

        let start_dirty = Instant::now();
        for _ in 0..ITERATIONS {
            let _ = g.snapshot_dirty();
        }
        let duration_dirty = start_dirty.elapsed();

        println!("\n[Scenario: Full Flush ({}/{} rows dirty)]", ROWS, ROWS);
        println!("Full Copy:    {:?} (avg {:?} per frame)", duration_full, duration_full / ITERATIONS);
        println!("Dirty Snap:   {:?} (avg {:?} per frame)", duration_dirty, duration_dirty / ITERATIONS);
        println!("Gain:         {:.2}x", duration_full.as_nanos() as f64 / duration_dirty.as_nanos() as f64);
    }
}
