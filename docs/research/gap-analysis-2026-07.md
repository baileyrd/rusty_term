# rusty_term Gap Analysis — July 2026

A fresh competitive gap analysis of rusty_term against the mid-2026 terminal
landscape: kitty (0.43+), Ghostty (1.3), WezTerm, Alacritty, Windows Terminal,
iTerm2, foot, Contour, Konsole/VTE, Rio, and Warp — plus multiplexers (tmux,
Zellij) where their features have migrated into emulators proper.

This document **complements** the earlier
[`capability-assessment-2026.md`](capability-assessment-2026.md) (28 items, 27
recommended; most Tier 1–4 items have since landed). It does two things:

1. **Section A** identifies **37 new gaps** not covered by that assessment,
   each verified against the current source tree (a `grep`/read of `src/` and
   `l13/` on 2026-07-11), with adopters and sizing.
2. **Section B** carries forward the **13 still-open items** from the earlier
   assessment so this file is a complete, single-page view of everything
   outstanding.

Every "Current" claim below names the file(s) checked. Adoption claims are
sourced from July-2026 documentation and release notes (see
[Sources](#sources)), notably Ghostty 1.3 (March 2026) and kitty 0.43+.

## Summary — new gaps (Section A)

Priority: **P1** = competitive table stakes or high daily-use value ·
**P2** = strong differentiation or parity with 2+ major peers · **P3** =
long-tail/legacy/cosmetic · **W** = watch, don't build yet.

| ID | Gap | Priority | Notable adopters | Size |
|---|---|---|---|---|
| ✅ G01 | OSC 9;4 progress reporting | P1 | Ghostty, Windows Terminal, kitty, Konsole, mintty | S–M |
| ✅ G02 | Light/dark scheme detection + mode 2031 notify | P1 | kitty, Ghostty, Contour, foot, iTerm2 | S–M |
| ✅ G03 | DECRQSS settings report (`DCS $ q`) | P1 | xterm, kitty, foot, WezTerm | S |
| ✅ G04 | XTSMGRAPHICS Sixel geometry query | P2 | xterm, foot, WezTerm, Contour | S |
| ✅ G05 | DECSLRM/DECLRMM left/right margins | P3 | xterm, foot, Contour | M |
| ✅ G06 | Bell: audible/visual/urgency handling | P1 | universal | S–M |
| ✅ G07 | Kitty graphics: animation + Unicode placeholders | P2 | kitty, Ghostty (placeholders) | L |
| ✅ G08 | OSC 99 rich desktop notifications | P2 | kitty, Ghostty | S–M |
| ✅ G09 | Primary selection + copy-on-select + OSC 52 `p` | P1 (Linux) | kitty, Alacritty, foot, WezTerm, VTE | S–M |
| G10 | win32-input-mode (ConPTY key fidelity) | P2 | Windows Terminal, WezTerm | M |
| ✅ G11 | Kitty keyboard flags 2/4/16 in GUI | P2 | kitty, Ghostty, WezTerm, foot | M |
| ✅ G12 | Keypad application mode (DECKPAM/DECKPNM) | P2 | universal (legacy but probed) | S |
| ✅ G13 | Native focus reporting (mode 1004) in GUI | P1 | universal | S |
| ✅ G14 | SGR-pixel mouse (mode 1016) in GUI | P2 | xterm, kitty, foot, WezTerm | S |
| ✅ G15 | DECSCA/DECSED/DECSEL protected areas | P3 | xterm, VTE, Contour | S–M |
| ✅ G16 | Implicit URL/path detection + hints mode | P1 | kitty, WezTerm, Alacritty, Windows Terminal | M |
| ✅ G17 | Double/triple-click word/line selection | P1 | universal | S–M |
| ✅ G18 | Keyboard copy mode (vi-style selection) | P2 | kitty, WezTerm, Windows Terminal, tmux | M |
| ✅ G19 | Scrollbar | P2 | Ghostty 1.3, kitty 0.43, Windows Terminal, Konsole | M |
| ✅ G20 | Command-completion notifications | P1 | Ghostty 1.3, iTerm2 | S–M |
| ✅ G21 | Click-to-move-cursor at prompt | P2 | Ghostty 1.3, kitty | M |
| ✅ G22 | Regex + Unicode case-folded search | P2 | kitty, WezTerm, Ghostty, iTerm2 | M |
| ✅ G23 | Drag-and-drop file → path paste | P1 | WezTerm, iTerm2, Windows Terminal, Ghostty | S |
| ✅ G24 | Color emoji fonts (COLR/CBDT/sbix) | P1 | kitty, Ghostty, WezTerm, iTerm2, Windows Terminal | L |
| ✅ G25 | Built-in box-drawing/Powerline glyph synthesis | P2 | Ghostty, kitty, WezTerm | M |
| ✅ G26 | Minimum contrast enforcement | P3 | WezTerm, Ghostty | S |
| ✅ G27 | Pane resize / zoom / directional focus | P1 | kitty, WezTerm, iTerm2, tmux, Zellij | M |
| ✅ G28 | Broadcast input across panes | P3 | iTerm2, Windows Terminal, tmux | S–M |
| G29 | Rich-text (HTML/RTF) clipboard copy | P3 | Ghostty 1.3 (macOS), iTerm2, Windows Terminal | M |
| G30 | Quake-style quick terminal + global hotkey | P2 | Ghostty, iTerm2, Windows Terminal, Guake | M–L |
| ✅ G31 | Single-instance / daemon mode | P2 | foot (server), kitty | M |
| ✅ G32 | Sessions / startup layouts / workspaces | P2 | kitty 0.43, WezTerm, Windows Terminal | M–L |
| ✅ G33 | Remote control / scripting API | P2 | kitty `@`, WezTerm CLI, Ghostty AppleScript | M |
| ✅ G34 | Profiles (shell+theme+font bundles) | P2 | Windows Terminal, iTerm2, Konsole, WezTerm | M |
| G35 | Multiple-cursors protocol | W | kitty 0.43 (originated) | M |
| G36 | Cursor trail / animated cursor | P3 | kitty 0.43, neovide-inspired | S–M |
| ✅ G37 | Fuzzing harness for hand-rolled decoders | P1 (infra) | industry practice (Alacritty, VTE fuzz targets) | M |

---

## Section A — New gaps

### A.1 Protocol & VT fidelity

#### G01 — OSC 9;4 progress reporting (ConEmu extension)
**Status: done.** `Grid::set_progress` (`src/core/grid.rs`), parsed from ConEmu `OSC 9;4;st;pr` (`src/core/osc.rs`) — states normal/error/indeterminate/warning, percent clamped, state 0/unknown clears, cleared on RIS — and relayed to the host for TUI mode. The window's tab strip renders it as a ` 42%` / ` !42%` (error/paused) / ` …` (indeterminate) suffix on the tab label (`chrome_row`).
**Current.** Explicitly excluded: `src/core/osc.rs` treats OSC 9 as an iTerm2
free-text notification and skips ConEmu's numeric `9;N;…` subcommands
(documented in `docs/FEATURES.md` #9).
**Target.** Parse `OSC 9;4;st;pr` (states: clear/normal/error/indeterminate/
warning) into a `Grid` progress field; render as a tab-strip / chrome
indicator in the GUI and relay in TUI mode. Windows: additionally surface via
`ITaskbarList3` taskbar progress.
**Why.** Adoption crossed the line in 2025–26: Ghostty 1.2+ draws a native
progress bar, Windows Terminal/ConEmu show taskbar progress, kitty, Konsole,
and mintty parse it — and emitters now include systemd, Zig, and (in
progress) uv/Textual. A terminal with a tab bar and notification plumbing
already has everywhere this needs to surface.
**Size** S–M · **Deps** none (chrome rendering: `gui` feature).

#### G02 — Light/dark appearance: detection, notification (mode 2031), query
**Status: done.** `theme = "auto"` follows the OS appearance (winit theme at window creation + live `ThemeChanged` events), resolving to `theme_dark` / `theme_light` presets (defaults: built-in dark / Solarized Light). `DSR ?996n` answers `CSI ?997;1|2n` from the live default background's luminance (`Grid::appearance_is_dark`, so OSC 11 changes count too — verified on a real PTY); DEC mode 2031 is tracked (DECRQM-answerable), relayed to the host in TUI mode, and an appearance flip under `apply_theme_live` sends the unsolicited report to subscribed panes.
**Current.** No matches for `2031`, `997`, or `996` in `src/`. Theme is a
static config key; the terminal neither knows nor reports OS appearance.
**Target.** Three pieces: (1) query the OS light/dark appearance (winit theme
event) and support `theme = "auto"` with paired light/dark palettes; (2)
answer `DSR ?996n` (report current color-scheme preference); (3) honor DEC
mode 2031 (unsolicited notification on appearance change). Contour originated
the spec; kitty, Ghostty, foot, and iTerm2 implement equivalents, and Neovim
consumes it to flip `background` automatically.
**Why.** OS-level dark-mode switching is mainstream; apps increasingly probe
for it, and rusty_term already has live retheming (`apply_theme_live`) to
hang this off.
**Size** S–M · **Deps** none for the VT side; `gui` for OS detection.

#### G03 — DECRQSS (`DCS $ q … ST`) — request selection or setting
**Status: done.** `AnsiParser::answer_decrqss`/`pen_sgr_params` (`src/core/parser.rs`), dispatched from `finish_dcs` on the `$q` prefix; answers `m` (current pen SGR, colon-form underline style, truecolor only when non-default), `r` (DECSTBM), and `SP q` (DECSCUSR), `DCS 0$r` otherwise. Verified end-to-end through the TUI binary on a real PTY.
**Current.** No handler; `grep '\$q' src/` is empty. Probes get silence.
**Target.** Answer at least the settings rusty_term tracks: SGR (`m`),
DECSTBM (`r`), DECSCUSR (` q`), and report "invalid" (`DCS 0$r`) otherwise.
**Why.** xterm, kitty, foot, and WezTerm answer it; vim and tmux use DECRQSS
to probe true SGR state, and its absence makes rusty_term look like a
lower-fidelity terminal to capability sniffers. Small, contained parser work
next to the existing XTGETTCAP responder.
**Size** S · **Deps** none.

#### G04 — XTSMGRAPHICS (`CSI ? Pi;Pa;Pv S`) — graphics attribute query/set
**Status: done.** `AnsiParser::answer_xtsmgraphics` (`src/core/parser.rs`) answers item 1 (color registers, 256) and item 2 (Sixel geometry, `sixel::MAX_DIM` = 2000²) for all four actions — the limits are fixed, so set/reset succeed by reporting actual values; item 3 (ReGIS) reports an item error. Verified end-to-end on a real PTY.
**Current.** No handler. Apps can't ask "how many Sixel color registers /
what max geometry?"
**Target.** Answer item 1 (color registers) and item 2 (Sixel geometry, from
`MAX_DIM` in `src/core/sixel.rs`); accept set requests by clamping.
**Why.** notcurses, chafa, and img2sixel probe XTSMGRAPHICS before choosing
an output strategy; without an answer they fall back to conservative
defaults or skip Sixel. Complements the already-shipped XTWINOPS work (C07).
**Size** S · **Deps** none.

#### G05 — DECSLRM / DECLRMM — left/right margins (DEC mode 69)
**Status: done.** `Grid::{lr_margin_mode, left_margin, right_margin}` with DECLRMM (`?69`, DECRQM-answerable; reset restores full width) gating `CSI Pl;Pr s` as DECSLRM vs legacy SCP. Margin-aware behaviors — all behind a single `side_margins_active()` guard so the common path pays one boolean: CR to the left margin, autowrap/pin at the right margin, LF/IND/RI and SU/SD scrolling only the margin band (never forming scrollback), IL/DL band-limited and requiring the cursor within the margins, DECOM addressing columns relative to the left margin, and margins reset on resize/RIS/DECSTR. Verified end-to-end on a real PTY (mode 69 DECRQM).
**Current.** Only top/bottom margins (DECSTBM) exist; no vertical-split
margin state in `Grid`.
**Target.** Track mode 69 (DECLRMM), `CSI Pl;Pr s` (DECSLRM), and make
scrolling/IND/RI/IL/DL respect the four-sided region.
**Why.** VT420 completeness with real users: xterm, foot, and Contour
implement it; esctest/vttest exercise it; some TUIs (and tmux's
pass-through) use side margins for fast column scrolling. Same tier as the
already-landed rectangular ops (C21).
**Size** M · **Deps** none, but touches the scroll paths broadly.

#### G06 — Bell: audible / visual bell, urgency, tab indicator
**Status: done (visual/urgency/badge; no audio).** BEL sets `Grid::bell` and relays the byte to the host (TUI mode rings the host's bell with the host's own policy). The window drains it (`App::service_alerts`): when unfocused it requests OS window attention (`request_user_attention`), and a background/unfocused tab gets a red `•` badge in the tab strip, cleared once the tab is active in a focused window. `bell = false` disables.
**Current.** BEL (0x07) is consumed as a C0 control with no observable
effect anywhere; no `bell` config key exists.
**Target.** Config-selectable: system beep, visual flash, window urgency
hint (`winit` attention request), and a bell badge on unfocused tabs.
Relay in TUI mode (already implicit).
**Why.** Every terminal in the field does *something* with BEL; silently
eating it breaks a decades-old contract (`echo -e '\a'`, IRC/mail TUIs,
`tput bel` in scripts). The tab-badge variant is table stakes in tabbed
terminals (Windows Terminal, Konsole, iTerm2).
**Size** S–M · **Deps** `gui` for visual/urgency variants.

#### G07 — Kitty graphics protocol: animation + Unicode placeholders
**Status: done (store/put/delete + Unicode placeholders + animation; z-index/quadrant composition out of scope).** The decoder (`src/core/kitty.rs`) now backs `a=t` with a bounded image store on the grid, `a=p` places by id (honoring `c`/`r` via `render_image_sized`), and `a=d` deletes (by id, or whole-store for the other scopes — honest over-deletion). **Placeholders:** `U=1` records a virtual placement; `U+10EEEE` cells decode the image id from the truecolor foreground (+ high byte from the third diacritic) and row/column from the first two diacritics — the 297-entry table lives in `src/core/kitty_diacritics.rs`, extracted verbatim from a published implementation of kitty's frozen `rowcolumn-diacritics.txt` (the docs host is blocked from this sandbox; the table is the wire format and must never be regenerated from newer Unicode data). Cells omitting diacritics infer from left/top neighbors per the spec. The CPU renderer paints each placeholder cell's slice of the placement grid (nearest-neighbor). **Animation:** `a=f` composites partial frames onto the previous frame at `x`/`y` with a `z` ms gap (floored to 40); `a=a` runs/stops; the window's frame timer advances playing images and repaints. TUI/GPU paths keep static first-frame half-blocks (existing parity note). Store+put verified end-to-end on a real PTY.
**Current.** `src/core/kitty.rs` implements transmit/display/query and
chunking, but not the animation subcommands (`a=a`, frame composition) nor
Unicode placeholders (`U+10EEEE` + diacritics, `p=`/virtual placements) —
no matches for either.
**Target.** (1) Unicode placeholder placements, which are how images survive
tmux/mux passthrough — arguably the highest-value slice; (2) animation
frames with a frame timer in the render loop.
**Why.** kitty clients (icat inside tmux, timg, notcurses) increasingly use
placeholders; Ghostty implements them too. Animation matters less but is
part of "full protocol" claims.
**Size** L (placeholders M on their own) · **Deps** frame timer for
animation; the CPU overlay compositor exists already.

#### G08 — OSC 99 (kitty desktop notifications protocol)
**Status: done (title/body incl. multi-part + base64; actions/icons/queries out of scope).** `src/core/osc.rs`'s `99` arm parses the metadata (`i`, `d`, `p`, `e`), accumulates multi-part notifications by identifier on the grid (bounded), base64-decodes `e=1` payloads, and finalizes into the existing notification pipeline on `d=1`; non-text payload types are ignored. Relayed to the host for TUI mode.
**Current.** Only OSC 9 (free text) and OSC 777 (`notify`) are handled
(`src/core/osc.rs`); OSC 99 has no matches.
**Target.** Parse OSC 99's metadata form (id, title/body parts, urgency,
`d=0/1` completeness) and feed the existing `Grid::notifications` +
`App::service_notifications` plumbing; report close/activation back where
the OS notifier allows.
**Why.** kitty and Ghostty support it; it's the only notification protocol
with IDs (update/replace), urgency, and actions. rusty_term already built
the delivery pipeline — this is a parser-front-end extension.
**Size** S–M · **Deps** existing notification plumbing.

#### G09 — Primary selection: copy-on-select, middle-click paste, OSC 52 `p`
**Status: done.** Copy-on-select (drag release and word/line multi-click) and middle-click paste target the PRIMARY selection via `arboard`'s Linux extension (`src/gui/window.rs::{copy_selection_primary, paste_primary}`; middle-click defers to a mouse-tracking child). OSC 52's selection field now routes: a leading `p` targets PRIMARY for both set and query (`Grid::{clipboard_set_primary, clipboard_query_primary}`, `osc52_reply` carries the target), still relayed to the host in TUI mode — verified on the wire. Non-Linux platforms fall back to the regular clipboard.
**Current.** Selection copies only via explicit Ctrl+Shift+C to the CLIPBOARD
target (`arboard` default); no matches for primary selection; middle-click
is only a mouse-report button; OSC 52 handles the `c` clipboard only.
**Target.** On X11/Wayland: update PRIMARY on selection (arboard supports
`LinuxClipboardKind::Primary`), paste PRIMARY on middle-click when the child
isn't mouse-tracking, and service OSC 52's `p` selection argument.
**Why.** Muscle-memory-level expectation for the Linux audience most likely
to run a from-scratch Rust terminal; kitty, Alacritty, foot, WezTerm, and
VTE all do it.
**Size** S–M · **Deps** `gui`; Linux-specific paths.

#### G10 — win32-input-mode (ConPTY keyboard fidelity)
**Current.** The Windows backend writes plain VT bytes to ConPTY; no matches
for win32-input-mode (`CSI … _` encoding).
**Target.** In GUI-on-Windows, honor a child's `DCS`-negotiated
win32-input-mode by encoding full key records (vk, scan code, key-down/up),
as Windows Terminal does; in TUI mode, relay the enable/disable sequences.
**Why.** Without it, Windows console apps under ConPTY can't see key-up
events or distinguish keys VT encoding conflates — the Windows analogue of
the Kitty keyboard protocol, and WSL/PowerShell users notice via `PSReadLine`
edge cases.
**Size** M · **Deps** Windows + `gui`.

#### G11 — Kitty keyboard protocol: flags 2/4/16 in the native GUI
**Status: done (flags 2/4/16; flag 8 report-all intentionally out).** `encode_full` (`src/gui/input.rs`) threads a key's phase (press/repeat/release from winit's `state`+`repeat`), the flag-4 alternate (the shifted key while Shift is held), and flag-16 associated text (control chars filtered, never on release) through the `CSI u` and legacy-functional encodings — event types ride the mods sub-parameter (`CSI 1;mods:event A`), releases of plain text keys stay silent per the spec's no-flag-8 fallback, and the window now forwards key releases to a flag-2 subscriber (UI layers still only ever see presses).
**Current.** C03 landed flag 1 (disambiguate) only; `src/gui/input.rs`
doesn't encode event types (flag 2), alternate keys (4), or associated text
(16) — documented as partial in the earlier assessment, promoted here to
its own item so it doesn't stay buried.
**Target.** Plumb winit press/repeat/release state and key-without-modifiers
data through the encoder; report event types and alternate keys, then
associated text.
**Why.** Neovim, Helix, and fish request higher enhancement levels when
offered; flag 2 (release events) is what enables hold-style keybindings.
**Size** M · **Deps** `gui`.

#### G12 — Keypad application mode (DECKPAM / DECKPNM, `ESC =` / `ESC >`)
**Status: done.** `Grid::app_keypad`, set by `ESC =`/`ESC >` (relayed to the host for TUI mode) and DEC mode 66 (DECNKM, DECRQM-answerable), reset by RIS/DECSTR; `gui/input.rs::encode_numpad` encodes numpad-located keys as `SS3 j`–`y`/`X`/`M` when the mode is on and no modifiers are held (modified keys keep the legacy encoding that carries the xterm modifier parameter).
**Current.** No matches for keypad-mode tracking; the GUI encoder has no
numpad application encoding.
**Target.** Track the mode in `Grid`; encode numpad keys as `SS3 p`–`SS3 y`
when set (GUI); relay in TUI (likely already passes through as ESC
dispatch — verify).
**Why.** Legacy but universally implemented, exercised by vttest, and some
full-screen apps still set it; cheap next to the existing DECCKM tracking.
**Size** S · **Deps** `gui` for the encoder side.

#### G13 — Native focus reporting (mode 1004) in the GUI window
**Status: done.** `Grid::focus_reporting`, tracked from DEC mode 1004 alongside the existing host relay and answerable via DECRQM; the window backend handles `WindowEvent::Focused` and writes `CSI I`/`CSI O` to the focused pane when the mode is set (plus a redraw either way — the cursor renders only while focused).
**Current.** Mode 1004 is relayed in TUI mode (`is_host_input_mode`), but
the GUI window never sends `CSI I`/`CSI O` — there is no
`WindowEvent::Focused` handler that reports to the child (checked
`src/gui/window.rs`; `Focused` isn't matched).
**Target.** Track mode 1004 in `Grid` (like `mouse_modes`) and emit
`CSI I`/`CSI O` on focus gain/loss.
**Why.** vim/neovim (`FocusGained`/`FocusLost` autocmds), tmux, and
lazygit rely on it for auto-reload and cursor-style switching; every GUI
competitor reports it. One of the smallest highest-value items in this
document.
**Size** S · **Deps** `gui`.

#### G14 — SGR-pixel mouse reporting (mode 1016) in the GUI
**Status: done.** `App::report_mouse` (`src/gui/window.rs`) overrides the report position with `pane_pixel_point` — the pointer's pixel offset within the focused pane's text area, clamped into the pane — whenever `?1016` is set (`mouse_modes.extended` bit 8, already tracked); the SGR encoder itself is unchanged, as 1016 shares the SGR format.
**Current.** `src/gui/mouse.rs` encodes SGR (1006) cell coordinates only;
1016 is relayed in TUI but not encoded natively.
**Target.** When the child sets 1016, report pixel coordinates in the SGR
format (the encoder already has the window's pixel geometry).
**Why.** Apps doing sub-cell hit-testing (image viewers, kitty's own
kittens, notcurses demos) use it; trivial delta on the existing encoder.
**Size** S · **Deps** `gui`.

#### G15 — Protected areas (DECSCA / DECSED / DECSEL)
**Status: done.** `ATTR_PROTECTED` (cell bit 13, deliberately not an SGR rendition — SGR 0 must not clear it, so the parser tracks DECSCA separately and stamps at write time); `CSI Ps " q` sets/clears it, `CSI ? Ps J`/`K` (DECSED/DECSEL) erase selectively around protected cells, DECRQSS answers `"q`, and RIS/DECSTR reset the protection state. Verified end-to-end on a real PTY (protected text survives `?2K` while unprotected text is erased).
**Current.** Not implemented; ED/EL erase everything indiscriminately.
**Target.** A per-cell protection bit set by `CSI Ps " q`, honored by the
selective variants `CSI ? Ps J` / `CSI ? Ps K`.
**Why.** Legacy completeness (xterm, VTE, Contour); exercised by vttest and
occasionally used by forms-style TUIs. Same "long-tail correctness" tier as
the landed LNM/rect-ops work.
**Size** S–M · **Deps** one attribute bit in `Cell`.

### A.2 Interaction & UX

#### G16 — Implicit URL/path detection + keyboard hints mode
**Status: done (detection + Ctrl+click + link menu; label-overlay hints deferred).** `detect_urls`/`Grid::url_at`/`Grid::visible_links` (`src/core/grid.rs`) scan logical lines (following soft wraps) for `http(s)/ftp/file/mailto/www.` URLs over RFC 3986 characters, trimming trailing sentence punctuation and unbalanced closers. Ctrl+click falls back to detected URLs when no OSC 8 link covers the cell; Ctrl+Shift+O (`open_links` action) lists every visible link — explicit and detected — in the dropdown menu for keyboard-driven opening. kitty-style on-screen label overlays remain future renderer work.
**Current.** Only explicit OSC 8 hyperlinks are clickable
(`Grid::link_at`); plain `https://…` text printed by any ordinary program is
inert. No regex machinery exists in the tree.
**Target.** (1) Detect URLs (and optionally file paths) in the viewport by
scanning logical lines — a hand-rolled matcher fits the no-deps ethos —
and make them Ctrl+clickable like OSC 8 links; (2) a keyboard "hints" mode
(kitty hints / WezTerm quick-select): overlay labels on matches, type the
label to open/copy without touching the mouse.
**Why.** The single most-used link affordance in practice — most output
comes from programs that never emit OSC 8. kitty, WezTerm, Alacritty,
Windows Terminal, iTerm2, and Ghostty all auto-detect URLs.
**Size** M · **Deps** `gui`; reuses the OSC 8 open path + scheme allowlist.

#### G17 — Double-click word / triple-click line selection
**Status: done.** Consecutive-click detection in `on_left_press` (`src/gui/window.rs`, `DOUBLE_CLICK_MS` window, same cell): two clicks call `Grid::select_word_at` — URL/path-friendly word characters (quotes/brackets/whitespace/`;`/`,`/`|` are the separators), a separator cell selects itself — and three call `Grid::select_line_at`, which follows the per-row soft-wrap bits to select the whole logical line; a fourth click cycles back to drag-selection.
**Current.** Double-click is only implemented on the drag strip (toggle
maximize); on the grid there is no multi-click selection — `src/gui/window.rs`
has single-click-drag selection only.
**Target.** Click-count detection on pane content: 2× selects a word
(configurable word characters, grapheme-aware), 3× selects a logical line
(following soft wraps — the reflow machinery already models this).
**Why.** Universal expectation in literally every competitor; its absence is
felt within minutes of real use.
**Size** S–M · **Deps** `gui`.

#### G18 — Keyboard copy mode (vi-style selection)
**Status: done.** Ctrl+Shift+Space (rebindable `copy_mode`) enters a vi-style keyboard selection mode: hjkl/arrows move (scrolling into history at the edges), `0`/`$`/Home/End, PageUp/Down, `g`/`G` top/bottom of scrollback, `v`/Space anchors, `y`/Enter copies (clipboard + primary) and exits, Esc/`q` cancels — with a chrome hint bar while active. Enabled by refactoring `Selection` to **absolute** coordinates (`(col, abs_row)` over scrollback+screen), which also fixes the pre-existing limitation that drag selection broke while scrolled: selections now stay anchored to their text across scrolling, and word/line selection works in history.
**Current.** Selection is mouse-only; no matches for a copy/mark mode.
**Target.** A keybind enters a mode where the keyboard moves a selection
cursor over scrollback (h/j/k/l + word/line motions, `v`/`V` to anchor,
`y` to copy), rendered with the existing selection highlight.
**Why.** kitty, WezTerm, Windows Terminal ("mark mode"), tmux, and Zellij
all have it; keyboard-centric users treat it as a requirement.
**Size** M · **Deps** `gui`; builds on the existing viewport/selection
model.

#### G19 — Scrollbar
**Status: done.** `Grid::scrollbar()` computes an auto-hiding thumb (`(first_row, rows, color)`, hidden at the live bottom) from the scroll state; the CPU renderer draws a sub-cell pixel bar hugging the pane's right edge, the GPU renderer draws cell-resolution background quads in the rightmost column (an accepted parity note, like the half-block images). Drag-to-scroll on the bar is future work.
**Current.** No scrollbar anywhere; scroll position is invisible except via
the search counter.
**Target.** A minimal overlay scrollbar (thumb = viewport/history ratio)
with drag-to-scroll and click-to-jump, auto-hiding when at bottom.
**Why.** 2026 made this table stakes: Ghostty 1.3 shipped native
scrollbars, kitty 0.43 added one, Windows Terminal/Konsole always had them.
Also the natural place to render search-match and prompt-mark positions.
**Size** M · **Deps** `gui`.

#### G20 — Command-completion notifications
**Status: done.** OSC 133/633 `C`/`D` now drive `Grid::command_timer_begin/end` (runtime + exit code, bounded queue); `App::service_alerts` notifies via the existing OS-notification path when a command ran ≥ `command_notify_secs` (config, default 10, `0` disables) and finished while the window was unfocused or the tab in the background, and badges the tab. Alerts for the watched (active + focused) tab are dropped.
**Current.** OSC 133 `D` (command end + exit code) is fully tracked
(`Grid::last_exit`, fold blocks, L13 push), and OS notification delivery
exists (#9) — but the two are not connected: a long build finishing in an
unfocused window announces nothing.
**Target.** Config-gated: when a command ran longer than N seconds and the
window/tab is unfocused, raise an OS notification (and tab badge, per G06)
with the command's exit status.
**Why.** A headline Ghostty 1.3 feature; iTerm2 has had it for years. For
rusty_term this is nearly pure glue — both halves already exist.
**Size** S–M · **Deps** OSC 133 marks (done), notifications (done), focus
state (G13).

#### G21 — Click-to-move-cursor at the prompt
**Status: done.** `Grid::prompt_cursor_moves` decides when a click may move the readline cursor — primary screen, live view, a current OSC 133 prompt mark, no open command capture, click at/below the prompt row — and yields the `(dx, dy)`; the window sends DECCKM-aware arrow presses (vertical then horizontal, capped at 400/axis) on a plain first click when mouse reporting is off. `click_to_move = false` disables. Works in any shell with 133 integration and multiline-aware line editors.
**Current.** Clicks either select text or get reported to a mouse-tracking
app; clicking elsewhere in the current prompt line does not move the shell
cursor.
**Target.** When on the primary screen at a prompt (OSC 133 marks give the
prompt region), translate a click into the arrow-key presses needed to move
the readline cursor from its current column to the clicked cell.
**Why.** Ghostty 1.3 shipped it (works in zsh/fish/nushell); kitty has an
equivalent. rusty_term already tracks the prompt region and cursor — this
is an input-synthesis feature over existing state.
**Size** M · **Deps** OSC 133 (done), `gui`.

#### G22 — Regex + Unicode case-folded search
**Status: done.** `Grid::search_with(query, regex)` — both modes fold case with simple Unicode folding (not just ASCII). Regex mode compiles with **`rusty_regx`** (in-house zero-dep, linear-time POSIX ERE on a Pike VM — a user-supplied pattern can't hang the search), matching per logical line with `^`/`$` anchoring to the line; malformed patterns find nothing. Ctrl+R toggles the mode inside the find bar (`Find:` ↔ `Find(re):`).
**Current.** `Grid::search` is a plain-substring, ASCII-case-insensitive
scan (`src/core/grid.rs`).
**Target.** Unicode case folding for the existing search, plus a regex mode
(a small hand-rolled NFA over grapheme clusters fits the dependency ethos;
or gate a `regex-lite` dep behind a feature).
**Why.** kitty, WezTerm, iTerm2, and Ghostty search support regex and
proper case folding; searching logs for patterns is the primary use of
terminal search.
**Size** M · **Deps** none (core).

#### G23 — Drag-and-drop files → quoted path paste
**Status: done.** `WindowEvent::DroppedFile` handler (`src/gui/window.rs`) pastes the shell-quoted path (`shell_quote`: single quotes with `'\''` escaping on Unix, double quotes on Windows when needed) plus a trailing space, through the same bracketed-paste-aware `encode_paste` path as a clipboard paste.
**Current.** `WindowEvent::DroppedFile` is not handled.
**Target.** On file drop, write the shell-quoted path(s) to the child
(bracketed-paste aware, space-separated for multiple files).
**Why.** WezTerm, iTerm2, Windows Terminal, Ghostty, and kitty all do it;
winit already delivers the event, so this is a small handler.
**Size** S · **Deps** `gui`.

#### G24 — Color emoji font rendering (COLR/CBDT/sbix)
**Status: done (CBDT/sbix PNG bitmap strikes, CPU renderer; COLR vector emoji and a GPU RGBA atlas deferred).** `FontCache::color_emoji` (`src/gui/font.rs`) finds a system color-emoji font (Noto Color Emoji / Segoe UI Emoji / Apple Color Emoji paths), reads its PNG bitmap strike via `ttf-parser`'s `glyph_raster_image`, decodes with the in-house PNG decoder, and contain-fits it into the emoji's two-cell footprint. `Glyph` gained an optional straight-alpha color image; the CPU blit composites it per-pixel (ignoring the pen color) while `coverage` still carries the alpha so the GPU path degrades to a monochrome silhouette rather than a blank.
**Current.** Glyphs render via `ab_glyph` outlines with a single fg color;
color-emoji tables aren't read (no COLR/CBDT/sbix matches), so emoji render
as monochrome silhouettes at best via fallback fonts.
**Target.** Rasterize color glyphs: CBDT/sbix embedded bitmaps (a decode
path the from-scratch PNG decoder can serve!) and COLRv0 layered outlines;
composite as RGBA over the cell like the existing image overlay.
**Why.** Emoji are ubiquitous in modern CLI output (test runners, npm/uv,
git tooling, LLM output). Every major competitor renders them in color;
monochrome boxes read as broken.
**Size** L · **Deps** `gui`; extends `font.rs`/`cpu.rs` (GPU path inherits
the atlas question from C08/C09).

#### G25 — Built-in box-drawing / block-element / Powerline glyph synthesis
**Status: done.** New `src/gui/boxdraw.rs` synthesizes U+2500–257F box drawing (a 109-entry arm table generated from Unicode names — light/heavy/double, plus dashes, rounded arcs, diagonals, half-lines), U+2580–259F block elements (fractional blocks, 25/50/75% shades, quadrants), U+2800–28FF braille, and Powerline U+E0B0–E0B3, all at exact cell geometry. `FontCache::glyph` intercepts these before font lookup (both renderers inherit it through `GlyphSource`), and the CPU ligature-run builder excludes them so GSUB can never substitute them away. Always on, matching kitty/Ghostty.
**Current.** Box-drawing chars (U+2500…), block elements, and Powerline
triangles come from whatever font/fallback provides them — with visible
seams and misalignment when metrics don't match the cell exactly.
**Target.** Procedurally draw U+2500–257F, U+2580–259F (already partially
relevant to the half-block image path), braille, and the Powerline/Nerd
private-use triangles/rounds at exact cell geometry, bypassing the font.
**Why.** Ghostty, kitty, and WezTerm synthesize these for pixel-perfect
TUI borders and statuslines; it's why their vim/tmux borders look seamless.
Doubles as insurance for the 1024-glyph GPU atlas budget.
**Size** M · **Deps** `gui`; CPU + GPU raster helpers.

#### G26 — Minimum contrast enforcement
**Status: done.** `core::color::{luminance, contrast_ratio, ensure_contrast}` implement WCAG contrast with a minimal-blend adjustment toward the better pole (hue-preserving, binary-searched); `minimum_contrast = <ratio>` config (clamped 1–21) lands on `Grid::min_contrast` and both renderers enforce it at the glyph pass (CPU against the cell's actually-painted background incl. cursor/selection/search swaps; GPU mirrored).
**Current.** Cells render whatever fg/bg the app chose; unreadable
combinations (dark-on-dark themes fighting app hardcodes) render as-is.
**Target.** Optional `minimum_contrast = <ratio>` that nudges fg
luminance until the WCAG contrast ratio is met, applied at render time.
**Why.** WezTerm and Ghostty ship it; cheap per-cell math at the one place
colors are resolved, and a real accessibility aid ahead of full C20.
**Size** S · **Deps** `gui`.

#### G27 — Pane management: resize, zoom, directional focus
**Status: done (keyboard; divider dragging deferred).** `Layout::resize` adjusts the nearest matching-axis split ratio (deepest first, clamped 0.1–0.9) — bound to Ctrl+Shift+arrows; `zoom_pane` (Ctrl+Shift+Z) temporarily gives the focused pane the whole tab area via zoom-aware `Tab::rects` (cleared when the pane set changes); directional focus (Ctrl+Alt+arrows) picks the nearest pane beyond the focused edge by center distance. All rebindable; arrow/space keys added to the chord vocabulary.
**Current.** `src/gui/layout.rs` splits are fixed at `ratio: 0.5` with no
API to change a ratio after the split; focus cycles with `focus_next` only;
no zoom/maximize-pane.
**Target.** Keybinds (and divider dragging) to resize splits, a pane-zoom
toggle (temporarily maximize the focused pane), and directional focus
(focus-left/right/up/down via rect geometry).
**Why.** Everything with splits — kitty, WezTerm, iTerm2, tmux, Zellij —
has all three; splits without resize are a demo, not a tool.
**Size** M · **Deps** `gui`; layout tree already stores ratios.

#### G28 — Broadcast input to multiple panes
**Status: done.** `broadcast` action (Ctrl+Shift+B, rebindable) toggles mirroring: encoded keystrokes (incl. numpad) and pastes route through `App::write_child`, which fans out to every pane in the active tab while on; the active tab label shows a `⇉` marker. Per-window state, off by default.
**Current.** Input goes to the focused pane only.
**Target.** A toggle to mirror keystrokes to all panes in the current tab
(with a visible indicator while active).
**Why.** iTerm2 ("broadcast input"), Windows Terminal, and tmux
(`synchronize-panes`) — a beloved ops-workflow feature (multi-host SSH).
**Size** S–M · **Deps** `gui`, panes (done).

#### G29 — Rich-text (HTML/RTF) clipboard copy
**Current.** Copy is plain text (`arboard` with image support explicitly
compiled out).
**Target.** Optional "copy with formatting": serialize the selection's
colors/attributes to HTML (and RTF on macOS) alongside plain text.
**Why.** Ghostty 1.3 added it on macOS; iTerm2 and Windows Terminal have
"copy formatting". Useful for pasting colored build/test output into
docs and chat.
**Size** M · **Deps** `gui`; arboard supports HTML on major platforms.

### A.3 Application model

#### G30 — Quake-style quick terminal + global hotkey
**Current.** One normal window per process; no global-hotkey or slide-down
mode.
**Target.** A summonable overlay window (top-of-screen slide-down) bound to
a global hotkey, per platform capability.
**Why.** Ghostty ("quick terminal"), iTerm2 (hotkey window), Windows
Terminal (quake mode), Guake/Yakuake heritage — a signature convenience
feature with strong pull.
**Size** M–L · **Deps** `gui`; global hotkeys need per-platform code beyond
winit; sequenced naturally after C13 (multi-window).

#### G31 — Single-instance / daemon mode
**Status: done (Unix; Windows named pipes deferred).** With `single_instance = true` (or `--single-instance`), the window serves a user-private control socket (`$XDG_RUNTIME_DIR/rusty_term.sock`, else `/tmp/rusty_term-<uid>.sock`, `0600`); a second `--gui --single-instance` launch pings it and, if an instance answers, forwards `new-tab` (carrying `--cwd`/`--profile`) and exits instead of opening a second window. Stale sockets from a dead instance are reclaimed after a failed ping; a live instance's socket is never stolen. Full startup-latency value arrives with multi-window (C13).
**Current.** Every launch cold-starts the process, loads fonts, and (GPU)
compiles pipelines.
**Target.** A `--single-instance`/server mode where subsequent launches ask
the running instance for a new window/tab over a local socket (foot's
`foot --server`, kitty's `--single-instance`).
**Why.** Startup latency is a headline metric in terminal comparisons;
daemon mode is how foot wins it. Also the substrate G33 (remote control)
wants anyway.
**Size** M · **Deps** C13 (multi-window) for full value.

#### G32 — Sessions / startup layouts / workspaces
**Status: done (declarative session files; save-current-layout deferred).** `config::load_session` parses a session file of `[tab]` sections (in order) with `profile`, `cwd`, `command` (whitespace-split argv, run in place of the shell), and `splits = "right,down,…"`; `--session <path>` or a `session` config key makes the window build its initial tab set from it (split panes inherit the tab's profile shell/cwd/theme). Same forgiving warn-and-skip contract as the config file; TUI mode warns and ignores (no tabs to build).
**Current.** No way to declare "open these tabs/splits, in these cwds,
running these commands" — every start is one shell.
**Target.** A session file (TOML, matching the config dialect) describing
tabs/splits/cwd/command, a `--session <file>` flag, and optionally
save-current-layout.
**Why.** kitty 0.43's flagship feature was session management; WezTerm has
workspaces; Windows Terminal has `startupActions`; tmux users script this
daily. rusty_term's tabs/splits/cwd-tracking make it mostly a
spawn-orchestration feature.
**Size** M–L · **Deps** `gui`, panes (done), OSC 7 cwd (done).

#### G33 — Remote control / scripting API
**Status: done (socket CLI; L13 mutating MCP tools deferred).** `src/gui/control.rs` implements a dependency-free line protocol (quoted values, `\n`/`\t` escapes, `ok`/`err` terminators) over the single-instance socket; requests are handed to the event loop as `UserEvent::Control` with a reply channel. `rusty_term ctl <cmd>` speaks it: `new-tab [cwd= profile= shell=]`, `send-text text=`, `list-tabs`, `focus-tab n=`, `ping`. Client framing verified over a real Unix socket in tests; a live end-to-end needs a display, so the in-window executor is exercised by unit tests only.
**Current.** The L13 side-channel exposes *introspection* (MCP tools:
get_screen, get_cwd, …) to the **child** over OSC — but nothing can
*control* the terminal (open tab, split, set title, send text), and
nothing outside the PTY can talk to it at all.
**Target.** A control surface: (1) add mutating methods to the L13 MCP
server (send_text, new_tab, split, set_title, focus) with an opt-in
permission model; (2) expose the same JSON-RPC over a local socket for
out-of-band scripting (`rusty_term ctl …`), which G31's socket provides.
**Why.** kitty `@` remote control, WezTerm's CLI, and now Ghostty's
AppleScript automation (1.3) — scriptability is a top power-user
differentiator, and rusty_term is uniquely positioned: it already has a
negotiated JSON-RPC protocol where competitors bolted on ad-hoc sockets.
**Size** M · **Deps** `l13` feature; G31 for the socket transport.

#### G34 — Profiles
**Status: done.** `[profile.<name>]` config sections bundle `shell`, `cwd`, and `theme` (per-pane — `new_pane` now takes a theme override, so profile tabs keep their own palette). Profiles appear at the top of the `▾` launcher dropdown (`Profile: <name>`), are referenced by session tabs, and `--profile <name>` layers one onto the top-level config at startup for both front-ends — verified end-to-end (profile cwd honored through the TUI binary on a real PTY). Font-per-profile is deferred (needs per-tab glyph caches).
**Current.** One global config; the shell-launcher dropdown (#17) picks a
shell but carries no per-shell theme/font/cwd/args.
**Target.** `[profile.<name>]` config sections bundling shell, args, cwd,
theme, font; surfaced in the existing dropdown and addressable from the
CLI (`--profile`).
**Why.** Windows Terminal's entire UX is profile-centric; iTerm2, Konsole,
and WezTerm (launch menu) agree. The dropdown and live-retheme plumbing
mean this is mostly config-schema work.
**Size** M · **Deps** existing settings/dropdown machinery.

### A.4 Watch list & cosmetics

#### G35 — Multiple-cursors protocol — **watch**
kitty 0.43 shipped a new protocol letting apps render multiple cursors
(multi-cursor editing). Single-implementation for now — same "wait for the
field to converge" posture as mode 2027/OSC 66 (C18/C19). Track; don't
build.

#### G36 — Cursor trail / animated cursor
kitty 0.43 added configurable cursor trails; smooth cursor motion
(neovide-style) is a recurring "delight" feature. Pure renderer work,
low priority, but cheap goodwill with zero protocol risk. **Size** S–M.

### A.5 Infrastructure

#### G37 — Fuzzing harness for the hand-rolled decoders
**Status: done.** The crate gained a library target (`src/lib.rs`; `main.rs` is now a thin CLI), enabling an out-of-tree `fuzz/` crate (workspace-excluded, cargo-fuzz/libFuzzer) with two coverage-guided targets driving the *public* parser surface: `parser` (arbitrary bytes through `AnsiParser::advance`, split across two chunks to exercise incremental state) and `graphics` (the same input framed as Sixel DCS / kitty APC / iTerm2 OSC so the base64→inflate→PNG/JPEG decoders see coherent framing immediately). Two deterministic xorshift-seeded stress tests (~256 KiB of escape-seasoned soup + hostile graphics payloads) run in the ordinary suite so every CI run gets a smoke-fuzz without nightly.
**Current.** `docs/repo-analysis.md` follow-up #6 remains open: the
from-scratch `inflate`/`png`/`jpeg`/`base64`/`sixel`/kitty-APC decoders were
hand-traced for memory safety but have no coverage-guided fuzzing; the
parser itself (untrusted input by definition) has none either.
**Target.** `cargo-fuzz` targets for each decoder and for
`AnsiParser::advance`, run in CI on a schedule; seed corpora from the
existing tests.
**Why.** These decoders parse attacker-controlled bytes (anything you
`cat`). The repeat-count DoS class found in review (`repo-analysis.md` §4)
is exactly what fuzzing finds mechanically. Every serious parser project
(VTE, Alacritty's vte crate) ships fuzz targets.
**Size** M · **Deps** none (dev-only tooling; no runtime deps added).

---

## Section B — Still open from capability-assessment-2026.md

Carried forward unchanged (see that document for full write-ups):

| ID | Item | Status there | Size |
|---|---|---|---|
| C03′ | Kitty keyboard flags 2/4/16 | partial — now promoted to **G11** | M |
| C08 | GPU renderer ligatures | ✅ done 2026-07 (see below) | L |
| C09 | GPU renderer pixel images | ✅ done 2026-07 (see below) | L |
| C12′ | GIF / WebP / progressive JPEG decode | deferred, each ~L | L×3 |
| C13 | Multiple top-level windows | assessed, deferred (App rework) | L |
| C14′ | CPU-renderer opacity + platform blur | partial (GPU-only opacity) | M |
| C17′ | Command-output folding — render path | data model done, rendering open | M–L |
| C18 | Unicode width mode 2027 | watch | M |
| C19 | Text-sizing protocol (OSC 66) | watch | L |
| C20 | Accessibility (accesskit) | open — still a field-wide gap, still a differentiator | L |
| C23 | io_uring backend (Linux) | open, perf-only | L |
| C24 | IOCP-native async (Windows) | open, perf-only | L |
| C25 | Bidi + normalization | open, needs own scoping pass | XL |
| C26/C27 | DAP/Jupyter bridges; full LSP/ACP backends | open, speculative | L |

**C08 + C09 status (2026-07):** the GPU renderer (`src/gui/gpu.rs`) was
rebuilt around a variable-width RGBA8 shelf-packed atlas (up to 2048²,
clamped to the device limit) with three draw layers per frame: opaque base
cells (`REPLACE` blend, alpha = window opacity), an overlay layer for
GSUB-shaped ligature-run glyphs and color emoji (straight-alpha over,
alpha channel preserved for compositor transparency), and an image layer
drawing per-image textures as cell-aligned quads. This closes the GPU/CPU
parity list in one pass: ligatures (C08), placed Kitty images, Unicode
placeholders, and animation frames (C09 — sharing `Grid::placeholder_map`
/ `placeholder_grid` with the CPU path), color-emoji bitmap strikes (G24
parity), and wide-glyph clipping (CJK/emoji now get two-cell tiles).
Image textures are cached keyed by (kind, id, frame/revision) with a
32-entry cap. Verified headless up to pipeline/shader/atlas creation plus
allocator and tile-cache unit tests (`gpu_core_builds`,
`shelf_allocator_packs_wraps_and_exhausts`,
`wide_chars_get_two_cell_tiles_and_cache_hits`); the full
render-to-texture readback (`gpu_renders_to_texture`) remains `#[ignore]`
because headless software drivers (lavapipe/dzn) crash on submit — run it
with `--ignored` on real GPU hardware.

## Suggested sequencing (Section A items)

| Wave | Items | Rationale |
|---|---|---|
| 1 | G13, G14, G12, G03, G04, G23 | Tiny protocol/input/window wins; each ≤S, no interdependencies. |
| 2 | G17, G06, G20, G01 | Daily-felt UX: multi-click selection, bell, command notifications, progress — G20/G01 share the notification/chrome surface. |
| 3 | G09, G16, G22 | Selection & discovery: primary selection, implicit URL detection + hints, better search. |
| 4 | G02, G08, G26 | Appearance + notifications: dark-mode plumbing, OSC 99, min-contrast. |
| 5 | G27, G19, G18 | Panes & navigation: resize/zoom, scrollbar, copy mode. |
| 6 | G25, G24 | Rendering: box-drawing synthesis first (helps everything), then color emoji. |
| 7 | G34, G32, G31, G33 | App model: profiles → sessions → daemon → remote control (each builds on the last). |
| 8 | G07, G10, G11, G05, G15, G21, G28, G29, G30, G36 | Long-tail protocol depth + platform features, pulled opportunistically. |
| ∥ | ✅ G37 | Fuzzing is independent — start any time; ideally before wave 6's new decoders (CBDT/COLR). |

Section B items keep their original sequencing advice; C13 (multi-window)
gates G30/G31's full value and should precede wave 7 if the app-model work
is prioritized.

## Sources

1. [Ghostty 1.3.0 Release Notes](https://ghostty.org/docs/install/release-notes/1-3-0) — scrollback search, native scrollbars, command notifications, click-to-move-cursor, AppleScript, rich-text copy
2. [Ghostty 1.3 terminal released — OMG! Ubuntu](https://www.omgubuntu.co.uk/2026/03/ghostty-1-3-terminal-brings-big-new-features)
3. [Kitty 0.43 Brings Session Management — Linuxiac](https://linuxiac.com/kitty-terminal-0-43-brings-session-management/) — sessions, multiple-cursors protocol, scrollbar, cursor trails
4. [kitty changelog](https://sw.kovidgoyal.net/kitty/changelog/)
5. [OSC 9;4 progress bar sequence — rockorager.dev](https://rockorager.dev/misc/osc-9-4-progress-bars/)
6. [ConEmu extensions (OSC 9;n) — Ghostty VT docs](https://ghostty.org/docs/vt/osc/conemu)
7. [Progress bars in Ghostty — Martin Emde](https://martinemde.com/blog/ghostty-progress-bars)
8. [kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)
9. [kitty desktop notifications (OSC 99)](https://sw.kovidgoyal.net/kitty/desktop-notifications/)
10. [kitty graphics protocol — Unicode placeholders](https://sw.kovidgoyal.net/kitty/graphics-protocol/)
11. [Contour: dark/light mode detection (mode 2031)](https://contour-terminal.org/vt-extensions/color-palette-update-notifications/)
12. [WezTerm vs Windows Terminal, 2026 — XDA](https://www.xda-developers.com/windows-terminal-versus-wezterm-differences/) — mux/workspaces, quake mode, 2026 AI preview
13. [Modern Terminal Emulators 2026: Ghostty, WezTerm, Alacritty — Calmops](https://calmops.com/tools/modern-terminal-emulators-2026-ghostty-wezterm-alacritty/)
14. [State of Linux Terminal Emulators in 2026 — DEV](https://dev.to/shrsv/state-of-linux-terminal-emulators-2026-1gh5)
15. Prior in-repo research: [`capability-assessment-2026.md`](capability-assessment-2026.md), [`implementation-status.md`](implementation-status.md), [`../FEATURES.md`](../FEATURES.md), [`../repo-analysis.md`](../repo-analysis.md)
