//! Minimal GSUB shaper for programming-font ligatures.
//!
//! `ab_glyph` maps a codepoint straight to a glyph (no shaping), so ligatures
//! (`==`, `=>`, `!=`, `fi`, ...) never form. This applies the font's `liga` and
//! `calt` GSUB features to a run of glyph ids, returning each output glyph with
//! the number of input glyphs (terminal cells) it spans. The font tables are
//! read with `ttf-parser` (already in the tree via `ab_glyph`); the substitution
//! *application* is hand-rolled here.
//!
//! Supported lookup types: 1 (single), 4 (ligature), 5 (context), 6 (chained
//! context) in all three formats, with nested lookup records applied
//! recursively. Multiple/alternate/reverse-chain substitutions are treated as
//! no-ops (the run renders unshaped), matching the "skip what we can't do"
//! stance of the image decoders.

use ttf_parser::gsub::{LigatureSubstitution, SingleSubstitution, SubstitutionSubtable};
use ttf_parser::opentype_layout::{
    ChainedContextLookup, ContextLookup, LayoutTable, SequenceLookupRecord,
};
use ttf_parser::{Face, GlyphId, Tag};

/// Recursion cap for nested contextual lookups (real fonts are shallow).
const MAX_DEPTH: u8 = 8;

/// One face's ligature shaper: the font bytes (re-parsed per call, since
/// `ttf-parser` borrows the data) plus the GSUB lookup indices enabled by the
/// `liga` and `calt` features, in application order.
pub(crate) struct Shaper {
    data: Vec<u8>,
    lookups: Vec<u16>,
}

impl Shaper {
    /// Build a shaper from font bytes, or `None` if the font has no GSUB
    /// `liga`/`calt` lookups (nothing to shape — the caller stays per-glyph).
    pub(crate) fn new(data: Vec<u8>) -> Option<Shaper> {
        let mut lookups: Vec<u16> = Vec::new();
        {
            let face = Face::parse(&data, 0).ok()?;
            let gsub = face.tables().gsub?;
            let (liga, calt) = (Tag::from_bytes(b"liga"), Tag::from_bytes(b"calt"));
            for feature in gsub.features {
                if feature.tag != liga && feature.tag != calt {
                    continue;
                }
                for i in 0..feature.lookup_indices.len() {
                    if let Some(idx) = feature.lookup_indices.get(i)
                        && !lookups.contains(&idx)
                    {
                        lookups.push(idx);
                    }
                }
            }
            lookups.sort_unstable();
        }
        if lookups.is_empty() {
            return None;
        }
        Some(Shaper { data, lookups })
    }

    /// The glyph id for `ch` via the face cmap (0 = `.notdef` / not in the font).
    #[cfg(test)]
    pub(crate) fn gid(&self, ch: char) -> u16 {
        Face::parse(&self.data, 0)
            .ok()
            .and_then(|f| f.glyph_index(ch))
            .map_or(0, |g| g.0)
    }

    /// Apply the enabled GSUB lookups to `input` glyph ids, returning each output
    /// glyph with the number of input glyphs it consumed (`span`). The summed
    /// spans always equal `input.len()`.
    pub(crate) fn shape(&self, input: &[u16]) -> Vec<(u16, u8)> {
        let mut buf: Vec<(u16, u8)> = input.iter().map(|&g| (g, 1)).collect();
        let Ok(face) = Face::parse(&self.data, 0) else {
            return buf;
        };
        let Some(gsub) = face.tables().gsub else {
            return buf;
        };
        for &li in &self.lookups {
            apply_lookup_scan(&gsub, li, &mut buf);
        }
        buf
    }
}

/// Scan the buffer left to right, applying lookup `li` (the top-level entry for
/// an enabled feature) at each position that matches.
fn apply_lookup_scan(gsub: &LayoutTable, li: u16, buf: &mut Vec<(u16, u8)>) {
    let Some(lookup) = gsub.lookups.get(li) else {
        return;
    };
    let subtables: Vec<SubstitutionSubtable> = lookup.subtables.into_iter().collect();
    let mut i = 0;
    while i < buf.len() {
        let mut advance = 1;
        for st in &subtables {
            if let Some(consumed) = apply_subtable_at(gsub, st, buf, i, 0) {
                advance = consumed.max(1);
                break;
            }
        }
        i += advance;
    }
}

/// Apply lookup `li` once at `pos` (used for nested context records). Returns the
/// number of input glyphs the match spanned, or `None` if nothing applied.
fn apply_lookup_at(
    gsub: &LayoutTable,
    li: u16,
    buf: &mut Vec<(u16, u8)>,
    pos: usize,
    depth: u8,
) -> Option<usize> {
    if depth > MAX_DEPTH {
        return None;
    }
    let lookup = gsub.lookups.get(li)?;
    for st in lookup.subtables.into_iter::<SubstitutionSubtable>() {
        if let Some(consumed) = apply_subtable_at(gsub, &st, buf, pos, depth) {
            return Some(consumed);
        }
    }
    None
}

fn apply_subtable_at(
    gsub: &LayoutTable,
    st: &SubstitutionSubtable,
    buf: &mut Vec<(u16, u8)>,
    pos: usize,
    depth: u8,
) -> Option<usize> {
    match st {
        SubstitutionSubtable::Single(s) => apply_single(s, buf, pos),
        SubstitutionSubtable::Ligature(l) => apply_ligature(l, buf, pos),
        SubstitutionSubtable::Context(c) => apply_context(gsub, c, buf, pos, depth),
        SubstitutionSubtable::ChainContext(c) => apply_chain(gsub, c, buf, pos, depth),
        // Multiple (1->many), Alternate, and reverse-chain don't form ligatures.
        _ => None,
    }
}

/// Type 1: substitute one glyph in place.
fn apply_single(s: &SingleSubstitution, buf: &mut [(u16, u8)], pos: usize) -> Option<usize> {
    let g = buf.get(pos)?.0;
    let cov = s.coverage().get(GlyphId(g))?;
    let new = match s {
        SingleSubstitution::Format1 { delta, .. } => (g as i32 + *delta as i32) as u16,
        SingleSubstitution::Format2 { substitutes, .. } => substitutes.get(cov)?.0,
    };
    buf[pos].0 = new;
    Some(1)
}

/// Type 4: replace a run of glyphs with a single ligature glyph, carrying the
/// summed cell span.
fn apply_ligature(
    l: &LigatureSubstitution,
    buf: &mut Vec<(u16, u8)>,
    pos: usize,
) -> Option<usize> {
    let g = buf.get(pos)?.0;
    let cov = l.coverage.get(GlyphId(g))?;
    let set = l.ligature_sets.get(cov)?;
    for li in 0..set.len() {
        let Some(lig) = set.get(li) else { continue };
        let comp = lig.components;
        let n = comp.len() as usize;
        let mut matched = true;
        for j in 0..n {
            let want = match comp.get(j as u16) {
                Some(c) => c.0,
                None => {
                    matched = false;
                    break;
                }
            };
            match buf.get(pos + 1 + j) {
                Some(e) if e.0 == want => {}
                _ => {
                    matched = false;
                    break;
                }
            }
        }
        if matched {
            let span: u16 = (0..=n).map(|j| buf[pos + j].1 as u16).sum();
            let merged = (lig.glyph.0, span.min(255) as u8);
            buf.splice(pos..pos + n + 1, std::iter::once(merged));
            return Some(1);
        }
    }
    None
}

/// Type 5: contextual substitution (input sequence, no surrounding context).
fn apply_context(
    gsub: &LayoutTable,
    c: &ContextLookup,
    buf: &mut Vec<(u16, u8)>,
    pos: usize,
    depth: u8,
) -> Option<usize> {
    match c {
        ContextLookup::Format1 { coverage, sets } => {
            let set = sets.get(coverage.get(GlyphId(buf.get(pos)?.0))?)?;
            for ri in 0..set.len() {
                let rule = set.get(ri)?;
                if seq_matches_gids(&rule.input, buf, pos) {
                    let len = rule.input.len() as usize + 1;
                    return Some(apply_records(gsub, rule.lookups, buf, pos, len, depth));
                }
            }
            None
        }
        ContextLookup::Format2 { coverage, classes, sets } => {
            coverage.get(GlyphId(buf.get(pos)?.0))?;
            let set = sets.get(classes.get(GlyphId(buf[pos].0)))?;
            for ri in 0..set.len() {
                let rule = set.get(ri)?;
                if seq_matches_classes(&rule.input, buf, pos, classes) {
                    let len = rule.input.len() as usize + 1;
                    return Some(apply_records(gsub, rule.lookups, buf, pos, len, depth));
                }
            }
            None
        }
        ContextLookup::Format3 { coverage, coverages, lookups } => {
            coverage.get(GlyphId(buf.get(pos)?.0))?;
            for k in 0..coverages.len() {
                let g = buf.get(pos + 1 + k as usize)?.0;
                if !coverages.get(k)?.contains(GlyphId(g)) {
                    return None;
                }
            }
            let len = coverages.len() as usize + 1;
            Some(apply_records(gsub, *lookups, buf, pos, len, depth))
        }
    }
}

/// Type 6: chained contextual substitution (backtrack + input + lookahead).
fn apply_chain(
    gsub: &LayoutTable,
    c: &ChainedContextLookup,
    buf: &mut Vec<(u16, u8)>,
    pos: usize,
    depth: u8,
) -> Option<usize> {
    match c {
        ChainedContextLookup::Format1 { coverage, sets } => {
            let set = sets.get(coverage.get(GlyphId(buf.get(pos)?.0))?)?;
            for ri in 0..set.len() {
                let rule = set.get(ri)?;
                let len = rule.input.len() as usize + 1;
                if seq_matches_gids(&rule.input, buf, pos)
                    && backtrack_gids(&rule.backtrack, buf, pos)
                    && lookahead_gids(&rule.lookahead, buf, pos + len)
                {
                    return Some(apply_records(gsub, rule.lookups, buf, pos, len, depth));
                }
            }
            None
        }
        ChainedContextLookup::Format2 {
            coverage,
            backtrack_classes,
            input_classes,
            lookahead_classes,
            sets,
        } => {
            coverage.get(GlyphId(buf.get(pos)?.0))?;
            let set = sets.get(input_classes.get(GlyphId(buf[pos].0)))?;
            for ri in 0..set.len() {
                let rule = set.get(ri)?;
                let len = rule.input.len() as usize + 1;
                if seq_matches_classes(&rule.input, buf, pos, input_classes)
                    && backtrack_classes_match(&rule.backtrack, buf, pos, backtrack_classes)
                    && lookahead_classes_match(&rule.lookahead, buf, pos + len, lookahead_classes)
                {
                    return Some(apply_records(gsub, rule.lookups, buf, pos, len, depth));
                }
            }
            None
        }
        ChainedContextLookup::Format3 {
            coverage,
            backtrack_coverages,
            input_coverages,
            lookahead_coverages,
            lookups,
        } => {
            coverage.get(GlyphId(buf.get(pos)?.0))?;
            for k in 0..input_coverages.len() {
                let g = buf.get(pos + 1 + k as usize)?.0;
                if !input_coverages.get(k)?.contains(GlyphId(g)) {
                    return None;
                }
            }
            let len = input_coverages.len() as usize + 1;
            for k in 0..backtrack_coverages.len() {
                let idx = pos.checked_sub(1 + k as usize)?;
                if !backtrack_coverages.get(k)?.contains(GlyphId(buf[idx].0)) {
                    return None;
                }
            }
            for k in 0..lookahead_coverages.len() {
                let g = buf.get(pos + len + k as usize)?.0;
                if !lookahead_coverages.get(k)?.contains(GlyphId(g)) {
                    return None;
                }
            }
            Some(apply_records(gsub, *lookups, buf, pos, len, depth))
        }
    }
}

/// Apply a context's nested lookup records at their input positions, tracking
/// length changes so later records target the right (shifted) glyphs. Returns
/// the input region's new length, for the caller's scan to advance past.
fn apply_records(
    gsub: &LayoutTable,
    records: ttf_parser::LazyArray16<SequenceLookupRecord>,
    buf: &mut Vec<(u16, u8)>,
    start: usize,
    input_len: usize,
    depth: u8,
) -> usize {
    if depth > MAX_DEPTH {
        return input_len;
    }
    let mut delta: isize = 0;
    for r in 0..records.len() {
        let Some(rec) = records.get(r) else { continue };
        let p = start as isize + rec.sequence_index as isize + delta;
        if p < 0 || p as usize >= buf.len() {
            continue;
        }
        let before = buf.len();
        apply_lookup_at(gsub, rec.lookup_list_index, buf, p as usize, depth + 1);
        delta += buf.len() as isize - before as isize;
    }
    (input_len as isize + delta).max(1) as usize
}

type U16Array<'a> = ttf_parser::LazyArray16<'a, u16>;

/// Input glyphs 1.. (index 0 is implied by the subtable coverage) match `seq`.
fn seq_matches_gids(seq: &U16Array, buf: &[(u16, u8)], pos: usize) -> bool {
    (0..seq.len()).all(|k| {
        seq.get(k)
            .zip(buf.get(pos + 1 + k as usize))
            .is_some_and(|(g, e)| g == e.0)
    })
}

/// Backtrack glyphs (in reverse, starting just before `pos`) match `seq`.
fn backtrack_gids(seq: &U16Array, buf: &[(u16, u8)], pos: usize) -> bool {
    (0..seq.len()).all(|k| {
        let idx = match pos.checked_sub(1 + k as usize) {
            Some(i) => i,
            None => return false,
        };
        seq.get(k) == Some(buf[idx].0)
    })
}

/// Lookahead glyphs (starting at `from`) match `seq`.
fn lookahead_gids(seq: &U16Array, buf: &[(u16, u8)], from: usize) -> bool {
    (0..seq.len()).all(|k| {
        seq.get(k)
            .zip(buf.get(from + k as usize))
            .is_some_and(|(g, e)| g == e.0)
    })
}

fn seq_matches_classes(
    seq: &U16Array,
    buf: &[(u16, u8)],
    pos: usize,
    cd: &ttf_parser::opentype_layout::ClassDefinition,
) -> bool {
    (0..seq.len()).all(|k| match buf.get(pos + 1 + k as usize) {
        Some(e) => seq.get(k) == Some(cd.get(GlyphId(e.0))),
        None => false,
    })
}

fn backtrack_classes_match(
    seq: &U16Array,
    buf: &[(u16, u8)],
    pos: usize,
    cd: &ttf_parser::opentype_layout::ClassDefinition,
) -> bool {
    (0..seq.len()).all(|k| match pos.checked_sub(1 + k as usize) {
        Some(idx) => seq.get(k) == Some(cd.get(GlyphId(buf[idx].0))),
        None => false,
    })
}

fn lookahead_classes_match(
    seq: &U16Array,
    buf: &[(u16, u8)],
    from: usize,
    cd: &ttf_parser::opentype_layout::ClassDefinition,
) -> bool {
    (0..seq.len()).all(|k| match buf.get(from + k as usize) {
        Some(e) => seq.get(k) == Some(cd.get(GlyphId(e.0))),
        None => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FONT: &[u8] = include_bytes!("testdata/ligtest.ttf");

    fn shaper() -> Shaper {
        Shaper::new(FONT.to_vec()).expect("test font has liga/calt GSUB")
    }

    #[test]
    fn maps_chars_to_gids() {
        let s = shaper();
        assert_eq!((s.gid('f'), s.gid('i')), (5, 6));
        assert_eq!((s.gid('<'), s.gid('=')), (4, 2));
        assert_eq!(s.gid('?'), 0); // not in the font
    }

    #[test]
    fn liga_merges_fi_into_one_wide_glyph() {
        let s = shaper();
        // f i -> fi (gid 7), spanning two cells.
        assert_eq!(s.shape(&[5, 6]), vec![(7, 2)]);
        // Back-to-back: f i f i -> fi fi.
        assert_eq!(s.shape(&[5, 6, 5, 6]), vec![(7, 2), (7, 2)]);
    }

    #[test]
    fn calt_chained_context_substitutes_in_context_only() {
        let s = shaper();
        // less equal -> less, eq_le: the chained-context lookup runs the nested
        // single substitution on `equal` because it follows `less`.
        assert_eq!(s.shape(&[4, 2]), vec![(4, 1), (8, 1)]);
        // equal alone is untouched (no preceding less).
        assert_eq!(s.shape(&[2]), vec![(2, 1)]);
        // greater equal is untouched (context requires less).
        assert_eq!(s.shape(&[3, 2]), vec![(3, 1), (2, 1)]);
    }

    #[test]
    fn runs_without_rules_pass_through() {
        let s = shaper();
        assert_eq!(s.shape(&[5, 2]), vec![(5, 1), (2, 1)]); // f =
        assert_eq!(s.shape(&[]), vec![]);
    }
}
