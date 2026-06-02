//! Core terminal logic for rusty_term.
//! Implements the Grid (source of truth) and the AnsiParser.

#[derive(Clone, Copy, Default, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: u32,
    pub bg: u32,
    pub flags: u16, // bits for bold, italic, etc.
}

pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<Cell>,       // row-major, len == cols * rows
    pub dirty: Vec<bool>,       // per-row damage flag
    pub cursor: (usize, usize), // (col, row)
    pub epoch: u64,
}

impl Grid {
    pub fn new(cols: usize, rows: usize) -> Self {
        let blank = Cell { ch: ' ', fg: 0xFFFFFF, bg: 0x000000, ..Default::default() };
        Self {
            cols,
            rows,
            cells: vec![blank; cols * rows],
            dirty: vec![false; rows],
            cursor: (0, 0),
            epoch: 0,
        }
    }

    pub fn set_cell(&mut self, x: usize, y: usize, cell: Cell) {
        if x < self.cols && y < self.rows {
            self.cells[y * self.cols + x] = cell;
            self.dirty[y] = true;
        }
    }

    pub fn carriage_return(&mut self) {
        self.cursor.0 = 0;
    }

    pub fn newline(&mut self) {
        self.cursor.1 += 1;
        if self.cursor.1 >= self.rows {
            self.scroll_up();
            self.cursor.1 = self.rows - 1;
        }
    }

    pub fn scroll_up(&mut self) {
        // Shift every row up by one
        self.cells.copy_within(self.cols.., 0);
        let last_row_start = (self.rows - 1) * self.cols;
        let blank = Cell { ch: ' ', fg: 0xFFFFFF, bg: 0x000000, ..Default::default() };
        for c in &mut self.cells[last_row_start..] {
            *c = blank;
        }
        // Everything is dirty now
        self.dirty.iter_mut().for_each(|d| *d = true);
    }

    pub fn put_char(&mut self, ch: char, fg: u32, bg: u32) {
        if self.cursor.0 >= self.cols {
            self.carriage_return();
            self.newline();
        }
        let (x, y) = self.cursor;
        self.set_cell(x, y, Cell { ch, fg, bg, ..Default::default() });
        self.cursor.0 += 1;
    }

    pub fn clear_dirty(&mut self) {
        self.dirty.iter_mut().for_each(|d| *d = false);
    }

    pub fn snapshot_dirty(&self) -> DirtyFrame {
        let rows = self.dirty.iter().enumerate()
            .filter(|&(_, d)| *d)
            .map(|(y, _)| {
                let start = y * self.cols;
                (y, self.cells[start..start + self.cols].to_vec())
            })
            .collect();
        DirtyFrame { epoch: self.epoch, rows }
    }
}

pub struct DirtyFrame {
    pub epoch: u64,
    pub rows: Vec<(usize, Vec<Cell>)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ParserState {
    Ground,
    Esc,
    Csi,
}

pub struct AnsiParser {
    state: ParserState,
    current_fg: u32,
    current_bg: u32,
    param_buffer: String,
}

impl AnsiParser {
    pub fn new() -> Self {
        Self {
            state: ParserState::Ground,
            current_fg: 0xFFFFFF,
            current_bg: 0x000000,
            param_buffer: String::new(),
        }
    }

    pub fn advance(&mut self, g: &mut Grid, bytes: &[u8]) {
        for &b in bytes {
            match self.state {
                ParserState::Ground => match b {
                    0x1b => self.state = ParserState::Esc,
                    b'\n' => {
                        g.carriage_return();
                        g.newline();
                    }
                    b'\r' => g.carriage_return(),
                    b'\t' => {
                        let pad = 8 - (g.cursor.0 % 8);
                        for _ in 0..pad {
                            g.put_char(' ', self.current_fg, self.current_bg);
                        }
                    }
                    0x20..=0x7e => g.put_char(b as char, self.current_fg, self.current_bg),
                    _ => {}
                },
                ParserState::Esc => {
                    self.state = if b == b'[' { ParserState::Csi } else { ParserState::Ground };
                    if self.state == ParserState::Csi {
                        self.param_buffer.clear();
                    }
                }
                ParserState::Csi => {
                    if b.is_ascii_digit() || b == b';' {
                        self.param_buffer.push(b as char);
                    } else {
                        self.handle_csi(b, g);
                        self.state = ParserState::Ground;
                        self.param_buffer.clear();
                    }
                }
            }
        }
    }

    fn handle_csi(&mut self, cmd: u8, g: &mut Grid) {
        let params: Vec<usize> = self.param_buffer
            .split(';')
            .filter_map(|s| s.parse().ok())
            .collect();

        match cmd {
            b'm' => {
                for &p in &params {
                    match p {
                        30 => self.current_fg = 0x000000,
                        31 => self.current_fg = 0xFF0000,
                        32 => self.current_fg = 0x00FF00,
                        33 => self.current_fg = 0xFFFF00,
                        34 => self.current_fg = 0x0000FF,
                        35 => self.current_fg = 0xFF00FF,
                        36 => self.current_fg = 0x00FFFF,
                        37 => self.current_fg = 0xCCCCCC,
                        39 => self.current_fg = 0xFFFFFF,
                        40 => self.current_bg = 0x000000,
                        41 => self.current_bg = 0xFF0000,
                        44 => self.current_bg = 0x0000FF,
                        49 => self.current_bg = 0x000000,
                        _ => {}
                    }
                }
            }
            b'H' | b'f' => {
                let y = params.get(0).cloned().unwrap_or(1);
                let x = params.get(1).cloned().unwrap_or(1);
                g.cursor = (x.saturating_sub(1), y.saturating_sub(1));
            }
            b'J' => {
                if params.get(0).cloned().unwrap_or(0) == 2 {
                    // Clear from cursor to end of screen
                    // Simplified: we'll just clear the whole grid for this prototype
                    let blank = Cell { ch: ' ', fg: 0xFFFFFF, bg: 0x000000, ..Default::default() };
                    g.cells.fill(blank);
                    g.dirty.iter_mut().for_each(|d| *d = true);
                    g.cursor = (0, 0);
                }
            }
            _ => {}
        }
    }
}
