
pub struct TerminalBuffer {
    pub width: usize,
    pub height: usize,
    pub cells: Vec<Cell>,
    pub cursor_x: usize,
    pub cursor_y: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct Cell {
    pub character: char,
    pub fg_color: u32,
    pub bg_color: u32,
}

impl TerminalBuffer {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cells: vec![Cell { character: ' ', fg_color: 0xFFFFFF, bg_color: 0x000000 }; width * height],
            cursor_x: 0,
            cursor_y: 0,
        }
    }

    pub fn set_cell(&mut self, x: usize, y: usize, cell: Cell) {
        if x < self.width && y < self.height {
            self.cells[y * self.width + x] = cell;
        }
    }

    pub fn move_cursor(&mut self, x: usize, y: usize) {
        self.cursor_x = x.min(self.width - 1);
        self.cursor_y = y.min(self.height - 1);
    }
}

pub struct AnsiParser {
    is_escaping: bool,
}

impl AnsiParser {
    pub fn new() -> Self {
        Self { is_escaping: false }
    }

    pub fn parse(&mut self, input: &[u8], buffer: &mut TerminalBuffer) {
        for &byte in input {
            if byte == 0x1B { // ESC
                self.is_escaping = true;
                continue;
            }
            
            if self.is_escaping {
                // Very basic ANSI support: just handle \n and \r for now, 
                // resetting escape state on non-escape bytes.
                // A real parser would implement a full state machine.
                self.is_escaping = false;
                continue;
            }

            if byte == b'\n' {
                buffer.cursor_y += 1;
                buffer.cursor_x = 0;
            } else if byte == b'\r' {
                buffer.cursor_x = 0;
            } else {
                buffer.set_cell(buffer.cursor_x, buffer.cursor_y, Cell {
                    character: byte as char,
                    fg_color: 0xFFFFFF,
                    bg_color: 0x000000,
                });
                buffer.cursor_x += 1;
            }

            // Word wrap
            if buffer.cursor_x >= buffer.width {
                buffer.cursor_x = 0;
                buffer.cursor_y += 1;
            }
            // Scrolling (naive: reset to top)
            if buffer.cursor_y >= buffer.height {
                buffer.cursor_y = 0;
                // In a real terminal, we'd shift the whole buffer up.
            }
        }
    }
}
