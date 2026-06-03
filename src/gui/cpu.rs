//! CPU rasterizer: composite a [`Grid`] into an RGBA pixel buffer.
//!
//! Pure (no window), so it is unit-tested headlessly; the windowed front-end
//! later hands the buffer to `softbuffer` for presentation. Pixels are
//! `0x00RRGGBB` (the format `softbuffer` expects).

use crate::core::{Grid, WIDE_TRAILER};

use super::font::{Glyph, GlyphSource};

/// Composite the grid's visible cells into `buf` (`width × height` pixels,
/// `len() == width * height`). Each cell is filled with its background, then its
/// glyph is blended on top in the foreground color. Geometry comes from the
/// font's cell size; cells past the buffer edge are clipped.
pub(crate) fn render(
    grid: &Grid,
    font: &mut dyn GlyphSource,
    buf: &mut [u32],
    width: usize,
    height: usize,
) {
    let (cw, ch) = font.cell_size();
    let baseline = font.baseline();
    if cw == 0 || ch == 0 {
        return;
    }

    // Pass 1: backgrounds. A wide glyph's bitmap may spill into its trailer
    // cell, so fill every cell (including trailers) before drawing glyphs.
    for (i, cell) in grid.cells.iter().enumerate() {
        let (x0, y0) = ((i % grid.cols) * cw, (i / grid.cols) * ch);
        for y in y0..(y0 + ch).min(height) {
            let base = y * width;
            for x in x0..(x0 + cw).min(width) {
                buf[base + x] = cell.bg;
            }
        }
    }

    // Pass 2: glyphs.
    for (i, cell) in grid.cells.iter().enumerate() {
        if cell.flags & WIDE_TRAILER != 0 || (cell.ch == ' ' && cell.cluster == 0) {
            continue;
        }
        let glyph = font.glyph(cell.ch);
        if glyph.width == 0 {
            continue;
        }
        let (col, row) = (i % grid.cols, i / grid.cols);
        let pen_x = (col * cw) as i32 + glyph.left;
        let pen_y = (row * ch) as i32 + baseline + glyph.top;
        blit(buf, width, height, &glyph, pen_x, pen_y, cell.fg);
    }
}

/// Alpha-blend a glyph's coverage in `fg` over whatever is already in `buf`.
fn blit(buf: &mut [u32], width: usize, height: usize, glyph: &Glyph, pen_x: i32, pen_y: i32, fg: u32) {
    for gy in 0..glyph.height {
        let py = pen_y + gy as i32;
        if py < 0 || py as usize >= height {
            continue;
        }
        let row = py as usize * width;
        for gx in 0..glyph.width {
            let px = pen_x + gx as i32;
            if px < 0 || px as usize >= width {
                continue;
            }
            let a = glyph.coverage[gy * glyph.width + gx];
            if a == 0 {
                continue;
            }
            let idx = row + px as usize;
            buf[idx] = blend(buf[idx], fg, a);
        }
    }
}

/// Blend `fg` over `bg` by 8-bit coverage `a` (per channel, `0x00RRGGBB`).
fn blend(bg: u32, fg: u32, a: u8) -> u32 {
    let a = a as u32;
    let inv = 255 - a;
    let chan = |shift: u32| {
        let f = (fg >> shift) & 0xff;
        let b = (bg >> shift) & 0xff;
        (f * a + b * inv) / 255
    };
    (chan(16) << 16) | (chan(8) << 8) | chan(0)
}

#[cfg(test)]
mod tests {
    use super::super::font::Glyph;
    use super::*;
    use crate::core::AnsiParser;
    use std::rc::Rc;

    /// A deterministic 4×8 cell whose every non-space glyph is a solid 2×2 block
    /// at the cell's top-left — no font file needed.
    struct MockFont;
    impl GlyphSource for MockFont {
        fn cell_size(&self) -> (usize, usize) {
            (4, 8)
        }
        fn baseline(&self) -> i32 {
            6
        }
        fn glyph(&mut self, ch: char) -> Rc<Glyph> {
            if ch == ' ' {
                return Rc::new(Glyph { width: 0, height: 0, left: 0, top: 0, coverage: Vec::new() });
            }
            // top = -baseline places the bitmap's top row at the cell's top.
            Rc::new(Glyph { width: 2, height: 2, left: 0, top: -6, coverage: vec![255; 4] })
        }
    }

    #[test]
    fn renders_background_and_glyph() {
        let mut g = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        // Red on blue.
        p.advance(&mut g, b"\x1b[38;2;255;0;0m\x1b[48;2;0;0;255mX");
        let (w, h) = (4usize, 8usize);
        let mut buf = vec![0u32; w * h];
        render(&g, &mut MockFont, &mut buf, w, h);
        // The 2×2 block at the top-left is the red foreground...
        assert_eq!(buf[0], 0xFF0000);
        assert_eq!(buf[1], 0xFF0000);
        assert_eq!(buf[w], 0xFF0000);
        assert_eq!(buf[w + 1], 0xFF0000);
        // ...everything else is the blue background.
        assert_eq!(buf[2], 0x0000FF);
        assert_eq!(buf[w * (h - 1)], 0x0000FF);
    }

    #[test]
    fn blank_cell_is_pure_background() {
        let mut g = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[48;2;0;128;0m "); // a space painted with green bg
        let mut buf = vec![0u32; 4 * 8];
        render(&g, &mut MockFont, &mut buf, 4, 8);
        assert!(buf.iter().all(|&px| px == 0x008000));
    }

    #[test]
    fn blend_endpoints() {
        assert_eq!(blend(0x000000, 0xFFFFFF, 255), 0xFFFFFF); // full coverage = fg
        assert_eq!(blend(0x0000FF, 0xFF0000, 0), 0x0000FF); // zero coverage = bg
        assert_eq!(blend(0x000000, 0xFFFFFF, 128), 0x808080); // half = mid-gray
    }

    #[test]
    fn real_font_fills_a_frame() {
        let Some(bytes) = super::super::font::load_default_font() else {
            eprintln!("no system font; skipping real-font render");
            return;
        };
        let mut fc = super::super::font::FontCache::new(bytes, 16.0).unwrap();
        let (cw, chh) = fc.cell_size();
        let mut g = Grid::new(3, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[48;2;0;0;0mabc");
        let (w, h) = (cw * 3, chh);
        let mut buf = vec![0u32; w * h];
        render(&g, &mut fc, &mut buf, w, h);
        // Glyphs were drawn: at least some pixels differ from the black bg.
        assert!(buf.iter().any(|&px| px != 0x000000), "expected rasterized glyph pixels");
    }
}
