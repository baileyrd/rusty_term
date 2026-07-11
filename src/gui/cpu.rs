//! CPU rasterizer: composite a [`Grid`] into an RGBA pixel buffer.
//!
//! Pure (no window), so it is unit-tested headlessly; the windowed front-end
//! later hands the buffer to `softbuffer` for presentation. Pixels are
//! `0x00RRGGBB` (the format `softbuffer` expects).

use std::rc::Rc;

use crate::core::{
    ATTR_BOLD, ATTR_ITALIC, ATTR_STRIKE, ATTR_UNDERLINE, ATTR_UNDERLINE_COLOR, Cell, CursorShape,
    Grid, UnderlineStyle, WIDE_TRAILER, char_width,
};

use super::font::{Glyph, GlyphSource, Style};

/// Search-match highlight: amber for a match, orange for the active one, with a
/// dark glyph so text stays legible on either.
const SEARCH_BG: u32 = 0xFFD24A;
const SEARCH_CUR_BG: u32 = 0xFF7A1A;
const SEARCH_FG: u32 = 0x101010;

/// Composite the grid's visible cells into `buf` (`width × height` pixels,
/// `len() == width * height`). Each cell is filled with its background, then its
/// glyph is blended on top in the foreground color. Geometry comes from the
/// font's cell size; cells past the buffer edge are clipped.
///
/// A non-empty `chrome` row (the window's own tab/caption bar) is painted as
/// the first cell row and pushes the grid one row down; empty paints the grid
/// at the top, unchanged.
#[cfg(test)]
pub(crate) fn render(
    grid: &Grid,
    chrome: &[Cell],
    font: &mut dyn GlyphSource,
    buf: &mut [u32],
    width: usize,
    height: usize,
    cursor_on: bool,
) {
    let (cw, ch) = font.cell_size();
    if cw == 0 || ch == 0 {
        return;
    }
    if !chrome.is_empty() {
        draw_chrome(buf, width, height, chrome, font, cw, ch);
    }
    let row0 = if chrome.is_empty() { 0 } else { 1 };
    draw_grid(buf, width, height, grid, 0, row0, true, cursor_on, font);
}

/// Paint the window's chrome bar (tabs + caption buttons) at pixel row 0.
pub(crate) fn draw_chrome(
    buf: &mut [u32],
    width: usize,
    height: usize,
    chrome: &[Cell],
    font: &mut dyn GlyphSource,
    cw: usize,
    ch: usize,
) {
    let baseline = font.baseline();
    for (col, cell) in chrome.iter().enumerate() {
        let x0 = col * cw;
        for y in 0..ch.min(height) {
            let base = y * width;
            for x in x0..(x0 + cw).min(width) {
                buf[base + x] = cell.bg;
            }
        }
    }
    for (col, cell) in chrome.iter().enumerate() {
        if cell.flags & WIDE_TRAILER != 0 || cell.ch == ' ' {
            continue;
        }
        let glyph = font.glyph(cell.ch, Style::Regular);
        if glyph.width == 0 {
            continue;
        }
        let pen_x = (col * cw) as i32 + glyph.left;
        let pen_y = baseline + glyph.top;
        blit(buf, width, height, &glyph, pen_x, pen_y, cell.fg);
    }
}

/// Composite one grid's visible cells into `buf` at cell offset `(col0, row0)`,
/// extent `grid.cols × grid.rows`. The cursor (block / bar / underline) and IME
/// preedit show only when `focused`; selection and search highlights come from
/// the grid's own state. Cells past the buffer edge are clipped.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_grid(
    buf: &mut [u32],
    width: usize,
    height: usize,
    grid: &Grid,
    col0: usize,
    row0: usize,
    focused: bool,
    cursor_on: bool,
    font: &mut dyn GlyphSource,
) {
    let (cw, ch) = font.cell_size();
    let baseline = font.baseline();
    if cw == 0 || ch == 0 {
        return;
    }
    let cursor = (focused && grid.cursor_visible && grid.view_offset == 0 && cursor_on)
        .then_some(grid.cursor);
    let shape = grid.cursor_shape;
    let block_cursor =
        |col: usize, row: usize| shape == CursorShape::Block && cursor == Some((col, row));
    let inverted = |col: usize, row: usize| grid.view_offset == 0 && grid.is_selected(col, row);
    let search_hl = |col: usize, row: usize| grid.search_highlight(col, row);
    let status = grid.status_row();
    let last_row = grid.rows.saturating_sub(1);

    // Pass 1: backgrounds (every cell, incl. wide trailers, before glyphs).
    for i in 0..grid.cols * grid.rows {
        let (col, row) = (i % grid.cols, i / grid.cols);
        let on_status = status.is_some() && row == last_row;
        let cell = if on_status { status.unwrap()[col] } else { grid.viewport_cell(col, row) };
        let bg = if !on_status && block_cursor(col, row) {
            grid.cursor_color
        } else if !on_status
            && let Some(cur) = search_hl(col, row)
        {
            if cur { SEARCH_CUR_BG } else { SEARCH_BG }
        } else if !on_status && inverted(col, row) {
            cell.fg
        } else {
            cell.bg
        };
        let (x0, y0) = ((col0 + col) * cw, (row0 + row) * ch);
        for y in y0..(y0 + ch).min(height) {
            let base = y * width;
            for x in x0..(x0 + cw).min(width) {
                buf[base + x] = bg;
            }
        }
    }

    // Ligature plan: per row, shape maximal runs of same-style, same-fg,
    // single-width, printable, non-highlighted cells through the font's GSUB
    // (liga/calt). `plan[i]` is the glyph drawn at cell `i` — a wide ligature
    // glyph overdraws the cells it spans; `None` means draw nothing (blank, wide
    // trailer, or a column consumed by a ligature to its left). Cursor/selection/
    // search cells stay out of runs, so ligatures never split across them.
    let mut plan: Vec<Option<Rc<Glyph>>> = vec![None; grid.cols * grid.rows];
    let mut run: Vec<char> = Vec::new();
    for row in 0..grid.rows {
        let on_status = status.is_some() && row == last_row;
        let cell_at = |col: usize| {
            if on_status { status.unwrap()[col] } else { grid.viewport_cell(col, row) }
        };
        let eligible = |col: usize, cell: &Cell| {
            let special = !on_status
                && (block_cursor(col, row) || inverted(col, row) || search_hl(col, row).is_some());
            !special
                && cell.flags & WIDE_TRAILER == 0
                && cell.cluster == 0
                && cell.ch != ' '
                && char_width(cell.ch) == 1
        };
        let mut col = 0;
        while col < grid.cols {
            let cell = cell_at(col);
            if !eligible(col, &cell) {
                let blank = cell.flags & WIDE_TRAILER != 0 || (cell.ch == ' ' && cell.cluster == 0);
                plan[row * grid.cols + col] = (!blank).then(|| {
                    let style = Style::new(cell.flags & ATTR_BOLD != 0, cell.flags & ATTR_ITALIC != 0);
                    font.glyph(cell.ch, style)
                });
                col += 1;
                continue;
            }
            let style = Style::new(cell.flags & ATTR_BOLD != 0, cell.flags & ATTR_ITALIC != 0);
            let fg = cell.fg;
            run.clear();
            let mut end = col;
            while end < grid.cols {
                let c2 = cell_at(end);
                let s2 = Style::new(c2.flags & ATTR_BOLD != 0, c2.flags & ATTR_ITALIC != 0);
                if !eligible(end, &c2) || s2 != style || c2.fg != fg {
                    break;
                }
                run.push(c2.ch);
                end += 1;
            }
            let mut pos = col;
            for (glyph, span) in font.shape(&run, style) {
                plan[row * grid.cols + pos] = Some(glyph);
                pos += span;
            }
            col = end;
        }
    }

    // Pass 2: glyphs, from the ligature plan.
    for (i, slot) in plan.iter().enumerate() {
        let Some(glyph) = slot.as_ref() else { continue };
        if glyph.width == 0 {
            continue;
        }
        let (col, row) = (i % grid.cols, i / grid.cols);
        let on_status = status.is_some() && row == last_row;
        let cell = if on_status { status.unwrap()[col] } else { grid.viewport_cell(col, row) };
        let (fg, under_bg) = if !on_status && block_cursor(col, row) {
            (cell.bg, grid.cursor_color)
        } else if !on_status && inverted(col, row) {
            (cell.bg, cell.fg)
        } else if !on_status
            && let Some(cur) = search_hl(col, row)
        {
            (SEARCH_FG, if cur { SEARCH_CUR_BG } else { SEARCH_BG })
        } else {
            (cell.fg, cell.bg)
        };
        // Minimum-contrast enforcement (`minimum_contrast` config): nudge the
        // glyph color against the background this cell actually painted.
        let fg = crate::core::ensure_contrast(fg, under_bg, grid.min_contrast);
        let pen_x = ((col0 + col) * cw) as i32 + glyph.left;
        let pen_y = ((row0 + row) * ch) as i32 + baseline + glyph.top;
        blit(buf, width, height, glyph, pen_x, pen_y, fg);
    }

    // Pass 2.5: underline / strikethrough decorations (SGR 4/9). Neither is a
    // glyph, so they're drawn as thin pixel stripes here rather than through
    // the font. A colored underline (SGR 58, or `4:2`.."4:5"` undercurl/
    // dotted/dashed styles) follows `cell.underline_color`; otherwise it
    // follows whatever color the glyph pass used (fg, or bg under a
    // cursor/selection swap) so the line matches the text it decorates.
    for i in 0..grid.cols * grid.rows {
        let (col, row) = (i % grid.cols, i / grid.cols);
        let on_status = status.is_some() && row == last_row;
        let cell = if on_status { status.unwrap()[col] } else { grid.viewport_cell(col, row) };
        if cell.flags & WIDE_TRAILER != 0 {
            continue;
        }
        let underline = cell.flags & ATTR_UNDERLINE != 0;
        let strike = cell.flags & ATTR_STRIKE != 0;
        if !underline && !strike {
            continue;
        }
        let swapped = !on_status && (block_cursor(col, row) || inverted(col, row));
        let base_fg = if swapped {
            cell.bg
        } else if !on_status && search_hl(col, row).is_some() {
            SEARCH_FG
        } else {
            cell.fg
        };
        let (x0, y0) = ((col0 + col) * cw, (row0 + row) * ch);
        if underline {
            let color = if !swapped && cell.flags & ATTR_UNDERLINE_COLOR != 0 {
                cell.underline_color
            } else {
                base_fg
            };
            draw_underline(buf, width, height, x0, y0, cw, ch, color, UnderlineStyle::from_attrs(cell.flags));
        }
        if strike {
            draw_strike(buf, width, height, x0, y0, cw, ch, base_fg);
        }
    }

    // Pixel images (Sixel/Kitty) composited over their reserved half-block cells
    // (pixel-perfect), scaled nearest-neighbor to the footprint and clipped to
    // the pane. The grid anchors them by serial, so this tracks scroll/history.
    for im in grid.images() {
        let (dst_w, dst_h) = ((im.cols * cw) as isize, (im.rows * ch) as isize);
        if dst_w <= 0 || dst_h <= 0 {
            continue;
        }
        let x0 = ((col0 + im.col) * cw) as isize;
        let y0 = (row0 as isize + grid.image_top_row(im)) * ch as isize;
        let pane_top = (row0 * ch) as isize;
        let pane_bottom = (((row0 + grid.rows) * ch).min(height)) as isize;
        let pane_right = (((col0 + grid.cols) * cw).min(width)) as isize;
        for dy in 0..dst_h {
            let py = y0 + dy;
            if py < pane_top || py >= pane_bottom {
                continue;
            }
            let sy = dy as usize * im.ph / dst_h as usize;
            for dx in 0..dst_w {
                let px = x0 + dx;
                if px < 0 || px >= pane_right {
                    continue;
                }
                let sx = dx as usize * im.pw / dst_w as usize;
                if let Some(c) = im.pixels[sy * im.pw + sx] {
                    buf[py as usize * width + px as usize] = c;
                }
            }
        }
    }

    // Underline / bar cursors overlay a thin stripe (block is the fg/bg swap).
    if let Some((ccol, crow)) = cursor
        && shape != CursorShape::Block
        && !(status.is_some() && crow == last_row)
    {
        let (x0, y0) = ((col0 + ccol) * cw, (row0 + crow) * ch);
        let color = grid.cursor_color;
        match shape {
            CursorShape::Underline => {
                let thick = (ch / 8).max(1);
                for y in (y0 + ch).saturating_sub(thick)..(y0 + ch).min(height) {
                    let base = y * width;
                    for x in x0..(x0 + cw).min(width) {
                        buf[base + x] = color;
                    }
                }
            }
            CursorShape::Bar => {
                let thick = (cw / 8).max(1);
                for y in y0..(y0 + ch).min(height) {
                    let base = y * width;
                    for x in x0..(x0 + thick).min(width) {
                        buf[base + x] = color;
                    }
                }
            }
            CursorShape::Block => {}
        }
    }

    // IME preedit (composition): reverse-video glyphs at the cursor.
    if focused && !grid.ime_preedit.is_empty() && grid.view_offset == 0 {
        let crow = grid.cursor.1;
        let mut col = grid.cursor.0;
        let y0 = (row0 + crow) * ch;
        for pch in grid.ime_preedit.chars() {
            let w = char_width(pch).max(1);
            if col + w > grid.cols {
                break;
            }
            let base = grid.viewport_cell(col, crow);
            let (fg, bg) = (base.bg, base.fg);
            let x0 = (col0 + col) * cw;
            for y in y0..(y0 + ch).min(height) {
                let b = y * width;
                for x in x0..(x0 + w * cw).min(width) {
                    buf[b + x] = bg;
                }
            }
            let glyph = font.glyph(pch, Style::Regular);
            if glyph.width != 0 {
                let pen_x = x0 as i32 + glyph.left;
                let pen_y = y0 as i32 + baseline + glyph.top;
                blit(buf, width, height, &glyph, pen_x, pen_y, fg);
            }
            col += w;
        }
    }
}

/// Fill a `cw`-wide, `thick`-tall horizontal stripe at `(x0, y0)` in `color`.
#[allow(clippy::too_many_arguments)]
fn hline(buf: &mut [u32], width: usize, height: usize, x0: usize, cw: usize, y0: usize, thick: usize, color: u32) {
    for y in y0..(y0 + thick).min(height) {
        let base = y * width;
        for x in x0..(x0 + cw).min(width) {
            buf[base + x] = color;
        }
    }
}

/// Draw one cell's underline stripe in `style`, near the bottom of the cell
/// box (leaving descenders visible above it).
#[allow(clippy::too_many_arguments)]
fn draw_underline(
    buf: &mut [u32],
    width: usize,
    height: usize,
    x0: usize,
    y0: usize,
    cw: usize,
    ch: usize,
    color: u32,
    style: UnderlineStyle,
) {
    let thick = (ch / 12).max(1);
    let base_y = (y0 + ch).saturating_sub(thick + 1);
    match style {
        UnderlineStyle::Straight => hline(buf, width, height, x0, cw, base_y, thick, color),
        UnderlineStyle::Double => {
            let y1 = base_y.saturating_sub(thick + 1);
            hline(buf, width, height, x0, cw, y1, thick, color);
            hline(buf, width, height, x0, cw, base_y, thick, color);
        }
        UnderlineStyle::Curly => {
            let amp = (ch / 10).max(1) as isize;
            let period = (cw / 2).max(2) as f32;
            for dx in 0..cw {
                let x = x0 + dx;
                if x >= width {
                    break;
                }
                let phase = (dx as f32 / period) * std::f32::consts::TAU;
                let offset = (phase.sin() * amp as f32).round() as isize;
                let y = base_y as isize + offset;
                for t in 0..thick as isize {
                    let py = y + t;
                    if py >= 0 && (py as usize) < height {
                        buf[py as usize * width + x] = color;
                    }
                }
            }
        }
        UnderlineStyle::Dotted => {
            for dx in (0..cw).step_by(2) {
                let x = x0 + dx;
                if x >= width {
                    break;
                }
                for t in 0..thick {
                    let py = base_y + t;
                    if py < height {
                        buf[py * width + x] = color;
                    }
                }
            }
        }
        UnderlineStyle::Dashed => {
            for dx in 0..cw {
                if (dx / 3) % 2 != 0 {
                    continue;
                }
                let x = x0 + dx;
                if x >= width {
                    break;
                }
                for t in 0..thick {
                    let py = base_y + t;
                    if py < height {
                        buf[py * width + x] = color;
                    }
                }
            }
        }
    }
}

/// Draw one cell's strikethrough stripe through the vertical middle of the
/// cell box.
#[allow(clippy::too_many_arguments)]
fn draw_strike(buf: &mut [u32], width: usize, height: usize, x0: usize, y0: usize, cw: usize, ch: usize, color: u32) {
    let thick = (ch / 12).max(1);
    let y = (y0 + ch / 2).saturating_sub(thick / 2);
    hline(buf, width, height, x0, cw, y, thick, color);
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
        fn glyph(&mut self, ch: char, _style: Style) -> Rc<Glyph> {
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
        render(&g, &[], &mut MockFont, &mut buf, w, h, true);
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
    fn chrome_row_paints_first_and_offsets_grid() {
        let mut g = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[48;2;0;0;255m "); // grid: blue bg space
        g.cursor_visible = false;
        // One chrome cell: 'X' in white on a red bar.
        let mut bar = crate::core::Cell::blank();
        bar.ch = 'X';
        bar.fg = 0xFFFFFF;
        bar.bg = 0xFF0000;
        let (w, h) = (4usize, 16usize); // one column, two cell rows
        let mut buf = vec![0u32; w * h];
        render(&g, &[bar], &mut MockFont, &mut buf, w, h, true);
        // Row 0 carries the chrome: red bg with the white 2×2 glyph at top-left.
        assert_eq!(buf[0], 0xFFFFFF, "chrome glyph at top");
        assert_eq!(buf[3], 0xFF0000, "chrome bar background");
        // The grid's blue cell starts one cell row down.
        assert_eq!(buf[8 * w], 0x0000FF, "grid offset below the chrome row");
    }

    #[test]
    fn blank_cell_is_pure_background() {
        let mut g = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[48;2;0;128;0m "); // a space painted with green bg
        let mut buf = vec![0u32; 4 * 8];
        render(&g, &[], &mut MockFont, &mut buf, 4, 8, true);
        assert!(buf.iter().all(|&px| px == 0x008000));
    }

    #[test]
    fn straight_underline_draws_a_stripe_near_the_bottom() {
        let mut g = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[38;2;255;0;0m\x1b[4m ");
        let (w, h) = (4usize, 8usize);
        let mut buf = vec![0u32; w * h];
        render(&g, &[], &mut MockFont, &mut buf, w, h, true);
        // Row 6 (near the bottom of an 8px cell, thick=1) is the underline
        // stripe in the text's foreground.
        assert_eq!(buf[6 * w], 0xFF0000);
        assert_eq!(buf[6 * w + 3], 0xFF0000);
        // The top of the cell is untouched background.
        assert_eq!(buf[0], 0x000000);
    }

    #[test]
    fn strikethrough_draws_a_stripe_through_the_middle() {
        let mut g = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[38;2;0;255;0m\x1b[9m ");
        let (w, h) = (4usize, 8usize);
        let mut buf = vec![0u32; w * h];
        render(&g, &[], &mut MockFont, &mut buf, w, h, true);
        assert_eq!(buf[4 * w], 0x00FF00);
        assert_eq!(buf[6 * w], 0x000000, "strike doesn't also draw near the bottom");
    }

    #[test]
    fn colored_underline_follows_underline_color_not_fg() {
        let mut g = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[38;2;255;0;0m\x1b[4m\x1b[58;2;0;0;255m ");
        let (w, h) = (4usize, 8usize);
        let mut buf = vec![0u32; w * h];
        render(&g, &[], &mut MockFont, &mut buf, w, h, true);
        assert_eq!(buf[6 * w], 0x0000FF);
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
        render(&g, &[], &mut MockFont, &mut buf, w, h, true);
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
        let Some(bytes) = super::super::font::load_default_font(None) else {
            eprintln!("no system font; skipping real-font render");
            return;
        };
        let set = super::super::font::FontSet { regular: bytes, ..Default::default() };
        let mut fc = super::super::font::FontCache::new(set, 16.0, false).unwrap();
        let (cw, chh) = fc.cell_size();
        let mut g = Grid::new(3, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[48;2;0;0;0mabc");
        let (w, h) = (cw * 3, chh);
        let mut buf = vec![0u32; w * h];
        render(&g, &[], &mut fc, &mut buf, w, h, true);
        // Glyphs were drawn: at least some pixels differ from the black bg.
        assert!(buf.iter().any(|&px| px != 0x000000), "expected rasterized glyph pixels");
    }

    #[test]
    fn cursor_paints_in_cursor_color() {
        // Red fg on blue bg, a space (no glyph) so only the block shows.
        let mut g = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[38;2;255;0;0m\x1b[48;2;0;0;255m ");
        g.cursor = (0, 0);
        g.cursor_visible = true;
        // Default cursor color (white): the cell is a white block.
        let mut buf = vec![0u32; 4 * 8];
        render(&g, &[], &mut MockFont, &mut buf, 4, 8, true);
        assert!(
            buf.iter().all(|&px| px == 0xFFFFFF),
            "default cursor is a white block"
        );
        // A configured/OSC-12 cursor color paints the block in that color.
        g.cursor_color = 0x00FF00;
        let mut buf = vec![0u32; 4 * 8];
        render(&g, &[], &mut MockFont, &mut buf, 4, 8, true);
        assert!(
            buf.iter().all(|&px| px == 0x00FF00),
            "cursor block honors cursor_color"
        );
    }

    #[test]
    fn osc12_recolors_the_cursor_block() {
        // The child sets the cursor color at runtime; the renderer follows.
        let mut g = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[48;2;0;0;255m \x1b]12;#ff8800\x07");
        g.cursor = (0, 0);
        g.cursor_visible = true;
        let mut buf = vec![0u32; 4 * 8];
        render(&g, &[], &mut MockFont, &mut buf, 4, 8, true);
        assert!(
            buf.iter().all(|&px| px == 0xFF8800),
            "OSC 12 color reaches the block cursor"
        );
    }

    #[test]
    fn underline_cursor_overlays_only_the_bottom_row() {
        let mut g = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        // Blue cell bg, green cursor color, steady underline (DECSCUSR 4).
        p.advance(&mut g, b"\x1b[48;2;0;0;255m \x1b]12;#00ff00\x07\x1b[4 q");
        g.cursor = (0, 0); // pin the cursor to the cell we wrote (1×1 grid wraps)
        let (w, h) = (4, 8);
        let mut buf = vec![0u32; w * h];
        render(&g, &[], &mut MockFont, &mut buf, w, h, true);
        assert_eq!(buf[0], 0x0000FF, "top of the cell keeps the cell bg");
        assert_eq!(buf[(h - 1) * w], 0x00FF00, "bottom row is the underline cursor");
    }

    #[test]
    fn bar_cursor_overlays_only_the_left_column() {
        let mut g = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        // Blue cell bg, green cursor color, steady bar (DECSCUSR 6).
        p.advance(&mut g, b"\x1b[48;2;0;0;255m \x1b]12;#00ff00\x07\x1b[6 q");
        g.cursor = (0, 0); // pin the cursor to the cell we wrote (1×1 grid wraps)
        let (w, h) = (4, 8);
        let mut buf = vec![0u32; w * h];
        render(&g, &[], &mut MockFont, &mut buf, w, h, true);
        assert_eq!(buf[0], 0x00FF00, "left column is the bar cursor");
        assert_eq!(buf[w - 1], 0x0000FF, "right of the cell keeps the cell bg");
    }

    #[test]
    fn blinking_cursor_hidden_in_off_phase() {
        let mut g = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[48;2;0;0;255m \x1b[1 q"); // blinking block
        let mut buf = vec![0u32; 4 * 8];
        // cursor_on == false models the blink off-phase: nothing is drawn.
        render(&g, &[], &mut MockFont, &mut buf, 4, 8, false);
        assert!(buf.iter().all(|&px| px == 0x0000FF), "off-phase draws no cursor");
    }

    #[test]
    fn ime_preedit_overlays_reverse_video_at_cursor() {
        let mut g = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[38;2;0;255;0;48;2;0;0;255m "); // green-on-blue cell
        g.cursor = (0, 0);
        g.ime_preedit = "x".to_string();
        let mut buf = vec![0u32; 4 * 8];
        render(&g, &[], &mut MockFont, &mut buf, 4, 8, true);
        // Reverse video: the cell bg becomes its fg (green); the glyph its bg (blue).
        // MockFont draws a 2x2 block top-left, so (3,0) is the reversed bg.
        assert_eq!(buf[3], 0x00FF00, "preedit cell bg is the reversed fg");
        assert_eq!(buf[0], 0x0000FF, "preedit glyph is the reversed bg");
    }

    #[test]
    fn search_match_cell_is_highlighted() {
        let mut g = Grid::new(5, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"xyz");
        assert_eq!(g.search_with("y", false), 1); // 'y' at col 1, the active (current) match
        let (w, h) = (5 * 4, 8); // 5 cols * 4px cell width
        let mut buf = vec![0u32; w * h];
        render(&g, &[], &mut MockFont, &mut buf, w, h, true);
        // Cell col 1 spans x in [4, 8); a corner with no glyph is the active-match bg.
        assert_eq!(buf[7 * w + 7], 0xFF7A1A, "active match cell is orange");
        // col 0 (no match) keeps the default background.
        assert_ne!(buf[7 * w], 0xFF7A1A);
    }

    #[test]
    fn draw_grid_honors_cell_offset() {
        let mut g = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[48;2;0;0;255m "); // blue-bg cell
        let (cw, ch) = (4usize, 8usize);
        let (w, h) = (3 * cw, 2 * ch);
        let mut buf = vec![0u32; w * h];
        // A split pane draws its grid at a cell offset; here (1, 1).
        draw_grid(&mut buf, w, h, &g, 1, 1, false, false, &mut MockFont);
        assert_eq!(buf[ch * w + cw], 0x0000FF, "the cell is painted at the offset");
        assert_eq!(buf[0], 0, "the origin is left untouched (a divider gap)");
    }

    #[test]
    fn image_pixels_overlay_the_cells() {
        let mut g = Grid::new(4, 2);
        g.render_image(2, 2, &[Some(0xFF0000); 4]); // a 2x2 red image at the cursor
        let (cw, ch) = (4usize, 8usize);
        let (w, h) = (4 * cw, 2 * ch);
        let mut buf = vec![0u32; w * h];
        draw_grid(&mut buf, w, h, &g, 0, 0, false, false, &mut MockFont);
        // The image composites as real pixels over its reserved half-block cell.
        assert_eq!(buf[0], 0xFF0000, "image pixel composited at the origin");
    }

    #[test]
    fn hidden_cursor_is_not_drawn() {
        let mut g = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[38;2;255;0;0m\x1b[48;2;0;0;255m ");
        g.cursor = (0, 0);
        g.cursor_visible = false;
        let mut buf = vec![0u32; 4 * 8];
        render(&g, &[], &mut MockFont, &mut buf, 4, 8, true);
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
        render(&g, &[], &mut MockFont, &mut buf, 4, 8, true);
        assert!(buf.iter().all(|&px| px == 0x0000FF), "scrolled back: no cursor");
    }

    #[test]
    fn scrolled_view_composites_history() {
        // A blue-bg line scrolls into history above a green live top row.
        let mut g = Grid::new(1, 2);
        let mut p = AnsiParser::new();
        p.advance(
            &mut g,
            b"\x1b[48;2;0;0;255m \r\n\x1b[48;2;0;128;0m \r\n\x1b[48;2;255;0;0m ",
        );
        g.cursor_visible = false;
        assert_eq!(g.scrollback.len(), 1, "blue line scrolled into history");
        // Cell is 4x8 (MockFont); two rows -> a 4x16 buffer. buf[0] is the top row.
        let mut buf = vec![0u32; 4 * 16];
        render(&g, &[], &mut MockFont, &mut buf, 4, 16, true);
        assert_eq!(buf[0], 0x008000, "live view top row: green");
        // Scroll up one line: the top row now shows the blue history line.
        assert!(g.scroll_view_up(1));
        let mut buf = vec![0u32; 4 * 16];
        render(&g, &[], &mut MockFont, &mut buf, 4, 16, true);
        assert_eq!(buf[0], 0x0000FF, "scrolled back: blue history in the top row");
        assert_eq!(buf[8 * 4], 0x008000, "row below shows the live top row (green)");
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
        render(&g, &[], &mut MockFont, &mut buf, w, h, true);
        // Col 0 inverted (red block), col 1 untouched (blue bg).
        assert_eq!(buf[0], 0xFF0000, "selected cell inverted");
        assert_eq!(buf[4], 0x0000FF, "unselected cell unchanged");
    }

    #[test]
    fn ligatures_collapse_a_run_into_one_glyph() {
        // The test font ligates `f i` -> `fi`; render "fi" with shaping on vs off
        // and compare the second cell. (Generated by extra/gen_ligtest_font.py.)
        let render_fi = |ligatures: bool| -> (Vec<u32>, usize, usize) {
            let set = super::super::font::FontSet {
                regular: include_bytes!("testdata/ligtest.ttf").to_vec(),
                ..Default::default()
            };
            let mut fc = super::super::font::FontCache::new(set, 40.0, ligatures).unwrap();
            let (cw, chh) = fc.cell_size();
            let mut g = Grid::new(2, 1);
            let mut p = AnsiParser::new();
            p.advance(&mut g, b"fi");
            let (w, h) = (cw * 2, chh);
            let mut buf = vec![0u32; w * h];
            draw_grid(&mut buf, w, h, &g, 0, 0, false, false, &mut fc);
            (buf, cw, chh)
        };
        // Count glyph (non-background) pixels in cell column `c`.
        let nonbg = |buf: &[u32], cw: usize, chh: usize, c: usize| -> usize {
            let (w, bg) = (cw * 2, buf[0]); // (0,0) is above the glyph -> background
            (0..chh)
                .flat_map(|y| (c * cw..(c + 1) * cw).map(move |x| y * w + x))
                .filter(|&i| buf[i] != bg)
                .count()
        };
        let (on, cw, chh) = render_fi(true);
        let (off, _, _) = render_fi(false);
        assert!(nonbg(&on, cw, chh, 0) > 0, "first cell renders (ligatures on)");
        assert!(nonbg(&off, cw, chh, 0) > 0, "first cell renders (ligatures off)");
        // With ligatures the run collapses to one glyph, leaving the 2nd cell empty.
        assert_eq!(nonbg(&on, cw, chh, 1), 0, "ligature consumes the second cell");
        // Without ligatures, the second cell renders its own glyph.
        assert!(nonbg(&off, cw, chh, 1) > 0, "no ligature: second cell renders");
    }
}
