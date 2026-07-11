//! UAX #9 bidirectional algorithm (no crates): resolved embedding levels and
//! the visual↔logical reorder map, per paragraph (= one logical line here).
//!
//! Storage everywhere else in this codebase stays in **logical order**; this
//! module is consumed at display time only, per the Terminal-WG "BiDi in
//! Terminal Emulators" model (see `docs/research/bidi-scoping-2026-07.md`).
//! Implemented in full: P2–P3, X1–X10 (explicit embeddings, overrides, and
//! isolates, 125-deep), W1–W7, N0 (bracket pairs, BD16), N1–N2, I1–I2, and
//! L1–L2 into a reorder map. Rules L3/L4 (mirroring) are exposed as
//! [`mirrored`] for the renderers.
//!
//! Explicit-code characters removed by rule X9 (LRE/RLE/LRO/RLO/PDF, plus
//! original-BN characters) are *retained* positionally — a terminal cell
//! exists for every stored char — but excluded from the weak/neutral rules
//! and given the level of the nearest preceding retained character so they
//! never split an L2 reversal run.
//!
//! Data tables are frozen in [`super::bidi_tables`], generated from UCD
//! 17.0.0 by `tools/gen_bidi_tables.py`. Conformance: a deterministic 2,479
//! case sample of the UCD `BidiCharacterTest.txt` runs in the suite.

use super::bidi_tables::{BRACKETS, CLASS_RANGES, MIRRORED};

/// UAX #9 Bidi_Class, spelled exactly as the spec names them (the acronyms
/// *are* the identifiers every bidi discussion uses).
#[allow(clippy::upper_case_acronyms)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Class {
    L, R, AL, EN, ES, ET, AN, CS, NSM, BN, B, S, WS, ON,
    LRE, RLE, LRO, RLO, PDF, LRI, RLI, FSI, PDI,
}

use Class::*;

/// Maximum explicit embedding depth (spec constant `max_depth`).
const MAX_DEPTH: u8 = 125;

/// The Bidi_Class of `ch` (frozen table lookup; the table omits `L`, the
/// overall default, so misses fall back to it).
pub(crate) fn class(ch: char) -> Class {
    let cp = ch as u32;
    match CLASS_RANGES.binary_search_by(|&(a, b, _)| {
        if cp < a {
            std::cmp::Ordering::Greater
        } else if cp > b {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    }) {
        Ok(i) => CLASS_RANGES[i].2,
        Err(_) => L,
    }
}

/// The mirrored form of `ch` (rule L4): what to *draw* when `ch` sits in an
/// RTL (odd-level) run. `None` when the char has no mirror.
#[cfg_attr(not(any(test, feature = "gui")), allow(dead_code))]
pub(crate) fn mirrored(ch: char) -> Option<char> {
    let cp = ch as u32;
    MIRRORED
        .binary_search_by_key(&cp, |&(a, _)| a)
        .ok()
        .and_then(|i| char::from_u32(MIRRORED[i].1))
}

/// Cheap pre-filter: does `text` contain anything that could produce an RTL
/// run? Pure-LTR rows skip the whole algorithm on the strength of this.
pub(crate) fn has_rtl(text: &[char]) -> bool {
    text.iter().any(|&c| matches!(class(c), R | AL | AN | RLE | RLO | RLI))
}

/// Whether rule X9 removes this original class (retained here as
/// positionally-present but rule-invisible).
fn removed_by_x9(c: Class) -> bool {
    matches!(c, RLE | LRE | RLO | LRO | PDF | BN)
}

/// P2–P3: the paragraph embedding level from the first strong character,
/// skipping isolate runs. `0` (LTR) when there is none.
pub(crate) fn paragraph_level(text: &[char]) -> u8 {
    first_strong(text.iter().map(|&c| class(c))).map_or(0, |rtl| rtl as u8)
}

/// First-strong scan shared by P2–P3 and FSI resolution: `Some(true)` for
/// R/AL, `Some(false)` for L, skipping isolate runs.
fn first_strong(classes: impl Iterator<Item = Class>) -> Option<bool> {
    let mut isolate = 0usize;
    for c in classes {
        match c {
            LRI | RLI | FSI => isolate += 1,
            PDI if isolate > 0 => isolate -= 1,
            L if isolate == 0 => return Some(false),
            R | AL if isolate == 0 => return Some(true),
            _ => {}
        }
    }
    None
}

/// One directional-status-stack entry (rules X1–X8).
#[derive(Clone, Copy)]
struct Status {
    level: u8,
    /// Directional override: forces subsequent types to L or R.
    over: Option<Class>,
    isolate: bool,
}

/// Resolved embedding levels for `text` at paragraph level `para`
/// (rules X1–I2; L1 applies in [`visual_map`], which owns line context).
pub(crate) fn resolve_levels(text: &[char], para: u8) -> Vec<u8> {
    let n = text.len();
    let orig: Vec<Class> = text.iter().map(|&c| class(c)).collect();
    let mut types = orig.clone();
    let mut levels = vec![para; n];

    // X1–X8: explicit embeddings, overrides, isolates.
    let mut stack = vec![Status { level: para, over: None, isolate: false }];
    let (mut overflow_iso, mut overflow_emb, mut valid_iso) = (0usize, 0usize, 0usize);
    let next_level = |cur: u8, rtl: bool| -> u8 {
        if rtl { (cur + 1) | 1 } else { (cur + 2) & !1 }
    };
    for i in 0..n {
        let top = *stack.last().unwrap();
        match orig[i] {
            RLE | LRE | RLO | LRO => {
                levels[i] = top.level;
                types[i] = BN; // X9, retained
                let rtl = matches!(orig[i], RLE | RLO);
                let new = next_level(top.level, rtl);
                if new <= MAX_DEPTH && overflow_iso == 0 && overflow_emb == 0 {
                    let over = match orig[i] {
                        RLO => Some(R),
                        LRO => Some(L),
                        _ => None,
                    };
                    stack.push(Status { level: new, over, isolate: false });
                } else if overflow_iso == 0 {
                    overflow_emb += 1;
                }
            }
            RLI | LRI | FSI => {
                // FSI: direction of the first strong char inside (BD9).
                let rtl = if orig[i] == FSI {
                    isolate_content_rtl(&orig, i)
                } else {
                    orig[i] == RLI
                };
                levels[i] = top.level;
                if let Some(o) = top.over {
                    types[i] = o;
                }
                let new = next_level(top.level, rtl);
                if new <= MAX_DEPTH && overflow_iso == 0 && overflow_emb == 0 {
                    valid_iso += 1;
                    stack.push(Status { level: new, over: None, isolate: true });
                } else {
                    overflow_iso += 1;
                }
            }
            PDI => {
                if overflow_iso > 0 {
                    overflow_iso -= 1;
                } else if valid_iso > 0 {
                    while !stack.last().unwrap().isolate {
                        stack.pop();
                    }
                    stack.pop();
                    valid_iso -= 1;
                }
                let top = *stack.last().unwrap();
                levels[i] = top.level;
                if let Some(o) = top.over {
                    types[i] = o;
                }
            }
            PDF => {
                levels[i] = top.level;
                types[i] = BN; // X9, retained
                if overflow_iso > 0 {
                    // ignored inside an overflowing isolate
                } else if overflow_emb > 0 {
                    overflow_emb -= 1;
                } else if !top.isolate && stack.len() > 1 {
                    stack.pop();
                }
            }
            B => {
                levels[i] = para; // X8 (won't occur mid-line here)
            }
            _ => {
                levels[i] = top.level;
                if let Some(o) = top.over {
                    types[i] = o;
                }
            }
        }
    }

    // The retained X9-removed chars must never split a level run: give them
    // the level of the nearest preceding retained char (or the paragraph).
    let mut prev_level = para;
    for i in 0..n {
        if removed_by_x9(orig[i]) {
            levels[i] = prev_level;
        } else {
            prev_level = levels[i];
        }
    }

    // Filtered view for X10/W/N: indices of retained chars.
    let kept: Vec<usize> = (0..n).filter(|&i| !removed_by_x9(orig[i])).collect();
    if kept.is_empty() {
        return levels;
    }

    // X10: level runs over the filtered sequence, chained into isolating run
    // sequences (a run ending in an isolate initiator joins the run starting
    // at its matching PDI).
    let matching_pdi = matching_pdis(&orig, &kept);
    let mut runs: Vec<Vec<usize>> = Vec::new(); // runs of `kept` positions
    {
        let mut cur: Vec<usize> = Vec::new();
        for (k, &i) in kept.iter().enumerate() {
            if let Some(&last) = cur.last()
                && levels[kept[last]] != levels[i]
            {
                runs.push(std::mem::take(&mut cur));
            }
            cur.push(k);
        }
        if !cur.is_empty() {
            runs.push(cur);
        }
    }
    let mut used = vec![false; runs.len()];
    let run_of_kpos: Vec<usize> = {
        let mut v = vec![0usize; kept.len()];
        for (ri, run) in runs.iter().enumerate() {
            for &k in run {
                v[k] = ri;
            }
        }
        v
    };
    let mut sequences: Vec<Vec<usize>> = Vec::new(); // kept-positions
    for ri in 0..runs.len() {
        if used[ri] {
            continue;
        }
        // Only start a new sequence at a run whose first char is not a PDI
        // matching some isolate initiator (those get chained onto).
        let first_k = runs[ri][0];
        if orig[kept[first_k]] == PDI && matching_pdi.iter().any(|&(_, p)| p == Some(kept[first_k])) {
            continue;
        }
        let mut seq: Vec<usize> = Vec::new();
        let mut cur = ri;
        loop {
            used[cur] = true;
            seq.extend(runs[cur].iter().copied());
            let &last_k = runs[cur].last().unwrap();
            let last_i = kept[last_k];
            if matches!(orig[last_i], LRI | RLI | FSI)
                && let Some(&(_, Some(pdi_i))) =
                    matching_pdi.iter().find(|&&(ini, _)| ini == last_i)
            {
                let pdi_k = kept.iter().position(|&i| i == pdi_i).unwrap();
                let nxt = run_of_kpos[pdi_k];
                if !used[nxt] {
                    cur = nxt;
                    continue;
                }
            }
            break;
        }
        sequences.push(seq);
    }

    // W and N rules per isolating run sequence.
    for seq in &sequences {
        let seq_level = levels[kept[seq[0]]];
        // sos/eos: compare against the adjacent retained char outside the
        // sequence (paragraph level at the edges; eos uses the paragraph
        // level when the sequence ends with an unmatched isolate initiator).
        let first_k = seq[0];
        let prev_level = if first_k == 0 { para } else { levels[kept[first_k - 1]] };
        let sos = if !seq_level.max(prev_level).is_multiple_of(2) { R } else { L };
        let &last_k = seq.last().unwrap();
        let last_i = kept[last_k];
        let ends_unmatched_iso = matches!(orig[last_i], LRI | RLI | FSI)
            && matching_pdi.iter().any(|&(ini, p)| ini == last_i && p.is_none());
        let next_level_ = if last_k + 1 >= kept.len() || ends_unmatched_iso {
            para
        } else {
            levels[kept[last_k + 1]]
        };
        let eos = if !levels[kept[last_k]].max(next_level_).is_multiple_of(2) { R } else { L };

        weak_and_neutral(seq, &kept, text, &orig, &mut types, sos, eos, seq_level);
    }

    // I1–I2: resolved levels from resolved types.
    for &i in &kept {
        let lvl = levels[i];
        levels[i] = match (lvl.is_multiple_of(2), types[i]) {
            (true, R) => lvl + 1,
            (true, AN | EN) => lvl + 2,
            (false, L | EN | AN) => lvl + 1,
            _ => lvl,
        };
    }
    // Re-run the no-split rule now levels moved.
    let mut prev_level = para;
    for i in 0..n {
        if removed_by_x9(orig[i]) {
            levels[i] = prev_level;
        } else {
            prev_level = levels[i];
        }
    }
    levels
}

/// FSI content direction: first strong char between the initiator at `at`
/// and its matching PDI.
fn isolate_content_rtl(orig: &[Class], at: usize) -> bool {
    let mut depth = 0usize;
    let inner = orig[at + 1..].iter().take_while(|&&c| {
        match c {
            LRI | RLI | FSI => depth += 1,
            PDI => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            _ => {}
        }
        true
    });
    first_strong(inner.copied()).unwrap_or(false)
}

/// For every isolate initiator (by original index): its matching PDI's
/// original index, or `None` when unmatched (BD9).
fn matching_pdis(orig: &[Class], kept: &[usize]) -> Vec<(usize, Option<usize>)> {
    let mut out = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for &i in kept {
        match orig[i] {
            LRI | RLI | FSI => stack.push(i),
            PDI => {
                if let Some(ini) = stack.pop() {
                    out.push((ini, Some(i)));
                }
            }
            _ => {}
        }
    }
    for ini in stack {
        out.push((ini, None));
    }
    out
}

/// W1–W7 and N0–N2 over one isolating run sequence (`seq` = positions into
/// `kept`).
#[allow(clippy::too_many_arguments)]
fn weak_and_neutral(
    seq: &[usize],
    kept: &[usize],
    text: &[char],
    orig: &[Class],
    types: &mut [Class],
    sos: Class,
    eos: Class,
    seq_level: u8,
) {
    let idx: Vec<usize> = seq.iter().map(|&k| kept[k]).collect();
    let m = idx.len();
    let e = if seq_level.is_multiple_of(2) { L } else { R };

    // W1: NSM takes the type of the previous char (sos at the start;
    // isolate initiators and PDI yield ON).
    for j in 0..m {
        if types[idx[j]] == NSM {
            types[idx[j]] = if j == 0 {
                sos
            } else {
                match types[idx[j - 1]] {
                    LRI | RLI | FSI | PDI => ON,
                    t => t,
                }
            };
        }
    }
    // W2: EN → AN when the last strong type was AL.
    let mut strong = sos;
    for j in 0..m {
        match types[idx[j]] {
            L | R | AL => strong = types[idx[j]],
            EN if strong == AL => types[idx[j]] = AN,
            _ => {}
        }
    }
    // W3: AL → R.
    for j in 0..m {
        if types[idx[j]] == AL {
            types[idx[j]] = R;
        }
    }
    // W4: single ES between EN,EN → EN; single CS between EN,EN or AN,AN.
    for j in 1..m.saturating_sub(1) {
        let (p, c, nx) = (types[idx[j - 1]], types[idx[j]], types[idx[j + 1]]);
        if c == ES && p == EN && nx == EN {
            types[idx[j]] = EN;
        }
        if c == CS && ((p == EN && nx == EN) || (p == AN && nx == AN)) {
            types[idx[j]] = p; // EN,CS,EN -> EN and AN,CS,AN -> AN alike
        }
    }
    // W5: runs of ET adjacent to EN → EN.
    let mut j = 0;
    while j < m {
        if types[idx[j]] == ET {
            let start = j;
            while j < m && types[idx[j]] == ET {
                j += 1;
            }
            let before_en = start > 0 && types[idx[start - 1]] == EN;
            let after_en = j < m && types[idx[j]] == EN;
            if before_en || after_en {
                for t in &idx[start..j] {
                    types[*t] = EN;
                }
            }
        } else {
            j += 1;
        }
    }
    // W6: leftover separators/terminators → ON.
    for &i in &idx {
        if matches!(types[i], ES | ET | CS) {
            types[i] = ON;
        }
    }
    // W7: EN → L when the last strong type was L.
    let mut strong = sos;
    for &i in &idx {
        match types[i] {
            L | R => strong = types[i],
            EN if strong == L => types[i] = L,
            _ => {}
        }
    }

    // N0: bracket pairs (BD16). Stack-pair opening brackets (canonical
    // equivalence: U+2329/U+3008 and U+232A/U+3009 pair across forms).
    let canon = |cp: u32| -> u32 {
        match cp {
            0x3008 => 0x2329,
            0x3009 => 0x232A,
            c => c,
        }
    };
    let bracket = |ch: char| -> Option<(u32, bool)> {
        let cp = ch as u32;
        BRACKETS
            .binary_search_by_key(&cp, |&(a, _, _)| a)
            .ok()
            .map(|i| (canon(BRACKETS[i].1), BRACKETS[i].2))
    };
    let mut stack: Vec<(u32, usize)> = Vec::new(); // (expected close, seq pos)
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for (j, &i) in idx.iter().enumerate() {
        if types[i] != ON {
            continue;
        }
        if let Some((close, open)) = bracket(text[i]) {
            if open {
                if stack.len() < 63 {
                    stack.push((close, j));
                } else {
                    break; // BD16: stop pairing on overflow
                }
            } else {
                let this = canon(text[i] as u32);
                if let Some(pos) = stack.iter().rposition(|&(c, _)| c == this) {
                    pairs.push((stack[pos].1, j));
                    stack.truncate(pos);
                }
            }
        }
    }
    pairs.sort_unstable();
    let strong_of = |t: Class| -> Option<Class> {
        match t {
            L => Some(L),
            R | EN | AN => Some(R),
            _ => None,
        }
    };
    for &(open, close) in &pairs {
        // Strong types inside the pair.
        let mut saw_e = false;
        let mut saw_o = false;
        for j in open + 1..close {
            if let Some(s) = strong_of(types[idx[j]]) {
                if s == e {
                    saw_e = true;
                } else {
                    saw_o = true;
                }
            }
        }
        let new = if saw_e {
            Some(e)
        } else if saw_o {
            // Opposite-only: context before the opening bracket decides.
            let mut ctx = sos;
            for j in (0..open).rev() {
                if let Some(s) = strong_of(types[idx[j]]) {
                    ctx = s;
                    break;
                }
            }
            Some(if ctx != e { ctx } else { e })
        } else {
            None
        };
        if let Some(nt) = new {
            types[idx[open]] = nt;
            types[idx[close]] = nt;
            // Trailing NSMs (by original class) after each changed bracket
            // follow it.
            for &b in &[open, close] {
                for j in b + 1..m {
                    if orig[idx[j]] == NSM {
                        types[idx[j]] = nt;
                    } else {
                        break;
                    }
                }
            }
        }
    }

    // N1–N2: NI runs take the surrounding direction if it matches on both
    // sides (EN/AN count as R), else the embedding direction.
    let is_ni = |t: Class| matches!(t, B | S | WS | ON | FSI | LRI | RLI | PDI);
    let side = |t: Class| -> Option<Class> {
        match t {
            L => Some(L),
            R | EN | AN => Some(R),
            _ => None,
        }
    };
    let mut j = 0;
    while j < m {
        if is_ni(types[idx[j]]) {
            let start = j;
            while j < m && is_ni(types[idx[j]]) {
                j += 1;
            }
            let before = if start == 0 { sos } else { side(types[idx[start - 1]]).unwrap_or(e) };
            let after = if j == m { eos } else { side(types[idx[j]]).unwrap_or(e) };
            let fill = if before == after { before } else { e };
            for t in &idx[start..j] {
                types[*t] = fill;
            }
        } else {
            j += 1;
        }
    }
}

/// L1–L2: the visual→logical reorder map for one line. `base_rtl` forces the
/// paragraph direction (`None` = auto-detect, P2–P3). Returns `None` when
/// the result is the identity (pure LTR — the common case, checked cheaply
/// up front via [`has_rtl`]).
pub(crate) fn visual_map(text: &[char], base_rtl: Option<bool>) -> Option<Vec<u16>> {
    if text.is_empty() || text.len() > u16::MAX as usize {
        return None;
    }
    if base_rtl != Some(true) && !has_rtl(text) {
        return None;
    }
    let para = match base_rtl {
        Some(rtl) => rtl as u8,
        None => paragraph_level(text),
    };
    let (_, map) = reorder(text, para);
    if map.iter().enumerate().all(|(i, &l)| i as u16 == l) {
        return None;
    }
    Some(map)
}

/// The full pipeline for one line at a known paragraph level: post-L1
/// resolved levels plus the L2 visual→logical map (exactly what the UCD
/// conformance file describes per test case).
pub(crate) fn reorder(text: &[char], para: u8) -> (Vec<u8>, Vec<u16>) {
    let mut levels = resolve_levels(text, para);

    // L1: segment separators and trailing whitespace (incl. isolate
    // formatting and retained X9 characters) reset to the paragraph level.
    let orig: Vec<Class> = text.iter().map(|&c| class(c)).collect();
    let resettable =
        |c: Class| matches!(c, WS | FSI | LRI | RLI | PDI) || removed_by_x9(c);
    let mut i = text.len();
    while i > 0 {
        i -= 1;
        match orig[i] {
            B | S => levels[i] = para,
            c if resettable(c) => levels[i] = para,
            _ => break,
        }
    }
    for i in 0..text.len() {
        if matches!(orig[i], B | S) {
            levels[i] = para;
            let mut j = i;
            while j > 0 && resettable(orig[j - 1]) {
                j -= 1;
                levels[j] = para;
            }
        }
    }

    // L2: reverse runs from the highest level down to the lowest odd level.
    let mut map: Vec<u16> = (0..text.len() as u16).collect();
    let max = *levels.iter().max().unwrap_or(&0);
    let lowest_odd = levels.iter().copied().filter(|l| !l.is_multiple_of(2)).min().unwrap_or(1);
    let mut lvl = max;
    while lvl >= lowest_odd && lvl > 0 {
        let mut i = 0;
        while i < map.len() {
            if levels[map[i] as usize] >= lvl {
                let start = i;
                while i < map.len() && levels[map[i] as usize] >= lvl {
                    i += 1;
                }
                map[start..i].reverse();
            } else {
                i += 1;
            }
        }
        lvl -= 1;
    }
    (levels, map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::base64;

    #[test]
    fn class_lookup_covers_the_majors() {
        assert_eq!(class('a'), L);
        assert_eq!(class('\u{5D0}'), R); // Hebrew alef
        assert_eq!(class('\u{627}'), AL); // Arabic alef
        assert_eq!(class('1'), EN);
        assert_eq!(class('\u{660}'), AN); // Arabic-Indic zero
        assert_eq!(class(' '), WS);
        assert_eq!(class('('), ON);
        assert_eq!(class('\u{202B}'), RLE);
        assert_eq!(class('\u{2066}'), LRI);
        assert_eq!(class('\u{2069}'), PDI);
        assert_eq!(class('\u{300}'), NSM);
    }

    #[test]
    fn mirrored_maps_brackets_both_ways() {
        assert_eq!(mirrored('('), Some(')'));
        assert_eq!(mirrored(')'), Some('('));
        assert_eq!(mirrored('\u{2264}'), Some('\u{2265}'));
        assert_eq!(mirrored('a'), None);
    }

    #[test]
    fn pure_ltr_maps_to_identity_none() {
        let t: Vec<char> = "hello 123 (ok)".chars().collect();
        assert!(!has_rtl(&t));
        assert_eq!(visual_map(&t, None), None);
    }

    #[test]
    fn hebrew_run_reverses_and_numbers_stay_ltr() {
        // "abc XYZ 12" with XYZ Hebrew: the RTL run reverses, and the
        // trailing European digits — promoted two levels by rule I1 as
        // numbers inside an RTL context — render LTR to the *left* of the
        // Hebrew: "abc 12 <gimel><bet><alef>".
        let t: Vec<char> = "abc \u{5D0}\u{5D1}\u{5D2} 12".chars().collect();
        let map = visual_map(&t, None).expect("mixed line reorders");
        assert_eq!(map, vec![0, 1, 2, 3, 8, 9, 7, 6, 5, 4]);
    }

    /// The UCD conformance file (a deterministic 1-in-37 sample of
    /// BidiCharacterTest-17.0.0.txt, zlib-compressed and inflated by the
    /// in-house decompressor). Each case checks the auto-detected paragraph
    /// level, the post-L1 resolved level of every retained character, and
    /// the visual order of the retained characters.
    #[test]
    fn ucd_bidi_character_test_sample_conforms() {
        const FIXTURE_ZB64: &str = "eNqtfW1yKzuu5P+zCi9BpO/RkUK/xqPZUy9/LEulwkcmCKAcHdGvu981hSJBEAQTmae/99PH6e99fJxO8/L4T/Pxb5/f//Xv1+N/O3//2/nv498e/+nv/fG/\
/b/Hv10f/9u/x79dbqfvf42P7zE+nv9u/nV7/G/z47+Pz4+/H+ePfx+Xj+vH+P6Hv//p+TE+/3wPvhnw+fltwPk8Xz/x+fn5M/r8Gfnn329/f0Z6/OfTn++f\
//6fnyZ9/9H3f/q/j7/8/tHT53j/p5/RHqZ/jza+/zU//vc9wv9eI87Hv25Pq84fj9G/rX2O/LTpNdT19bfPf/7085cP2+e/1z/3+ornP/ey9/a29DHTm31y\
0OfkvYy5PT/t9Y9va/L4xJ/5//mBn4X5+djH/P+s2Onf13PZvn/g/zz+63379u//x+tHz5/b/+P83+Nvr4+/+O+1qH/vf1//23O1n8t7fn/J2Obp8c0///rf\
9zw9/rX93/lj+Oc2p7dvz5nfY37/Pz6//7/Pdf9Z7Y/x38c4f4zv/3p5eMH1e+LH9fvXP+b338330s7zZZuBlznjZ642p3ovgFio+fNRp9dC/Zi9L8DjH9kG\
vT7+03CD7sN+/Cf/4LUYz79Sk7Kt2O6Mu9nbX4lfGOYXzsP8sz9LK+3/nkrxC2+v2OxSTgRM0m60/eBVfMNUfyBn/fXPvqdq6qmSn/Lx1/3l/kXfvyy3BNjE\
ZqLlD79/c4DftDP+sy2e1urtKrfs6y83Q/cf3v9SLRj6zf3ntr88yd9k32mW+2mtcRL4nfJHLnJV5NwO9JvmLzeP2ed2/8vT7gfbb3pPOO0euq/n9OsplvL9\
m49g9frO8frSgVblZxuI7zwFW07+Y2Z20u76/r33jg2nVM+Jjivhb4JQtf9Xtenxb4ogAX5zkN9En+jDwDMQkG0pooCYYBcP9E9/nO0Q1nkf0dfM9LcrPc9g\
YsX2X4eI9taKEVihVs2H9Pf8B1bsQzzTlAtceDcX0OH2wbZjS6yGswKcLWZCbBTBKyI2jHJBcLwt5sJ48RnHz8gv9IfsJ1rKChX/rYsmvPNsPPpkF1UfIZsV\
Jkd45ydqX75y2KoVYHZtZGBzobzKDLHeI/pUe33SZ30u9PbQIU5aMbh36vRBx7qnX8xgLmiM9SsyFnvE2bMNodKnn8hprKAHtpqLsYqdJnBrV0/uEZm8nj70\
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\
xVPbfjA6fSdhQAEYtJoCApSoiug/imyoZk7NjW+UXnhNlDpg6ZqQRe8oE/e/LdVx/1+kJxR3HdV0BAH3hY9SB/SEfgOBSMlkUJSyWd82JI5SkPqjde57GOJY\
8ImUVt+/8Y0uVjIQKQK95qNSPXeVmK9DXfHrklQN1ZlZqAaqMyRQ0heJhupZPKeWrT8b+SlmTNakuzotFWLEmlaDy9GrGUrw+WfMNLBQlFnoCZ2P3fgW+KaS\
pVwAgtWke/coSKfY5GuXwIl1zl9SlDkPXOcnxFYZpQ7SKNbKUHDHzemYpeGO6tT6lo0cvnpeVuWlGId+zo8uJv6FV9xNupbqSm/1bgre43F+OvvKR6x+2lA+\
YjSjaPVLc8pKnem9z0WKOAlgX59NyTOhOR31eKrXyGs1lPXZZCn2V+KpsRSeUcvV537qy8eHFbrQ5+t4mtcPNqEPWVpT6CLOj7K+ujLnklUw+cLLCMiVOmlH\
UYbROzZu0ayJCwExmgqyTkSJ7KjR0BBVPWwR5WxSQzQa1N5ONkv3AbmijBRR6lUmuPKRTSZmV5WXnSTFexR1fjOnu0rPvptYBzf+fJykthQPkZwSEDnpaomp\
ogTOpUbh3Ler7+qnw/mp3PlpS1ml93Wfuj0Zu9aWnnGQRnt/JPJTVJMhlYnqGQU+X/qp9ICape9KB1/9+t5X5ZN2BQ3qXomFSrPf/n9yAMTc";
        let comp = base64::decode(FIXTURE_ZB64.as_bytes()).expect("fixture base64");
        let raw = crate::core::inflate::zlib_decompress(&comp, 1 << 20).expect("fixture zlib");
        let data = String::from_utf8(raw).expect("fixture utf8");
        let mut cases = 0usize;
        for line in data.lines() {
            let f: Vec<&str> = line.split(';').map(str::trim).collect();
            assert_eq!(f.len(), 5, "malformed fixture line: {line}");
            let text: Vec<char> = f[0]
                .split_whitespace()
                .map(|h| char::from_u32(u32::from_str_radix(h, 16).unwrap()).unwrap())
                .collect();
            let dir: u8 = f[1].parse().unwrap();
            let want_para: u8 = f[2].parse().unwrap();
            let para = match dir {
                0 => 0,
                1 => 1,
                _ => paragraph_level(&text),
            };
            assert_eq!(para, want_para, "paragraph level for {line}");
            let (levels, map) = reorder(&text, para);
            for (i, lv) in f[3].split_whitespace().enumerate() {
                if lv == "x" {
                    continue; // removed by X9: level unobservable
                }
                assert_eq!(
                    levels[i],
                    lv.parse::<u8>().unwrap(),
                    "level of char {i} in {line}"
                );
            }
            let removed: Vec<bool> = text
                .iter()
                .map(|&c| removed_by_x9(class(c)))
                .collect();
            let got: Vec<usize> = map
                .iter()
                .map(|&l| l as usize)
                .filter(|&l| !removed[l])
                .collect();
            let want: Vec<usize> =
                f[4].split_whitespace().map(|n| n.parse().unwrap()).collect();
            assert_eq!(got, want, "visual order for {line}");
            cases += 1;
        }
        assert!(cases > 2000, "fixture unexpectedly small: {cases}");
    }
}
