# rusty_term — Implementation Status & Backlog

Status of the codebase mapped against `terminal-stack-spec-tree.html` (14-layer
catalog) and `terminal-stack-synthesis.md` (milestone ladder). Verified against
`src/` and the ~130-test suite in `src/core/tests.rs` on 2026-06-03.

## Architecture lens (read this first)

`rusty_term` is a **passthrough/relay terminal emulator that renders into a host
terminal** — it parses a child shell's output into a `Grid`, then re-emits ANSI
to its own stdout. It does **not** own a GPU window. Consequences:

- The entire windowed stack (font shaping, GPU raster, winit/wgpu, IME,
  accessibility) is **out of the current architecture**, not merely unbuilt.
- Mouse, bracketed paste, focus reporting, cursor-key mode, and clipboard are
  **deliberately relayed to the host terminal** (`is_host_input_mode`,
  `src/core/parser.rs:636`; OSC 52, `src/core/osc.rs:31`). Apps get working
  behavior via passthrough; the encodings themselves are not implemented here.

By the synthesis milestone ladder this sits at the **"Week" milestone**: a
working TUI-mode terminal you could run inside another terminal. The parser +
grid core is built and well-tested.

Legend: `[x]` implemented · `[~]` partial / relayed · `[ ]` not implemented.

> **Update 2026-06-03** — Backlog P0–P2 landed: DEC line-drawing charset, dynamic
> palette + default colors (OSC 4/10/11/12/104/110-112), OSC 1, XTVERSION/DA3, a
> shipped terminfo entry + smart `TERM` selection, OSC 133 prompt marks with
> prompt-to-prompt scrollback navigation + shell-integration scripts, UAX #29
> grapheme clustering, and DECDWL/DECDHL double-width/height lines. **L07 graphics**
> then landed — Sixel **and** Kitty (a from-scratch base64/inflate/PNG stack, no
> crates) rendered as half-blocks — and **L09 input** was completed by relaying the
> Kitty keyboard + xterm modifyOtherKeys protocols to the host. Checkboxes below
> reflect all of this; the only remaining `[ ]` items are the L13 structured
> channel, the native window-backend fork, and small leftovers (iTerm2, XTGETTCAP,
> OSC notifications, bidi, …).

## Appendix C — Minimum Viable Modern Terminal

- [x] **OS interface** — POSIX.1 (`libc`) + Windows ConPTY (`windows-sys`). Hand-rolled, not `portable-pty`. Windows path type-checked but unrun (`src/backend/windows.rs:10`).
- [x] **PTY** — Unix98 `openpty` + fork/exec/`TIOCSCTTY` (`src/backend/unix.rs:41`); `CreatePseudoConsole` (`src/backend/windows.rs`).
- [x] **Line discipline** — host raw mode via `cfmakeraw` + `VMIN=1/VTIME=0`, exact save/restore (`src/backend/unix.rs:88`). Complete for a PTY emulator: the cooked-mode line discipline (echo, canonical editing, signal chars, CR/LF) is the **kernel's** `N_TTY` on the PTY slave, configured by the child — not the emulator's to implement.
- [x] **Encoding** — UTF-8 decode + UAX #11 width + **UAX #29 grapheme clustering** (`unicode-segmentation`; ZWJ emoji, skin tones, unbounded combining via interned clusters, `src/core/grid.rs`).
- [x] **Escape codes** — broad pragmatic VT100/ECMA-48 subset (not a full xterm superset). See layer L06.
- [x] **OSC minimum (0/1/2·4·7·8·52·133)** — 0/2 ✓, 1 ✓(host-forwarded), 4 ✓, 7 ✓, 8 ✓, 52 ✓(relayed), 133 ✓(prompt marks + nav) (`src/core/osc.rs`).
- [x] **Graphics (Sixel and/or Kitty)** — **both Sixel and Kitty done, no crates**. Sixel: hand-rolled decoder (`src/core/sixel.rs`). Kitty: full APC protocol with a hand-rolled `base64` → DEFLATE/zlib `inflate` → `png` decode stack (`src/core/{base64,inflate,png,kitty}.rs`), incl. raw RGB/RGBA (`f=24/32`), PNG (`f=100`), `o=z` compression, chunked transmission, and query/transmit `OK` responses. Both render through the shared half-block path (`Grid::render_image`) — cell resolution, not pixel-perfect (no framebuffer). iTerm2 still out.
- [x] **Input (SGR mouse 1006·bracketed paste 2004·Kitty keyboard)** — all relayed to the host: mouse 1006 + paste 2004 + **Kitty keyboard (`CSI >/</=/? … u`) and xterm modifyOtherKeys (`CSI > … m`)**. A capable host does the actual enhanced encoding and answers the query; native encoding is a window-backend concern (we see encoded bytes, not key events), so relay is both the consistent and the more-correct choice.
- [x] **Identification (DA1·XTVERSION·terminfo)** — DA1/DA2/DA3 ✓, XTVERSION ✓ (`src/core/parser.rs`), XTGETTCAP still missing; `rusty_term` terminfo entry shipped (`extra/rusty_term.terminfo`) with probe-and-fallback `TERM` selection (`src/term.rs`).
- [x] **Truecolor** — full 24-bit + 256-color (`src/core/color.rs`, `src/core/parser.rs:602`); advertises `COLORTERM=truecolor` (`src/main.rs:35`).
- [x] **Shell integration** — OSC 133 emitter scripts for bash/zsh/fish/pwsh (`extra/shell-integration/`).
- [ ] **Structured channel** — private OSC + schema + fallback: not started.

## Layer-by-layer (L00–L13)

### L01 OS Interface — [x]
- [x] POSIX via `libc` (direct, not `nix`)
- [x] Windows via `windows-sys`

### L02 PTY — [x]
- [x] Unix98 `openpty`, fork/exec, `TIOCSCTTY`, `TIOCGWINSZ`, `TIOCSWINSZ` (`src/backend/unix.rs`)
- [x] ConPTY `CreatePseudoConsole` + pipes (`src/backend/windows.rs`)

### L03 Line discipline — [x]
- [x] Host controlling-tty raw mode (`cfmakeraw` + `VMIN=1/VTIME=0`) with exact save/restore (`src/backend/unix.rs:88`); raw is the only correct setting for an emulator's own tty, so there are no useful "finer" knobs
- [x] PTY slave gets the kernel's sane cooked defaults (`openpty(.., NULL, &ws)`); the real line discipline (echo/canonical/`ISIG`/`ONLCR`) is the kernel's `N_TTY`, reconfigured by the child — correctly out of the emulator's scope

### L04 I/O — [x] (epoll via tokio; io_uring/IOCP absent)
- [x] Threaded runtime: parser/input/render threads + condvar (`src/runtime/threaded.rs`)
- [x] Tokio runtime: `AsyncFd`→mio→epoll, Unix-only (`src/runtime/tokio_rt.rs`)
- [x] SIGWINCH-driven reflow in both runtimes; ~60Hz frame coalescing
- [ ] io_uring
- [ ] Windows IOCP (uses blocking `ReadFile` on threads)

### L05 Encoding — [x] (bidi/normalization out)
- [x] UTF-8 incremental decode with split-chunk resilience
- [x] UAX #11 East Asian width via `unicode-width`
- [x] UAX #29 grapheme clustering via `unicode-segmentation` — unbounded combining, ZWJ emoji, skin tones, variation selectors as one cell (interned clusters, `src/core/grid.rs`)
- [x] Charset designation + DEC Special Graphics line-drawing (`ESC ( ) * + 0`, SI/SO; `src/core/charset.rs`)
- [ ] Bidi, normalization

### L06 Control sequences — [x] (strong subset)
- [x] SGR: attrs (bold/dim/italic/underline/blink/reverse/hidden/strike) + 16/256/truecolor
- [x] Cursor: CUU/CUD/CUF/CUB/CNL/CPL/CHA/VPA/CUP/HVP + DSR-CPR
- [x] Erase: ED (0/1/2/3), EL (0/1/2)
- [x] Edit: ICH/DCH/ECH/IL/DL
- [x] Scroll: SU/SD, DECSTBM scrolling region
- [x] Save/restore: DECSC/DECRC, SCP/RCP
- [x] Modes: DECAWM(7), DECOM(6), DECTCEM(25), IRM(4)
- [x] Alt screen: 47 / 1047 / 1049 with correct cursor semantics
- [x] Tabs: HTS/TBC/CHT/CBT, resize-stable stops
- [x] ESC dispatch: IND/NEL/RI/HTS, RIS, DECSTR, DECALN, REP
- [x] Charset translation (G0–G3 designation, SI/SO GL shift, DEC line-drawing; `src/core/charset.rs`)
- [x] Double-width/height lines (`ESC # 3/4/5/6` → per-row `LineAttr`, relayed to host; `src/core/grid.rs`, `src/render.rs`)
- [ ] ANSI (non-private) modes beyond IRM

### L07 Graphics — [x] (Sixel + Kitty; iTerm2 out)
- [x] Sixel: hand-rolled decoder (RGB + HLS color, repeat, bands, raster attrs) → half-block rendering with fit-to-width downsample + scroll (`src/core/sixel.rs`, DCS wiring in `parser.rs`)
- [x] Kitty graphics protocol: APC parse + chunked accumulation + query/transmit responses (`src/core/kitty.rs`), with a from-scratch `base64`, DEFLATE/zlib `inflate`, and `png` decoder (8-bit, color types 0/2/3/6, all filters) — formats `f=24/32/100` and `o=z`
- [x] Shared `Grid::render_image` half-block renderer used by both
- [ ] iTerm2 inline images (base64 PNG/JPEG — would reuse the same png/inflate stack + a JPEG decoder)

### L08 OSC — [x] (notifications out)
- [x] 0/2 window title; 1 icon name (host-forwarded)
- [x] 4 / 104 palette set/reset/query; 10/11/12 + 110/111/112 default fg/bg/cursor set/reset/query (`src/core/osc.rs`, `src/core/color.rs`)
- [x] 7 working directory (captured to `Grid.cwd`)
- [x] 8 hyperlinks (interned, bounded, rendered)
- [x] 133 semantic prompt marks → prompt-to-prompt scrollback navigation (`src/core/grid.rs`, keys in `src/input.rs`)
- [~] 52 clipboard: set-requests relayed to host; query (`?`) path unwired
- [ ] 9 / 777 notifications; 633 (VS Code), 1337 (iTerm2)

### L09 Input — [x] (relay model; native encoding awaits the window fork)
- [x] Scrollback browse + prompt-nav keys intercepted locally (`src/input.rs`)
- [x] Host stdin forwarded verbatim to child
- [x] All input-generating modes relayed to the host: mouse 1000/1002/1003/1005/1006/1015/1016, focus 1004, paste 2004, DECCKM 1 (`is_host_input_mode`), **Kitty keyboard (`CSI >/</=/? … u`) and xterm modifyOtherKeys / XTMODKEYS (`CSI > … m`)** (`handle_private_csi`)
- [ ] Native mouse/key *encoding* — impossible in TUI-mode (no key-event source); unlocked only by the native window backend (#10)

### L10 Identification — [x] (XTGETTCAP out)
- [x] DA1 (`CSI c` → `?1;2c`), DA2 (`CSI > c`), DA3 (`CSI = c`)
- [x] DSR (`CSI 5n` status, `CSI 6n` CPR)
- [x] XTVERSION (`CSI > q` → `DCS >|rusty_term(<ver>) ST`)
- [x] Shipped `rusty_term`/`rusty_term-256color` terminfo entry + probe-and-fallback `TERM` selection (`extra/rusty_term.terminfo`, `src/term.rs`)
- [ ] XTGETTCAP (DCS `+q`)

### L11 Shell — [x] (by delegation)
- [x] Spawns external `$SHELL`/bash (Unix) and `%COMSPEC%`/cmd (Windows)

### L13 Adjacent protocols — [ ]
- [ ] Structured side-channel (private OSC + schema + ANSI fallback) — the synthesis's flagship novelty
- [ ] LSP/DAP/MCP/Jupyter bridges

## Backlog

P0–P2 are **done** (see the 2026-06-03 update note); P3 and the architectural
fork remain. Ordered by leverage-to-effort for a TUI-mode (host-rendered) terminal.

### P0 — Correctness gaps that silently corrupted output — DONE
1. [x] **Charset designation + DEC Special Graphics line-drawing** — G0–G3
   designation (`ESC ( ) * +`), SI/SO GL shift, and the DEC line-drawing table
   (`src/core/charset.rs`); `put_char` maps printable bytes through the active
   charset. Reset on RIS/DECSTR. Tested (`dec_line_drawing_*`, `so_si_toggle_*`).

### P1 — Identification & ecosystem fit — DONE
2. [x] **XTVERSION (`CSI > q`) + DA3 (`CSI = c`)** in `handle_private_csi`.
3. [x] **terminfo entry** (`extra/rusty_term.terminfo`, compiles under `tic`) +
   probe-and-fallback `TERM` selection (`src/term.rs`).
4. [x] **OSC 1 / 4 / 10-12** (+ 104/110-112) — icon name, dynamic palette and
   default fg/bg/cursor with set/reset/query, threaded through a live `Palette`
   (`src/core/color.rs`) and a grid erase-cell so OSC 11 colors cleared regions.

### P2 — Feature parity for modern TUIs — DONE
5. [x] **OSC 133 semantic prompt marks** + prompt-to-prompt scrollback navigation
   (Ctrl+Shift+PageUp/PageDown) + bash/zsh/fish/pwsh emitter scripts
   (`extra/shell-integration/`).
6. [x] **UAX #29 grapheme clustering** (`unicode-segmentation`) — interned
   clusters replace the fixed 2-mark array; ZWJ emoji/skin tones/combining as one
   cell, with the ASCII fast path preserved.
7. [x] **Double-width/height lines** (`ESC # 3/4/5/6`) — per-row `LineAttr`,
   shifted in lockstep through scrolling, relayed to the host with a half-column
   emission cap so double rows don't overflow.

### P3 — Strategic / thesis-defining (pick based on project intent)
8. **L07 graphics.** [x] **Sixel + Kitty done** — hand-rolled, no crates: Sixel
   decoder, plus a full `base64`/`inflate`/`png` stack feeding the Kitty APC
   protocol (raw, PNG, `o=z`, chunked, responses). Both render via the shared
   half-block `Grid::render_image`. Remaining: iTerm2 (needs a JPEG decoder too).
9. **L13 structured side-channel** — the synthesis's headline novelty: a private
    OSC carrying a versioned JSON/msgpack schema with graceful ANSI fallback.
    Worth a design doc before code (channel framing, capability negotiation via
    XTVERSION/XTGETTCAP, fallback contract).

### Architectural fork (blocks P3 graphics at scale)
10. **Native window backend** (`tcore-font` + `tcore-app`, synthesis §12):
    winit/wgpu window + cosmic-text/swash shaping. This is the jump from
    "TUI-mode" to a standalone terminal and unlocks real graphics, mouse, IME,
    and accessibility instead of relaying them. Large, separate track.
