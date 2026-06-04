# rusty_term — Implementation Status & Backlog

Status of the codebase mapped against `terminal-stack-spec-tree.html` (14-layer
catalog) and `terminal-stack-synthesis.md` (milestone ladder). Verified against
`src/` and the test suite (~220 tests; 230 with `--features l13`, plus the
`gui`/`gui-gpu` window-backend tests) on 2026-06-03.

## Architecture lens (read this first)

`rusty_term` has two front-ends over one shared parser + `Grid` core:

- **TUI mode (default).** A **passthrough/relay** emulator: it parses a child
  shell's output into a `Grid`, then re-emits ANSI to its own stdout, running
  inside a host terminal. It owns no window here, so input-generating modes
  (mouse, bracketed paste, focus, cursor-key, clipboard) are **deliberately
  relayed to the host** (`is_host_input_mode`, `src/core/parser.rs:636`; OSC 52,
  `src/core/osc.rs:31`) — apps get working behavior via passthrough; the
  encodings themselves aren't implemented here.
- **Native window backend (optional, features `gui` / `gui-gpu`).** A standalone
  `winit` window that renders the same `Grid` with real glyphs and encodes key
  events natively (see L09 and backlog #10). `--gui` selects it; `--gpu` picks
  the `wgpu` GPU renderer over the `softbuffer` CPU one.

By the synthesis milestone ladder the core sits past the **"Week" milestone** —
a working TUI-mode terminal you can run inside another terminal — and the
optional window backend begins the jump to a standalone GUI app. The parser +
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
> reflect all of this. **L13** then landed too: a full-duplex JSON-RPC structured
> side-channel over a private OSC (feature `l13`), hosting a complete MCP server
> (terminal introspection) plus LSP/ACP negotiation, built on `rusty_lsp`.
> Finally, the **native window-backend fork** was built (features `gui`/`gui-gpu`):
> a `winit` window with CPU (`softbuffer`) and GPU (`wgpu`) renderers + `ab_glyph`
> glyphs + native key encoding — the jump from TUI-mode to a standalone window
> (`--gui`). Its font/CPU-render/input/GPU-pipeline layers are headless-tested,
> and the live window (CPU **and** GPU) has since been run and verified on
> Windows 11 — including a maximized window past the 2048px GPU texture limit,
> after fixing a `Surface::configure` panic that the resize first surfaced. It
> still can't run in this headless CI env (no display; software Vulkan drivers
> crash on submit). Remaining `[ ]` items are now
> only small leftovers (iTerm2, XTGETTCAP, OSC notifications, bidi, DAP/Jupyter,
> in-window mouse/clipboard/IME, …).
>
> **Update 2026-06-03 (later)** — Resize is now a **wrap-aware reflow**
> (`Grid::resize` → `reflow_history`, `src/core/grid.rs`): a per-row soft-wrap
> bit (set on DECAWM autowrap, carried through scrolls and into scrollback) lets
> a resize rejoin wrapped runs across scrollback + screen into logical lines and
> re-wrap them to the new width — narrowing pushes overflow into history, widening
> pulls continuations (and history lines) back — carrying the cursor, OSC-133
> prompt marks, and per-row line-size attributes through, and never splitting a
> double-width glyph across the margin. The alternate screen is still
> clipped/extended (its apps repaint on resize). This closes the last
> daily-use fidelity gap; the previous top-left clip truncated long lines and
> dropped prompt marks + line attrs. Suite: **238** core tests (255 with `gui`).

## Appendix C — Minimum Viable Modern Terminal

- [x] **OS interface** — POSIX.1 (`libc`) + Windows ConPTY (`windows-sys`). Hand-rolled, not `portable-pty`. The Windows path is **run & verified on Windows 11** (build 26200): shell spawn, child `TERM`/`COLORTERM` env, bidirectional relay, and OSC title capture; host resize propagation is a known gap (no `SIGWINCH` equivalent wired; `src/backend/windows.rs`).
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
- [x] **Structured channel** — full-duplex JSON-RPC over a private OSC (`OSC 5379 ; <protocol> ; <json> ST`) with capability negotiation and graceful ANSI fallback (`src/core/channel.rs`, feature `l13`). Hosts a complete **MCP** server (terminal introspection) plus **LSP/ACP** negotiation; reuses `rusty_lsp`'s JSON-RPC model + LSP types.

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

### L09 Input — [x] (TUI mode relays; native key encoding in the `gui` backend)
- [x] Scrollback browse + prompt-nav keys intercepted locally (`src/input.rs`)
- [x] Host stdin forwarded verbatim to child
- [x] All input-generating modes relayed to the host: mouse 1000/1002/1003/1005/1006/1015/1016, focus 1004, paste 2004, DECCKM 1 (`is_host_input_mode`), **Kitty keyboard (`CSI >/</=/? … u`) and xterm modifyOtherKeys / XTMODKEYS (`CSI > … m`)** (`handle_private_csi`)
- [x] Native **key** encoding in the `gui` window backend — winit key → terminal
  bytes, incl. DECCKM application-cursor + modifier params (`src/gui/input.rs`, #10)
- [x] Windowed UX (`gui`): **block cursor**, **left-drag text selection**, and
  **clipboard copy/paste** (Ctrl+Shift+C / Ctrl+Shift+V, bracketed-paste aware),
  plus a Windows child-exit watcher (`src/gui/window.rs`, #10)
- [ ] Native **mouse reporting** to the child in the window (SGR/1006 etc. not yet
  emitted; selection is local-only); DECCKM application-cursor not tracked in `gui`

### L10 Identification — [x] (XTGETTCAP out)
- [x] DA1 (`CSI c` → `?1;2c`), DA2 (`CSI > c`), DA3 (`CSI = c`)
- [x] DSR (`CSI 5n` status, `CSI 6n` CPR)
- [x] XTVERSION (`CSI > q` → `DCS >|rusty_term(<ver>) ST`)
- [x] Shipped `rusty_term`/`rusty_term-256color` terminfo entry + probe-and-fallback `TERM` selection (`extra/rusty_term.terminfo`, `src/term.rs`)
- [ ] XTGETTCAP (DCS `+q`)

### L11 Shell — [x] (by delegation)
- [x] Spawns external `$SHELL`/bash (Unix) and `%COMSPEC%`/cmd (Windows)

### L13 Adjacent protocols — [x] (channel + MCP complete; LSP/ACP negotiable)
- [x] Structured side-channel: full-duplex JSON-RPC 2.0 over a private OSC (`OSC 5379 ; <protocol> ; <json> ST`), one message per OSC, replies written to the child's stdin via the response channel; unaware terminals ignore the OSC (graceful degradation). Feature-gated `l13`, runtime-agnostic. (`src/core/channel.rs`, parser routing in `finish_osc`)
- [x] Versioned negotiation + schema discovery: `channel/initialize` negotiates a channel version (`min(client, ours)`, erroring with the supported range below the floor), intersects the client's requested protocol set, and advertises a per-protocol capability map; `channel/describe` returns the machine-readable schema (version range + per-protocol method lists) so a client discovers the contract programmatically
- [x] **MCP** server exposing the terminal to agents — `initialize`, `tools/list`, `tools/call` (`get_screen` / `get_scrollback` / `get_cwd` / `get_title` / `get_dimensions` / `get_cursor`) **and** `resources/list` / `resources/read` over `terminal://{screen,scrollback,cwd,title,dimensions,cursor,exit,command}` URIs (advertises `tools` + `resources` capabilities)
- [x] **MCP push notifications + command lifecycle** — `resources/subscribe` / `resources/unsubscribe` register a client's interest; a real state change at the source emits a push on the child channel. Wired for the **full OSC 133 command lifecycle**: `A`=prompt start drives scrollback nav, `C`=output start anchors a capture, `D[;exit]`=command end. State changes that map to a resource (`terminal://cwd` via OSC 7, `terminal://title` via OSC 0/2, `terminal://command` = captured output text) push generic `notifications/resources/updated {uri}`; **command completion** pushes a typed `notifications/command_finished { exit }` to `terminal://exit` subscribers, carrying the exit code (or `null`) in the push itself so the client needs no follow-up read. **Resize** pushes `notifications/resources/updated {terminal://dimensions}`: the resize path runs in the runtime driver *outside* `advance` (no `responses` in hand), so `Grid::resize_notification()` builds the frame and the driver writes it to the child via the PTY handle (both the threaded and tokio runtimes), best-effort. The output-capture anchor decays with scrollback eviction and is **remapped across a resize** (rides the reflow like a prompt mark), keeping the capture even when the output rewraps. Notifications fire only on an actual change. High-churn resources (screen/scrollback/cursor) are deliberately poll-only and reject subscription. State on the `Grid` (`channel`, `last_exit`, `command_start`, `last_command_output`); shared `row_text` extractor. (`src/core/channel.rs`, `src/core/grid.rs`, hooks in `src/core/osc.rs`, `src/runtime/{threaded,tokio_rt}.rs`)
- [x] **`render` protocol — structured render primitive** (the "terminal meets GUI toolkit" frontier): `set_status` / `clear_status` drive a terminal-owned status-line overlay composited over the bottom row, independent of the child's text stream. Pre-rendered cells stored on the `Grid` (`Grid::status_line`), re-laid out on resize, wide-glyph aware; honored by **all three** render paths — the `DirtyFrame` snapshot (passthrough renderer) and the direct-cell `cpu`/`gpu` GUI renderers — and suppressed on the alternate screen. (`src/core/grid.rs` `StatusLine`, `src/core/channel.rs`, `src/gui/cpu.rs`, `src/gui/gpu.rs`)
- [x] **LSP** and **ACP** negotiable endpoints — `initialize` handshakes implemented (LSP via `rusty_lsp`'s `InitializeResult`/`ServerCapabilities`; ACP per the v1 schema); deeper methods return `method not found` until a language/agent backend is registered
- [x] Built on `rusty_lsp` (JSON-RPC 2.0 `Message` model + LSP types), no reinvented RPC
- [ ] DAP / Jupyter bridges; full LSP/ACP backends (need a language server / agent behind them)

## Backlog

P0–P2, P3, and the architectural fork (#10, the native window backend) have all
landed (see the 2026-06-03 update note). Items below are kept as a record; the
only open work is the long-tail leftovers flagged inline (iTerm2 images,
DAP/Jupyter + full LSP/ACP backends, in-window mouse/clipboard/IME).

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
9. **L13 structured side-channel.** [x] **Done** — full-duplex JSON-RPC over a
    private OSC (`OSC 5379`), one message per OSC, with capability negotiation
    (`channel/initialize`) and graceful ANSI fallback. Hosts a complete MCP
    server (terminal introspection) plus LSP/ACP `initialize` negotiation, built
    on `rusty_lsp` (JSON-RPC model + LSP types). Feature `l13`, runtime-agnostic.
    Remaining: DAP/Jupyter bridges and full LSP/ACP backends.

### Architectural fork — native window backend
10. **Native window backend** (`tcore-font` + `tcore-app`, synthesis §12). [x]
    **Built** behind `gui` (CPU) / `gui-gpu` (GPU): a `winit` window driving the
    shared parser/grid, with a `Renderer` trait over a `softbuffer` CPU
    compositor and a `wgpu` GPU compositor (glyph-atlas + instanced quads,
    WGSL), `ab_glyph` rasterization, **native** key encoding (the L09 native
    side, replacing relay), and resize→`TIOCSWINSZ`. `--gui` launches it,
    `--gpu` selects the GPU renderer (CPU fallback).
    - **Verified headless:** font rasterization, the CPU `Grid`→pixel-buffer
      compositor (render-to-buffer asserts), key encoding, and GPU adapter +
      WGSL-shader + pipeline + atlas creation (`gpu_core_builds`).
    - **Verified on real Windows 11 hardware (not headless CI):** the live window
      runs in both CPU and GPU modes — including a maximized GPU window past the
      2048px texture limit, after the `Surface::configure` surface-limit fix
      (#26) that the resize first surfaced. The headless CI box still can't run
      it (lavapipe/dzn segfault on submit), so `gpu_renders_to_texture` stays
      `#[ignore]`d there.
    - **Now wired in the window:** block cursor, left-drag text selection with
      clipboard copy/paste (Ctrl+Shift+C / Ctrl+Shift+V, bracketed-paste aware,
      injection-guarded), and a child-exit watcher so the window closes when the
      shell quits (the Windows ConPTY output pipe only EOFs at teardown).
    - **Documented gaps (not stubs):** mouse *reporting* to the child (selection is
      local-only), OSC 52 programmatic clipboard, IME, DECCKM tracking in the
      window, in-window scrollback, and host resize propagation on Windows. Text is
      pixel-resolution; inline images still render at cell resolution (half-blocks).