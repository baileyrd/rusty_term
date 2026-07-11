#!/usr/bin/env python3
"""Generate src/core/canon_tables.rs from UnicodeData.txt: each char with a
canonical decomposition maps to its recursively-resolved base (first) char,
giving the search pipeline accent-insensitive canonical folding. Hangul
syllables (algorithmic decompositions) are excluded — folding them to their
leading jamo would collide distinct syllables.
"""
import sys

def main(ucd):
    decomp = {}
    for line in open(f"{ucd}/UnicodeData.txt"):
        f = line.split(";")
        cp = int(f[0], 16)
        d = f[5]
        if not d or d.startswith("<"):
            continue  # none, or compatibility (tagged) — canonical only
        decomp[cp] = [int(x, 16) for x in d.split()]

    def base(cp, depth=0):
        if depth > 8 or cp not in decomp:
            return cp
        return base(decomp[cp][0], depth + 1)

    pairs = []
    for cp in sorted(decomp):
        b = base(cp)
        if b != cp:
            pairs.append((cp, b))

    out = ["//! Frozen canonical-base folding table, generated from UCD 17.0.0",
           "//! UnicodeData.txt by `tools/gen_canon_table.py` (do not edit; see the",
           "//! generator). Maps each char with a canonical decomposition to its",
           "//! recursively-resolved first (base) character, so search folds",
           "//! precomposed and decomposed spellings (and their accents) together.",
           "",
           "/// `(char, canonical base char)` — sorted by char.",
           "#[rustfmt::skip]",
           "pub(super) const CANON_BASE: &[(u32, u32)] = &["]
    out += [f"    (0x{a:04X}, 0x{b:04X})," for a, b in pairs]
    out += ["];"]
    open("src/core/canon_tables.rs", "w").write("\n".join(out) + "\n")
    print(f"wrote {len(pairs)} canonical-base pairs")

if __name__ == "__main__":
    main(sys.argv[1])
