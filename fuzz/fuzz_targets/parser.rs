//! Fuzz the whole untrusted-input surface through its real entry point:
//! arbitrary bytes into `AnsiParser::advance`, exactly as a hostile child
//! process would deliver them. This transitively exercises the UTF-8
//! decoder, the CSI/OSC/DCS/APC state machine, SGR parsing, Sixel, the
//! Kitty graphics stack (base64 → inflate → png), iTerm2 (base64 → png/
//! jpeg), and every grid mutation they drive.

#![no_main]
use libfuzzer_sys::fuzz_target;
use rusty_term::core::{AnsiParser, Grid};

fuzz_target!(|data: &[u8]| {
    let mut grid = Grid::new(80, 24);
    let mut parser = AnsiParser::new();
    // Split the input in two so incremental (chunk-boundary) state is
    // exercised too — split-UTF-8 and split-escape handling regress easily.
    let mid = data.len() / 2;
    parser.advance(&mut grid, &data[..mid]);
    parser.advance(&mut grid, &data[mid..]);
    let _ = parser.take_responses();
    let _ = grid.take_host_out();
});
