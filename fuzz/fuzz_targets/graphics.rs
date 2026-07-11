//! Focused graphics fuzzing: wrap the input as each image protocol's
//! payload so the decoders see coherent framing immediately, instead of
//! waiting for the general fuzzer to invent `ESC P q` by chance. Still
//! drives everything through the public parser entry point.

#![no_main]
use libfuzzer_sys::fuzz_target;
use rusty_term::core::{AnsiParser, Grid};

fuzz_target!(|data: &[u8]| {
    let mut grid = Grid::new(80, 24);
    let mut parser = AnsiParser::new();
    let mut feed = |frame: Vec<u8>| {
        parser.advance(&mut grid, &frame);
        let _ = parser.take_responses();
        let _ = grid.take_host_out();
    };
    // Sixel: DCS q <data> ST.
    let mut sixel = b"\x1bPq".to_vec();
    sixel.extend_from_slice(data);
    sixel.extend_from_slice(b"\x1b\\");
    feed(sixel);
    // Kitty graphics: APC G <data> ST (control keys + payload both attacker
    // controlled in real use, so raw bytes are the right shape).
    let mut kitty = b"\x1b_G".to_vec();
    kitty.extend_from_slice(data);
    kitty.extend_from_slice(b"\x1b\\");
    feed(kitty);
    // iTerm2 inline image: OSC 1337;File=inline=1:<base64-ish> BEL.
    let mut iterm = b"\x1b]1337;File=inline=1:".to_vec();
    iterm.extend_from_slice(data);
    iterm.push(0x07);
    feed(iterm);
});
