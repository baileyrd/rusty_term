# rusty_term — parity-loop gap analysis

**Reference:** `docs/research/capability-assessment-2026.md` (the repo's own
hand-curated competitive audit against kitty, Ghostty, WezTerm, Alacritty,
Windows Terminal, Konsole/VTE, iTerm2, Contour, Warp — 28 items, pinned as of
its own last update). Cross-checked against `docs/research/implementation-status.md`
(14-layer stack scorecard) for consistency; the two docs agree.

**Source path used:** roadmap (step 0 found an existing, current, hand-curated
scope doc — no mechanical diff or spec extraction was needed or used).

**Run date:** 2026-07-23.

Of the 28 evaluated items, **20 are already done** (C01–C07, C10, C11, C15,
C16, C21, C22 fully; C12/C14/C17 partially — the done portion of each is
excluded below). R01 was investigated and explicitly rejected by the roadmap
itself (not a gap). C18/C19 are explicitly "watch, don't build yet" per the
roadmap's own recommendation — excluded as out of scope for this run, not
silently dropped.

What's left is the remainder — **11 rows**, all sized L or XL. Every M/S item
in the roadmap has already shipped; nothing "small" is left to mechanically
auto-implement under this skill's normal small-issue loop.

| Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Issue | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| GPU renderer ligature shaping (C08) | fn (existing, GPU path only) | roadmap | both (gui-gpu) | `docs/research/capability-assessment-2026.md#c08` | no | L | [#125](https://github.com/baileyrd/rusty_term/issues/125) — `blocked` | Blocked on the GPU multi-cell-quad pipeline, which doesn't exist yet and isn't itself a sized item — building it is a prerequisite project, not a subtask. |
| GPU renderer pixel image compositing (C09) | fn (existing, GPU path only) | roadmap | both (gui-gpu) | `...#c09` | no | L | [#126](https://github.com/baileyrd/rusty_term/issues/126) — `blocked` | Same multi-cell-quad-pipeline blocker as C08. |
| iTerm2 GIF decode (C12 remainder) | fn | roadmap | both (gui) | `...#c12` | no | L | [#127](https://github.com/baileyrd/rusty_term/issues/127) — `needs-human` | Hand-rolled per repo convention (no image crates) — animated-GIF needs a frame-timer hook the current synchronous decode-and-place path doesn't have. |
| iTerm2 WebP decode (C12 remainder) | fn | roadmap | both (gui) | `...#c12` | no | L | [#128](https://github.com/baileyrd/rusty_term/issues/128) — `needs-human` | Hand-rolled; a materially bigger decoder than PNG/JPEG were — "on par with a second image codec project" per the roadmap's own sizing. |
| iTerm2 progressive JPEG decode (C12 remainder) | fn (existing, extends baseline JPEG decoder) | roadmap | both (gui) | `...#c12` | no | L | [#129](https://github.com/baileyrd/rusty_term/issues/129) — `needs-human` | Needs multi-scan coefficient accumulation the current single-scan decoder isn't structured for. |
| Multiple top-level OS windows (C13) | fn | roadmap | both (gui) | `...#c13` | **yes (arch.)** | L | [#130](https://github.com/baileyrd/rusty_term/issues/130) — `needs-human` | Roadmap already investigated this: `App`/`gui/window.rs` (~2,270 lines) is built entirely around one implicit window; ~80 call sites assume single-window state. Not additive — a data-model rewrite. Roadmap's own verdict: "left for a dedicated pass with its own design + review." |
| CPU-renderer window transparency (C14 remainder) | fn (existing, CPU path only) | roadmap | both (gui) | `...#c14` | no | L | [#131](https://github.com/baileyrd/rusty_term/issues/131) — `needs-human` | `softbuffer` 0.4's buffer format has no alpha channel at all (confirmed by the roadmap's own inspection) — needs a different presentation path, not a bolt-on. May require a new/replacement rendering dependency. |
| Platform window blur (C14 remainder) | fn | roadmap | macOS, Linux/KDE | `...#c14` | no | L | [#132](https://github.com/baileyrd/rusty_term/issues/132) — `needs-human` | Needs unsafe platform-specific FFI (`NSVisualEffectView`, KDE blur-behind X11 property) outside winit's cross-platform surface — flagged by the roadmap as unverifiable in this sandbox (no macOS/KDE to build or test against). |
| Command-output fold render-path integration (C17 remainder) | fn (existing data model, no renderer consumer) | roadmap | both (gui) | `...#c17` | **yes (risk)** | L | [#133](https://github.com/baileyrd/rusty_term/issues/133) — `needs-human` | Data model (`Grid::fold_blocks` etc.) already landed and tested. Rendering it means changing `viewport_cell`/`snapshot_viewport`'s row↔logical-line mapping, which selection, search, and click-hit-testing all key off directly — roadmap explicitly declined to bolt this on to avoid regressing well-tested, currently-correct subsystems. |
| Accessibility / screen-reader support (C20) | fn | roadmap | both (gui) | `...#c20` | no | L | [#134](https://github.com/baileyrd/rusty_term/issues/134) — loop-eligible | Needs `accesskit` — a new third-party dependency, approved by the repo owner for this run. |
| Bidi text + Unicode normalization (C25) | fn | roadmap | both | `...#c25` | no | XL | [#135](https://github.com/baileyrd/rusty_term/issues/135) — loop-eligible | Needs `unicode-bidi` + `unicode-normalization` (both new deps, confirmed absent from the dependency tree, approved by the repo owner for this run). Touches the grid's cell-layout model broadly enough that the roadmap itself says it needs its own scoping pass before it can even be sequenced. |

**Excluded from the table (checked, not missing):**
- **C18, C19** — real protocols, but the roadmap explicitly recommends
  watch-and-wait (field hasn't converged / too early to build against a
  settled reference). Not a gap for this run.
- **C23 (io_uring), C24 (IOCP-native)** — Tier 7, performance-only refinements
  to an already-correct I/O path (not features), each platform-specific and
  each **L**, each likely needing a new dependency (`io-uring`/`tokio-uring`,
  or a Windows IOCP crate). Left out of the table rather than filed as
  "parity gaps" since they're not capability gaps at all — no visible
  behavior differs.
- **C26 (DAP/Jupyter), C27 (full LSP/ACP backends)** — Tier 9, rusty_term's
  own protocol invention with "no external comparison" and, per the roadmap,
  "unclear what the terminal itself would be a language server *for*" — no
  known client asking for either. Speculative, not a parity gap against a
  competitor.
- **R01** — investigated and rejected by the roadmap itself (alt-screen
  resize reflow); xterm/kitty/Alacritty all behave the way rusty_term already
  does.

## Why nothing here is a normal "file it and go" issue

This skill's loop is built for small, additive, no-new-dependency gaps that
can be implemented unattended. Every one of the 11 rows above fails at least
one of those tests:

- **2 are blocked on an unbuilt prerequisite** (C08, C09 — the GPU
  multi-cell-quad pipeline).
- **3 are each their own project-sized decoder** (C12's GIF/WebP/progressive
  JPEG), sized L individually by the roadmap's own honest accounting, not
  bundleable into one "wave."
- **3 were already investigated by the roadmap's own author and explicitly
  deferred** with stated reasoning — one as an architectural rewrite (C13),
  two as "not a bolt-on, needs a dedicated pass" to avoid regressing tested
  subsystems (C14's CPU-transparency remainder, C17's render integration).
  Auto-implementing these on a fresh pass would either redo that judgment
  call or risk exactly the regression it warned about.
- **2 need a new third-party dependency** (C20 → `accesskit`, C25 →
  `unicode-bidi`/`unicode-normalization`) — per this skill's own rules, a new
  dependency is a stop-and-ask, not an auto-implement, same as a breaking
  change.
- **1 is XL** and self-describes as needing its own scoping pass before it
  can even be sequenced (C25).

No row is a clean "small, pure-addition, no-new-dep" issue as-is.

## Resolution for this run

Presented to the repo owner as a checkpoint (per this skill's step 1). Decision:
approve `accesskit` (C20) and `unicode-bidi`/`unicode-normalization` (C25) as
allowed new dependencies, file all 11 rows as tracked issues, and only attempt
C20 (#134) and C25 (#135) automatically this run. The other 9 are filed and
labeled `blocked` (C08/C09, waiting on the GPU multi-cell-quad pipeline) or
`needs-human` (everything else — architectural rewrites, project-sized
decoders, or subsystems the roadmap's own author already declined to bolt
things onto without a dedicated design pass) so `next_issue.sh` skips them
automatically and they surface in the wrap-up report instead of being
silently attempted or dropped.
