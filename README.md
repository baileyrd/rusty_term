# rusty_term

A terminal emulator written from scratch in Rust, with a deliberately small
dependency surface. It parses a child shell's output into an in-memory `Grid`
and renders it two ways:

- **TUI mode (default)** — a *passthrough/relay* emulator that re-emits ANSI to
  its own stdout, so it runs inside a host terminal (tmux-like). The host does
  the actual pixel drawing; `rusty_term` owns the PTY, the parser, the grid, and
  the protocol surface.
- **Native window backend (optional)** — a standalone `winit` window that draws
  the same grid with real font glyphs and encodes key events natively, with a
  CPU (`softbuffer`) or GPU (`wgpu`) renderer.

It implements all 14 layers of the terminal stack catalogued in
[`docs/research/terminal-stack-spec-tree.html`](docs/research/terminal-stack-spec-tree.html);
the per-layer scorecard and backlog live in
[`docs/research/implementation-status.md`](docs/research/implementation-status.md),
and the narrative design synthesis in
[`docs/research/terminal-stack-synthesis.md`](docs/research/terminal-stack-synthesis.md).

> **Platform support.** Unix (Linux/macOS) is the primary, fully exercised
> target. A Windows ConPTY backend exists and type-checks but is currently
> unrun. The optional window backend drives a live window and so is not
> exercisable in a headless CI environment (its font, compositor, input, and
> GPU-pipeline layers are unit-tested independently).

## Build & run

Requires a recent stable Rust (edition 2024).

```sh
# Default: threaded runtime, TUI/passthrough mode.
cargo run

# Run the test suite.
cargo test
```

`cargo run` spawns your `$SHELL` (Unix) / `%COMSPEC%` (Windows) as a child and
relays it through the parser into the host terminal. Before spawning, it sets
`TERM` (to the shipped `rusty_term` terminfo entry when installed, else
`xterm-256color`) and `COLORTERM=truecolor` for the child.

### Cargo features

| Feature        | Default | What it adds |
|----------------|:-------:|--------------|
| `threaded`     |   yes   | Threaded runtime: one OS thread each for parse / input / render, coordinated by a condvar. No async deps. |
| `tokio-runtime`|         | Single async reactor (`AsyncFd` → mio → epoll) driving the PTY master, host stdin, SIGWINCH, and render coalescing. **Unix-only**; takes precedence over `threaded`, so build with just `--features tokio-runtime`. |
| `gui`          |         | Native window backend: a `winit` window with a `softbuffer` CPU renderer and `ab_glyph` glyph rasterization. |
| `gui-gpu`      |         | Adds a `wgpu` GPU renderer (glyph atlas + instanced quads) alongside the CPU one. Implies `gui`. |
| `l13`          |         | L13 structured side-channel: a private-OSC JSON-RPC transport hosting MCP plus LSP/ACP negotiation. Requires a sibling `rusty_lsp` checkout (see below). |

```sh
# Async (tokio) runtime instead of the threaded default.
cargo run --features tokio-runtime

# Native window, CPU renderer.
cargo run --features gui -- --gui

# Native window, GPU renderer (falls back to CPU if no adapter).
cargo run --features gui-gpu -- --gui --gpu

# Structured side-channel (requires ../rusty_lsp; see below).
cargo run --features l13
```

### Command-line flags & environment

| Flag    | Requires    | Effect |
|---------|-------------|--------|
| `--gui` | `gui`       | Launch the native window backend instead of TUI/passthrough mode. |
| `--gpu` | `gui-gpu`   | Use the `wgpu` GPU renderer in the window (CPU fallback on failure). |

| Variable          | Effect |
|-------------------|--------|
| `RUSTY_TERM_FONT` | Path to a monospace font for the window backend. If unset, a list of common system locations is searched. |
| `SHELL` / `COMSPEC` | The child program to spawn (falls back to `bash` / `cmd`). |
| `TERM`, `COLORTERM` | Set by `rusty_term` for the child before spawn — not read from your environment. |

## Shell integration (OSC 133)

`rusty_term` recognizes OSC 133 semantic prompt marks and uses them for
prompt-to-prompt scrollback navigation (Ctrl+Shift+PageUp / PageDown). Source
the matching script for your shell so it emits the marks:

```sh
# bash / zsh
source extra/shell-integration/bash.sh   # or zsh.sh
# fish
source extra/shell-integration/fish.fish
# PowerShell
. extra/shell-integration/pwsh.ps1
```

## terminfo

A self-describing terminfo entry ships in
[`extra/rusty_term.terminfo`](extra/rusty_term.terminfo). Install it so the
`rusty_term` / `rusty_term-256color` `TERM` values resolve (otherwise the child
sees `xterm-256color`):

```sh
tic -x extra/rusty_term.terminfo
```

## Structured side-channel (`l13`)

The `l13` feature adds a full-duplex JSON-RPC 2.0 transport over a private OSC
(`OSC 5379 ; <protocol> ; <json> ST`). One message per OSC, replies written to
the child's stdin; terminals that don't understand the OSC ignore it. It hosts:

- an **MCP** server exposing the terminal to agents (`get_screen`,
  `get_scrollback`, `get_cwd`, `get_title`, `get_dimensions`);
- **LSP** and **ACP** `initialize` negotiation endpoints.

It reuses the JSON-RPC model and LSP types from the sibling `rusty_lsp` crate,
so the feature expects a checkout at `../rusty_lsp` relative to this repo.

## Repository layout

```
src/
  main.rs            entry point + runtime/window dispatch
  backend/           OS interface: PTY spawn, raw mode, resize
    unix.rs            openpty + fork/exec (libc)
    windows.rs         ConPTY (windows-sys)
  runtime/           the I/O loops
    threaded.rs        threaded runtime (default)
    tokio_rt.rs        tokio runtime (feature `tokio-runtime`)
  core/              parser + grid + protocol surface
    parser.rs          VT/ANSI state machine
    grid.rs            cells, scrollback, reflow, image rendering
    charset.rs         G0–G3 + DEC line-drawing
    color.rs           palette + truecolor
    osc.rs             OSC dispatch
    sixel.rs           Sixel decoder
    base64/inflate/png/kitty.rs   from-scratch Kitty graphics stack
    channel.rs         L13 structured side-channel (feature `l13`)
    tests.rs           the core test suite
  render.rs          TUI-mode ANSI re-emission
  input.rs           TUI-mode input handling + scrollback keys
  term.rs            TERM probe-and-fallback selection
  gui/               native window backend (features `gui` / `gui-gpu`)
    font.rs            ab_glyph glyph cache
    cpu.rs             Grid → pixel-buffer compositor
    gpu.rs             wgpu glyph-atlas renderer
    render.rs          shared Renderer trait + CPU presenter
    input.rs           native winit-key → terminal-byte encoding
    window.rs          winit event loop
  bin/bench_metrics.rs   grid-handoff microbenchmark
extra/
  rusty_term.terminfo
  shell-integration/   bash / zsh / fish / pwsh OSC 133 emitters
docs/research/         spec tree, synthesis, implementation status
```

## Design notes

- **Small dependency surface.** The core depends only on `libc`, `parking_lot`,
  `unicode-width`, and `unicode-segmentation`; Sixel and the full Kitty graphics
  stack (base64 → zlib/DEFLATE → PNG) are hand-rolled, no crates. Windowing, GPU,
  and font crates are pulled in only behind the `gui` / `gui-gpu` features.
- **Runtime-agnostic protocol logic.** Everything in `core/` works identically
  under both runtimes; replies ride a response channel back to the PTY master,
  which both runtimes drain.
- **Relay, not re-encode (TUI mode).** Input-generating protocols (mouse,
  bracketed paste, cursor-key mode, Kitty keyboard, modifyOtherKeys) are relayed
  to the host terminal rather than natively encoded — the emulator sees encoded
  bytes, not key events. Native encoding lives in the window backend.
