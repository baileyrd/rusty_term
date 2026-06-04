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

    // Block cursor + drag-selection are drawn by inverting a cell's fg/bg. The
    // cursor shows only on the live view, not while scrolled into history.
    let cursor = (grid.cursor_visible && grid.view_offset == 0).then_some(grid.cursor);
    let inverted = |col: usize, row: usize| cursor == Some((col, row)) || grid.is_selected(col, row);

    // The status-line overlay (L13), when present, replaces the bottom row.
    let status = grid.status_row();
    let last_row = grid.rows.saturating_sub(1);

    // Pass 1: backgrounds. A wide glyph's bitmap may spill into its trailer
    // cell, so fill every cell (including trailers) before drawing glyphs.
    for (i, cell) in grid.cells.iter().enumerate() {
        let (col, row) = (i % grid.cols, i / grid.cols);
        let on_status = status.is_some() && row == last_row;
        let cell = if on_status { status.unwrap()[col] } else { *cell };
        let bg = if !on_status && inverted(col, row) { cell.fg } else { cell.bg };
        let (x0, y0) = (col * cw, row * ch);
        for y in y0..(y0 + ch).min(height) {
            let base = y * width;
            for x in x0..(x0 + cw).min(width) {
                buf[base + x] = bg;
            }
        }
    }

    // Pass 2: glyphs.
    for (i, cell) in grid.cells.iter().enumerate() {
        let (col, row) = (i % grid.cols, i / grid.cols);
        let on_status = status.is_some() && row == last_row;
        let cell = if on_status { status.unwrap()[col] } else { *cell };
        if cell.flags & WIDE_TRAILER != 0 || (cell.ch == ' ' && cell.cluster == 0) {
            continue;
        }
        let glyph = font.glyph(cell.ch);
        if glyph.width == 0 {
            continue;
        }
        let fg = if !on_status && inverted(col, row) { cell.bg } else { cell.fg };
        let pen_x = (col * cw) as i32 + glyph.left;
        let pen_y = (row * ch) as i32 + baseline + glyph.top;
        blit(buf, width, height, &glyph, pen_x, pen_y, fg);
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

    #[cfg(feature = "l13")]
    #[test]
    fn status_line_overlays_bottom_cell_row() {
        let mut g = Grid::new(2, 2);
        // Distinct status bg (0x123456) so the overlay is detectable; white fg.
        g.set_status_line("X".into(), Some(0xFFFFFF), Some(0x123456));
        let (cw, chh) = (4usize, 8usize);
        let (w, h) = (cw * 2, chh * 2);
        let mut buf = vec![0u32; w * h];
        render(&g, &mut MockFont, &mut buf, w, h);
        // Bottom cell-row, second cell (no glyph there) is pure status bg.
        assert_eq!(buf[chh * w + cw + 1], 0x123456, "bottom row is the status overlay");
        // A non-cursor top-row cell (col 1) is untouched: default black background.
        assert_eq!(buf[cw], 0x000000, "top row not overlaid");
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

    #[test]
    fn cursor_inverts_its_cell() {
        // Red fg on blue bg, a space (no glyph) so only the block shows.
        let mut g = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[38;2;255;0;0m\x1b[48;2;0;0;255m ");
        g.cursor = (0, 0);
        g.cursor_visible = true;
        let mut buf = vec![0u32; 4 * 8];
        render(&g, &mut MockFont, &mut buf, 4, 8);
        // Inverted: the cell is painted in the fg color (red), not the blue bg.
        assert!(buf.iter().all(|&px| px == 0xFF0000), "cursor cell is a red block");
    }

    #[test]
    fn hidden_cursor_is_not_drawn() {
        let mut g = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[38;2;255;0;0m\x1b[48;2;0;0;255m ");
        g.cursor = (0, 0);
        g.cursor_visible = false;
        let mut buf = vec![0u32; 4 * 8];
        render(&g, &mut MockFont, &mut buf, 4, 8);
        assert!(buf.iter().all(|&px| px == 0x0000FF), "no cursor: plain blue bg");
    }

    #[test]
    fn cursor_hidden_while_scrolled_back() {
        let mut g = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[38;2;255;0;0m\x1b[48;2;0;0;255m ");
        g.cursor = (0, 0);
        g.cursor_visible = true;
        g.view_offset = 1; // browsing history — live cursor must not draw
        let mut buf = vec![0u32; 4 * 8];
        render(&g, &mut MockFont, &mut buf, 4, 8);
        assert!(buf.iter().all(|&px| px == 0x0000FF), "scrolled back: no cursor");
    }

    #[test]
    fn selection_inverts_cells() {
        let mut g = Grid::new(2, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[38;2;255;0;0m\x1b[48;2;0;0;255m  ");
        g.cursor_visible = false; // isolate selection from the cursor
        g.selection = Some(crate::core::Selection { anchor: (0, 0), head: (0, 0) });
        let (w, h) = (8usize, 8usize); // 2 cols * 4px
        let mut buf = vec![0u32; w * h];
        render(&g, &mut MockFont, &mut buf, w, h);
        // Col 0 inverted (red block), col 1 untouched (blue bg).
        assert_eq!(buf[0], 0xFF0000, "selected cell inverted");
        assert_eq!(buf[4], 0x0000FF, "unselected cell unchanged");
    }
}
