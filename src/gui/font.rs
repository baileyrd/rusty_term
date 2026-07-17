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
    /// Row-major straight-alpha color pixels (`0xAARRGGBB`), for color-emoji
    /// bitmap strikes (CBDT/sbix). `None` for ordinary fg-tinted glyphs.
    /// `coverage` still carries the alpha channel, so a renderer without a
    /// color path (the GPU atlas) degrades to a monochrome silhouette.
    pub color: Option<Vec<u32>>,
}

impl Glyph {
    /// An empty (whitespace / unoutlined) glyph.
    fn blank() -> Glyph {
        Glyph { width: 0, height: 0, left: 0, top: 0, coverage: Vec::new(), color: None }
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
    /// A color-emoji font's raw bytes (CBDT or sbix bitmap strikes), for the
    /// color-glyph path; `None` when no emoji font was found.
    pub emoji: Option<Vec<u8>>,
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
    /// A color-emoji font's raw bytes, parsed on demand for bitmap strikes
    /// (`ttf-parser` borrows, so the bytes are kept, not a `Face`).
    emoji: Option<Vec<u8>>,
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
            emoji: set.emoji,
            scale,
            cell_w,
            cell_h,
            baseline,
            cache: HashMap::new(),
            gid_cache: HashMap::new(),
        })
    }

    /// Whether any face has an active GSUB shaper — i.e. ligatures are enabled
    /// in config and the font actually carries `liga`/`calt` lookups. Renderers
    /// use this to skip run planning entirely when shaping is a 1:1 no-op.
    /// Only the GPU renderer (`gpu.rs`) calls this today, so a `gui`-without-
    /// `gui-gpu` build (CI's default combo) sees it as unused.
    #[cfg_attr(not(feature = "gui-gpu"), allow(dead_code))]
    pub(crate) fn has_ligatures(&self) -> bool {
        self.shapers.iter().any(|s| s.is_some())
    }

    /// The face that has a glyph for `ch` in `style`: the styled face (or regular
    /// if that variant is absent), else the first fallback font covering `ch`,
    /// else the styled face (renders notdef).
    /// Rasterize `ch` from the color-emoji font's bitmap strikes (CBDT/sbix
    /// PNG), scaled to the cell height, or `None` when no emoji font is
    /// loaded / it lacks the char / the strike isn't PNG. Only consulted for
    /// emoji-block scalars so ordinary text never pays the lookup (G24).
    fn color_emoji(&self, ch: char) -> Option<Glyph> {
        if !matches!(ch as u32, 0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF | 0xFE0F | 0x2049 | 0x203C) {
            return None;
        }
        let data = self.emoji.as_deref()?;
        let face = ttf_parser::Face::parse(data, 0).ok()?;
        let gid = face.glyph_index(ch)?;
        let (cw, chh) = self.cell_size();
        let img = face.glyph_raster_image(gid, chh as u16)?;
        if img.format != ttf_parser::RasterImageFormat::PNG {
            return None;
        }
        let decoded = crate::core::png_decode(img.data)?;
        if decoded.width == 0 || decoded.height == 0 {
            return None;
        }
        // "Contain"-fit into the emoji's two-cell footprint, preserving
        // aspect (bitmap strikes are square in practice).
        let max_w = (cw * 2).max(1);
        let scale = (chh as f32 / decoded.height as f32).min(max_w as f32 / decoded.width as f32);
        let w = ((decoded.width as f32 * scale).round() as usize).max(1);
        let h = ((decoded.height as f32 * scale).round() as usize).max(1);
        let mut color = Vec::with_capacity(w * h);
        let mut coverage = Vec::with_capacity(w * h);
        for y in 0..h {
            let sy = y * decoded.height / h;
            for x in 0..w {
                let sx = x * decoded.width / w;
                let i = (sy * decoded.width + sx) * 4;
                let (r, g, b, a) =
                    (decoded.rgba[i], decoded.rgba[i + 1], decoded.rgba[i + 2], decoded.rgba[i + 3]);
                color.push(((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | b as u32);
                coverage.push(a);
            }
        }
        Some(Glyph {
            width: w,
            height: h,
            left: 0,
            top: -self.baseline() + (chh as i32 - h as i32), // bottom-align in the cell
            coverage,
            color: Some(color),
        })
    }

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
        // Box-drawing / blocks / braille / Powerline are synthesized at exact
        // cell geometry instead of asking the font — seamless joins beat any
        // font's own metrics-mismatched glyphs (G25).
        let (cw, chh) = self.cell_size();
        let g = Rc::new(
            super::boxdraw::synthesize(ch, cw, chh, self.baseline())
                .or_else(|| self.color_emoji(ch))
                .unwrap_or_else(|| {
                    let g = rasterize(self.face_for(ch, style), self.scale, ch);
                    // Nerd Font icons and other Private Use Area glyphs are
                    // frequently drawn larger than the cell their width-1
                    // classification allots; contain-fit them so icons never
                    // clip or bleed into the neighbor cell (Ghostty-style
                    // glyph constraining).
                    if is_private_use(ch) {
                        constrain_to_cell(g, crate::core::char_width(ch).max(1) * cw, chh, self.baseline())
                    } else {
                        g
                    }
                }),
        );
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

/// Whether `ch` is a Private Use Area scalar (BMP PUA or planes 15–16) —
/// where Nerd Fonts and friends park their icons.
fn is_private_use(ch: char) -> bool {
    matches!(ch as u32, 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x10_0000..=0x10_FFFD)
}

/// Contain-fit `g` into a `box_w × box_h` cell box anchored at the text
/// baseline: a glyph that already fits comes back untouched; an oversized
/// one is scaled down (nearest-neighbor, aspect preserved) and centered.
fn constrain_to_cell(g: Glyph, box_w: usize, box_h: usize, baseline: i32) -> Glyph {
    if g.width == 0 || g.height == 0 || box_w == 0 || box_h == 0 {
        return g;
    }
    // The cell box in glyph coordinates: x in [0, box_w), y (relative to the
    // baseline) in [-baseline, box_h - baseline).
    let fits = g.left >= 0
        && g.left as i64 + g.width as i64 <= box_w as i64
        && g.top >= -baseline
        && g.top as i64 + g.height as i64 <= (box_h as i64 - baseline as i64);
    if fits {
        return g;
    }
    let scale = (box_w as f32 / g.width as f32)
        .min(box_h as f32 / g.height as f32)
        .min(1.0);
    let (nw, nh) = (
        ((g.width as f32 * scale) as usize).clamp(1, box_w),
        ((g.height as f32 * scale) as usize).clamp(1, box_h),
    );
    let mut coverage = vec![0u8; nw * nh];
    for y in 0..nh {
        let sy = y * g.height / nh;
        for x in 0..nw {
            let sx = x * g.width / nw;
            coverage[y * nw + x] = g.coverage[sy * g.width + sx];
        }
    }
    Glyph {
        width: nw,
        height: nh,
        left: ((box_w - nw) / 2) as i32,
        top: -baseline + ((box_h - nh) / 2) as i32,
        coverage,
        color: None,
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
        color: None,
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
    // Auto-discover an installed Nerd Fonts symbols companion (the unpatched
    // icons-only face): with it in the chain, any base font gets working
    // Powerline/devicon/codicon glyphs without a patched font or any config.
    // An explicit `font_fallback` still wins (it's first in the chain).
    if let Some(b) = find_nerd_symbols() {
        fb.push(b);
    }
    fb.extend(SYSTEM_FALLBACKS.iter().filter_map(|p| std::fs::read(p).ok()).take(2));
    let emoji = EMOJI_FONTS.iter().find_map(|p| std::fs::read(p).ok());
    Some(FontSet {
        regular,
        bold: load_variant(bold, regular_path, &["Bold", "bold"]),
        italic: load_variant(italic, regular_path, &["Italic", "Oblique", "italic"]),
        bold_italic: load_variant(bold_italic, regular_path, &["BoldItalic", "BoldOblique"]),
        fallback: fb,
        emoji,
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

/// Locate the Nerd Fonts "Symbols Nerd Font" companion, if installed: fixed
/// well-known paths first, then a shallow scan of the user/system font dirs
/// for its canonical file names (the `getnf`/package-manager installs land
/// under assorted subdirectory names, so the scan is one level deep).
fn find_nerd_symbols() -> Option<Vec<u8>> {
    const NAMES: [&str; 4] = [
        "SymbolsNerdFont-Regular.ttf",
        "SymbolsNerdFontMono-Regular.ttf",
        "Symbols Nerd Font.ttf",
        "Symbols Nerd Font Mono.ttf",
    ];
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Some(h) = &home {
        dirs.push(h.join(".local/share/fonts"));
        dirs.push(h.join("Library/Fonts")); // macOS user fonts
    }
    dirs.push("/usr/share/fonts/nerd-fonts".into());
    dirs.push("/usr/share/fonts/TTF".into());
    dirs.push("/usr/share/fonts/truetype".into());
    dirs.push("/usr/local/share/fonts".into());
    for dir in &dirs {
        for name in NAMES {
            if let Ok(b) = std::fs::read(dir.join(name)) {
                return Some(b);
            }
        }
        // One level of subdirectories (e.g. ~/.local/share/fonts/NerdFonts/).
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            for name in NAMES {
                if let Ok(b) = std::fs::read(path.join(name)) {
                    return Some(b);
                }
            }
        }
    }
    None
}

/// System fonts with broad CJK/symbol coverage, used to seed the fallback chain.
/// Well-known color-emoji font locations (CBDT on Linux/Windows, sbix on
/// macOS), tried in order.
const EMOJI_FONTS: &[&str] = &[
    "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
    "/usr/share/fonts/noto/NotoColorEmoji.ttf",
    "/usr/share/fonts/google-noto-emoji/NotoColorEmoji.ttf",
    "C:\\Windows\\Fonts\\seguiemj.ttf",
    "/System/Library/Fonts/Apple Color Emoji.ttc",
];

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
    fn constrain_scales_oversized_pua_glyphs_and_leaves_fitting_ones() {
        // A glyph 3x the cell box shrinks to fit, centered, aspect kept.
        let big = Glyph {
            width: 30,
            height: 60,
            left: -4,
            top: -50,
            coverage: vec![255; 30 * 60],
            color: None,
        };
        let g = constrain_to_cell(big, 10, 20, 16);
        assert!(g.width <= 10 && g.height <= 20, "{}x{}", g.width, g.height);
        assert_eq!(g.height, 20, "limited by height (aspect 1:2 in a 1:2 box)");
        assert!(g.left >= 0 && g.left as usize + g.width <= 10, "centered inside");
        assert!(g.top >= -16 && g.top + g.height as i32 <= 4, "inside the cell vertically");
        assert!(g.coverage.iter().any(|&c| c != 0));
        // A fitting glyph passes through untouched.
        let ok = Glyph {
            width: 6,
            height: 10,
            left: 2,
            top: -12,
            coverage: vec![128; 60],
            color: None,
        };
        let g2 = constrain_to_cell(ok, 10, 20, 16);
        assert_eq!((g2.width, g2.height, g2.left, g2.top), (6, 10, 2, -12));
        // Degenerates don't panic.
        let _ = constrain_to_cell(Glyph::blank(), 10, 20, 16);
    }

    #[test]
    fn private_use_detection_covers_nerd_font_planes() {
        assert!(is_private_use('\u{E0A0}')); // Powerline branch symbol
        assert!(is_private_use('\u{F0001}')); // Material (NF v3, plane 15)
        assert!(!is_private_use('A'));
        assert!(!is_private_use('\u{2500}'));
    }

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

    #[test]
    fn box_drawing_is_synthesized_and_emoji_degrades_without_a_font() {
        let Some(bytes) = load_default_font(None) else {
            return; // no system font on this host; covered by boxdraw's own tests
        };
        let mut fc =
            FontCache::new(FontSet { regular: bytes, ..Default::default() }, 18.0, false).expect("font parses");
        let (w, h) = fc.cell_size();
        // Box drawing comes from the synthesizer: exactly cell-sized, at the
        // cell origin — regardless of what the font provides.
        let g = fc.glyph('─', Style::Regular);
        assert_eq!((g.width, g.height), (w, h));
        assert_eq!((g.left, g.top), (0, -fc.baseline()));
        assert!(g.color.is_none());
        // With no emoji font loaded (FontSet::default has none), an emoji
        // scalar takes the ordinary rasterize path without panicking.
        let e = fc.glyph('\u{1F600}', Style::Regular);
        assert!(e.color.is_none());
    }
}
