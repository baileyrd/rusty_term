# rusty_term — parity-loop gap analysis

**Reference:** `docs/research/gap-analysis-2026-07.md` — the current, most
recent hand-curated roadmap doc in this repo. It **supersedes**
`docs/research/capability-assessment-2026.md` (28-item audit, most items now
landed) with 37 additional gaps found against the mid-2026 terminal
landscape (kitty 0.43+, Ghostty 1.3, WezTerm, Alacritty, Windows Terminal,
iTerm2, foot, Contour, Konsole/VTE, Rio, Warp), and it carries forward and
re-audits the 13 items that were still open in the earlier document.

**Source path used:** roadmap (step 0 found an existing hand-curated scope
doc — no mechanical diff or spec extraction was needed).

**Run date:** 2026-07-23.

## Correction (read this first)

The first pass of this run read only `capability-assessment-2026.md` and
`implementation-status.md`, missed `gap-analysis-2026-07.md` sitting in the
same `docs/research/` directory, and filed 11 issues (#125–#135) against the
older document's stale gap list. **9 of those 11 turned out to already be
implemented** — landed in commits the older document was never updated to
reflect. All 9 have been closed as `not_planned` with a comment on each
explaining what shipped and pointing at the specific section of
`gap-analysis-2026-07.md` that confirms it:

| Issue | Item | Resolution |
| --- | --- | --- |
| [#125](https://github.com/baileyrd/rusty_term/issues/125) | GPU renderer ligature shaping (C08) | done — GPU atlas rebuild, see "C08 + C09 status (2026-07)" |
| [#126](https://github.com/baileyrd/rusty_term/issues/126) | GPU renderer pixel image compositing (C09) | done — same section |
| [#127](https://github.com/baileyrd/rusty_term/issues/127) | iTerm2 GIF decode (C12) | done — `src/core/gif.rs`, "C12′ status (2026-07)" |
| [#128](https://github.com/baileyrd/rusty_term/issues/128) | iTerm2 WebP decode (C12) | done (VP8L lossless) — `src/core/webp.rs`; lossy VP8 deliberately out of scope |
| [#129](https://github.com/baileyrd/rusty_term/issues/129) | iTerm2 progressive JPEG decode (C12) | done — `src/core/jpeg.rs` reworked |
| [#130](https://github.com/baileyrd/rusty_term/issues/130) | Multiple top-level OS windows (C13) | done — `App` router + per-window `WindowState`, "C13 status (2026-07)" |
| [#131](https://github.com/baileyrd/rusty_term/issues/131) | CPU-renderer window transparency (C14) | resolved, not implemented — closed **GPU-only by design** (`softbuffer` has no alpha channel), "C14′ resolution (2026-07)" |
| [#132](https://github.com/baileyrd/rusty_term/issues/132) | Platform window blur (C14) | resolved alongside #131 — same GPU-only-by-design call |
| [#133](https://github.com/baileyrd/rusty_term/issues/133) | Command-output fold render-path integration (C17) | done — "C17′ status (2026-07)"; the regression risk this issue flagged was addressed via a display-line remap layer, not avoided |
| [#135](https://github.com/baileyrd/rusty_term/issues/135) | Bidi text + Unicode normalization (C25) | done, all 5 phases — see `docs/research/bidi-scoping-2026-07.md`; the new-dependency approval for this issue turned out to be moot |

**[#134](https://github.com/baileyrd/rusty_term/issues/134) (accessibility,
C20) is the only issue from the first pass that's still valid** — confirmed
still open both in `gap-analysis-2026-07.md`'s Section B and by a direct
`grep` of `src/`/`Cargo.toml` (no `accesskit` anywhere). It stays open and is
being implemented this run.

## Current assessment (corrected)

Cross-referencing `gap-analysis-2026-07.md`'s summary tables (Section A's 37
new gaps + Section B's 13 carried-forward items from the older document)
against the source tree:

- **Section A (37 new gaps): 36 done**, 1 explicitly watch-listed (see
  below). No open, actionable gap remains here.
- **Section B (13 carried-forward items):**
  - Done as of 2026-07: C03′ (→ G11), C08, C09, C12′, C13, C17′, C25.
  - Resolved-not-implemented by design: C14′ (GPU-only, softbuffer has no
    alpha — see #131/#132 above).
  - Deliberately watch-listed, not gaps: C18 (Unicode width mode 2027), C19
    (OSC 66 text-sizing protocol) — both explicitly "wait for the field to
    converge," same posture as G35 below.
  - **Still open: C20 (accessibility) — the one item this run is
    implementing** (#134).
  - Still open but **excluded from this run as not real capability gaps**:
    C23 (io_uring, Linux-only perf refinement to an already-correct I/O
    path), C24 (IOCP-native async, Windows-only perf refinement), C26/C27
    (DAP/Jupyter bridges, full LSP/ACP backends — speculative extensions of
    rusty_term's own protocol invention, "no external comparison," no known
    client asking for either). Each is L, none change any user-visible
    behavior competitors are measured against — same reasoning as the first
    pass's exclusion of these four, which the newer document's own framing
    ("open, perf-only" / "open, speculative") confirms rather than
    contradicts.
- **G35 (multiple-cursors protocol)** — explicitly watch-listed ("single
  implementation for now... track; don't build"), same posture as C18/C19.

## Resolution for this run

| Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Issue | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Accessibility / screen-reader support (C20) | fn | roadmap | both (gui) | `docs/research/gap-analysis-2026-07.md` §Section B | no | L | [#134](https://github.com/baileyrd/rusty_term/issues/134) — loop-eligible | Needs `accesskit` (new dependency, approved by the repo owner for this run). Still a field-wide gap per the roadmap — no competitor has meaningful screen-reader support either — so this is differentiation, not catch-up. **Only item this run implements.** |

Everything else surviving either doc's own gap lists is done, deliberately
watch-listed, resolved-not-implemented by design, or excluded as
perf-only/speculative non-capability work (see above) — no further issues
filed this run.
