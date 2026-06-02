//! Color palette and SGR color resolution (L06).
//!
//! Holds the classic 16-color ANSI palette and the helpers that resolve the
//! extended SGR color selectors (`38/48;5;n` 256-color and `38/48;2;r;g;b`
//! truecolor) into `0xRRGGBB` values.

use super::cell::DEFAULT_FG;

/// Standard 16-color ANSI palette (indices 0-7 normal, 8-15 bright), in
/// `0xRRGGBB` form. Roughly matches the classic xterm palette.
pub(crate) const PALETTE_16: [u32; 16] = [
    0x000000, 0x800000, 0x008000, 0x808000, 0x000080, 0x800080, 0x008080, 0xC0C0C0,
    0x808080, 0xFF0000, 0x00FF00, 0xFFFF00, 0x0000FF, 0xFF00FF, 0x00FFFF, 0xFFFFFF,
];

/// Parse the tail of an extended SGR color selector (the part after `38`/`48`).
///
/// Returns the resolved `0xRRGGBB` color and how many *additional* params were
/// consumed, or `None` if the form is unrecognized.
///
/// * `5; n`        → 256-color palette index `n` (3 params consumed)
/// * `2; r; g; b`  → truecolor (4 params consumed)
pub(crate) fn parse_extended_color(rest: &[usize]) -> Option<(u32, usize)> {
    match rest.first().copied() {
        Some(5) => {
            let n = *rest.get(1)?;
            Some((xterm_256_to_rgb(n), 2))
        }
        Some(2) => {
            let r = (*rest.get(1)? & 0xFF) as u32;
            let g = (*rest.get(2)? & 0xFF) as u32;
            let b = (*rest.get(3)? & 0xFF) as u32;
            Some(((r << 16) | (g << 8) | b, 4))
        }
        _ => None,
    }
}

/// Convert an xterm 256-color index to `0xRRGGBB`.
///
/// 0-15 map to the base palette, 16-231 form a 6×6×6 color cube, and 232-255
/// are a 24-step grayscale ramp.
fn xterm_256_to_rgb(n: usize) -> u32 {
    match n {
        0..=15 => PALETTE_16[n],
        16..=231 => {
            let n = n - 16;
            let steps = [0u32, 95, 135, 175, 215, 255];
            let r = steps[(n / 36) % 6];
            let g = steps[(n / 6) % 6];
            let b = steps[n % 6];
            (r << 16) | (g << 8) | b
        }
        232..=255 => {
            let level = 8 + (n - 232) as u32 * 10;
            (level << 16) | (level << 8) | level
        }
        _ => DEFAULT_FG,
    }
}
