
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
    
    pub fn clear_screen(&mut self) {
        self.cells.fill(Cell { character: ' ', fg_color: 0xFFFFFF, bg_color: 0x000000 });
        self.cursor_x = 0;
        self.cursor_y = 0;
    }
}

pub struct AnsiParser {
    state: ParserState,
    current_fg: u32,
    current_bg: u32,
    param_buffer: String,
}

enum ParserState {
    Normal,
    Escape,
    Csi,
}

impl AnsiParser {
    pub fn new() -> Self {
        Self {
            state: ParserState::Normal,
            current_fg: 0xFFFFFF,
            current_bg: 0x000000,
            param_buffer: String::new(),
        }
    }

    pub fn parse(&mut self, input: &[u8], buffer: &mut TerminalBuffer) {
        for &byte in input {
            match self.state {
                ParserState::Normal => {
                    if byte == 0x1B {
                        self.state = ParserState::Escape;
                    } else if byte == b'\n' {
                        buffer.cursor_y += 1;
                        buffer.cursor_x = 0;
                    } else if byte == b'\r' {
                        buffer.cursor_x = 0;
                    } else {
                        buffer.set_cell(buffer.cursor_x, buffer.cursor_y, Cell {
                            character: byte as char,
                            fg_color: self.current_fg,
                            bg_color: self.current_bg,
                        });
                        buffer.cursor_x += 1;
                    }
                }
                ParserState::Escape => {
                    if byte == b'[' {
                        self.state = ParserState::Csi;
                        self.param_buffer.clear();
                    } else {
                        self.state = ParserState::Normal;
                    }
                }
                ParserState::Csi => {
                    if byte.is_ascii_digit() || byte == b';' {
                        self.param_buffer.push(byte as char);
                    } else {
                        self.handle_csi(byte, buffer);
                        self.state = ParserState::Normal;
                        self.param_buffer.clear();
                    }
                }
            }

            // Word wrap and scrolling
            if buffer.cursor_x >= buffer.width {
                buffer.cursor_x = 0;
                buffer.cursor_y += 1;
            }
            if buffer.cursor_y >= buffer.height {
                // Scroll: simple reset for now, though a real terminal shifts rows
                buffer.cursor_y = 0;
            }
        }
    }

    fn handle_csi(&mut self, cmd: u8, buffer: &mut TerminalBuffer) {
        let params: Vec<usize> = self.param_buffer
            .split(';')
            .filter_map(|s| s.parse().ok())
            .collect();

        match cmd {
            b'm' => { // Select Graphic Rendition (Colors)
                for &p in &params {
                    match p {
                        30 => self.current_fg = 0x000000, // Black
                        31 => self.current_fg = 0xFF0000, // Red
                        32 => self.current_fg = 0x00FF00, // Green
                        33 => self.current_fg = 0xFFFF00, // Yellow
                        34 => self.current_fg = 0x0000FF, // Blue
                        35 => self.current_fg = 0xFF00FF, // Magenta
                        36 => self.current_fg = 0x00FFFF, // Cyan
                        37 => self.current_fg = 0xCCCCCC, // White
                        39 => self.current_fg = 0xFFFFFF, // Default
                        40 => self.current_bg = 0x000000,
                        41 => self.current_bg = 0xFF0000,
                        44 => self.current_bg = 0x0000FF,
                        49 => self.current_bg = 0x000000,
                        _ => {}
                    }
                }
            }
            b'H' | b'f' => { // Cursor Position
                let y = params.get(0).cloned().unwrap_or(1);
                let x = params.get(1).cloned().unwrap_or(1);
                buffer.move_cursor(x - 1, y - 1);
            }
            b'J' => { // Erase in Display
                if params.get(0).cloned().unwrap_or(0) == 2 {
                    buffer.clear_screen();
                }
            }
            _ => {}
        }
    }
}
