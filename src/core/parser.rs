//! The escape-sequence parser (L06): an incremental state machine that turns a
//! shell's output byte stream into [`Grid`] mutations.
//!
//! It implements a pragmatic subset of the VT100/ECMA-48 escape repertoire
//! (SGR colors, cursor positioning, erase line/display, scrolling region,
//! alternate screen, device-attribute replies) and tracks the current SGR
//! colors across `advance` calls. OSC strings are handed to [`osc::dispatch`];
//! DCS/APC/PM/SOS strings are consumed opaquely so they never leak as text.

use super::cell::{DEFAULT_BG, DEFAULT_FG};
use super::color::{parse_extended_color, PALETTE_16};
use super::grid::{alt_mode, Grid};
use super::osc;

/// States of the escape-sequence recognizer.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ParserState {
    /// Printable / control bytes go straight to the grid.
    Ground,
    /// Saw `ESC`; awaiting the sequence introducer.
    Esc,
    /// Inside a `CSI` (`ESC [`) sequence; accumulating parameter bytes.
    Csi,
    /// Mid-way through a multibyte UTF-8 code point; awaiting continuation bytes.
    Utf8,
    /// Inside an `OSC` (`ESC ]`) string; bytes are consumed until a `BEL` or
    /// `ST` terminator. We don't act on OSC payloads (titles, cwd reports), we
    /// just keep them from leaking onto the screen.
    Osc,
    /// Saw `ESC` while inside an OSC string; awaiting the `\` of an `ST`
    /// (`ESC \`) terminator.
    OscEsc,
    /// A charset-designation escape (`ESC ( B`, etc.); consume one more byte.
    EscCharset,
    /// Inside a string-type control sequence — DCS (`ESC P`), APC (`ESC _`),
    /// PM (`ESC ^`), or SOS (`ESC X`). Like [`ParserState::Osc`], the body is
    /// consumed opaquely (we don't act on Sixel, DECRQSS, Kitty graphics, …)
    /// so it never leaks onto the screen. Terminated by `ST` (`ESC \`).
    StrSink,
    /// Saw `ESC` while inside a [`ParserState::StrSink`] string; awaiting the
    /// `\` of an `ST` terminator.
    StrSinkEsc,
}

/// Incremental parser that turns a shell's output byte stream into [`Grid`]
/// mutations. Tracks the current SGR colors across `advance` calls.
pub struct AnsiParser {
    state: ParserState,
    current_fg: u32,
    current_bg: u32,
    param_buffer: String,
    /// Set when a CSI sequence carries a private marker (`?`, `<`, `=`, `>`).
    /// Such sequences (e.g. DEC private mode set/reset) are consumed but not
    /// acted upon.
    csi_private: bool,
    /// The actual private-marker byte (`?`/`<`/`=`/`>`) of the CSI in flight, or
    /// `0` if none. Lets the dispatcher tell DA2 (`CSI > c`) from DEC private
    /// modes (`CSI ? … h/l`). Reset at the start of each CSI.
    csi_marker: u8,
    /// Code point accumulated so far while in [`ParserState::Utf8`].
    utf8_acc: u32,
    /// Number of UTF-8 continuation bytes still expected.
    utf8_remaining: usize,
    /// Bytes the parser owes the host in reply to a query (DA1/DA2/DSR). The
    /// driver drains these via [`AnsiParser::take_responses`] after each
    /// `advance` and writes them back to the PTY master, where the child reads
    /// them as terminal input.
    responses: Vec<u8>,
    /// Raw bytes of the OSC string currently being collected (between `ESC ]`
    /// and its `BEL`/`ST` terminator). Decoded as UTF-8 at dispatch. Capped at
    /// [`OSC_MAX`] so a pathological unterminated OSC can't grow without bound.
    osc_buffer: Vec<u8>,
}

/// Upper bound on the bytes buffered for a single OSC string. Real titles and
/// cwd URIs are far shorter; past this we keep consuming the string but stop
/// storing it.
const OSC_MAX: usize = 4096;

impl Default for AnsiParser {
    fn default() -> Self {
        Self::new()
    }
}

impl AnsiParser {
    /// Create a parser in the ground state with default colors.
    pub fn new() -> Self {
        Self {
            state: ParserState::Ground,
            current_fg: DEFAULT_FG,
            current_bg: DEFAULT_BG,
            param_buffer: String::new(),
            csi_private: false,
            csi_marker: 0,
            utf8_acc: 0,
            utf8_remaining: 0,
            responses: Vec::new(),
            osc_buffer: Vec::new(),
        }
    }

    /// Drain the bytes the parser owes the host in reply to queries (DA1/DA2/
    /// DSR). Returns an empty vector when there is nothing to send. The driver
    /// calls this after `advance` and writes the result to the PTY master.
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.responses)
    }

    /// Feed a chunk of bytes, applying their effects to `g`. Parser state
    /// persists across calls, so escape sequences may straddle chunk boundaries.
    pub fn advance(&mut self, g: &mut Grid, bytes: &[u8]) {
        for &b in bytes {
            match self.state {
                ParserState::Ground => self.ground_byte(b, g),
                ParserState::Utf8 => {
                    if (0x80..=0xbf).contains(&b) {
                        // Valid continuation byte: fold in its 6 payload bits.
                        self.utf8_acc = (self.utf8_acc << 6) | (b as u32 & 0x3f);
                        self.utf8_remaining -= 1;
                        if self.utf8_remaining == 0 {
                            let ch = char::from_u32(self.utf8_acc).unwrap_or('\u{FFFD}');
                            g.put_char(ch, self.current_fg, self.current_bg);
                            self.state = ParserState::Ground;
                        }
                    } else {
                        // Truncated sequence: emit a replacement char, then
                        // reprocess this byte from the ground state.
                        g.put_char('\u{FFFD}', self.current_fg, self.current_bg);
                        self.state = ParserState::Ground;
                        self.ground_byte(b, g);
                    }
                }
                ParserState::Esc => match b {
                    b'[' => {
                        self.param_buffer.clear();
                        self.csi_private = false;
                        self.csi_marker = 0;
                        self.state = ParserState::Csi;
                    }
                    b']' => {
                        self.osc_buffer.clear();
                        self.state = ParserState::Osc;
                    }
                    // DECSC / DECRC: save and restore the cursor.
                    b'7' => {
                        g.save_cursor();
                        self.state = ParserState::Ground;
                    }
                    b'8' => {
                        g.restore_cursor();
                        self.state = ParserState::Ground;
                    }
                    // Charset designation (`ESC ( B`, etc.): one more byte follows.
                    b'(' | b')' | b'*' | b'+' => self.state = ParserState::EscCharset,
                    // String-type introducers — DCS (`P`), SOS (`X`), PM (`^`),
                    // APC (`_`). Their bodies are consumed opaquely until ST so
                    // they don't leak as printed text (cf. tmux DCS passthrough,
                    // DECRQSS replies, Kitty graphics, Sixel).
                    b'P' | b'X' | b'^' | b'_' => self.state = ParserState::StrSink,
                    // Any other ESC X sequence is a single byte we don't model;
                    // consuming b returns us to ground without leaking it.
                    _ => self.state = ParserState::Ground,
                },
                ParserState::Csi => match b {
                    // Parameter bytes.
                    b'0'..=b'9' | b';' => self.param_buffer.push(b as char),
                    // Private markers (`<`, `=`, `>`, `?`): flag, remember which,
                    // and keep collecting.
                    0x3c..=0x3f => {
                        self.csi_private = true;
                        self.csi_marker = b;
                    }
                    // Intermediate bytes (space..`/`): ignored but part of the sequence.
                    0x20..=0x2f => {}
                    // Final byte: dispatch and reset. Private sequences (with a
                    // `?`/`<`/`=`/`>` marker) go to their own handler.
                    0x40..=0x7e => {
                        if self.csi_private {
                            self.handle_private_csi(b, g);
                        } else {
                            self.handle_csi(b, g);
                        }
                        self.state = ParserState::Ground;
                        self.param_buffer.clear();
                        self.csi_private = false;
                    }
                    // CAN / SUB cancel the sequence.
                    0x18 | 0x1a => {
                        self.state = ParserState::Ground;
                        self.param_buffer.clear();
                        self.csi_private = false;
                    }
                    // ESC starts a fresh escape sequence.
                    0x1b => {
                        self.state = ParserState::Esc;
                        self.param_buffer.clear();
                        self.csi_private = false;
                    }
                    // Other C0 controls execute in place; the CSI continues
                    // (VT500 parser semantics) so its parameters are preserved.
                    0x00..=0x17 | 0x19 | 0x1c..=0x1f => {
                        self.ground_byte(b, g);
                        self.state = ParserState::Csi;
                    }
                    // DEL is ignored inside a CSI; any other byte aborts.
                    _ => {
                        if b != 0x7f {
                            self.state = ParserState::Ground;
                            self.param_buffer.clear();
                            self.csi_private = false;
                        }
                    }
                },
                ParserState::Osc => match b {
                    0x07 => {
                        // BEL terminator: act on the collected string.
                        osc::dispatch(&self.osc_buffer, g);
                        self.state = ParserState::Ground;
                    }
                    0x1b => self.state = ParserState::OscEsc, // possible ST (ESC \)
                    _ => {
                        // Accumulate the payload byte, bounded.
                        if self.osc_buffer.len() < OSC_MAX {
                            self.osc_buffer.push(b);
                        }
                    }
                },
                ParserState::OscEsc => {
                    // Whether or not this is the `\` of an ST, the OSC string is
                    // over; act on it and return to ground.
                    osc::dispatch(&self.osc_buffer, g);
                    self.state = ParserState::Ground;
                }
                ParserState::EscCharset => self.state = ParserState::Ground,
                ParserState::StrSink => match b {
                    0x1b => self.state = ParserState::StrSinkEsc, // possible ST (ESC \)
                    0x18 | 0x1a => self.state = ParserState::Ground, // CAN / SUB abort
                    _ => {}                                          // consume body byte
                },
                ParserState::StrSinkEsc => {
                    // Whether or not this is the `\` of an ST, the string is
                    // over; drop the byte and return to ground.
                    self.state = ParserState::Ground;
                }
            }
        }
    }

    /// Handle a single byte while in the ground state: C0 controls, printable
    /// ASCII, and the lead byte of a UTF-8 code point (which transitions into
    /// [`ParserState::Utf8`]).
    fn ground_byte(&mut self, b: u8, g: &mut Grid) {
        match b {
            0x1b => self.state = ParserState::Esc,
            0x08 => g.cursor.0 = g.cursor.0.saturating_sub(1), // backspace
            b'\n' => {
                g.carriage_return();
                g.newline();
            }
            b'\r' => g.carriage_return(),
            b'\t' => {
                // Advance to the next 8-column tab stop, clamped at the right
                // margin so we never wrap/scroll on a tab.
                let next_stop = (g.cursor.0 / 8 + 1) * 8;
                let target = next_stop.min(g.cols.saturating_sub(1));
                while g.cursor.0 < target {
                    g.put_char(' ', self.current_fg, self.current_bg);
                }
            }
            0x20..=0x7e => g.put_char(b as char, self.current_fg, self.current_bg),
            // UTF-8 lead bytes: stash the payload bits and how many continuation
            // bytes to expect. (0xC0/0xC1 are always overlong, hence excluded.)
            0xc2..=0xdf => {
                self.utf8_acc = (b as u32) & 0x1f;
                self.utf8_remaining = 1;
                self.state = ParserState::Utf8;
            }
            0xe0..=0xef => {
                self.utf8_acc = (b as u32) & 0x0f;
                self.utf8_remaining = 2;
                self.state = ParserState::Utf8;
            }
            0xf0..=0xf4 => {
                self.utf8_acc = (b as u32) & 0x07;
                self.utf8_remaining = 3;
                self.state = ParserState::Utf8;
            }
            // Stray continuation or otherwise invalid lead byte.
            0x80..=0xbf | 0xc0..=0xc1 | 0xf5..=0xff => {
                g.put_char('\u{FFFD}', self.current_fg, self.current_bg);
            }
            // Other C0 controls are ignored.
            _ => {}
        }
    }

    /// Handle a private CSI sequence (one carrying a `?`/`<`/`=`/`>` marker).
    ///
    /// Only the alternate-screen DEC modes are acted upon; other private modes
    /// (bracketed paste `2004`, cursor visibility `25`, …) are consumed and
    /// ignored so they never leak as text.
    fn handle_private_csi(&mut self, cmd: u8, g: &mut Grid) {
        // DA2 (Secondary Device Attributes): `CSI > c`. Reply with a terminal
        // type (0), a firmware "version", and a ROM cartridge field (0) — the
        // values are conventional; programs care that an answer arrives.
        if self.csi_marker == b'>' && cmd == b'c' {
            self.responses.extend_from_slice(b"\x1b[>0;1;0c");
            return;
        }
        let params = self.parse_params();
        // 47 / 1047 / 1049 select the alternate screen buffer; the mode governs
        // cursor save/restore (only 1049). The leave path uses the mode stashed
        // on entry, so the reset parameter's exact number doesn't matter.
        let mode = params.iter().flatten().copied().find_map(alt_mode);
        match (cmd, mode) {
            (b'h', Some(mode)) => g.enter_alt_screen(mode),
            (b'l', Some(_)) => g.leave_alt_screen(),
            _ => {}
        }
    }

    /// Parse `param_buffer` into positional parameters. An empty slot (e.g. the
    /// leading field of `CSI ;5H`) becomes `None` so callers can apply the
    /// per-command default in the correct position, per ECMA-48 §5.4.2.
    fn parse_params(&self) -> Vec<Option<usize>> {
        if self.param_buffer.is_empty() {
            return Vec::new();
        }
        self.param_buffer
            .split(';')
            .map(|s| if s.is_empty() { None } else { s.parse().ok() })
            .collect()
    }

    /// Dispatch a completed CSI sequence given its final command byte.
    fn handle_csi(&mut self, cmd: u8, g: &mut Grid) {
        let params = self.parse_params();
        // Positional parameter `i` with `default` applied to an absent/empty slot.
        let p = |i: usize, default: usize| params.get(i).copied().flatten().unwrap_or(default);
        // Most cursor-motion commands take a single count that defaults to (and
        // treats 0 as) 1.
        let count = p(0, 1).max(1);

        match cmd {
            b'm' => {
                // SGR: an empty list resets; otherwise an empty slot means 0.
                let sgr: Vec<usize> = params.iter().map(|o| o.unwrap_or(0)).collect();
                self.apply_sgr(&sgr);
            }
            b'H' | b'f' => {
                // CSI row ; col H — both 1-based; default to 1.
                g.set_cursor(p(1, 1).saturating_sub(1), p(0, 1).saturating_sub(1));
            }
            b'A' => g.set_cursor(g.cursor.0, g.cursor.1.saturating_sub(count)), // CUU
            b'B' => g.set_cursor(g.cursor.0, g.cursor.1.saturating_add(count)), // CUD
            b'C' => g.set_cursor(g.cursor.0.saturating_add(count), g.cursor.1), // CUF
            b'D' => g.set_cursor(g.cursor.0.saturating_sub(count), g.cursor.1), // CUB
            b'G' => g.set_cursor(p(0, 1).saturating_sub(1), g.cursor.1),        // CHA
            b'd' => g.set_cursor(g.cursor.0, p(0, 1).saturating_sub(1)),        // VPA
            b'@' => g.insert_chars(count), // ICH
            b'P' => g.delete_chars(count), // DCH
            b'X' => g.erase_chars(count),  // ECH
            b's' => g.save_cursor(),       // SCP
            b'u' => g.restore_cursor(),    // RCP
            b'r' => {
                // DECSTBM — set top/bottom scrolling margins (1-based).
                let top = p(0, 1).saturating_sub(1);
                let bottom = p(1, g.rows).saturating_sub(1);
                g.set_scroll_region(top, bottom);
            }
            b'J' => {
                // ED — erase in display.
                let (cx, cy) = g.cursor;
                match p(0, 0) {
                    0 => {
                        // Cursor to end of screen.
                        g.clear_row_range(cy, cx, g.cols);
                        for y in (cy + 1)..g.rows {
                            g.clear_row_range(y, 0, g.cols);
                        }
                    }
                    1 => {
                        // Start of screen to cursor (inclusive).
                        for y in 0..cy {
                            g.clear_row_range(y, 0, g.cols);
                        }
                        g.clear_row_range(cy, 0, cx + 1);
                    }
                    2 | 3 => g.clear_all(),
                    _ => {}
                }
            }
            b'K' => {
                // EL — erase in line.
                let (cx, cy) = g.cursor;
                match p(0, 0) {
                    0 => g.clear_row_range(cy, cx, g.cols),
                    1 => g.clear_row_range(cy, 0, cx + 1),
                    2 => g.clear_row_range(cy, 0, g.cols),
                    _ => {}
                }
            }
            b'c' => {
                // DA1 (Primary Device Attributes). Only the default/`0` form is
                // a query; reply that we're a VT100 with Advanced Video Option,
                // a level apps widely accept. A program that sent this would
                // otherwise block waiting for the answer.
                if p(0, 0) == 0 {
                    self.responses.extend_from_slice(b"\x1b[?1;2c");
                }
            }
            b'n' => match p(0, 0) {
                // DSR — Device Status Report.
                5 => self.responses.extend_from_slice(b"\x1b[0n"), // terminal OK
                6 => {
                    // CPR — report the cursor position, 1-based row;col.
                    let (cx, cy) = g.cursor;
                    self.responses
                        .extend_from_slice(format!("\x1b[{};{}R", cy + 1, cx + 1).as_bytes());
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// Reset SGR state to the default colors.
    fn reset_sgr(&mut self) {
        self.current_fg = DEFAULT_FG;
        self.current_bg = DEFAULT_BG;
    }

    /// Apply an SGR (`CSI … m`) parameter list. Supports reset, the 16-color
    /// palette (normal + bright), and the extended `38/48;5;n` (256-color) and
    /// `38/48;2;r;g;b` (truecolor) forms. An empty list means reset.
    fn apply_sgr(&mut self, params: &[usize]) {
        if params.is_empty() {
            self.reset_sgr();
            return;
        }
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => self.reset_sgr(),
                30..=37 => self.current_fg = PALETTE_16[params[i] - 30],
                38 => {
                    if let Some((color, consumed)) = parse_extended_color(&params[i + 1..]) {
                        self.current_fg = color;
                        i += consumed;
                    }
                }
                39 => self.current_fg = DEFAULT_FG,
                40..=47 => self.current_bg = PALETTE_16[params[i] - 40],
                48 => {
                    if let Some((color, consumed)) = parse_extended_color(&params[i + 1..]) {
                        self.current_bg = color;
                        i += consumed;
                    }
                }
                49 => self.current_bg = DEFAULT_BG,
                90..=97 => self.current_fg = PALETTE_16[8 + (params[i] - 90)],
                100..=107 => self.current_bg = PALETTE_16[8 + (params[i] - 100)],
                _ => {}
            }
            i += 1;
        }
    }
}
