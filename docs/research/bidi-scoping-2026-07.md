# Bidi + Normalization Scoping Pass (C25) — July 2026

The scoping pass C25 has been waiting for. Goal: turn "XL, touches
everything" into a phased plan with honest sizes, a testing story, and the
architectural decisions made up front — so implementation can start as
ordinary waves instead of an open-ended rewrite.

## What exists today

- The grid stores cells in **logical order**, one column per cell, strictly
  left-to-right (`src/core/grid.rs`). Everything downstream — selection,
  search, copy, mouse hit-testing, both renderers — assumes visual column ==
  logical column.
- UAX #29-ish grapheme clustering already works: combining marks and ZWJ
  sequences ride along as interned cluster suffixes (`Cell.cluster`), and
  `char_width` classifies display width. Emoji and Indic clusters render;
  RTL runs render **backwards** (in logical order), and Arabic renders in
  isolated letter forms.
- No normalization anywhere: what the app writes is what's stored, compared,
  and copied (deliberately — round-trip fidelity).
- Zero-dep ethos: no `unicode-bidi` / `unicode-normalization` crates, and
  this plan doesn't add them.

## Prior art — what terminals actually do

The field is thinner than it looks:

- **kitty** deliberately refuses bidi at the terminal layer (kovidgoyal's
  long-standing position: the terminal can't know the app's semantics;
  full-screen apps must do their own). Renders logical order.
- **mlterm** is the only widely-cited terminal with real implicit bidi.
- **Konsole/VTE** have partial, often-broken implicit handling.
- **The Terminal-WG bidi draft** (Egmont Koblinger's *BiDi in Terminal
  Emulators*, the de-facto spec) is the design to follow: storage stays
  logical; the terminal reorders **at display time only**; two modes —
  *implicit* (terminal reorders, per-line autodetection, for line-oriented
  apps like shells) and *explicit* (app opts out and does its own layout,
  for full-screen apps) — switched by escape sequences, explicit being the
  safe default for alt-screen apps.

That draft resolves the scariest architectural question for free: **the
grid model does not change.** Bidi is a per-row *view transform*, exactly
like the fold-summary display layer added for C17′.

## Architecture decision

One new concept: a per-viewport-row **visual↔logical column permutation**.

```
fn bidi_map(row_text) -> Option<Vec<u16>>   // None = identity (pure LTR)
```

- Computed per *logical line* (soft-wrap-joined, like search) so a paragraph
  reorders coherently across wraps, then sliced per visual row.
- Renderers draw `viewport_cell(map[col], row)` instead of
  `viewport_cell(col, row)`. Both renderers go through `viewport_cell`
  already, so — as with folding — they inherit it from one place.
- Mouse hit-testing, selection endpoints, and copy-mode cursor *display*
  invert the map; selection/copy semantics stay **logical** (this matches
  the draft spec and keeps copied text correct).
- The terminal cursor stays logical in the model and is *drawn* at
  `map⁻¹[cursor.col]`.
- Fast path: a row with no character of bidi class R/AL/AN (one range scan)
  gets `None` and costs nothing. Pure-LTR sessions never pay.

Cache: the map is derived state, recomputed for dirty rows only, stored
alongside the row like search highlights are.

## Data tables (zero-dep, frozen like `kitty_diacritics`)

Generated once from the UCD and checked in as compact range tables with the
generator script in `tools/` (regeneration is reviewable, not a build step):

| Table | Source | ~Size |
|---|---|---|
| Bidi_Class (the 12 classes UAX #9 needs) | `DerivedBidiClass.txt` | ~6 KB ranges |
| Bracket pairs (rule N0) | `BidiBrackets.txt` | ~1 KB |
| Arabic joining type (phase 3) | `ArabicShaping.txt` | ~3 KB |

## Algorithm subset

Full UAX #9 per paragraph (= logical line), levels capped at 125 as spec'd:
P2–P3 (paragraph level; per the terminal draft, autodetect first-strong,
overridable), X1–X10 (explicit codes + isolates — apps do emit LRI/RLI),
W1–W7, N0–N2, I1–I2, and L2 reordering into the map. L1 whitespace reset
matters (trailing spaces at line ends must not float left). L3/L4 (mirroring)
handled by a small mirrored-brackets table at render: `(` draws `)` in RTL
runs.

## Normalization (the second half of C25)

Recommendation: **do not normalize storage** — round-trip fidelity is a
terminal correctness property (apps repaint what they wrote and expect
column parity). Instead:

- Search: extend the existing `fold_char` pipeline with NFD-based canonical
  folding so `é` (NFC) matches `e`+combining-acute (NFD) — a decomposition
  table (~5 KB, BMP) applied to both needle and haystack chars.
- Copy: unchanged (logical, as stored).

This scopes "normalization" down from XL-sounding to S–M with a clear test.

## Phases

Status (2026-07): **all five phases done.** Phases 1–2:
`src/core/bidi.rs` (full UAX #9, 2,479-case UCD conformance sample in the
suite), frozen UCD 17.0.0 tables (generators in `tools/`),
`Grid::bidi_row` / `logical_col`, both renderers, and the mouse paths;
wide glyphs reorder as lead+trailer units, shaped ligature runs are
disabled on reordered rows, and fold summaries compose. Phase 3:
`src/core/arabic.rs` — joining-type analysis (ArabicShaping.txt +
Mn/Me/Cf transparency from UnicodeData) selecting presentation forms
(isolated/final/initial/medial) in logical order, surfaced through
`BidiRow::shaped` to both renderers; the GSUB `init`/`medi`/`fina` path
and the lam-alef ligature remain future refinements. Phase 4: BDSM
(ECMA-48 mode 8, DECRQM-answerable) switches implicit/explicit; the
alternate screen defaults to explicit so full-screen apps stay unbroken;
SCP (`CSI Ps1;Ps2 SP k`) fixes the paragraph direction while private
mode 2501 (autodetection, default on) is reset. Phase 5: search folds
canonical decompositions to their base char (`fold_char` +
`src/core/canon_tables.rs`), so "e" finds "é" in either spelling.
`bidi = "auto"` remains opt-in; with the explicit-mode protection now in
place, flipping the default is a product decision, not a technical one.

| Phase | Deliverable | Size | Verifiable headlessly? |
|---|---|---|---|
| 1 | Bidi_Class tables + UAX #9 levels + reorder map, pure function, `bidi = "auto"/"off"` config (default **off** until phase 2 lands) | L | Yes — UCD `BidiTest.txt`/`BidiCharacterTest.txt` subsets embedded as fixtures |
| 2 | Render integration: `viewport_cell` remap, cursor/selection/mouse inversion, implicit-mode autodetect per line | M–L | Yes — grid-level tests (viewport text of RTL lines, click mapping) |
| 3 | Arabic joining: joining-type table + GSUB `init`/`medi`/`fina`/`isol` features through the existing shaper (fonts without them fall back to presentation forms) | L | Yes — shaper tests with an Arabic-subset font fixture |
| 4 | Explicit/implicit mode switching per the Terminal-WG draft escape sequences; alt-screen defaults to explicit | S–M | Yes — parser tests |
| 5 | Canonical-fold search (NFD table into `fold_char`) | S–M | Yes |

Phases are independent waves; 1+2 give visible RTL correctness for shells,
3 makes Arabic *readable*, 4 keeps vim/emacs unbroken, 5 closes the
normalization half.

## Risks / known sharp edges

- **Wide chars in RTL runs**: the map is per-cell; a wide glyph's two cells
  must stay adjacent after reordering (treat lead+trailer as one unit when
  building the map).
- **Cursor UX at run boundaries**: logical cursor at a direction boundary
  has two defensible visual positions; the draft says draw at the level-run
  the cursor's *logical successor* belongs to. Follow it, don't invent.
- **Reflow**: the map is derived, so resize just invalidates it — no
  interaction with the reflow machinery.
- **Interaction with folding (C17′)**: summary rows are synthetic LTR;
  ordinary rows compose (fold remap first, bidi map second).

## Recommendation

Adopt the Terminal-WG model, implement phases 1–2 next time
internationalization is prioritized (~two waves at this repo's cadence),
and keep `bidi = "off"` as the default until phase 4 protects full-screen
apps. Nothing in phases 1–5 requires a dependency or a grid-model change.
