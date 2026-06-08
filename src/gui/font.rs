//! Glyph rasterization for the window backend.
//!
//! A monospace glyph cache over a TrueType/OpenType font (via `ab_glyph`).
//! Renderers consume rasterized coverage bitmaps through the [`GlyphSource`]
//! trait — which a test mock also implements, so the rasterizers can be verified
//! headlessly without a real font file.

use std::collections::HashMap;
use std::rc::Rc;

use ab_glyph::{Font, FontVec, GlyphId, PxScale, ScaleFont};

use super::shape::Shaper;

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

/// Bold/italic combination selecting which face renders a cell.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub(crate) enum Style {
    #[default]
    Regular,
    Bold,
    Italic,
    BoldItalic,
}

impl Style {
    /// Map SGR bold/italic flags to a [`Style`].
    pub(crate) fn new(bold: bool, italic: bool) -> Self {
        match (bold, italic) {
            (false, false) => Style::Regular,
            (true, false) => Style::Bold,
            (false, true) => Style::Italic,
            (true, true) => Style::BoldItalic,
        }
    }
}

/// Font files for a session: the required regular face, optional styled faces,
/// and a fallback chain tried (in order) for glyphs the main font lacks.
#[derive(Default)]
pub(crate) struct FontSet {
    pub regular: Vec<u8>,
    pub bold: Option<Vec<u8>>,
    pub italic: Option<Vec<u8>>,
    pub bold_italic: Option<Vec<u8>>,
    pub fallback: Vec<Vec<u8>>,
}

/// Source of cell metrics and rasterized glyphs. [`FontCache`] is the real
/// implementation; tests provide a deterministic mock.
pub(crate) trait GlyphSource {
    /// Cell size in pixels, `(width, height)`.
    fn cell_size(&self) -> (usize, usize);
    /// Distance from the cell top down to the text baseline, in pixels.
    fn baseline(&self) -> i32;
    /// The (cached) rasterized glyph for `ch` in `style`.
    fn glyph(&mut self, ch: char, style: Style) -> Rc<Glyph>;
    /// Shape a run of cells (same style, single-width, printable) into a sequence
    /// of `(glyph, span)`, where `span` is the number of cells the glyph covers
    /// (>1 for a ligature). The summed spans equal `text.len()`. The default maps
    /// each char to its own glyph — no ligatures; [`FontCache`] overrides it with
    /// GSUB shaping.
    fn shape(&mut self, text: &[char], style: Style) -> Vec<(Rc<Glyph>, usize)> {
        text.iter().map(|&c| (self.glyph(c, style), 1)).collect()
    }
}

/// A glyph cache over `ab_glyph` fonts at a fixed pixel size, with bold/italic
/// faces and a fallback chain.
pub(crate) struct FontCache {
    /// `[Regular, Bold, Italic, BoldItalic]`; index 0 is always present, the
    /// others fall back to Regular when a variant font wasn't provided.
    faces: [Option<FontVec>; 4],
    /// Per-face GSUB ligature shapers (same indexing as `faces`); `None` when the
    /// face has no `liga`/`calt` lookups or ligatures are disabled.
    shapers: [Option<Shaper>; 4],
    /// Fonts tried in order when the chosen face lacks a glyph (CJK, symbols, …).
    fallback: Vec<FontVec>,
    scale: PxScale,
    cell_w: usize,
    cell_h: usize,
    baseline: i32,
    cache: HashMap<(char, Style), Rc<Glyph>>,
    /// Glyphs rasterized by glyph id (ligatures / contextual substitutions),
    /// which the `(char, Style)` cache can't key.
    gid_cache: HashMap<(u16, Style), Rc<Glyph>>,
}

impl FontCache {
    /// Build a cache from a [`FontSet`] at a `px` pixel size. Cell metrics come
    /// from the regular face. `None` if the regular bytes aren't a usable font.
    pub(crate) fn new(set: FontSet, px: f32, ligatures: bool) -> Option<Self> {
        let mut shapers: [Option<Shaper>; 4] = [None, None, None, None];
        if ligatures {
            shapers[0] = Shaper::new(set.regular.clone());
        }
        let regular = FontVec::try_from_vec(set.regular).ok()?;
        let scale = PxScale::from(px);
        let scaled = regular.as_scaled(scale);
        let cell_w = scaled.h_advance(regular.glyph_id('M')).ceil().max(1.0) as usize;
        let cell_h = (scaled.ascent() - scaled.descent() + scaled.line_gap()).ceil().max(1.0) as usize;
        let baseline = scaled.ascent().ceil() as i32;
        // Parse a styled variant, building its GSUB shaper from the same bytes.
        let styled = |slot: usize, bytes: Option<Vec<u8>>, sh: &mut [Option<Shaper>; 4]| {
            let bytes = bytes?;
            if ligatures {
                sh[slot] = Shaper::new(bytes.clone());
            }
            FontVec::try_from_vec(bytes).ok()
        };
        let faces = [
            Some(regular),
            styled(1, set.bold, &mut shapers),
            styled(2, set.italic, &mut shapers),
            styled(3, set.bold_italic, &mut shapers),
        ];
        let fallback = set.fallback.into_iter().filter_map(|b| FontVec::try_from_vec(b).ok()).collect();
        Some(FontCache {
            faces,
            shapers,
            fallback,
            scale,
            cell_w,
            cell_h,
            baseline,
            cache: HashMap::new(),
            gid_cache: HashMap::new(),
        })
    }

    /// The face that has a glyph for `ch` in `style`: the styled face (or regular
    /// if that variant is absent), else the first fallback font covering `ch`,
    /// else the styled face (renders notdef).
    fn face_for(&self, ch: char, style: Style) -> &FontVec {
        let styled =
            self.faces[style as usize].as_ref().unwrap_or_else(|| self.faces[0].as_ref().unwrap());
        if styled.glyph_id(ch).0 != 0 {
            return styled;
        }
        self.fallback.iter().find(|f| f.glyph_id(ch).0 != 0).unwrap_or(styled)
    }

    /// The face index used to shape and render `style`: the styled face if
    /// present, else regular (index 0). Shaping (cmap + GSUB) and outlining must
    /// use the same face so glyph ids line up.
    fn eff_face(&self, style: Style) -> usize {
        if self.faces[style as usize].is_some() { style as usize } else { 0 }
    }

    /// Rasterize and cache a glyph by glyph id (ligatures / contextual
    /// substitutions, which the `(char, Style)` cache can't key), outlined from
    /// face `eff`.
    fn glyph_by_gid(&mut self, gid: u16, eff: usize, style: Style) -> Rc<Glyph> {
        if let Some(g) = self.gid_cache.get(&(gid, style)) {
            return Rc::clone(g);
        }
        let face = self.faces[eff].as_ref().unwrap_or_else(|| self.faces[0].as_ref().unwrap());
        let g = Rc::new(rasterize_id(face, self.scale, GlyphId(gid)));
        self.gid_cache.insert((gid, style), Rc::clone(&g));
        g
    }
}

impl GlyphSource for FontCache {
    fn cell_size(&self) -> (usize, usize) {
        (self.cell_w, self.cell_h)
    }

    fn baseline(&self) -> i32 {
        self.baseline
    }

    fn glyph(&mut self, ch: char, style: Style) -> Rc<Glyph> {
        if let Some(g) = self.cache.get(&(ch, style)) {
            return Rc::clone(g);
        }
        let g = Rc::new(rasterize(self.face_for(ch, style), self.scale, ch));
        self.cache.insert((ch, style), Rc::clone(&g));
        g
    }

    fn shape(&mut self, text: &[char], style: Style) -> Vec<(Rc<Glyph>, usize)> {
        let eff = self.eff_face(style);
        // Glyph ids via the effective face's cmap (the same face GSUB indexes).
        let face = self.faces[eff].as_ref().unwrap_or_else(|| self.faces[0].as_ref().unwrap());
        let gids: Vec<u16> = text.iter().map(|&c| face.glyph_id(c).0).collect();
        let shaped: Vec<(u16, u8)> = match self.shapers[eff].as_ref() {
            Some(sh) => sh.shape(&gids),
            None => gids.iter().map(|&g| (g, 1)).collect(),
        };
        let mut out = Vec::with_capacity(shaped.len());
        let mut src = 0usize;
        for (gid, span) in shaped {
            let span = span as usize;
            // Unchanged glyphs (and fallback chars, gid 0 in this face) go through
            // the full per-char path so the CJK/symbol fallback chain still works;
            // substitutions and ligatures rasterize by their shaped glyph id.
            let glyph = if (span == 1 && gid == gids[src]) || gid == 0 {
                self.glyph(text[src], style)
            } else {
                self.glyph_by_gid(gid, eff, style)
            };
            out.push((glyph, span));
            src += span;
        }
        out
    }
}

/// Rasterize `ch` to a coverage bitmap (whitespace / unoutlined glyphs yield an
/// empty bitmap), via its glyph id.
fn rasterize(font: &FontVec, scale: PxScale, ch: char) -> Glyph {
    rasterize_id(font, scale, font.glyph_id(ch))
}

/// Rasterize a glyph id to a coverage bitmap.
fn rasterize_id(font: &FontVec, scale: PxScale, id: GlyphId) -> Glyph {
    let glyph = id.with_scale(scale);
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

/// Load a monospace font: the configured path (the `font` config key) if set
/// and readable, else `$RUSTY_TERM_FONT`, else the first hit from a list of
/// common system locations. `None` if nothing is found.
pub(crate) fn load_default_font(configured: Option<&std::path::Path>) -> Option<Vec<u8>> {
    if let Some(path) = configured
        && let Ok(bytes) = std::fs::read(path)
    {
        return Some(bytes);
    }
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

/// Load the full [`FontSet`]: the regular face (see [`load_default_font`]),
/// optional bold/italic/bold-italic faces (explicit config paths or filename-
/// derived siblings of the regular font), and a fallback chain (an optional
/// config path plus common system CJK/symbol fonts).
pub(crate) fn load_set(
    regular_path: Option<&std::path::Path>,
    bold: Option<&std::path::Path>,
    italic: Option<&std::path::Path>,
    bold_italic: Option<&std::path::Path>,
    fallback: Option<&std::path::Path>,
) -> Option<FontSet> {
    let regular = load_default_font(regular_path)?;
    let mut fb: Vec<Vec<u8>> = Vec::new();
    if let Some(b) = fallback.and_then(|p| std::fs::read(p).ok()) {
        fb.push(b);
    }
    fb.extend(SYSTEM_FALLBACKS.iter().filter_map(|p| std::fs::read(p).ok()).take(2));
    Some(FontSet {
        regular,
        bold: load_variant(bold, regular_path, &["Bold", "bold"]),
        italic: load_variant(italic, regular_path, &["Italic", "Oblique", "italic"]),
        bold_italic: load_variant(bold_italic, regular_path, &["BoldItalic", "BoldOblique"]),
        fallback: fb,
    })
}

/// Load a styled variant: the explicit `configured` path if readable, else a
/// sibling of `regular` whose name swaps "Regular" for, or appends, a `token`.
fn load_variant(
    configured: Option<&std::path::Path>,
    regular: Option<&std::path::Path>,
    tokens: &[&str],
) -> Option<Vec<u8>> {
    if let Some(b) = configured.and_then(|p| std::fs::read(p).ok()) {
        return Some(b);
    }
    let reg = regular?;
    let name = reg.file_name()?.to_str()?;
    let (stem, ext) = name.rsplit_once('.')?;
    for token in tokens {
        for cand in [stem.replace("Regular", token).replace("regular", token), format!("{stem}-{token}")] {
            if cand == stem {
                continue; // no substitution happened -> not a real variant name
            }
            if let Ok(b) = std::fs::read(reg.with_file_name(format!("{cand}.{ext}"))) {
                return Some(b);
            }
        }
    }
    None
}

/// System fonts with broad CJK/symbol coverage, used to seed the fallback chain.
const SYSTEM_FALLBACKS: &[&str] = &[
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    "/System/Library/Fonts/PingFang.ttc",
    "C:\\Windows\\Fonts\\msyh.ttc",
    "C:\\Windows\\Fonts\\segoeui.ttf",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_font_metrics_and_glyphs() {
        // Uses a system font if one is present; skips cleanly otherwise so the
        // suite stays green on hosts without the candidate fonts.
        let Some(bytes) = load_default_font(None) else {
            eprintln!("no system monospace font found; skipping font integration test");
            return;
        };
        let mut fc =
            FontCache::new(FontSet { regular: bytes, ..Default::default() }, 18.0, false).expect("font parses");
        let (w, h) = fc.cell_size();
        assert!(w > 0 && h > 0, "cell size must be positive: {w}x{h}");
        assert!(fc.baseline() > 0);
        assert!(fc.glyph('M', Style::Regular).width > 0, "'M' should rasterize");
        assert_eq!(fc.glyph(' ', Style::Regular).width, 0, "space has no outline");
        assert!(Rc::ptr_eq(&fc.glyph('M', Style::Regular), &fc.glyph('M', Style::Regular)));
        // A missing variant face falls back to regular but caches per style.
        assert!(fc.glyph('M', Style::Bold).width > 0);
    }
}
