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
use super::render::{SEARCH_BG, SEARCH_CUR_BG, SEARCH_FG};

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
        // The test harness draws the bar flush (no band) — band geometry is
        // exercised through the real `CpuRenderer::render` path.
        draw_chrome(buf, width, height, chrome, font, cw, ch, 0, 0);
    }
    let row0 = if chrome.is_empty() { 0 } else { 1 };
    draw_grid(buf, width, height, grid, 0, row0, 0, 0, true, cursor_on, None, font);
}

/// Paint the window's chrome bar (tabs + caption buttons): a thin
/// strip-colored band at pixel row 0 (`inset` tall, `bar_bg`), then the
/// chrome cells pushed down below it — so a tab's top edge reads as distinct
/// from the window's instead of running flush into the frame.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_chrome(
    buf: &mut [u32],
    width: usize,
    height: usize,
    chrome: &[Cell],
    font: &mut dyn GlyphSource,
    cw: usize,
    ch: usize,
    inset: usize,
    bar_bg: u32,
) {
    for y in 0..inset.min(height) {
        let base = y * width;
        for x in 0..width {
            buf[base + x] = bar_bg;
        }
    }
    draw_bar(buf, width, height, chrome, font, cw, ch, inset);
}

/// Paint one pre-laid row of bar cells (chrome tabs or the bottom status
/// ribbon) at pixel row `y0`: each cell's background, any underline
/// decoration, then the glyphs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_bar(
    buf: &mut [u32],
    width: usize,
    height: usize,
    cells: &[Cell],
    font: &mut dyn GlyphSource,
    cw: usize,
    ch: usize,
    y0: usize,
) {
    let baseline = font.baseline();
    for (col, cell) in cells.iter().enumerate() {
        let x0 = col * cw;
        for y in y0..(y0 + ch).min(height) {
            let base = y * width;
            for x in x0..(x0 + cw).min(width) {
                buf[base + x] = cell.bg;
            }
        }
        // The active tab's accent line (and any other decorated chrome cell)
        // rides the ordinary underline machinery; spaces included, so the
        // line runs the tab's full width.
        if cell.flags & ATTR_UNDERLINE != 0 {
            let color = if cell.flags & ATTR_UNDERLINE_COLOR != 0 { cell.underline_color } else { cell.fg };
            draw_underline(buf, width, height, x0, y0, cw, ch, color, UnderlineStyle::from_attrs(cell.flags));
        }
    }
    for (col, cell) in cells.iter().enumerate() {
        if cell.flags & WIDE_TRAILER != 0 || cell.ch == ' ' {
            continue;
        }
        let style = Style::new(cell.flags & ATTR_BOLD != 0, cell.flags & ATTR_ITALIC != 0);
        let glyph = font.glyph(cell.ch, style);
        if glyph.width == 0 {
            continue;
        }
        let pen_x = (col * cw) as i32 + glyph.left;
        let pen_y = baseline + glyph.top + y0 as i32;
        blit(buf, width, height, &glyph, pen_x, pen_y, cell.fg);
    }
}

/// Composite one grid's visible cells into `buf` at cell offset `(col0, row0)`
/// plus a pixel offset `(ox, oy)` (the window padding band), extent
/// `grid.cols × grid.rows`. The cursor (block / bar / underline) and IME
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
    ox: usize,
    oy: usize,
    focused: bool,
    cursor_on: bool,
    hover_link: Option<(usize, usize, usize)>,
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
    let inverted = |col: usize, row: usize| grid.is_selected(col, row);
    let search_hl = |col: usize, row: usize| grid.search_highlight(col, row);
    let status = grid.status_row();
    let last_row = grid.rows.saturating_sub(1);
    // Bidi (implicit mode): per-row visual/logical permutations. `None` for
    // pure-LTR rows, status rows, and when `bidi` is off — the common case,
    // costing one flag check. Cell *state* stays keyed by logical position;
    // only the draw X position (and glyph mirroring) go through the map.
    let bidi: Vec<Option<crate::core::BidiRow>> = (0..grid.rows)
        .map(|r| {
            if status.is_some() && r == last_row { None } else { grid.bidi_row(r) }
        })
        .collect();
    let vis = |col: usize, row: usize| -> usize {
        bidi[row].as_ref().and_then(|b| b.log2vis.get(col)).map_or(col, |&v| v as usize)
    };

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
        let (x0, y0) = (ox + (col0 + vis(col, row)) * cw, oy + (row0 + row) * ch);
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
                // Synthesized glyphs (box drawing etc.) bypass shaping so a
                // font's GSUB can never substitute them away from the exact
                // cell-geometry bitmaps; Kitty placeholders draw image
                // content, never a glyph.
                && !crate::gui::boxdraw::is_synthesized(cell.ch)
                && cell.ch != '\u{10EEEE}'
                // A reordered (bidi) row draws cell-by-cell; shaping a run
                // across visually non-adjacent cells would garble it.
                && bidi[row].is_none()
        };
        let mut col = 0;
        while col < grid.cols {
            let cell = cell_at(col);
            if !eligible(col, &cell) {
                // Kitty Unicode placeholders draw image content (the pass
                // below), never a font glyph.
                let blank = cell.flags & WIDE_TRAILER != 0
                    || (cell.ch == ' ' && cell.cluster == 0)
                    || cell.ch == '\u{10EEEE}';
                plan[row * grid.cols + col] = (!blank).then(|| {
                    let style = Style::new(cell.flags & ATTR_BOLD != 0, cell.flags & ATTR_ITALIC != 0);
                    // Arabic contextual form first (phase 3), then rule L4
                    // mirroring for RTL-run chars (Arabic never mirrors, so
                    // the order can't double-substitute).
                    let ch = match &bidi[row] {
                        Some(b) => {
                            let ch = b
                                .shaped
                                .as_ref()
                                .and_then(|s| s[col])
                                .unwrap_or(cell.ch);
                            if b.rtl[col] {
                                crate::core::bidi_mirrored(ch).unwrap_or(ch)
                            } else {
                                ch
                            }
                        }
                        None => cell.ch,
                    };
                    font.glyph(ch, style)
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
        let pen_x = (ox + (col0 + vis(col, row)) * cw) as i32 + glyph.left;
        let pen_y = (oy + (row0 + row) * ch) as i32 + baseline + glyph.top;
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
        // Ctrl-hovered hyperlink (G22): forces a plain underline over its
        // column span regardless of the cell's own SGR underline attribute —
        // the click affordance, not a text style.
        let hovered = !on_status
            && hover_link.is_some_and(|(hr, s, e)| row == hr && col >= s && col <= e);
        let underline = cell.flags & ATTR_UNDERLINE != 0 || hovered;
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
        let (x0, y0) = (ox + (col0 + vis(col, row)) * cw, oy + (row0 + row) * ch);
        if underline {
            let color = if !swapped && !hovered && cell.flags & ATTR_UNDERLINE_COLOR != 0 {
                cell.underline_color
            } else {
                base_fg
            };
            let style =
                if hovered { UnderlineStyle::Straight } else { UnderlineStyle::from_attrs(cell.flags) };
            draw_underline(buf, width, height, x0, y0, cw, ch, color, style);
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
        let x0 = ox as isize + ((col0 + im.col) * cw) as isize;
        let y0 = oy as isize + (row0 as isize + grid.image_top_row(im)) * ch as isize;
        let pane_top = (oy + row0 * ch) as isize;
        let pane_bottom = ((oy + (row0 + grid.rows) * ch).min(height)) as isize;
        let pane_right = ((ox + (col0 + grid.cols) * cw).min(width)) as isize;
        // An animated image (inline GIF) draws its backing animation's current
        // frame; the stored snapshot is the fallback if the store evicted it.
        let pixels: &[Option<u32>] = im
            .anim
            .and_then(|id| grid.kitty_frame(id))
            .filter(|&(w, h, _)| w == im.pw && h == im.ph)
            .map(|(_, _, px)| px)
            .unwrap_or(&im.pixels);
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
                if let Some(c) = pixels[sy * im.pw + sx] {
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
        let (x0, y0) = (ox + (col0 + vis(ccol, crow)) * cw, oy + (row0 + crow) * ch);
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

    // Kitty Unicode placeholders (`U+10EEEE`): each cell paints its slice
    // of a virtually placed image — the placement mechanism that survives
    // multiplexers. Row/column come from the cell's diacritics; cells that
    // omit them inherit from the left/top neighbor (spec inference).
    if let Some(ph) = grid.placeholder_map() {
        for (i, entry) in ph.iter().enumerate() {
            let Some((id, prow, pcol)) = *entry else { continue };
            let Some((iw, ihh, pixels)) = grid.kitty_frame(id) else { continue };
            let Some((pcols, prows)) = grid.placeholder_grid(id, cw, ch) else { continue };
            let (prow, pcol) = (prow as usize, pcol as usize);
            if prow >= prows || pcol >= pcols {
                continue; // an index past the placement grid shows nothing
            }
            let (col, row) = (i % grid.cols, i / grid.cols);
            let (sx0, sx1) = (pcol * iw / pcols, ((pcol + 1) * iw / pcols).max(pcol * iw / pcols + 1));
            let (sy0, sy1) = (prow * ihh / prows, ((prow + 1) * ihh / prows).max(prow * ihh / prows + 1));
            let (x0, y0) = (ox + (col0 + col) * cw, oy + (row0 + row) * ch);
            for dy in 0..ch {
                let py = y0 + dy;
                if py >= height {
                    break;
                }
                let sy = (sy0 + dy * (sy1 - sy0) / ch).min(ihh - 1);
                for dx in 0..cw {
                    let px = x0 + dx;
                    if px >= width {
                        break;
                    }
                    let sx = (sx0 + dx * (sx1 - sx0) / cw).min(iw - 1);
                    if let Some(rgb) = pixels[sy * iw + sx] {
                        buf[py * width + px] = rgb;
                    }
                }
            }
        }
    }

    // Scrollbar overlay (auto-hides at the live bottom): a thin bar hugging
    // the pane's right edge, thumb sized/positioned from the scroll state.
    if let Some((first, len, color)) = grid.scrollbar() {
        let bar_w = (cw / 3).max(2);
        let x1 = (ox + (col0 + grid.cols) * cw).min(width);
        let x0 = x1.saturating_sub(bar_w);
        let y0 = oy + (row0 + first) * ch;
        let y1 = (oy + (row0 + first + len) * ch).min(height);
        for y in y0..y1 {
            for x in x0..x1 {
                buf[y * width + x] = color;
            }
        }
    }

    // IME preedit (composition): reverse-video glyphs at the cursor.
    if focused && !grid.ime_preedit.is_empty() && grid.view_offset == 0 {
        let crow = grid.cursor.1;
        let mut col = grid.cursor.0;
        let y0 = oy + (row0 + crow) * ch;
        for pch in grid.ime_preedit.chars() {
            let w = char_width(pch).max(1);
            if col + w > grid.cols {
                break;
            }
            let base = grid.viewport_cell(col, crow);
            let (fg, bg) = (base.bg, base.fg);
            let x0 = ox + (col0 + col) * cw;
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
            let gi = gy * glyph.width + gx;
            let idx = row + px as usize;
            // Color glyphs (emoji bitmap strikes) carry their own pixels and
            // ignore the pen color; ordinary glyphs tint coverage with `fg`.
            if let Some(color) = &glyph.color {
                let argb = color[gi];
                let a = (argb >> 24) as u8;
                if a != 0 {
                    buf[idx] = blend(buf[idx], argb & 0x00FF_FFFF, a);
                }
                continue;
            }
            let a = glyph.coverage[gi];
            if a == 0 {
                continue;
            }
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


/// Blend the cursor-trail ghosts (G36) over the finished pane: each entry is
/// a cell position plus an alpha, drawn as a cursor-colored block mixed into
/// what's already there.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_trail(
    buf: &mut [u32],
    width: usize,
    height: usize,
    grid: &crate::core::Grid,
    col0: usize,
    row0: usize,
    ox: usize,
    oy: usize,
    trail: &[(usize, usize, f32)],
    font: &mut dyn GlyphSource,
) {
    if trail.is_empty() {
        return;
    }
    let (cw, ch) = font.cell_size();
    let color = grid.cursor_color;
    for &(col, row, alpha) in trail {
        if col >= grid.cols || row >= grid.rows {
            continue;
        }
        let a = alpha.clamp(0.0, 1.0);
        let (x0, y0) = (ox + (col0 + col) * cw, oy + (row0 + row) * ch);
        for y in y0..(y0 + ch).min(height) {
            for x in x0..(x0 + cw).min(width) {
                let px = &mut buf[y * width + x];
                *px = blend(*px, color, (a * 255.0) as u8);
            }
        }
    }
}


/// The ghost cells of a cursor trail `from -> to` at animation progress `t`
/// (`0.0` = just moved, `1.0` = done): up to eight cells sampled along the
/// straight line between the two positions, alpha graded toward the head and
/// fading with `t`. Shared by both renderers so the effect can't drift.
pub(crate) fn trail_ghosts(
    from: (usize, usize),
    to: (usize, usize),
    t: f32,
) -> Vec<(usize, usize, f32)> {
    if t >= 1.0 || from == to {
        return Vec::new();
    }
    let (fx, fy) = (from.0 as f32, from.1 as f32);
    let (tx, ty) = (to.0 as f32, to.1 as f32);
    let dist = ((tx - fx).powi(2) + (ty - fy).powi(2)).sqrt();
    let n = (dist.ceil() as usize).clamp(1, 8);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        // Sample from the old position toward (but excluding) the live cursor.
        let s = i as f32 / n as f32;
        let (x, y) = (fx + (tx - fx) * s, fy + (ty - fy) * s);
        let cell = (x.round() as usize, y.round() as usize);
        if cell == to {
            continue;
        }
        // Brighter near the head, all of it fading out over the animation.
        let alpha = (0.15 + 0.45 * s) * (1.0 - t);
        match out.last_mut() {
            Some((lx, ly, la)) if (*lx, *ly) == cell => *la = alpha.max(*la),
            _ => out.push((cell.0, cell.1, alpha)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::font::Glyph;
    use super::*;
    use crate::core::AnsiParser;
    use std::rc::Rc;

    /// Ghost math: samples run old->new, brighten toward the head, fade with
    /// time, never include the live cursor cell, and expire at t >= 1.
    #[test]
    fn trail_ghosts_sample_fade_and_expire() {
        let g = trail_ghosts((0, 0), (6, 0), 0.0);
        assert!(!g.is_empty() && g.len() <= 8);
        assert!(g.iter().all(|&(c, r, _)| r == 0 && c < 6), "between, excluding the head: {g:?}");
        assert_eq!(g[0].0, 0, "starts at the old position");
        assert!(g.last().unwrap().2 > g[0].2, "alpha grades toward the head");
        // Fading: same hop later in the animation is uniformly dimmer.
        let later = trail_ghosts((0, 0), (6, 0), 0.5);
        assert!(later[0].2 < g[0].2);
        assert!(trail_ghosts((0, 0), (6, 0), 1.0).is_empty(), "expired");
        assert!(trail_ghosts((3, 3), (3, 3), 0.0).is_empty(), "no hop, no trail");
        // Adjacent-cell hop still leaves one ghost at the old position.
        let one = trail_ghosts((4, 2), (5, 2), 0.0);
        assert_eq!(one.len(), 1);
        assert_eq!((one[0].0, one[0].1), (4, 2));
    }

    /// draw_trail blends the cursor color into the buffer at ghost cells and
    /// leaves everything else untouched.
    #[test]
    fn draw_trail_blends_cursor_color() {
        let mut grid = Grid::new(4, 2);
        grid.cursor_color = 0xFF0000;
        let mut font = MockFont;
        let (cw, ch) = font.cell_size();
        let (w, h) = (4 * cw, 2 * ch);
        let mut buf = vec![0u32; w * h];
        draw_trail(&mut buf, w, h, &grid, 0, 0, 0, 0, &[(1, 0, 0.5)], &mut font);
        let inside = buf[(ch / 2) * w + cw + cw / 2];
        assert!((inside >> 16) & 0xFF > 0x40, "red blended in: {inside:06x}");
        assert_eq!(buf[0], 0, "cells outside the ghost untouched");
        // Out-of-range ghosts are ignored, not panicking.
        draw_trail(&mut buf, w, h, &grid, 0, 0, 0, 0, &[(99, 99, 1.0)], &mut font);
    }

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
                return Rc::new(Glyph { width: 0, height: 0, left: 0, top: 0, coverage: Vec::new(), color: None });
            }
            // top = -baseline places the bitmap's top row at the cell's top.
            Rc::new(Glyph { width: 2, height: 2, left: 0, top: -6, coverage: vec![255; 4], color: None })
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

    #[test]
    fn hovered_link_span_draws_a_stripe_even_without_sgr_underline() {
        let mut g = Grid::new(3, 1);
        let mut p = AnsiParser::new();
        // Plain red text, no SGR underline (4) anywhere.
        p.advance(&mut g, b"\x1b[38;2;255;0;0mABC");
        let (w, h) = (4usize * 3, 8usize);
        let mut buf = vec![0u32; w * h];
        // Hover span covers cols 0..=1 ('A','B') on row 0.
        draw_grid(&mut buf, w, h, &g, 0, 0, 0, 0, true, false, Some((0, 0, 1)), &mut MockFont);
        assert_eq!(buf[6 * w], 0xFF0000, "col 0 underlined even with no ATTR_UNDERLINE");
        assert_eq!(buf[6 * w + 4], 0xFF0000, "col 1 underlined too");
        assert_eq!(buf[6 * w + 8], 0x000000, "col 2 is outside the hover span");
    }

    #[test]
    fn hovered_link_underline_always_uses_fg_not_underline_color() {
        let mut g = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        // Already has SGR underline + a colored underline (58) set — hover
        // must still win with plain fg, since it's a click affordance, not
        // a text style the app chose.
        p.advance(&mut g, b"\x1b[38;2;255;0;0m\x1b[4m\x1b[58;2;0;0;255m ");
        let (w, h) = (4usize, 8usize);
        let mut buf = vec![0u32; w * h];
        draw_grid(&mut buf, w, h, &g, 0, 0, 0, 0, true, false, Some((0, 0, 0)), &mut MockFont);
        assert_eq!(buf[6 * w], 0xFF0000);
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
    fn draw_bar_paints_cells_at_the_given_pixel_row() {
        // 2 cols × 3 cell rows (4×8 MockFont cells); the bar sits flush at the
        // bottom, pixel row 16.
        let (w, h) = (8usize, 24usize);
        let mut buf = vec![0u32; w * h];
        let mut cells = vec![Cell::blank(); 2];
        for c in &mut cells {
            c.fg = 0xABCDEF;
            c.bg = 0x123456;
        }
        cells[0].ch = 'x';
        draw_bar(&mut buf, w, h, &cells, &mut MockFont, 4, 8, 16);
        // MockFont draws a 2x2 block at the cell's top-left: (0, 16) is glyph.
        assert_eq!(buf[16 * w], 0xABCDEF, "glyph pixel at the bar row");
        assert_eq!(buf[16 * w + 3], 0x123456, "bar cell background");
        assert_eq!(buf[0], 0, "pixels above the bar untouched");
        assert_eq!(buf[15 * w], 0, "row just above the bar untouched");
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
        draw_grid(&mut buf, w, h, &g, 1, 1, 0, 0, false, false, None, &mut MockFont);
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
        draw_grid(&mut buf, w, h, &g, 0, 0, 0, 0, false, false, None, &mut MockFont);
        // The image composites as real pixels over its reserved half-block cell.
        assert_eq!(buf[0], 0xFF0000, "image pixel composited at the origin");
    }

    #[test]
    fn kitty_placeholders_paint_the_image_slice() {
        let mut g = Grid::new(4, 2);
        let mut p = crate::core::AnsiParser::new();
        // A 2x1 image (red|green) stored + virtually placed on a 2x1 grid.
        p.advance(&mut g, b"\x1b_Gf=32,s=2,v=1,a=T,U=1,i=5,c=2,r=1;/wAA//8AAP8=\x1b\\");
        // Wait — payload is red|red; use two colors: ff0000ff 00ff00ff.
        p.advance(&mut g, b"\x1b_Gf=32,s=2,v=1,a=T,U=1,i=5,c=2,r=1;/wAA/wD/AP8=\x1b\\");
        // Two placeholder cells, row 0 cols 0/1 (diacritics 0 and 1), id in fg.
        p.advance(&mut g, b"\x1b[38;2;0;0;5m");
        p.advance(&mut g, "\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}".as_bytes());
        let (cw, ch) = (4usize, 8usize);
        let (w, h) = (4 * cw, 2 * ch);
        let mut buf = vec![0u32; w * h];
        draw_grid(&mut buf, w, h, &g, 0, 0, 0, 0, false, false, None, &mut MockFont);
        assert_eq!(buf[0], 0xFF0000, "cell (0,0) shows the red half");
        assert_eq!(buf[cw], 0x00FF00, "cell (1,0) shows the green half");
        // Inference: a third placeholder with no diacritics continues the
        // row; column 2 is past the 2-wide placement, so it paints nothing.
        p.advance(&mut g, "\u{10EEEE}".as_bytes());
        let mut buf2 = vec![0u32; w * h];
        draw_grid(&mut buf2, w, h, &g, 0, 0, 0, 0, false, false, None, &mut MockFont);
        assert_eq!(buf2[2 * cw], 0, "index past the placement grid stays empty");
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
            draw_grid(&mut buf, w, h, &g, 0, 0, 0, 0, false, false, None, &mut fc);
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

    #[test]
    fn blit_color_glyph_uses_its_own_pixels_not_fg() {
        let mut buf = vec![0u32; 4 * 4];
        let glyph = Glyph {
            width: 2,
            height: 2,
            left: 0,
            top: 0,
            coverage: vec![255, 0, 255, 128],
            // Opaque red, transparent, opaque green, half-alpha blue.
            color: Some(vec![0xFFFF0000, 0x00000000, 0xFF00FF00, 0x800000FF]),
        };
        blit(&mut buf, 4, 4, &glyph, 0, 0, 0x123456 /* fg must be ignored */);
        assert_eq!(buf[0], 0xFF0000, "opaque red pixel");
        assert_eq!(buf[1], 0x000000, "transparent pixel leaves bg");
        assert_eq!(buf[4], 0x00FF00, "opaque green pixel");
        // Half-alpha blue over black: roughly half-intensity blue.
        let b = buf[5] & 0xFF;
        assert!((100..=160).contains(&b), "{b}");
    }
}
