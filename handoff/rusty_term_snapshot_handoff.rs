//! rusty_term — snapshot handoff sketch (std-only, NO external crates)
//!
//! Two variants of the parser <-> renderer handoff that keep the lock OFF the
//! draw path. Both use only `std::sync`. Contrast notes vs. the event-pipeline
//! proposal are at the bottom.

#![allow(dead_code)]

use std::io::Read;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Core grid types — the single source of truth.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
pub struct Cell {
    pub ch: char,
    pub fg: u32,
    pub bg: u32,
    pub flags: u16, // bold / italic / underline ... bitset
}

pub struct Grid {
    cols: usize,
    rows: usize,
    cells: Vec<Cell>, // row-major, len == rows * cols
    dirty: Vec<bool>, // per-row damage flag
    cursor: (usize, usize),
    epoch: u64, // bumped once per applied read() batch
}

impl Grid {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            cols,
            rows,
            cells: vec![Cell::default(); cols * rows],
            dirty: vec![false; rows],
            cursor: (0, 0),
            epoch: 0,
        }
    }

    #[inline]
    pub fn set_cell(&mut self, x: usize, y: usize, cell: Cell) {
        self.cells[y * self.cols + x] = cell;
        self.dirty[y] = true; // damage tracking: only this row needs redraw
    }

    fn clear_dirty(&mut self) {
        self.dirty.iter_mut().for_each(|d| *d = false);
    }
}

// ---------------------------------------------------------------------------
// VARIANT A — minimal diff from your current Arc<Mutex<TerminalBuffer>>.
//   Keep the mutex. The only change: the renderer copies what it needs under
//   a SHORT lock, releases, THEN draws. The lock is never held across the GPU.
// ---------------------------------------------------------------------------

pub type SharedGrid = Arc<Mutex<Grid>>;

/// What the renderer carries out of the critical section: only the dirty rows.
pub struct DirtyFrame {
    pub cols: usize,
    pub epoch: u64,
    pub rows: Vec<(usize, Vec<Cell>)>, // (row_index, row_cells)
    pub cursor: (usize, usize),
}

impl Grid {
    /// Clone out only damaged rows. Cost is O(dirty cells), not O(grid).
    pub fn snapshot_dirty(&self) -> DirtyFrame {
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
        DirtyFrame { cols: self.cols, epoch: self.epoch, rows, cursor: self.cursor }
    }
}

pub fn parser_loop_a(grid: SharedGrid, mut pty: impl Read) {
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = match pty.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        // Lock held ONLY for the apply — microseconds, not a frame.
        let mut g = grid.lock().unwrap();
        parse_into(&mut g, &buf[..n]); // mutates cells + marks dirty rows
        g.epoch += 1;
    } // unlock
}

pub fn render_loop_a(grid: SharedGrid) {
    loop {
        wait_for_vsync();
        // --- short critical section: copy + clear, then release ---
        let frame = {
            let mut g = grid.lock().unwrap();
            let frame = g.snapshot_dirty();
            g.clear_dirty();
            frame
        }; // <-- lock released HERE, before any drawing
        gpu_draw_dirty(&frame); // no lock held during GPU work
    }
}

// ---------------------------------------------------------------------------
// VARIANT B — published immutable snapshot (std-only, no arc-swap).
//   Parser owns the live grid. It publishes an immutable Arc<GridSnapshot>.
//   The renderer's "lock" is a single Arc clone (a refcount bump) — the
//   critical section is a pointer op, so contention is effectively nil.
//   Coalescing is FREE: the renderer always loads the newest Arc; snapshots
//   the renderer never read are simply dropped when their refcount hits zero.
// ---------------------------------------------------------------------------

pub struct GridSnapshot {
    pub cols: usize,
    pub rows: usize,
    pub cells: Box<[Cell]>,
    pub cursor: (usize, usize),
    pub epoch: u64,
}

pub struct Published {
    live: Mutex<Grid>,                // parser-owned authoritative state
    latest: Mutex<Arc<GridSnapshot>>, // renderer reads this; clone-and-go
}

impl Published {
    pub fn new(cols: usize, rows: usize) -> Arc<Self> {
        let snap = Arc::new(GridSnapshot {
            cols,
            rows,
            cells: vec![Cell::default(); cols * rows].into_boxed_slice(),
            cursor: (0, 0),
            epoch: 0,
        });
        Arc::new(Self {
            live: Mutex::new(Grid::new(cols, rows)),
            latest: Mutex::new(snap),
        })
    }

    /// Parser side: build a fresh immutable snapshot and publish it.
    /// Snapshot cost is bounded by GRID size (viewport), independent of how
    /// many bytes the shell just dumped.
    fn publish(&self, g: &Grid) {
        let snap = Arc::new(GridSnapshot {
            cols: g.cols,
            rows: g.rows,
            cells: g.cells.clone().into_boxed_slice(),
            cursor: g.cursor,
            epoch: g.epoch,
        });
        *self.latest.lock().unwrap() = snap; // critical section = one pointer store
    }

    /// Renderer side: grab the newest snapshot. Critical section = one Arc clone.
    fn load(&self) -> Arc<GridSnapshot> {
        self.latest.lock().unwrap().clone()
    }
}

pub fn parser_loop_b(state: Arc<Published>, mut pty: impl Read) {
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = match pty.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let mut g = state.live.lock().unwrap();
        parse_into(&mut g, &buf[..n]);
        g.epoch += 1;
        // Publish at most one snapshot per batch. If the renderer is slow it
        // simply misses intermediate epochs — that IS the coalescing.
        // (Optional: throttle publish() to frame cadence to skip clones the
        //  renderer would never read.)
        state.publish(&g);
    }
}

pub fn render_loop_b(state: Arc<Published>) {
    let mut last_drawn = u64::MAX;
    loop {
        wait_for_vsync();
        let snap = state.load(); // ~wait-free: just clones an Arc
        if snap.epoch == last_drawn {
            continue; // nothing new since last frame
        }
        last_drawn = snap.epoch;
        gpu_draw_full(&snap); // draw from the immutable snapshot, no lock held
    }
}

// ---------------------------------------------------------------------------
// Stubs — your real impls live elsewhere.
// ---------------------------------------------------------------------------

fn parse_into(_g: &mut Grid, _bytes: &[u8]) { /* vte state machine -> set_cell */ }
fn gpu_draw_dirty(_frame: &DirtyFrame) { /* upload only damaged rows */ }
fn gpu_draw_full(_snap: &GridSnapshot) { /* upload instance buffer */ }
fn wait_for_vsync() { /* block until next frame */ }

// ---------------------------------------------------------------------------
// DIFF vs. the event-pipeline proposal
// ---------------------------------------------------------------------------
// Event pipeline:        handoff = O(mutations). A 10 MB dump = millions of
//                        SetChar events through the SPSC queue; the renderer
//                        must REPLAY every one. State duplicated on both sides.
//                        Bounded queue -> backpressure stalls the parser;
//                        unbounded -> memory blows up during the flood.
//                        No coalescing unless you rebuild a grid by hand.
//
// Variant A (this file): handoff = O(dirty rows) clone, once per frame.
//                        Single source of truth. Smallest change from today.
//
// Variant B (this file): reads = O(1) Arc clone; publish = O(grid) snapshot on
//                        the PARSER side, bounded by viewport not input size.
//                        Coalescing is automatic — renderer reads newest only.
//
// Both keep the lock OFF the draw path, which was the actual "toll booth."
// Truly wait-free reads (seqlock / left-right buffering) are doable std-only
// but need `unsafe`; not worth it until a flamegraph says the Arc clone is hot.
