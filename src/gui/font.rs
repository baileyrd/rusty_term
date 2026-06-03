//! Glyph rasterization for the window backend.
//!
//! A monospace glyph cache over a TrueType/OpenType font (via `ab_glyph`).
//! Renderers consume rasterized coverage bitmaps through the [`GlyphSource`]
//! trait — which a test mock also implements, so the rasterizers can be verified
//! headlessly without a real font file.

use std::collections::HashMap;
use std::rc::Rc;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};

/// A rasterized glyph: a `width × height` 8-bit coverage (alpha) bitmap plus its
/// placement relative to the cell pen origin (left edge, text baseline).
pub(crate) struct Glyph {
    pub width: usize,
    pub height: usize,
    /// X offset from the cell's left edge to the bitmap's left column.
    pub left: i32,
    /// Y offset from the text baseline to the bitmap's top row (negative = up).
    pub top: i32,
    /// Row-major coverage, `len() == width * height`.
    pub coverage: Vec<u8>,
}

impl Glyph {
    /// An empty (whitespace / unoutlined) glyph.
    fn blank() -> Glyph {
        Glyph { width: 0, height: 0, left: 0, top: 0, coverage: Vec::new() }
    }
}

/// Source of cell metrics and rasterized glyphs. [`FontCache`] is the real
/// implementation; tests provide a deterministic mock.
pub(crate) trait GlyphSource {
    /// Cell size in pixels, `(width, height)`.
    fn cell_size(&self) -> (usize, usize);
    /// Distance from the cell top down to the text baseline, in pixels.
    fn baseline(&self) -> i32;
    /// The (cached) rasterized glyph for `ch`.
    fn glyph(&mut self, ch: char) -> Rc<Glyph>;
}

/// A glyph cache backed by an `ab_glyph` font at a fixed pixel size.
pub(crate) struct FontCache {
    font: FontVec,
    scale: PxScale,
    cell_w: usize,
    cell_h: usize,
    baseline: i32,
    cache: HashMap<char, Rc<Glyph>>,
}

impl FontCache {
    /// Build a cache from font bytes at a `px` pixel size. `None` if the bytes
    /// aren't a usable font.
    pub(crate) fn new(font_bytes: Vec<u8>, px: f32) -> Option<Self> {
        let font = FontVec::try_from_vec(font_bytes).ok()?;
        let scale = PxScale::from(px);
        let scaled = font.as_scaled(scale);
        // Monospace advance of a representative glyph sets the cell width.
        let cell_w = scaled.h_advance(font.glyph_id('M')).ceil().max(1.0) as usize;
        let cell_h = (scaled.ascent() - scaled.descent() + scaled.line_gap())
            .ceil()
            .max(1.0) as usize;
        let baseline = scaled.ascent().ceil() as i32;
        Some(FontCache { font, scale, cell_w, cell_h, baseline, cache: HashMap::new() })
    }
}

impl GlyphSource for FontCache {
    fn cell_size(&self) -> (usize, usize) {
        (self.cell_w, self.cell_h)
    }

    fn baseline(&self) -> i32 {
        self.baseline
    }

    fn glyph(&mut self, ch: char) -> Rc<Glyph> {
        if let Some(g) = self.cache.get(&ch) {
            return Rc::clone(g);
        }
        let g = Rc::new(rasterize(&self.font, self.scale, ch));
        self.cache.insert(ch, Rc::clone(&g));
        g
    }
}

/// Rasterize `ch` to a coverage bitmap; whitespace and unoutlined glyphs yield
/// an empty bitmap.
fn rasterize(font: &FontVec, scale: PxScale, ch: char) -> Glyph {
    let glyph = font.glyph_id(ch).with_scale(scale);
    let Some(outlined) = font.outline_glyph(glyph) else {
        return Glyph::blank();
    };
    let bounds = outlined.px_bounds();
    let width = bounds.width().ceil() as usize;
    let height = bounds.height().ceil() as usize;
    if width == 0 || height == 0 {
        return Glyph::blank();
    }
    let mut coverage = vec![0u8; width * height];
    outlined.draw(|x, y, c| {
        let (x, y) = (x as usize, y as usize);
        if x < width && y < height {
            coverage[y * width + x] = (c * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    });
    Glyph {
        width,
        height,
        left: bounds.min.x.round() as i32,
        top: bounds.min.y.round() as i32,
        coverage,
    }
}

/// Load a monospace font: `$RUSTY_TERM_FONT` if set and readable, else the first
/// hit from a list of common system locations. `None` if nothing is found.
pub(crate) fn load_default_font() -> Option<Vec<u8>> {
    if let Some(path) = std::env::var_os("RUSTY_TERM_FONT")
        && let Ok(bytes) = std::fs::read(&path)
    {
        return Some(bytes);
    }
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/adwaita-mono-fonts/AdwaitaMono-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        "/System/Library/Fonts/SFNSMono.ttf",
        "/System/Library/Fonts/Menlo.ttc",
        "C:\\Windows\\Fonts\\consola.ttf",
    ];
    CANDIDATES.iter().find_map(|p| std::fs::read(p).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_font_metrics_and_glyphs() {
        // Uses a system font if one is present; skips cleanly otherwise so the
        // suite stays green on hosts without the candidate fonts.
        let Some(bytes) = load_default_font() else {
            eprintln!("no system monospace font found; skipping font integration test");
            return;
        };
        let mut fc = FontCache::new(bytes, 18.0).expect("font should parse");
        let (w, h) = fc.cell_size();
        assert!(w > 0 && h > 0, "cell size must be positive: {w}x{h}");
        assert!(fc.baseline() > 0);
        // 'M' has an outline; space does not.
        assert!(fc.glyph('M').width > 0, "'M' should rasterize to a bitmap");
        assert_eq!(fc.glyph(' ').width, 0, "space has no outline");
        // The cache returns the same Rc on the second lookup.
        assert!(Rc::ptr_eq(&fc.glyph('M'), &fc.glyph('M')));
    }
}
