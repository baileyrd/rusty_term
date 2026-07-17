//! Built-in box-drawing / block-element / braille / Powerline glyph
//! synthesis (G25): these glyphs are drawn procedurally at exact cell
//! geometry instead of through the font, so TUI borders, statuslines, and
//! progress blocks join seamlessly regardless of the font's own metrics —
//! the same approach kitty, Ghostty, and WezTerm take. Always on: a font's
//! own box glyphs at a mismatched size are never preferable to exact ones.
//!
//! Covered: U+2500–257F box drawing (arms via a table generated from the
//! Unicode names, plus dashes, rounded arcs, and diagonals), U+2580–259F
//! block elements (fractional blocks, shades, quadrants), U+2800–28FF
//! braille, and the Powerline separators U+E0B0–U+E0BF (the core triangles
//! plus the extended semicircles and slants), so Powerline/starship prompts
//! render seamlessly with no Nerd Font installed.

use super::font::Glyph;

/// Whether `ch` is synthesized here (and must therefore bypass font lookup
/// and GSUB shaping runs).
pub(crate) fn is_synthesized(ch: char) -> bool {
    matches!(ch as u32, 0x2500..=0x259F | 0x2800..=0x28FF | 0xE0B0..=0xE0BF)
}

/// Synthesize `ch` at a `cw x ch_px` cell with the given `baseline`
/// (pixels from cell top to the text baseline), or `None` for characters
/// this module doesn't cover.
pub(crate) fn synthesize(ch: char, cw: usize, ch_px: usize, baseline: i32) -> Option<Glyph> {
    if cw == 0 || ch_px == 0 || !is_synthesized(ch) {
        return None;
    }
    let mut c = Canvas::new(cw, ch_px);
    let cp = ch as u32;
    match cp {
        0x2500..=0x257F => draw_box(&mut c, cp)?,
        0x2580..=0x259F => draw_block(&mut c, cp),
        0x2800..=0x28FF => draw_braille(&mut c, cp - 0x2800),
        0xE0B0..=0xE0B3 => draw_powerline(&mut c, cp),
        0xE0B4..=0xE0BF => draw_powerline_ext(&mut c, cp),
        _ => return None,
    }
    Some(c.into_glyph(baseline))
}

/// A cell-sized coverage canvas.
struct Canvas {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        Canvas {
            w,
            h,
            px: vec![0; w * h],
        }
    }

    /// Light stroke thickness, scaled to the cell.
    fn light(&self) -> usize {
        (self.w.min(self.h) / 8).max(1)
    }

    fn heavy(&self) -> usize {
        self.light() * 2
    }

    fn set(&mut self, x: usize, y: usize) {
        if x < self.w && y < self.h {
            self.px[y * self.w + x] = 255;
        }
    }

    /// Fill the half-open pixel rect, clipped.
    fn rect(&mut self, x0: usize, y0: usize, x1: usize, y1: usize) {
        for y in y0..y1.min(self.h) {
            for x in x0..x1.min(self.w) {
                self.px[y * self.w + x] = 255;
            }
        }
    }

    fn into_glyph(self, baseline: i32) -> Glyph {
        Glyph {
            width: self.w,
            height: self.h,
            left: 0,
            top: -baseline,
            coverage: self.px,
            color: None,
        }
    }
}

const ARMS: &[(u32, [u8; 4])] = &[
    (0x2500, [0, 0, 1, 1]), // LIGHT HORIZONTAL
    (0x2501, [0, 0, 2, 2]), // HEAVY HORIZONTAL
    (0x2502, [1, 1, 0, 0]), // LIGHT VERTICAL
    (0x2503, [2, 2, 0, 0]), // HEAVY VERTICAL
    (0x250C, [0, 1, 0, 1]), // LIGHT DOWN AND RIGHT
    (0x250D, [0, 1, 0, 2]), // DOWN LIGHT AND RIGHT HEAVY
    (0x250E, [0, 2, 0, 1]), // DOWN HEAVY AND RIGHT LIGHT
    (0x250F, [0, 2, 0, 1]), // HEAVY DOWN AND RIGHT
    (0x2510, [0, 1, 1, 0]), // LIGHT DOWN AND LEFT
    (0x2511, [0, 1, 2, 0]), // DOWN LIGHT AND LEFT HEAVY
    (0x2512, [0, 2, 1, 0]), // DOWN HEAVY AND LEFT LIGHT
    (0x2513, [0, 2, 1, 0]), // HEAVY DOWN AND LEFT
    (0x2514, [1, 0, 0, 1]), // LIGHT UP AND RIGHT
    (0x2515, [1, 0, 0, 2]), // UP LIGHT AND RIGHT HEAVY
    (0x2516, [2, 0, 0, 1]), // UP HEAVY AND RIGHT LIGHT
    (0x2517, [2, 0, 0, 1]), // HEAVY UP AND RIGHT
    (0x2518, [1, 0, 1, 0]), // LIGHT UP AND LEFT
    (0x2519, [1, 0, 2, 0]), // UP LIGHT AND LEFT HEAVY
    (0x251A, [2, 0, 1, 0]), // UP HEAVY AND LEFT LIGHT
    (0x251B, [2, 0, 1, 0]), // HEAVY UP AND LEFT
    (0x251C, [1, 1, 0, 1]), // LIGHT VERTICAL AND RIGHT
    (0x251D, [1, 1, 0, 2]), // VERTICAL LIGHT AND RIGHT HEAVY
    (0x251E, [2, 1, 0, 1]), // UP HEAVY AND RIGHT DOWN LIGHT
    (0x251F, [1, 2, 0, 1]), // DOWN HEAVY AND RIGHT UP LIGHT
    (0x2520, [2, 2, 0, 1]), // VERTICAL HEAVY AND RIGHT LIGHT
    (0x2521, [2, 1, 0, 2]), // DOWN LIGHT AND RIGHT UP HEAVY
    (0x2522, [1, 2, 0, 2]), // UP LIGHT AND RIGHT DOWN HEAVY
    (0x2523, [2, 2, 0, 1]), // HEAVY VERTICAL AND RIGHT
    (0x2524, [1, 1, 1, 0]), // LIGHT VERTICAL AND LEFT
    (0x2525, [1, 1, 2, 0]), // VERTICAL LIGHT AND LEFT HEAVY
    (0x2526, [2, 1, 1, 0]), // UP HEAVY AND LEFT DOWN LIGHT
    (0x2527, [1, 2, 1, 0]), // DOWN HEAVY AND LEFT UP LIGHT
    (0x2528, [2, 2, 1, 0]), // VERTICAL HEAVY AND LEFT LIGHT
    (0x2529, [2, 1, 2, 0]), // DOWN LIGHT AND LEFT UP HEAVY
    (0x252A, [1, 2, 2, 0]), // UP LIGHT AND LEFT DOWN HEAVY
    (0x252B, [2, 2, 1, 0]), // HEAVY VERTICAL AND LEFT
    (0x252C, [0, 1, 1, 1]), // LIGHT DOWN AND HORIZONTAL
    (0x252D, [0, 1, 2, 1]), // LEFT HEAVY AND RIGHT DOWN LIGHT
    (0x252E, [0, 1, 1, 2]), // RIGHT HEAVY AND LEFT DOWN LIGHT
    (0x252F, [0, 1, 2, 2]), // DOWN LIGHT AND HORIZONTAL HEAVY
    (0x2530, [0, 2, 1, 1]), // DOWN HEAVY AND HORIZONTAL LIGHT
    (0x2531, [0, 2, 2, 1]), // RIGHT LIGHT AND LEFT DOWN HEAVY
    (0x2532, [0, 2, 1, 2]), // LEFT LIGHT AND RIGHT DOWN HEAVY
    (0x2533, [0, 2, 1, 1]), // HEAVY DOWN AND HORIZONTAL
    (0x2534, [1, 0, 1, 1]), // LIGHT UP AND HORIZONTAL
    (0x2535, [1, 0, 2, 1]), // LEFT HEAVY AND RIGHT UP LIGHT
    (0x2536, [1, 0, 1, 2]), // RIGHT HEAVY AND LEFT UP LIGHT
    (0x2537, [1, 0, 2, 2]), // UP LIGHT AND HORIZONTAL HEAVY
    (0x2538, [2, 0, 1, 1]), // UP HEAVY AND HORIZONTAL LIGHT
    (0x2539, [2, 0, 2, 1]), // RIGHT LIGHT AND LEFT UP HEAVY
    (0x253A, [2, 0, 1, 2]), // LEFT LIGHT AND RIGHT UP HEAVY
    (0x253B, [2, 0, 1, 1]), // HEAVY UP AND HORIZONTAL
    (0x253C, [1, 1, 1, 1]), // LIGHT VERTICAL AND HORIZONTAL
    (0x253D, [1, 1, 2, 1]), // LEFT HEAVY AND RIGHT VERTICAL LIGHT
    (0x253E, [1, 1, 1, 2]), // RIGHT HEAVY AND LEFT VERTICAL LIGHT
    (0x253F, [1, 1, 2, 2]), // VERTICAL LIGHT AND HORIZONTAL HEAVY
    (0x2540, [2, 1, 1, 1]), // UP HEAVY AND DOWN HORIZONTAL LIGHT
    (0x2541, [1, 2, 1, 1]), // DOWN HEAVY AND UP HORIZONTAL LIGHT
    (0x2542, [2, 2, 1, 1]), // VERTICAL HEAVY AND HORIZONTAL LIGHT
    (0x2543, [2, 1, 2, 1]), // LEFT UP HEAVY AND RIGHT DOWN LIGHT
    (0x2544, [2, 1, 1, 2]), // RIGHT UP HEAVY AND LEFT DOWN LIGHT
    (0x2545, [1, 2, 2, 1]), // LEFT DOWN HEAVY AND RIGHT UP LIGHT
    (0x2546, [1, 2, 1, 2]), // RIGHT DOWN HEAVY AND LEFT UP LIGHT
    (0x2547, [2, 1, 2, 2]), // DOWN LIGHT AND UP HORIZONTAL HEAVY
    (0x2548, [1, 2, 2, 2]), // UP LIGHT AND DOWN HORIZONTAL HEAVY
    (0x2549, [2, 2, 2, 1]), // RIGHT LIGHT AND LEFT VERTICAL HEAVY
    (0x254A, [2, 2, 1, 2]), // LEFT LIGHT AND RIGHT VERTICAL HEAVY
    (0x254B, [2, 2, 1, 1]), // HEAVY VERTICAL AND HORIZONTAL
    (0x2550, [0, 0, 3, 3]), // DOUBLE HORIZONTAL
    (0x2551, [3, 3, 0, 0]), // DOUBLE VERTICAL
    (0x2552, [0, 1, 0, 3]), // DOWN SINGLE AND RIGHT DOUBLE
    (0x2553, [0, 3, 0, 1]), // DOWN DOUBLE AND RIGHT SINGLE
    (0x2554, [0, 3, 0, 1]), // DOUBLE DOWN AND RIGHT
    (0x2555, [0, 1, 3, 0]), // DOWN SINGLE AND LEFT DOUBLE
    (0x2556, [0, 3, 1, 0]), // DOWN DOUBLE AND LEFT SINGLE
    (0x2557, [0, 3, 1, 0]), // DOUBLE DOWN AND LEFT
    (0x2558, [1, 0, 0, 3]), // UP SINGLE AND RIGHT DOUBLE
    (0x2559, [3, 0, 0, 1]), // UP DOUBLE AND RIGHT SINGLE
    (0x255A, [3, 0, 0, 1]), // DOUBLE UP AND RIGHT
    (0x255B, [1, 0, 3, 0]), // UP SINGLE AND LEFT DOUBLE
    (0x255C, [3, 0, 1, 0]), // UP DOUBLE AND LEFT SINGLE
    (0x255D, [3, 0, 1, 0]), // DOUBLE UP AND LEFT
    (0x255E, [1, 1, 0, 3]), // VERTICAL SINGLE AND RIGHT DOUBLE
    (0x255F, [3, 3, 0, 1]), // VERTICAL DOUBLE AND RIGHT SINGLE
    (0x2560, [3, 3, 0, 1]), // DOUBLE VERTICAL AND RIGHT
    (0x2561, [1, 1, 3, 0]), // VERTICAL SINGLE AND LEFT DOUBLE
    (0x2562, [3, 3, 1, 0]), // VERTICAL DOUBLE AND LEFT SINGLE
    (0x2563, [3, 3, 1, 0]), // DOUBLE VERTICAL AND LEFT
    (0x2564, [0, 1, 3, 3]), // DOWN SINGLE AND HORIZONTAL DOUBLE
    (0x2565, [0, 3, 1, 1]), // DOWN DOUBLE AND HORIZONTAL SINGLE
    (0x2566, [0, 3, 1, 1]), // DOUBLE DOWN AND HORIZONTAL
    (0x2567, [1, 0, 3, 3]), // UP SINGLE AND HORIZONTAL DOUBLE
    (0x2568, [3, 0, 1, 1]), // UP DOUBLE AND HORIZONTAL SINGLE
    (0x2569, [3, 0, 1, 1]), // DOUBLE UP AND HORIZONTAL
    (0x256A, [1, 1, 3, 3]), // VERTICAL SINGLE AND HORIZONTAL DOUBLE
    (0x256B, [3, 3, 1, 1]), // VERTICAL DOUBLE AND HORIZONTAL SINGLE
    (0x256C, [3, 3, 1, 1]), // DOUBLE VERTICAL AND HORIZONTAL
    (0x2574, [0, 0, 1, 0]), // LIGHT LEFT
    (0x2575, [1, 0, 0, 0]), // LIGHT UP
    (0x2576, [0, 0, 0, 1]), // LIGHT RIGHT
    (0x2577, [0, 1, 0, 0]), // LIGHT DOWN
    (0x2578, [0, 0, 2, 0]), // HEAVY LEFT
    (0x2579, [2, 0, 0, 0]), // HEAVY UP
    (0x257A, [0, 0, 0, 2]), // HEAVY RIGHT
    (0x257B, [0, 2, 0, 0]), // HEAVY DOWN
    (0x257C, [0, 0, 1, 2]), // LIGHT LEFT AND HEAVY RIGHT
    (0x257D, [1, 2, 0, 0]), // LIGHT UP AND HEAVY DOWN
    (0x257E, [0, 0, 2, 1]), // HEAVY LEFT AND LIGHT RIGHT
    (0x257F, [2, 1, 0, 0]), // HEAVY UP AND LIGHT DOWN
];

/// The vertical pixel band of a stroke of `t` centered on the cell midline.
fn band(center: usize, t: usize, max: usize) -> (usize, usize) {
    let lo = center.saturating_sub(t / 2);
    (lo, (lo + t).min(max))
}

/// Draw one U+2500–257F char; `None` for the few we don't model (none in
/// practice — every code point in the block is handled by a branch here).
fn draw_box(c: &mut Canvas, cp: u32) -> Option<()> {
    let (w, h) = (c.w, c.h);
    let (cx, cy) = (w / 2, h / 2);
    let lt = c.light();
    let ht = c.heavy();
    let t_of = |weight: u8| if weight == 2 { ht } else { lt };

    // Dashed variants: the arm pattern of the solid char, gapped.
    let dashes: Option<(u32, usize)> = match cp {
        0x2504 | 0x2505 => Some((cp - 4, 3)), // triple dash horizontal
        0x2508 | 0x2509 => Some((cp - 8, 4)), // quadruple dash horizontal
        0x2506 | 0x2507 => Some((cp - 4, 3)), // triple dash vertical
        0x250A | 0x250B => Some((cp - 8, 4)), // quadruple dash vertical
        0x254C | 0x254D => Some((cp - 0x4C, 2)), // double dash horiz -> 2500/2501
        0x254E | 0x254F => Some((cp - 0x4C + 0x02, 2)), // double dash vert -> 2502/2503
        _ => None,
    };
    if let Some((solid, n)) = dashes {
        draw_box(c, solid)?;
        // Cut evenly spaced gaps along the stroke direction.
        let vertical = matches!(solid, 0x2502 | 0x2503);
        let len = if vertical { h } else { w };
        let seg = (len / n).max(2);
        for i in 1..=n {
            let g0 = i * seg - seg / 4;
            for g in g0..(g0 + (seg / 4).max(1)).min(len) {
                for k in 0..if vertical { w } else { h } {
                    let (x, y) = if vertical { (k, g) } else { (g, k) };
                    if x < w && y < h {
                        c.px[y * w + x] = 0;
                    }
                }
            }
        }
        return Some(());
    }

    // Rounded corners (arcs): quarter circle joining the two arm midlines.
    if let 0x256D..=0x2570 = cp {
        let r = cx.min(cy);
        // Which quadrant: 256D down+right, 256E down+left, 256F up+left, 2570 up+right.
        let (ccx, ccy): (isize, isize) = match cp {
            0x256D => (cx as isize + r as isize, cy as isize + r as isize),
            0x256E => (cx as isize - r as isize, cy as isize + r as isize),
            0x256F => (cx as isize - r as isize, cy as isize - r as isize),
            _ => (cx as isize + r as isize, cy as isize - r as isize),
        };
        for y in 0..h as isize {
            for x in 0..w as isize {
                let (dx, dy) = ((x - ccx) as f32 + 0.5, (y - ccy) as f32 + 0.5);
                if (dx * dx + dy * dy).sqrt().round() as usize == r {
                    c.set(x as usize, y as usize);
                }
            }
        }
        // The straight arm stubs from the arc ends to the cell edges.
        let (yb0, yb1) = band(cy, lt, h);
        let (xb0, xb1) = band(cx, lt, w);
        match cp {
            0x256D => {
                c.rect(cx + r, yb0, w, yb1);
                c.rect(xb0, cy + r, xb1, h);
            }
            0x256E => {
                c.rect(0, yb0, cx.saturating_sub(r), yb1);
                c.rect(xb0, cy + r, xb1, h);
            }
            0x256F => {
                c.rect(0, yb0, cx.saturating_sub(r), yb1);
                c.rect(xb0, 0, xb1, cy.saturating_sub(r));
            }
            _ => {
                c.rect(cx + r, yb0, w, yb1);
                c.rect(xb0, 0, xb1, cy.saturating_sub(r));
            }
        }
        return Some(());
    }

    // Diagonals.
    if let 0x2571..=0x2573 = cp {
        let t = lt as f32;
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                // Normalized diagonal distance for both directions.
                let d1 = (fx / w as f32 + fy / h as f32 - 1.0).abs() * w.min(h) as f32 / 1.42;
                let d2 = (fx / w as f32 - fy / h as f32).abs() * w.min(h) as f32 / 1.42;
                let hit = match cp {
                    0x2571 => d1 <= t / 2.0, // ╱
                    0x2572 => d2 <= t / 2.0, // ╲
                    _ => d1 <= t / 2.0 || d2 <= t / 2.0,
                };
                if hit {
                    c.set(x, y);
                }
            }
        }
        return Some(());
    }

    // Everything else: the arm table (singles, heavies, doubles, halves).
    let arms = ARMS.iter().find(|(k, _)| *k == cp).map(|(_, a)| *a)?;
    let [up, down, left, right] = arms;
    // Double strokes: two light lines with a light gap, i.e. total 3*lt.
    let draw_arm = |c: &mut Canvas, dir: usize, weight: u8| {
        if weight == 0 {
            return;
        }
        let vertical = dir < 2; // 0 up, 1 down, 2 left, 3 right
        // Arm extent: edge -> just past center so joins connect.
        let reach = |wt: u8, cross_double: bool| {
            // A double crossing needs arms to reach the far rail.
            if cross_double { 2 * lt } else { t_of(wt) / 2 }
        };
        let cross_is_double = if vertical {
            left == 3 || right == 3
        } else {
            up == 3 || down == 3
        };
        let over = reach(weight, cross_is_double);
        if weight == 3 {
            // Two rails, centered lt*1.5 either side of the midline.
            let rails = |mid: usize, max: usize| {
                let a = band(mid.saturating_sub(lt), lt, max);
                let b = band(mid + lt, lt, max);
                [a, b]
            };
            if vertical {
                for (x0, x1) in rails(cx, w) {
                    let (y0, y1) = if dir == 0 {
                        (0, cy + over)
                    } else {
                        (cy.saturating_sub(over), h)
                    };
                    c.rect(x0, y0, x1, y1);
                }
            } else {
                for (y0, y1) in rails(cy, h) {
                    let (x0, x1) = if dir == 2 {
                        (0, cx + over)
                    } else {
                        (cx.saturating_sub(over), w)
                    };
                    c.rect(x0, y0, x1, y1);
                }
            }
            return;
        }
        let t = t_of(weight);
        if vertical {
            let (x0, x1) = band(cx, t, w);
            let (y0, y1) = if dir == 0 {
                (0, cy + over)
            } else {
                (cy.saturating_sub(over), h)
            };
            c.rect(x0, y0, x1, y1);
        } else {
            let (y0, y1) = band(cy, t, h);
            let (x0, x1) = if dir == 2 {
                (0, cx + over)
            } else {
                (cx.saturating_sub(over), w)
            };
            c.rect(x0, y0, x1, y1);
        }
    };
    draw_arm(c, 0, up);
    draw_arm(c, 1, down);
    draw_arm(c, 2, left);
    draw_arm(c, 3, right);
    // Double corners/junctions leave a hole at the center where the rails
    // would meet; fill the joint conservatively.
    if arms.contains(&3) {
        let n = arms.iter().filter(|&&a| a != 0).count();
        if n >= 2 {
            c.rect(
                cx.saturating_sub(2 * lt).max(if left != 0 {
                    0
                } else {
                    cx.saturating_sub(2 * lt)
                }),
                cy.saturating_sub(2 * lt),
                cx + 2 * lt,
                cy + 2 * lt,
            );
            // Re-cut the hollow between rails for straight-through doubles.
            if up == 3 && down == 3 && left == 0 && right == 0 {
                let (x0, x1) = band(cx, lt, w);
                c.px.iter_mut().enumerate().for_each(|(i, p)| {
                    let x = i % w;
                    if x >= x0 && x < x1 {
                        *p = 0;
                    }
                });
            }
            if left == 3 && right == 3 && up == 0 && down == 0 {
                let (y0, y1) = band(cy, lt, h);
                for y in y0..y1 {
                    for x in 0..w {
                        c.px[y * w + x] = 0;
                    }
                }
            }
        }
    }
    Some(())
}

/// U+2580–259F block elements.
fn draw_block(c: &mut Canvas, cp: u32) {
    let (w, h) = (c.w, c.h);
    let eighth_h = |n: usize| (h * n).div_ceil(8);
    let eighth_w = |n: usize| (w * n).div_ceil(8);
    match cp {
        0x2580 => c.rect(0, 0, w, h / 2), // upper half
        0x2581..=0x2588 => {
            let n = (cp - 0x2580) as usize; // lower n/8
            c.rect(0, h - eighth_h(n), w, h);
        }
        0x2589..=0x258F => {
            let n = 8 - (cp - 0x2588) as usize; // left n/8
            c.rect(0, 0, eighth_w(n), h);
        }
        0x2590 => c.rect(w / 2, 0, w, h), // right half
        0x2591..=0x2593 => {
            // Shades: 25% / 50% / 75% checker dithers.
            let keep = (cp - 0x2590) as usize; // 1..=3
            for y in 0..h {
                for x in 0..w {
                    let phase = (x + y * 2) % 4;
                    if phase < keep {
                        c.set(x, y);
                    }
                }
            }
        }
        0x2594 => c.rect(0, 0, w, eighth_h(1)), // upper eighth
        0x2595 => c.rect(w - eighth_w(1), 0, w, h), // right eighth
        0x2596..=0x259F => {
            // Quadrants: bit per filled quarter, from the code chart.
            let (hw, hh) = (w / 2, h / 2);
            let quads: [u8; 10] = [
                0b0010, 0b0001, 0b1000, 0b1011, 0b1101, 0b0100, 0b0111, 0b1110, 0b0110, 0b1001,
            ];
            let q = quads[(cp - 0x2596) as usize];
            if q & 0b1000 != 0 {
                c.rect(0, 0, hw, hh)
            } // upper left
            if q & 0b0100 != 0 {
                c.rect(hw, 0, w, hh)
            } // upper right
            if q & 0b0010 != 0 {
                c.rect(0, hh, hw, h)
            } // lower left
            if q & 0b0001 != 0 {
                c.rect(hw, hh, w, h)
            } // lower right
        }
        _ => {}
    }
}

/// U+2800–28FF braille: a 2x4 dot grid keyed by the codepoint's dot bits.
fn draw_braille(c: &mut Canvas, bits: u32) {
    let (w, h) = (c.w, c.h);
    // Dot n (bit n-1) -> (column, row): 1..3 left column rows 0..2, 4..6
    // right column rows 0..2, 7 left row 3, 8 right row 3.
    const POS: [(usize, usize); 8] = [
        (0, 0),
        (0, 1),
        (0, 2),
        (1, 0),
        (1, 1),
        (1, 2),
        (0, 3),
        (1, 3),
    ];
    let r = (w.min(h) / 8).max(1);
    for (bit, &(col, row)) in POS.iter().enumerate() {
        if bits & (1 << bit) == 0 {
            continue;
        }
        let cx = (w * (2 * col + 1)) / 4;
        let cy = (h * (2 * row + 1)) / 8;
        for y in cy.saturating_sub(r)..(cy + r).min(h) {
            for x in cx.saturating_sub(r)..(cx + r).min(w) {
                c.set(x, y);
            }
        }
    }
}

/// Powerline separators U+E0B0–U+E0B3: filled and outline triangles.
fn draw_powerline(c: &mut Canvas, cp: u32) {
    let (w, h) = (c.w, c.h);
    let t = c.light() as f32;
    for y in 0..h {
        // The triangle edge x at this row: 0 at the tip rows, w at the base.
        let frac = if y < h / 2 {
            y as f32 / (h as f32 / 2.0)
        } else {
            (h - 1 - y) as f32 / (h as f32 / 2.0)
        };
        let edge = frac * w as f32;
        for x in 0..w {
            let fx = x as f32 + 0.5;
            let (filled, outline) = (fx <= edge, (fx - edge).abs() <= t / 2.0);
            let hit = match cp {
                0xE0B0 => filled,                               // solid right triangle
                0xE0B1 => outline,                              // right angle line
                0xE0B2 => (w as f32 - fx) <= edge,              // solid left triangle
                _ => ((w as f32 - fx) - edge).abs() <= t / 2.0, // left angle line
            };
            if hit {
                c.set(x, y);
            }
        }
    }
}

/// Extended Powerline separators U+E0B4–U+E0BF: semicircle caps (filled and
/// outline, both directions) and the slant triangles / diagonal lines.
fn draw_powerline_ext(c: &mut Canvas, cp: u32) {
    let (w, h) = (c.w, c.h);
    let t = c.light() as f32;
    // Semicircle geometry: a half-ellipse anchored to the cell's left (for
    // right-pointing) or right (for left-pointing) edge, full cell height.
    let (rw, rh) = (w as f32, h as f32 / 2.0);
    let diag = ((w * w + h * h) as f32).sqrt();
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let cy = fy - h as f32 / 2.0;
            // Normalized ellipse distance from each anchoring edge (1.0 = on
            // the arc), and pixel distance from each diagonal.
            let right = ((fx / rw).powi(2) + (cy / rh).powi(2)).sqrt();
            let left = (((w as f32 - fx) / rw).powi(2) + (cy / rh).powi(2)).sqrt();
            let ring = t / (2.0 * rw.min(rh));
            let d_back = (fx * h as f32 - fy * w as f32).abs() / diag; // `\`
            let d_fwd = ((w as f32 - fx) * h as f32 - fy * w as f32).abs() / diag; // `/`
            let (dx, dy) = (fx / w as f32, fy / h as f32);
            let hit = match cp {
                0xE0B4 => right <= 1.0,                // solid right semicircle
                0xE0B5 => (right - 1.0).abs() <= ring, // right semicircle line
                0xE0B6 => left <= 1.0,                 // solid left semicircle
                0xE0B7 => (left - 1.0).abs() <= ring,  // left semicircle line
                0xE0B8 => dx <= dy,                    // solid lower-left slant
                0xE0B9 => d_back <= t / 2.0 + 0.5,     // backslash line
                0xE0BA => dx + dy >= 1.0,              // solid lower-right slant
                0xE0BB => d_fwd <= t / 2.0 + 0.5,      // forward-slash line
                0xE0BC => dx + dy <= 1.0,              // solid upper-left slant
                0xE0BD => d_fwd <= t / 2.0 + 0.5,      // forward-slash line
                0xE0BE => dx >= dy,                    // solid upper-right slant
                _ => d_back <= t / 2.0 + 0.5,          // 0xE0BF backslash line
            };
            if hit {
                c.set(x, y);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cov(ch: char, w: usize, h: usize) -> Vec<u8> {
        synthesize(ch, w, h, 12).expect("synthesized").coverage
    }

    fn at(px: &[u8], w: usize, x: usize, y: usize) -> bool {
        px[y * w + x] != 0
    }

    #[test]
    fn extended_powerline_semicircles_and_slants_synthesize() {
        let (w, h) = (10, 20);
        // Solid right semicircle: hugs the left edge, empty at the right edge.
        let b4 = cov('\u{E0B4}', w, h);
        assert!(at(&b4, w, 0, h / 2), "solid at the anchoring edge");
        assert!(!at(&b4, w, w - 1, 0), "top-right corner outside the arc");
        // Outline covers strictly less than the fill.
        let b5 = cov('\u{E0B5}', w, h);
        let (fill, line) = (
            b4.iter().filter(|&&p| p != 0).count(),
            b5.iter().filter(|&&p| p != 0).count(),
        );
        assert!(
            line > 0 && line < fill,
            "arc outline thinner than fill: {line} vs {fill}"
        );
        // Mirror: solid left semicircle anchors right.
        let b6 = cov('\u{E0B6}', w, h);
        assert!(at(&b6, w, w - 1, h / 2));
        assert!(!at(&b6, w, 0, 0));
        // Lower-left slant: bottom-left solid, top-right empty (and vice
        // versa for the upper-right slant).
        let b8 = cov('\u{E0B8}', w, h);
        assert!(at(&b8, w, 0, h - 1) && !at(&b8, w, w - 1, 0));
        let be = cov('\u{E0BE}', w, h);
        assert!(at(&be, w, w - 1, 0) && !at(&be, w, 0, h - 1));
        // The diagonal lines are thin but present.
        for ch in ['\u{E0B9}', '\u{E0BB}', '\u{E0BD}', '\u{E0BF}'] {
            let c = cov(ch, w, h);
            let n = c.iter().filter(|&&p| p != 0).count();
            assert!(n > 0 && n < w * h / 3, "{ch:?}: thin diagonal, got {n}");
        }
        // The whole extended range is claimed by the synthesizer.
        for cp in 0xE0B4..=0xE0BF {
            assert!(is_synthesized(char::from_u32(cp).unwrap()));
        }
    }

    #[test]
    fn straight_lines_span_the_whole_cell() {
        let (w, h) = (10, 20);
        let horiz = cov('─', w, h);
        assert!(at(&horiz, w, 0, h / 2) && at(&horiz, w, w - 1, h / 2));
        assert!(!at(&horiz, w, 0, 0));
        let vert = cov('│', w, h);
        assert!(at(&vert, w, w / 2, 0) && at(&vert, w, w / 2, h - 1));
        // Two adjacent cells of ─ join: coverage reaches both edges.
    }

    #[test]
    fn corners_and_junctions_meet_at_center() {
        let (w, h) = (10, 20);
        let corner = cov('┌', w, h); // down + right
        assert!(at(&corner, w, w / 2, h - 1), "down arm reaches bottom");
        assert!(at(&corner, w, w - 1, h / 2), "right arm reaches right");
        assert!(!at(&corner, w, 0, 0), "no arm toward up-left");
        let cross = cov('┼', w, h);
        for (x, y) in [(0, h / 2), (w - 1, h / 2), (w / 2, 0), (w / 2, h - 1)] {
            assert!(at(&cross, w, x, y));
        }
    }

    #[test]
    fn heavy_is_thicker_than_light() {
        let (w, h) = (12, 24);
        let count = |px: &[u8]| px.iter().filter(|&&p| p != 0).count();
        assert!(count(&cov('━', w, h)) > count(&cov('─', w, h)));
        assert!(count(&cov('┃', w, h)) > count(&cov('│', w, h)));
    }

    #[test]
    fn double_lines_have_a_hollow_between_rails() {
        let (w, h) = (12, 24);
        let px = cov('║', w, h);
        // Some column strictly between the rails is empty at mid-height.
        assert!(!at(&px, w, w / 2, h / 2), "hollow center");
        // And rails exist either side.
        let row: Vec<bool> = (0..w).map(|x| at(&px, w, x, h / 2)).collect();
        assert!(row.iter().filter(|&&b| b).count() >= 2);
    }

    #[test]
    fn blocks_fill_expected_fractions() {
        let (w, h) = (8, 16);
        let count = |ch: char| cov(ch, w, h).iter().filter(|&&p| p != 0).count();
        assert_eq!(count('█'), w * h);
        assert_eq!(count('▄'), w * h / 2);
        assert_eq!(count('▀'), w * h / 2);
        assert_eq!(count('▌'), w * h / 2);
        // Shades are strictly ordered by density.
        assert!(count('░') < count('▒') && count('▒') < count('▓'));
        // Quadrants: a single quarter.
        assert_eq!(count('▘'), (w / 2) * (h / 2));
    }

    #[test]
    fn braille_dot_count_scales_with_bits() {
        let (w, h) = (8, 16);
        let count = |ch: char| cov(ch, w, h).iter().filter(|&&p| p != 0).count();
        assert_eq!(count('\u{2800}'), 0); // blank pattern
        let one = count('\u{2801}');
        assert!(one > 0);
        assert_eq!(count('\u{28FF}'), one * 8); // all eight dots
    }

    #[test]
    fn powerline_triangle_fills_half_the_cell_roughly() {
        let (w, h) = (10, 20);
        let filled = cov('\u{E0B0}', w, h).iter().filter(|&&p| p != 0).count();
        let total = (w * h) as f32;
        let frac = filled as f32 / total;
        assert!((0.3..0.7).contains(&frac), "{frac}");
        // The outline variant is much sparser.
        let outline = cov('\u{E0B1}', w, h).iter().filter(|&&p| p != 0).count();
        assert!(outline < filled / 2);
    }

    #[test]
    fn coverage_is_cell_sized_and_positioned_at_cell_top() {
        let g = synthesize('─', 9, 18, 14).unwrap();
        assert_eq!((g.width, g.height), (9, 18));
        assert_eq!(g.left, 0);
        assert_eq!(g.top, -14); // pen sits at baseline; top pulls to cell top
    }
}
