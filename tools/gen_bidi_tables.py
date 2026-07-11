#!/usr/bin/env python3
"""Generate src/core/bidi_tables.rs from released UCD files.

Usage: gen_bidi_tables.py <ucd_dir>  where <ucd_dir> holds
DerivedBidiClass.txt (the extracted/ variant), BidiBrackets.txt, and
BidiMirroring.txt from one released UCD version. The output is frozen and
checked in; regenerate only against a released UCD and review the diff.
"""
import re
import sys

CLASSES = ["L","R","AL","EN","ES","ET","AN","CS","NSM","BN","B","S","WS","ON",
           "LRE","RLE","LRO","RLO","PDF","LRI","RLI","FSI","PDI"]
CIDX = {c: i for i, c in enumerate(CLASSES)}
LONG = {  # @missing lines use long property value names
    "Left_To_Right": "L", "Right_To_Left": "R", "Arabic_Letter": "AL",
    "European_Terminator": "ET", "Other_Neutral": "ON",
}
MAX = 0x110000

def main(ucd):
    classes = [CIDX["L"]] * MAX
    missing, assigned = [], []
    for line in open(f"{ucd}/DerivedBidiClass.txt"):
        m = re.match(r"# @missing: ([0-9A-F]+)\.\.([0-9A-F]+); (\w+)", line)
        if m:
            missing.append((int(m.group(1), 16), int(m.group(2), 16),
                            LONG.get(m.group(3), m.group(3))))
            continue
        line = line.split("#")[0].strip()
        if not line:
            continue
        rng, cls = [p.strip() for p in line.split(";")]
        a, b = ([int(x, 16) for x in rng.split("..")] if ".." in rng
                else [int(rng, 16)] * 2)
        assigned.append((a, b, cls))
    for a, b, c in missing:
        classes[a:b + 1] = [CIDX[c]] * (b - a + 1)
    for a, b, c in assigned:
        classes[a:b + 1] = [CIDX[c]] * (b - a + 1)

    ranges = []
    start, cur = 0, classes[0]
    for cp in range(1, MAX):
        if classes[cp] != cur:
            ranges.append((start, cp - 1, cur))
            start, cur = cp, classes[cp]
    ranges.append((start, MAX - 1, cur))
    non_l = [(a, b, c) for a, b, c in ranges if c != CIDX["L"]]

    brk = []
    for line in open(f"{ucd}/BidiBrackets.txt"):
        line = line.split("#")[0].strip()
        if not line:
            continue
        cp, paired, typ = [p.strip() for p in line.split(";")]
        brk.append((int(cp, 16), int(paired, 16), typ))

    mir = []
    for line in open(f"{ucd}/BidiMirroring.txt"):
        line = line.split("#")[0].strip()
        if not line:
            continue
        a, b = [int(p.strip(), 16) for p in line.split(";")]
        mir.append((a, b))

    out = ["//! Frozen UAX #9 data tables, generated from UCD 17.0.0 by",
           "//! `tools/gen_bidi_tables.py` (do not edit by hand; regenerate from the",
           "//! released UCD files and review the diff). Sources: DerivedBidiClass.txt",
           "//! (assigned classes + `@missing` defaults folded into ranges, `L` omitted",
           "//! as the lookup fallback), BidiBrackets.txt (rule N0 / BD16), and",
           "//! BidiMirroring.txt (rule L4 render mirroring).",
           "",
           "use super::bidi::Class;",
           "",
           "/// `(first, last, class)` — sorted, non-overlapping, `L` ranges omitted.",
           "#[rustfmt::skip]",
           "pub(super) const CLASS_RANGES: &[(u32, u32, Class)] = &["]
    out += [f"    (0x{a:04X}, 0x{b:04X}, Class::{CLASSES[c]})," for a, b, c in non_l]
    out += ["];", "",
            "/// `(bracket, paired bracket, is_open)` — sorted by bracket (BD16).",
            "#[rustfmt::skip]",
            "pub(super) const BRACKETS: &[(u32, u32, bool)] = &["]
    out += [f"    (0x{cp:04X}, 0x{p:04X}, {'true' if t == 'o' else 'false'})," for cp, p, t in brk]
    out += ["];", "",
            "/// `(char, mirrored form)` — sorted (rule L4; drawn in RTL runs).",
            "#[rustfmt::skip]",
            "pub(super) const MIRRORED: &[(u32, u32)] = &["]
    out += [f"    (0x{a:04X}, 0x{b:04X})," for a, b in mir]
    out += ["];"]
    open("src/core/bidi_tables.rs", "w").write("\n".join(out) + "\n")
    print(f"wrote {len(non_l)} class ranges, {len(brk)} brackets, {len(mir)} mirrors")

if __name__ == "__main__":
    main(sys.argv[1])
