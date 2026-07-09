# rusty_term Capability Assessment — 2026

A capability audit of rusty_term against the 2026 terminal landscape: kitty,
Ghostty, WezTerm, Alacritty, Windows Terminal, Konsole/VTE, iTerm2, Contour,
and Warp. 28 items evaluated — 27 recommended, 1 explicitly considered and
rejected.

**Status:** implementation follows the suggested sequencing. Done items are
marked ✅ below and in the summary table; each carries a `Status: done` line
under its own heading naming the commit/PR.

## How this was built

rusty_term's own capability docs
([`terminal-stack-spec-tree.html`](terminal-stack-spec-tree.html),
[`../FEATURES.md`](../FEATURES.md),
[`implementation-status.md`](implementation-status.md)) were read in full and
cross-checked against the source for protocols competitor terminals have
adopted since those docs were last updated — synchronized output, undercurl,
the Kitty keyboard protocol, OSC 22, title stack, DECRQM, window
transparency, and others. Where a capability wasn't mentioned in rusty_term's
own gap-tracking at all, it's flagged below as a **silent gap** — something
competitive pressure will surface before the roadmap does.

Adoption claims are sourced from current (2026) terminal documentation and
comparison writeups, not training-data recall; see [Sources](#sources). This
is the **entire candidate pool** evaluated, including items dropped from an
initial top-20 cut for being out of scope, too diffuse to size honestly, or —
in one case — investigated and rejected outright.

## Summary

| ID | Capability | Tier | Notable adopters | Size |
|---|---|---|---|---|
| ✅ C01 | Synchronized output (mode 2026) | T1 | kitty, Ghostty, Windows Terminal | M |
| ✅ C02 | Undercurl + colored underlines | T1 | kitty, VTE, Alacritty, iTerm2 | M |
| ✅ C03 | Kitty keyboard protocol (native GUI) | T1 | kitty, WezTerm, Ghostty, +5 more | M–L |
| ✅ C04 | OSC 22 pointer shape | T1 | xterm, kitty | S |
| ✅ C05 | Window title stack | T1 | xterm, VTE | S |
| ✅ C06 | DECRQM mode query | T1 | xterm, kitty | S–M |
| ✅ C07 | XTWINOPS pixel-size queries | T1 | xterm, kitty, WezTerm | S |
| C08 | GPU renderer ligatures | T2 | internal parity | L |
| C09 | GPU renderer image protocols | T2 | internal parity | L |
| ✅ C10 | GUI mouse motion + buttons | T2 | internal parity | M |
| ✅ C11 | GUI DECCKM tracking | T2 | internal parity | S |
| C12 | iTerm2 geometry hints + formats | T2 | iTerm2 spec | M |
| C13 | Multiple top-level windows | T3 | kitty, WezTerm, Ghostty, Alacritty | M |
| C14 | Background opacity + blur | T3 | kitty, WezTerm, Alacritty, Ghostty | M |
| C15 | OSC 633 (VS Code superset) | T4 | VS Code | S–M |
| C16 | Alternate scroll mode (1007) | T4 | xterm, Alacritty, kitty | S |
| C17 | Command-output folding (OSC 133) | T4 | Warp-inspired | M |
| C18 | Unicode width mode (2027) | T5 | Contour, ~10 others | M |
| C19 | Text-sizing protocol / OSC 66 | T5 | kitty, Ghostty (partial) | L |
| C20 | Accessibility / screen readers | T6 | industry-wide gap | L |
| C21 | Rectangular-area ops (VT420) | T6 | legacy VT420 | S–M |
| C22 | Line/New Line mode (LNM) | T6 | legacy ECMA-48 | S |
| C23 | io_uring backend (Linux) | T7 | kitty (explored) | L |
| C24 | IOCP-native async (Windows) | T7 | perf refinement | L |
| C25 | Bidi text + Unicode normalization | T8 | partial industry-wide | XL |
| C26 | DAP / Jupyter protocol bridges | T9 | no external comparison | L |
| C27 | Full LSP/ACP backends | T9 | no external comparison | L |
| R01 | Alt-screen resize reflow | — | **rejected** — xterm/kitty/Alacritty behave the same | — |

---

## Tier 1 — Protocol fidelity, broad adoption, contained scope

The highest-value tier: each item is a self-contained parser/render
addition (no architectural change), and each is now supported by enough of
the field that its absence reads as a compatibility bug rather than a
missing extra.

### C01 — Synchronized output (DEC private mode 2026)
**Status: done.** `Grid::set_sync_output`/`sync_output_active` (`src/core/grid.rs`),
wired in the parser's DECSET/DECRST handler and gating the render-trigger
call sites in `src/runtime/tokio_rt.rs` (both Unix and Windows) and
`src/gui/window.rs`'s `Redraw` handler. Includes the timeout safety valve.

**Current (before).** No matches anywhere in `src/`. The grid renders whatever
partial state exists mid-update in both TUI passthrough and native GUI mode
— full-screen apps (vim, htop, tig) doing a fast multi-line redraw can show
visible tearing.

**Target.** Honor `CSI ?2026h` / `CSI ?2026l`: buffer grid mutations between
the two and defer the render-loop's "publish to renderer" step until the
closing sequence (with a timeout safety valve so a misbehaving app that
never closes can't freeze the display permanently).

**Why it matters.** `kitty` · `Ghostty (since 1.0)` · `Windows Terminal
1.24` · `Alacritty (dev)` · 14/44 tracked terminals as of April 2026. This
has crossed from "nice to have" to assumed baseline for a terminal that
claims not to flicker.

**Size** M · **Deps** none.

### C02 — Undercurl + colored underlines (SGR 4:3, 58/59)
**Status: done.** `UnderlineStyle` + `ATTR_UNDERLINE_COLOR` (`src/core/cell.rs`)
pack style and a colored-underline flag alongside the existing attribute
bits; `underline_color` is a new field on both `Cell` and `Pen`. The parser's
`parse_sgr_params` (`src/core/parser.rs`) distinguishes the ECMA-48 colon
sub-parameter form (`4:3`, one code) from the semicolon form (`4;3`, two
independent codes) via a synthetic sentinel, so the two are never confused;
SGR 58/59 reuse the existing `Palette::extended` truecolor/256-color decoder,
so both `58;2;r;g;b` and the colon `58:2:r:g:b` form work identically. The
CPU rasterizer (`src/gui/cpu.rs`) draws straight/double/curly/dotted/dashed
strokes and strikethrough as pixel stripes — a new decoration pass, since
neither was drawn anywhere before (the existing "underline" code there is
the DECSCUSR *cursor* shape, unrelated). The GPU renderer (`src/gui/gpu.rs`)
gets the same five styles + strikethrough drawn directly in the WGSL
fragment shader via two new per-instance fields, so the two render paths
stay at parity. TUI passthrough mode re-emits the style/color via `sgr_for`
(`src/render.rs`) so a capable host terminal renders the same thing.

**Current (before).** `ATTR_UNDERLINE` is a single boolean bit; no curly/dashed/
dotted underline styles and no separate underline-color channel. No matches
for SGR 58/59 anywhere in `src/`.

**Target.** Parse SGR `4:1`–`4:5` (straight/double/curly/dotted/dashed) and
`58`/`59` (underline color, 256 or truecolor), store per-cell alongside the
existing attribute bits, render curly as a distinct sine-wave stroke.

**Why it matters.** `kitty (originated)` · `VTE/GNOME Terminal` ·
`Alacritty` · `Konsole` · `iTerm2` · `xterm.js`. Neovim's built-in LSP
diagnostics assume this. rusty_term's whole shell-integration angle (OSC
133, the L13 side-channel) targets exactly the power users who run Neovim +
LSP daily — shipping without undercurl means squiggly-underline diagnostics
silently degrade to plain underlines.

**Size** M · **Deps** none.

### C03 — Kitty keyboard protocol (native GUI backend)
**Status: partially done.** `Grid::kitty_flags_stack` (`src/core/grid.rs`)
implements the full push/pop/set/query state machine (`CSI > flags u` /
`CSI < n u` / `CSI = flags ; mode u` / `CSI ? u`), applied from
`handle_private_csi` (`src/core/parser.rs`) alongside the existing host
relay. `gui/input.rs`'s key encoder reads the current flags and, when bit 1
(disambiguate escape codes) is set, encodes Escape/Enter/Tab/Backspace and
Ctrl+letter combinations as `CSI u` instead of their legacy (ambiguous)
bytes — covering the enhancement level the overwhelming majority of clients
actually request (Neovim's default `kitty_keyboard_protocol`, etc.). Bits 2/4/16
(event-type, alternate-key, and associated-text reporting) are **not**
implemented: they need release/repeat key events and IME-layout data that
aren't plumbed through the native window's input path today, and a client
that requests them gets the disambiguated-but-legacy-shaped encoding rather
than a fabricated answer. Widening this further is future work, tracked as
a known gap rather than silently claimed.

**Current (before).** TUI mode correctly *relays* the Kitty keyboard protocol and
xterm `modifyOtherKeys` to the host terminal (`src/core/parser.rs`,
`handle_private_csi`) — appropriate, since TUI mode is a passthrough. But
`src/gui/input.rs`'s own native key encoding has no progressive-enhancement
support: no enhancement-flag stack, no `CSI u` disambiguation, no
press/repeat/release event-type reporting.

**Target.** Implement the push/pop enhancement-flag stack and `CSI u`
encoding in `gui/input.rs`, so a standalone rusty_term window is a
first-class Kitty-keyboard-protocol terminal in its own right, not only when
relaying through a host that already speaks it.

**Why it matters.** `kitty` · `foot` · `WezTerm` · `Ghostty` · `Alacritty` ·
`iTerm2` · `VS Code terminal` · `Warp` · `Windows Terminal (preview)`.
Client-side adoption is just as broad — Neovim, Vim, Helix, fish, and
nushell all detect and use it. Without it, the native GUI window can't
distinguish Shift+Enter from Enter or report key-release, which several TUI
apps use for hold-to-repeat behavior.

**Size** M–L · **Deps** `gui` feature.

### C04 — OSC 22 (mouse pointer/cursor shape)
**Status: done.** `Grid::cursor_icon` (`src/core/grid.rs`), set by `osc.rs`'s
OSC 22 handler, mapped from CSS `cursor` keywords to `winit::CursorIcon` by
`parse_cursor_icon` and applied in the `CursorMoved` handler
(`src/gui/window.rs`) whenever the pointer is over pane content.

**Current (before).** No matches anywhere in `src/`.

**Target.** Let the child set the OS mouse cursor icon over the window
(e.g. an editor requesting an I-beam instead of a pointer) via OSC 22; map
the requested shape onto winit's `CursorIcon` — already wired for the
resize-border hover in `gui/window.rs`, so this is mostly new OSC parsing,
not new rendering machinery.

**Why it matters.** `xterm` · `kitty`. Small and cheap relative to its
payoff — the plumbing to change the cursor already exists in the codebase.

**Size** S · **Deps** `gui` feature.

### C05 — Window title stack (XTPUSHTITLE / XTPOPTITLE)
**Status: done.** `Grid::push_title`/`pop_title` (`src/core/grid.rs`), bounded
by `TITLE_STACK_MAX`, dispatched from the parser's `CSI t` handler
(`src/core/parser.rs`) on sub-params 22/23.

**Current (before).** No matches for `CSI 22t` / `CSI 23t`; `window.rs` only ever
reads the latest `g.title`, with no memory of what it was before.

**Target.** A small stack on `Grid`, pushed on `CSI 22;0/1/2t` and popped on
`CSI 23;0/1/2t`, so an app that sets a working title (vim, tmux) restores
the caller's title on exit instead of leaving a stale one in the window.

**Why it matters.** `xterm` · `VTE`. Standard xterm behavior many
full-screen apps rely on without checking for support first; without it,
quitting vim can leave the window titled "vim" indefinitely.

**Size** S · **Deps** none — core-side; surfaces through the existing
title-fallback path from the `--title` work.

### C06 — DECRQM (mode query/report)
**Status: done.** `AnsiParser::report_dec_mode`/`report_ansi_mode`
(`src/core/parser.rs`) answer `CSI ? Ps $p` (DEC private) and `CSI Ps $p`
(ANSI) with a DECRPM report for every mode rusty_term actually tracks state
for (DECOM, DECAWM, DECTCEM, alt-screen, mouse tracking + extended
encodings, bracketed paste, synchronized output C01, IRM); a mode we only
relay to the host and don't track ourselves (DECCKM, focus reporting)
answers "not recognized" (`0`) honestly rather than guessing, and so does
any genuinely unimplemented mode (e.g. LNM, until C22).

**Current (before).** rusty_term accepts DECSET/DECRST (mode-setting) but has no
matches for answering `CSI Ps $p` ("is mode *N* currently set?") queries at
all.

**Target.** Track a canonical table of every mode rusty_term supports and
reply `1` (set), `2` (reset), `3` (permanently set), or `4` (permanently
reset) as appropriate; unknown modes report `0`.

**Why it matters.** `xterm` · `kitty`. Well-behaved TUI libraries probe
capability via DECRQM before relying on a mode — including synchronized
output (C01) and bracketed paste — instead of assuming support and hoping.

**Size** S–M · **Deps** best done after C01, so there's a
synchronized-output state worth reporting.

### C07 — XTWINOPS pixel-size queries (CSI 14t / 16t / 18t)
**Status: done.** `Grid::cell_px` (`src/core/grid.rs`), set by the GUI
backend at pane creation and on every font rebuild (`src/gui/window.rs`);
`18t` (always answerable from `cols`/`rows`) works in TUI mode too, `14t`/`16t`
decline gracefully when `cell_px` is `None` (TUI, or before the first frame).

**Current (before).** No matches for the `CSI t` window-manipulation family. Apps
that need to size an image precisely have no way to ask "how many pixels is
one cell?"

**Target.** Answer `16t` (cell size in pixels) and `18t` (text-area size in
cells) using the GUI backend's known `cell_w`/`cell_h`; `14t` (text-area
size in pixels) follows directly from the two.

**Why it matters.** `xterm` · `kitty` · `WezTerm`. Directly relevant to a
terminal that already invested in pixel-perfect graphics: without this
query, an app placing a Sixel or Kitty image has to guess cell dimensions
instead of computing them.

**Size** S · **Deps** `gui` feature (needs real pixel dimensions; TUI mode
can report best-effort or decline).

---

## Tier 2 — GPU / native-GUI backend parity

Not competitive gaps so much as internal ones — every item here is already
named in rusty_term's own docs as a known shortfall of one render/input/
protocol path relative to another. Listed for completeness since a
capability audit that only looked outward would miss them.

### C08 — GPU renderer: no ligature shaping
**Current.** `docs/FEATURES.md` #11 states outright: "the GPU renderer
keeps per-cell glyphs (no ligatures)." The CPU renderer's hand-rolled GSUB
shaper (`gui/shape.rs`) has no GPU-path counterpart.

**Target.** Run the same shaping pass before building the GPU glyph-atlas
instance list — or, if the multi-cell-quad pipeline this depends on stays
deprioritized, downgrade the marketing claim so `--gpu` doesn't silently
look feature-complete.

**Why it matters.** A user who picks `--features gui-gpu` for performance
shouldn't have to know they've traded away a feature the CPU path has for
free. About the two render paths agreeing with each other, not catching up
to a competitor.

**Size** L · **Deps** `gui-gpu` feature; blocked on the GPU multi-cell-quad
pipeline already flagged as an open item in `implementation-status.md`.

### C09 — GPU renderer: no pixel image compositing
**Current.** Only the CPU renderer composites pixel-perfect Sixel/Kitty/
iTerm2 images; both the GPU renderer and TUI mode fall back to half-block
approximation.

**Target.** A texture-upload path for the GPU renderer mirroring the CPU
compositor's per-image placement and scroll-eviction logic
(`Grid::render_image`, `evict_scrolled_images`).

**Why it matters.** Same rationale as C08 — GPU-path users get a visibly
worse experience for one of rusty_term's headline investments.

**Size** L · **Deps** `gui-gpu` feature.

### C10 — Native GUI: mouse motion reporting + right/middle buttons
**Status: done.** `gui/mouse.rs`'s `MouseEvent`/`SgrEncoder` gained a
`MouseButtonKind` (left/middle/right) and a `Move { dragging }` event kind;
`window.rs` now matches all three `winit::MouseButton`s (not just `Left`) and
reports `CursorMoved` through the encoder — `?1000` stays click-only, `?1002`
reports motion only while a button is held, `?1003` reports every motion
(idle hover uses xterm's "no button" motion code, a held button reports its
number).

**Current (before).** `docs/FEATURES.md` #2 explicitly flags this as "not yet": the
native GUI backend reports click/release/scroll but not
motion-while-button-held (`?1002`) or all-motion (`?1003`), and doesn't
report right/middle clicks.

**Target.** Wire motion tracking and the missing buttons into
`gui/mouse.rs`'s existing SGR/1006 encoder.

**Why it matters.** Drag-to-resize panes inside tmux, mouse-driven TUI file
managers, and Neovim mouse mode all depend on motion reporting.

**Size** M · **Deps** `gui` feature.

### C11 — Native GUI: DECCKM application-cursor tracking
**Status: done.** `Grid::app_cursor_keys` (`src/core/grid.rs`), set from
`handle_private_csi` on DEC mode `1` alongside the existing host relay;
`window.rs` reads it from the focused pane's grid on every key press instead
of a hardcoded `false`.

**Current (before).** `implementation-status.md` flags that `gui/input.rs`'s key
encoding hardcodes `app_cursor=false` rather than tracking the mode the
running app actually set.

**Target.** Thread the parser's DECCKM state through to the key encoder so
arrow keys encode as `CSI` vs `SS3` correctly inside apps that toggle
application-cursor mode (vim, less).

**Why it matters.** Without it, cursor-key behavior can silently diverge
from what the app expects, natively-GUI-mode-only.

**Size** S · **Deps** `gui` feature.

### C12 — iTerm2 protocol: geometry hints + additional formats
**Current.** `docs/FEATURES.md` #14 notes that OSC 1337's
`width`/`height`/`preserveAspectRatio` geometry hints go unhonored, and that
only PNG + baseline JPEG decode (via the from-scratch `core/{iterm,jpeg}.rs`
stack) — no GIF, no WebP, no progressive JPEG.

**Target.** Honor the geometry parameters when placing an image; add GIF
(static frame) and WebP decoding, and progressive JPEG support.

**Why it matters.** rusty_term's iTerm2 support is otherwise complete and
genuinely differentiated (a from-scratch decoder, no image crates); these
are the specific documented edges left short of full protocol compliance.

**Size** M · **Deps** none beyond the existing `core/iterm.rs`,
`core/jpeg.rs`.

---

## Tier 3 — Window & rendering features

Architectural rather than protocol work — both items touch the window/App
model in `gui/window.rs`, not just the parser.

### C13 — Multiple top-level OS windows
**Current.** The `App` struct holds a single `window: Option<Arc<Window>>`
and one `winit::EventLoop`; the model is one OS window with tabs and splits
inside it. There's no "open a second independent window" action.

**Target.** Support spawning an additional top-level window — either
drag-a-tab-out or a plain "new window" shortcut — sharing the backend and
config but owning its own tab set.

**Why it matters.** `kitty` · `WezTerm` · `Ghostty` · `Alacritty`. Every
major GUI competitor supports independent windows.

**Size** M · **Deps** `gui` feature; touches the winit `ApplicationHandler`
lifecycle, the largest architectural change among the recommended items.

### C14 — Background opacity + blur
**Current.** Both renderers paint fully opaque; no config key or CLI flag
for window transparency exists.

**Target.** A `[window] opacity` config key composited at the
window-surface level (not per-cell), plus platform blur where the OS
exposes it (macOS, KDE) as a best-effort extra.

**Why it matters.** `kitty` · `WezTerm` · `Alacritty` · `Ghostty`.
Universal among current GUI terminals.

**Size** M · **Deps** `gui` feature; softbuffer's opaque-only presentation
path needs a compositing change, wgpu's alpha blending is more direct.

---

## Tier 4 — Protocol & shell-integration extensions

Natural extensions of ground rusty_term already covers well — OSC 133 and
the L13 side-channel both suggest immediate next moves.

### C15 — OSC 633 (VS Code shell-integration superset)
**Current.** rusty_term implements OSC 133 (prompt marks, command
lifecycle, scrollback nav) in full. OSC 633 is explicitly named as an open
item in the repo's own docs ("Still open (intentional long-tail): OSC
633").

**Target.** Extend the existing OSC 133 handler to also accept the 633
command family, reusing the prompt-mark infrastructure already in place.

**Why it matters.** `VS Code integrated terminal`. VS Code's terminal is the
de facto reference implementation many shell-integration scripts target
first.

**Size** S–M · **Deps** builds directly on the existing OSC 133
implementation in `src/core/osc.rs`.

### C16 — Alternate scroll mode (DEC private mode 1007)
**Current.** No matches found. Mouse-wheel scroll always maps to
scrollback navigation; there's no translation to arrow-key presses inside
the alternate screen.

**Target.** When mode 1007 is set and the alt screen is active, translate
wheel events to Up/Down (or Page Up/Down) key sequences, matching xterm's
behavior.

**Why it matters.** `xterm` · `Alacritty` · `kitty`. Lets the mouse wheel
navigate `less`, `man`, and other pagers that never registered native mouse
support.

**Size** S · **Deps** none — small, self-contained.

### C17 — Command-output folding via OSC 133 marks
**Current.** rusty_term already tracks prompt starts (OSC 133;A) and
command boundaries (133;C/D) for scrollback navigation, but doesn't use
those marks for anything beyond jumping between them.

**Target.** Let a finished command's output collapse to one line in
scrollback (click or keybind to expand) — a scoped, achievable slice of
what Warp calls "command blocks," built on marks rusty_term already has.

**Why it matters.** `Warp (full block model, much larger scope)`. Genuinely
differentiating without committing to Warp's whole reimagining of the
terminal as a structured log.

**Size** M · **Deps** `gui` feature (folding is a rendering/interaction
concept; TUI passthrough has no natural place to put a fold toggle).

---

## Tier 5 — Forward-looking: watch, don't build yet

Real protocols with real adoption, but both are young enough that building
today risks targeting the wrong final shape. Included so they're on the
radar, not the backlog.

### C18 — Unicode width mode (DEC private mode 2027)
**Current.** rusty_term already does real UAX #29 grapheme clustering
(`unicode-segmentation`); mode 2027 would let an app explicitly opt in/out
of that behavior per-session.

**Target.** Defer. Originated with Contour, adopted narrowly, and current
research describes activity as stalled — testing across ~35 terminals found
grapheme-aware implementations rarely diverge from a simple 2-cell cap, and
Kitty has since proposed a superseding "text-sizing protocol" (C19) rather
than committing further to 2027.

**Why it matters.** `Contour` · `~10 others, narrow`. Worth tracking because
rusty_term's grapheme-clustering foundation means adding the mode later is
cheap — but building it now risks standardizing on the losing approach.

**Size** M · **Deps** none technically, but sequencing-wise: wait for the
field to converge.

### C19 — Text-sizing protocol (OSC 66)
**Current.** Not implemented; no per-cluster cell-width override exists in
the grid model.

**Target.** Defer for now. Kitty's protocol lets an app explicitly declare
how many cells a glyph cluster occupies — aimed at correctly rendering
complex scripts (Malayalam, Arabic, Devanagari). Even Ghostty has only
landed the OSC parser, with cell-association and rendering still open.

**Why it matters.** `kitty (originated)` · `Ghostty (parser only, 2026)`.
The right long-term answer to a real problem, but too early to build
against a settled reference implementation.

**Size** L · **Deps** meaningful complex-script rendering also needs a
HarfBuzz-equivalent shaping step rusty_term doesn't have outside the GUI
ligature shaper — and overlaps C25's territory.

---

## Tier 6 — Architecture & long-tail completeness

Lower urgency for different reasons — C20 is a genuine differentiation
opportunity nobody in the field has claimed; C21 and C22 are legacy VT/
ECMA-48 completeness with a small, real user base.

### C20 — Accessibility (screen readers / assistive tech)
**Current.** No accessibility integration in the native GUI backend;
current research turned up no evidence that kitty, WezTerm, Alacritty, or
Ghostty have meaningful screen-reader support either.

**Target.** Wire winit's accessibility tree (via `accesskit`, already a
known gap the wider Rust-terminal ecosystem shares per rusty_term's own
synthesis doc) to expose the visible screen and cursor position to
assistive technology.

**Why it matters.** Not a catch-up item — an open opportunity. Every
competitor researched shares this gap, so shipping even partial support
would be a genuine differentiator rather than table stakes.

**Size** L · **Deps** `gui` feature; `accesskit` integration with winit.

### C21 — Rectangular-area operations (DECCRA / DECFRA / DECERA)
**Current.** VT420 rectangular copy/fill/erase operations are cataloged in
rusty_term's own spec-tree reference but not implemented; no matches in
`src/`.

**Target.** Add the three operations to `Grid`: copy a rectangular cell
region (DECCRA), fill one with a character (DECFRA), erase one (DECERA).

**Why it matters.** Occasionally used by TUI apps for fast full-pane
operations. Included for ECMA-48/VT420 completeness rather than
competitive pressure.

**Size** S–M · **Deps** none.

### C22 — ECMA-48 Line/New Line mode (LNM, mode 20)
**Current.** rusty_term implements IRM (insert/replace mode) but none of
ECMA-48's other ANSI (non-DEC-private) modes — GATM, KAM, CRM, SRM, VEM,
HEM, PUM, FEAM, FETM, MATM, TTM, SATM, TSM, EBM, LNM — are handled.

**Target.** Implement LNM specifically (whether a bare LF also performs a
CR); treat the rest of the family as intentionally out of scope — they're
vestigial even in xterm's own implementation.

**Why it matters.** The smallest-value item on the whole list. LNM
specifically has occasionally-relied-upon behavior some legacy
line-oriented tools assume, unlike its siblings.

**Size** S · **Deps** none.

---

## Tier 7 — OS-interface internals

Async I/O backend depth, not user-facing capability — these change how
rusty_term talks to the kernel, not what it can display. Both explicitly
named `[ ]` in the repo's own `implementation-status.md`. They compete on
throughput/latency under heavy I/O, not on features.

### C23 — io_uring backend (Linux)
**Current.** rusty_term's Unix runtime uses tokio's `AsyncFd`/epoll path
uniformly; `implementation-status.md` marks io_uring `[ ]`.

**Target.** An io_uring-backed reactor as an alternative to `AsyncFd` on
Linux, for lower syscall overhead under high-throughput output.

**Why it matters.** `kitty (explored)`. Real but marginal for interactive
use — most terminal I/O is nowhere near syscall-bound.

**Size** L · **Deps** Linux-only; would need to coexist with the existing
`AsyncFd` path for portability to macOS/*BSD.

### C24 — IOCP-native async (Windows)
**Current.** The Windows runtime bridges ConPTY's blocking pipes into
tokio channels via dedicated threads rather than using IOCP directly;
`implementation-status.md` marks this `[ ]`.

**Target.** An IOCP-native I/O path replacing the thread-bridging approach.

**Why it matters.** Would reduce thread overhead on Windows under heavy
output; the current thread-bridge approach is simple and already proven to
work (verified on Windows 11), so this is a performance refinement, not a
correctness fix.

**Size** L · **Deps** Windows-only.

---

## Tier 8 — Internationalization

One item, sized honestly — big enough that it deserves its own scoping pass
rather than a slot in a sprint alongside seven other things.

### C25 — Bidirectional text + Unicode normalization
**Current.** No `unicode-bidi` or `unicode-normalization` crate in the
dependency tree; explicitly flagged absent in `implementation-status.md`
("Bidi/normalization... explicitly out"). rusty_term's UAX #29 grapheme
clustering handles emoji/combining marks correctly but not right-to-left
script layout (Arabic, Hebrew) or canonical normalization (NFC/NFD).

**Target.** UAX #9 bidirectional algorithm for RTL script layout, plus NFC
normalization at the grapheme-cluster boundary for search/selection
correctness. Large enough to want its own design pass — the grid's
cell-layout model assumes strict left-to-right column order throughout, and
bidi genuinely breaks that assumption.

**Why it matters.** A real, if less common, correctness requirement
(Arabic/Hebrew shells, filenames, LLM output in RTL languages). Not urgent
competitively — most researched terminals have partial-at-best RTL support
too — but the single largest deferred item across this entire assessment.

**Size** XL · **Deps** none technically, but touches the grid's
cell-layout model broadly enough to warrant its own scoping pass; overlaps
C19's complex-script-shaping territory.

---

## Tier 9 — L13 protocol depth (no external comparison)

These extend rusty_term's own invention. No other terminal has an
MCP/LSP/ACP side-channel to compare adoption against, so the "who else has
this" lens that drives every other tier doesn't apply. Listed because the
repo's own docs flag them as intentional long-tail, not because of
competitive pressure.

### C26 — DAP / Jupyter protocol bridges
**Current.** L13 speaks `channel`/`mcp`/`lsp`/`acp`/`render`. DAP (Debug
Adapter Protocol) and the Jupyter kernel protocol are named in the repo's
own docs as intentional long-tail items, not implemented.

**Target.** Additional sub-protocol tags under the same OSC 5379 transport,
following the initialize-negotiation pattern already established for
`lsp`/`acp` in `l13/src/lib.rs`.

**Why it matters.** Would let a debugger or notebook kernel running inside
rusty_term expose itself the same way MCP tools do today. Speculative value
until a concrete client exists that wants it.

**Size** L · **Deps** `l13` feature; the `rusty_term_l13` crate's
`TerminalState` trait boundary.

### C27 — Full LSP/ACP backends behind the existing negotiation endpoints
**Current.** The `lsp` and `acp` sub-protocols implement only the
`initialize` handshake, correctly advertising empty capabilities since "a
terminal has no language/agent backend of its own" (`l13/src/lib.rs`).

**Target.** An actual backend behind one or both endpoints — e.g.
registering `rusty_lsp`'s own `Server` so rusty_term speaks real LSP
methods, not just negotiates them.

**Why it matters.** Turns a protocol stub into a working feature, but it's
unclear what the terminal itself would be a language server *for* — closer
to "close the loop for completeness" than a felt need from any known
client.

**Size** L · **Deps** `l13` feature; would pull in more of `rusty_lsp`'s
surface than the JSON-RPC/type layer currently used.

---

## Considered and rejected

### R01 — Alternate-screen resize reflow
**Considered.** Whether the alternate screen should get the same
wrap-aware reflow the primary screen has (`Grid::resize` →
`reflow_history`), instead of being clipped/extended on resize as it is
today.

**Verdict: not recommended.** Full-screen apps (vim, htop, less) own their
own repaint on `SIGWINCH` and don't expect the terminal to preserve
alt-screen content across a resize the way scrollback does — xterm, kitty,
and Alacritty all treat the alt screen the same clipped way rusty_term
already does. Included so the audit shows its work: this was checked, not
missed.

---

## Suggested sequencing

Cheapest wins that unblock the next wave, most-requested first. Tiers 7–9
and the watch-list items in Tier 5 aren't sequenced — pull any forward
opportunistically; none block anything else here.

| Wave | Items | Rationale |
|---|---|---|
| 1 | C01, C04, C05, C07 | Cheapest, most self-contained protocol wins — sync output, OSC 22, title stack, pixel-size queries. No dependencies between them; parallelizable. |
| 2 | C02, C06 | Undercurl is the highest-value remaining Tier 1 item; DECRQM benefits from C01 already existing to report on. |
| 3 | C10, C11, C03 | Native-GUI input parity, ending with the larger Kitty-keyboard-protocol lift once the smaller input fixes are out of the way. |
| 4 | C15, C16, C17 | Shell-integration extensions that build directly on OSC 133 infrastructure already in place. |
| 5 | C14, C13 | Window-model work — opacity first (contained), multi-window second (touches the App/EventLoop lifecycle). |
| 6 | C08, C09, C12 | GPU/CPU render-path parity, batched together — C08/C09 gated on the multi-cell-quad pipeline, C12 independent but thematically adjacent. |
| 7 | C22, C21 | Cheap legacy-completeness cleanup once everything with real user impact is done. |
| — | C18, C19, C20, C23, C24, C25, C26, C27 | Not sequenced. C18/C19 are watch-and-wait (protocol churn); C20 is a standalone differentiation bet; C23/C24 are performance-only refinements; C25 needs its own scoping pass before it can be sequenced at all; C26/C27 are speculative L13 depth with no known client asking for them yet. |

## Sources

1. [Terminal Emulators Comparison Table (2026) — Terminal Trove](https://terminaltrove.com/compare/terminals/)
2. [Terminal Spec: Synchronized Output — C. Parpart](https://gist.github.com/christianparpart/d8a62cc1ab659194337d73e399004036)
3. [Synchronized Output — Contour Terminal Emulator](https://contour-terminal.org/vt-extensions/synchronized-output/)
4. [Implement synchronized update control sequences — microsoft/terminal](https://github.com/microsoft/terminal/issues/8331)
5. [terminal-unicode-core — Unicode Core specification for Terminal](https://github.com/contour-terminal/terminal-unicode-core)
6. [Grapheme Clustering (mode 2027) — kovidgoyal/kitty#7799](https://github.com/kovidgoyal/kitty/issues/7799)
7. [The text sizing protocol — kitty](https://sw.kovidgoyal.net/kitty/text-sizing-protocol/)
8. [Implement the Text Sizing Protocol (OSC 66) — ghostty-org/ghostty#10333](https://github.com/ghostty-org/ghostty/issues/10333)
9. [Rendering complex scripts in terminal and OSC 66 — S. Thottingal](https://thottingal.in/blog/2026/03/22/complex-scripts-in-terminal/)
10. [Pull of the Undercurl — R. Travitz](https://ryantravitz.com/blog/2023-02-18-pull-of-the-undercurl/)
11. [Curly Underlines in Kitty + Tmux + Neovim](https://evantravers.com/articles/2021/02/05/curly-underlines-in-kitty-tmux-neovim/)
12. [Comprehensive keyboard handling in terminals — kitty](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)
13. [Your Terminal Can't Tell Shift+Enter from Enter](https://blog.fsck.com/agent-blog/2026/02/26/terminal-keyboard-protocol/)
14. [Keyboard Encoding — Wez's Terminal Emulator](https://wezterm.org/config/key-encoding.html)
15. [Windows Terminal Preview 1.25: Kitty protocol](https://4sysops.com/archives/windows-terminal-preview-125-kitty-protocol-settings-search-and-gui-for-key-bindings/)
16. [Multiplexing — Wez's Terminal Emulator](https://wezterm.org/multiplexing.html)
17. [kde_window_background_blur — WezTerm](https://wezterm.org/config/lua/config/kde_window_background_blur.html)
18. [Choosing a terminal emulator in 2026 — Luminoid](https://blog.luminoid.dev/Terminal-Emulator-Comparison-2026/)
19. [Agent Mode: Natural-Language Coding Agents in Warp](https://www.warp.dev/ai)
20. [Warp Guide 2026: Agent Mode, MCP, Open Source & Deployments](https://www.deployhq.com/guides/warp)
21. [State of Terminal Emulators in 2025: The Errant Champions — J. Quast](https://www.jeffquast.com/post/state-of-terminal-emulation-2025/)
