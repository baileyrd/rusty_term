//! Arabic contextual shaping (bidi phase 3): pick each letter's presentation
//! form — isolated, final, initial, or medial — from its joining context, in
//! **logical order** (joining is defined on the logical sequence; the bidi
//! reorder happens after). The substitution targets the Unicode Arabic
//! Presentation Forms blocks, the classic terminal approach (mlterm et al.):
//! monospace fonts with Arabic coverage carry these glyphs, and the cell
//! grid keeps its one-char-per-cell model. Fonts with proper GSUB
//! `init`/`medi`/`fina` would render better with real shaping — noted as
//! future work in `docs/research/bidi-scoping-2026-07.md`; the mandatory
//! lam-alef ligature is likewise drawn as two contextual forms here.
//!
//! Tables are frozen in [`super::arabic_tables`], generated from UCD 17.0.0
//! by `tools/gen_arabic_tables.py`.

use super::arabic_tables::{FORMS, JOIN_RANGES};

/// ArabicShaping.txt joining type. `U` (non-joining, the default) is what a
/// table miss means; `T` (transparent — combining marks) is skipped when
/// scanning for the join neighbor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Joining {
    /// Right-joining (connects to the *preceding* logical char only).
    R,
    /// Left-joining (rare; connects to the following logical char only).
    L,
    /// Dual-joining.
    D,
    /// Join-causing but shapeless itself (tatweel, ZWJ).
    C,
    /// Transparent: doesn't join, doesn't break a join (diacritics).
    T,
}

/// The joining type of `ch`, `None` for non-joining (`U`).
fn joining(ch: char) -> Option<Joining> {
    let cp = ch as u32;
    JOIN_RANGES
        .binary_search_by(|&(a, b, _)| {
            if cp < a {
                std::cmp::Ordering::Greater
            } else if cp > b {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .ok()
        .map(|i| JOIN_RANGES[i].2)
}

/// The presentation form of `base` (0 isolated, 1 final, 2 initial,
/// 3 medial), or `None` when the block has no such form for it.
fn form(base: char, f: u8) -> Option<char> {
    let cp = base as u32;
    FORMS
        .binary_search_by_key(&(cp, f), |&(b, ff, _)| (b, ff))
        .ok()
        .and_then(|i| char::from_u32(FORMS[i].2))
}

/// Contextually shape one row's chars (logical order): for every Arabic
/// letter with presentation forms, substitute the joined form its neighbors
/// call for. Everything else comes back unchanged. `None` when the row has
/// nothing to shape (the common case — one cheap scan).
pub(crate) fn shape_row(text: &[char]) -> Option<Vec<char>> {
    if !text.iter().any(|&c| joining(c).is_some()) {
        return None;
    }
    let types: Vec<Option<Joining>> = text.iter().map(|&c| joining(c)).collect();
    // The nearest non-transparent joining neighbor on each side.
    let neighbor = |from: usize, forward: bool| -> Option<Joining> {
        let mut i = from;
        loop {
            i = if forward {
                if i + 1 >= text.len() {
                    return None;
                }
                i + 1
            } else {
                if i == 0 {
                    return None;
                }
                i - 1
            };
            match types[i] {
                Some(Joining::T) => continue,
                t => return t,
            }
        }
    };
    let mut out = text.to_vec();
    let mut changed = false;
    for (i, &ch) in text.iter().enumerate() {
        let t = match types[i] {
            Some(t @ (Joining::R | Joining::L | Joining::D)) => t,
            _ => continue, // U/C/T never substitute
        };
        // Does this char connect backward (to the previous logical char) /
        // forward (to the next)? The neighbor must join from its own side.
        let prev = neighbor(i, false);
        let next = neighbor(i, true);
        let back = matches!(t, Joining::R | Joining::D)
            && matches!(prev, Some(Joining::D | Joining::L | Joining::C));
        let fwd = matches!(t, Joining::L | Joining::D)
            && matches!(next, Some(Joining::D | Joining::R | Joining::C));
        let f = match (back, fwd) {
            (true, true) => 3,   // medial
            (true, false) => 1,  // final
            (false, true) => 2,  // initial
            (false, false) => 0, // isolated
        };
        // Isolated substitution is a no-op visually for most fonts, but the
        // presentation block's isolated glyph is the safe canonical pick.
        if let Some(shaped) = form(ch, f)
            && shaped != ch
        {
            out[i] = shaped;
            changed = true;
        }
    }
    changed.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joining_types_cover_the_staples() {
        assert_eq!(joining('\u{628}'), Some(Joining::D)); // beh
        assert_eq!(joining('\u{627}'), Some(Joining::R)); // alef
        assert_eq!(joining('\u{640}'), Some(Joining::C)); // tatweel
        assert_eq!(joining('\u{64B}'), Some(Joining::T)); // fathatan
        assert_eq!(joining('a'), None);
        assert_eq!(joining('\u{621}'), None); // hamza: non-joining
    }

    #[test]
    fn shapes_a_word_with_medial_final_and_initial_forms() {
        // "محمد" (Muhammad): meem + hah + meem + dal, logical order.
        // meem(D) initial, hah(D) medial, meem(D) medial, dal(R) final.
        let t: Vec<char> = "\u{645}\u{62D}\u{645}\u{62F}".chars().collect();
        let s = shape_row(&t).expect("shapes");
        assert_eq!(s[0], '\u{FEE3}', "initial meem");
        assert_eq!(s[1], '\u{FEA4}', "medial hah");
        assert_eq!(s[2], '\u{FEE4}', "medial meem");
        assert_eq!(s[3], '\u{FEAA}', "final dal");
    }

    #[test]
    fn non_joining_neighbors_isolate_and_transparents_pass_through() {
        // beh + fathatan(T) + beh: the mark is transparent, so both behs
        // still join through it (initial + final).
        let t: Vec<char> = "\u{628}\u{64B}\u{628}".chars().collect();
        let s = shape_row(&t).expect("shapes");
        assert_eq!(s[0], '\u{FE91}', "initial beh joins through the mark");
        assert_eq!(s[1], '\u{64B}', "mark itself untouched");
        assert_eq!(s[2], '\u{FE90}', "final beh");
        // A lone letter after a space is isolated.
        let t2: Vec<char> = " \u{628} ".chars().collect();
        let s2 = shape_row(&t2).expect("shapes");
        assert_eq!(s2[1], '\u{FE8F}', "isolated beh");
        // Pure-Latin rows don't allocate.
        assert!(shape_row(&"hello".chars().collect::<Vec<_>>()).is_none());
    }

    #[test]
    fn right_joining_alef_takes_final_after_dual_joiner() {
        // lam + alef: alef(R) connects backward to lam(D) -> final alef;
        // lam gets its initial form. (The lam-alef ligature proper is
        // documented future work.)
        let t: Vec<char> = "\u{644}\u{627}".chars().collect();
        let s = shape_row(&t).expect("shapes");
        assert_eq!(s[0], '\u{FEDF}', "initial lam");
        assert_eq!(s[1], '\u{FE8E}', "final alef");
    }
}
