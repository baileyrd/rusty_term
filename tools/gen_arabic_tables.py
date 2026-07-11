#!/usr/bin/env python3
"""Generate src/core/arabic_tables.rs from released UCD files.

Usage: gen_arabic_tables.py <ucd_dir> with ArabicShaping.txt and
UnicodeData.txt from one released UCD version. Output is frozen and checked
in (see tools/gen_bidi_tables.py for the same pattern).
"""
import sys

def main(ucd):
    # Joining types (R/L/D/C/U/T); U is the default, omitted from the table.
    # ArabicShaping.txt only lists R/L/D/C explicitly: everything of general
    # category Mn, Me, or Cf that it doesn't list is T (transparent), per the
    # file's own header note — pull those from UnicodeData.txt.
    explicit = {}
    for line in open(f"{ucd}/ArabicShaping.txt"):
        line = line.split("#")[0].strip()
        if not line:
            continue
        cp, _name, jt, _grp = [p.strip() for p in line.split(";")]
        explicit[int(cp, 16)] = jt
    joins = [(cp, jt) for cp, jt in explicit.items() if jt != "U"]
    range_start = None
    for line in open(f"{ucd}/UnicodeData.txt"):
        f = line.split(";")
        cp = int(f[0], 16)
        cat = f[2]
        # Handle First/Last range pairs (none are Mn/Me/Cf in practice, but
        # stay correct if that changes).
        if f[1].endswith(", First>"):
            range_start = (cp, cat)
            continue
        if f[1].endswith(", Last>"):
            start, cat0 = range_start
            if cat0 in ("Mn", "Me", "Cf"):
                for c in range(start, cp + 1):
                    if c not in explicit:
                        joins.append((c, "T"))
            range_start = None
            continue
        if cat in ("Mn", "Me", "Cf") and cp not in explicit:
            joins.append((cp, "T"))
    joins.sort()
    # Merge into ranges of equal type.
    ranges = []
    for cp, jt in joins:
        if ranges and ranges[-1][1] == cp - 1 and ranges[-1][2] == jt:
            ranges[-1] = (ranges[-1][0], cp, jt)
        else:
            ranges.append((cp, cp, jt))

    # Presentation forms: single-char <isolated>/<initial>/<medial>/<final>
    # compatibility decompositions in the Arabic presentation blocks.
    forms = {}  # (base, form) -> presentation cp
    for line in open(f"{ucd}/UnicodeData.txt"):
        f = line.split(";")
        cp = int(f[0], 16)
        if not (0xFB50 <= cp <= 0xFDFF or 0xFE70 <= cp <= 0xFEFF):
            continue
        d = f[5]
        for tag, form in [("<isolated>", 0), ("<final>", 1),
                          ("<initial>", 2), ("<medial>", 3)]:
            if d.startswith(tag):
                parts = d[len(tag):].split()
                if len(parts) == 1:  # single-char decompositions only
                    base = int(parts[0], 16)
                    forms.setdefault((base, form), cp)
    entries = sorted(forms.items())

    out = ["//! Frozen Arabic-shaping data, generated from UCD 17.0.0 by",
           "//! `tools/gen_arabic_tables.py` (do not edit by hand; regenerate from",
           "//! released UCD files and review the diff). Sources: ArabicShaping.txt",
           "//! (joining types, `U` omitted as the lookup fallback) and",
           "//! UnicodeData.txt (single-char presentation-form decompositions from",
           "//! the Arabic Presentation Forms blocks).",
           "",
           "use super::arabic::Joining;",
           "",
           "/// `(first, last, joining type)` — sorted, non-overlapping.",
           "#[rustfmt::skip]",
           "pub(super) const JOIN_RANGES: &[(u32, u32, Joining)] = &["]
    out += [f"    (0x{a:04X}, 0x{b:04X}, Joining::{t})," for a, b, t in ranges]
    out += ["];", "",
            "/// `(base char, form 0=isolated 1=final 2=initial 3=medial,",
            "/// presentation form)` — sorted by (base, form).",
            "#[rustfmt::skip]",
            "pub(super) const FORMS: &[(u32, u8, u32)] = &["]
    out += [f"    (0x{b:04X}, {f}, 0x{p:04X})," for (b, f), p in entries]
    out += ["];"]
    open("src/core/arabic_tables.rs", "w").write("\n".join(out) + "\n")
    print(f"wrote {len(ranges)} join ranges, {len(entries)} presentation forms")

if __name__ == "__main__":
    main(sys.argv[1])
