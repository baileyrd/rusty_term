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
> target. The Windows ConPTY backend has been run and verified on Windows 11
> (build 26200): shell spawn, child `TERM`/`COLORTERM` env, bidirectional relay,
> and OSC window-title capture all work — though host **resize propagation is a
> known gap** there (no `SIGWINCH` equivalent is wired yet). The optional window
> backend has likewise been run on Windows (CPU and GPU), including a maximized
> window past the 2048px GPU texture limit; it can't be exercised in a headless
> CI environment, so there its font, compositor, input, and GPU-pipeline layers
> are unit-tested independently.

## Build & run

Requires a recent stable Rust (edition 2024).

```sh
# TUI/passthrough mode (default).
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
| `gui`          |         | Native window backend: a `winit` window with a `softbuffer` CPU renderer and `ab_glyph` glyph rasterization. |
| `gui-gpu`      |         | Adds a `wgpu` GPU renderer (glyph atlas + instanced quads) alongside the CPU one. Implies `gui`. |
| `l13`          |         | L13 structured side-channel: a private-OSC JSON-RPC transport hosting MCP plus LSP/ACP negotiation. Requires a sibling `rusty_lsp` checkout (see below). |

The runtime is always **tokio** — a single async reactor. On Unix it registers
the PTY master + `/dev/tty` with the reactor (`AsyncFd` → mio → epoll) and takes
`SIGWINCH` from a signal stream; on Windows, where ConPTY's pipes aren't
pollable, blocking reader/writer/stdin threads bridge into tokio channels and a
timer polls the console size for resizes.

```sh
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
| `--config <path>` | — | Read the configuration file from `<path>` (see below). |
| `--list-shells` | — | Print the shells detected on this machine and exit. |

| Variable          | Effect |
|-------------------|--------|
| `RUSTY_TERM_CONFIG` | Path to the configuration file (when `--config` is not given). |
| `RUSTY_TERM_FONT` | Path to a monospace font for the window backend. If unset, a list of common system locations is searched. |
| `SHELL` / `COMSPEC` | The child program to spawn when no `shell` is configured. On Windows, when `COMSPEC` is unset or the stock `cmd.exe`, rusty_term auto-detects a better default (`pwsh` > `powershell` > `cmd`); a custom `COMSPEC` is honored as-is. On Unix `$SHELL` always wins (falls back to `bash`). |
| `TERM`, `COLORTERM` | Set by `rusty_term` for the child before spawn — not read from your environment. |

### Configuration file

Looked up as `--config <path>` > `$RUSTY_TERM_CONFIG` > the platform default:
`$XDG_CONFIG_HOME/rusty_term/config.toml` (Unix, `~/.config` when unset) or
`%APPDATA%/rusty_term/config.toml` (Windows). A missing file means built-in
defaults; a malformed line or unknown key prints a warning to stderr and is
skipped — the config can never stop the terminal from starting. The syntax is
a TOML subset (`key = value`, `[section]`, `#` comments, quoted strings with
backslash escapes, integers, floats) parsed without any dependency:

```toml
shell = "/usr/bin/fish"  # child to spawn; default $SHELL / %COMSPEC%
scrollback = 5000        # history line cap; default 10000, 0 disables
theme = "gruvbox-dark"   # preset: default, gruvbox-dark, dracula,
                         # solarized-dark, solarized-light, nord, one-dark
```

On Windows, `shell` accepts a bare name resolved through the standard search
path — `"powershell"`, `"pwsh"`, `"wsl"`, `"cmd"` all work — as well as a full
path (quoted automatically if it contains spaces) and trailing arguments
(`"wsl -d Ubuntu"`, `"cmd /K clink inject"`):

```toml
shell = "pwsh"           # or "wsl", "powershell", "C:\\tools\\nu.exe", ...

[window]                 # windowed (--gui) front-end only
cols = 120               # initial size in cells; default 80x24
rows = 40
font = "/path/to/mono.ttf"  # else $RUSTY_TERM_FONT, else system search
font-size = 16           # pixels; default 18

[colors]                 # startup theme; resets (RIS/OSC 1xx) restore it
foreground = "#d8d8d8"
background = "#1d1f21"
cursor = "#aeafad"
color0 = "#282a2e"       # ANSI palette, color0..color15
color1 = "#cc6666"
```

The `[colors]` theme is what every reset path (`RIS`, `DECSTR`, OSC
104/110/111/112) restores, so a configured look survives a `reset` exactly
the way the hardware defaults would. A `theme = "name"` preset seeds all the
colors at once; `[colors]` keys placed after it override individual entries.
The windowed block cursor is painted in the `cursor` color (and follows
OSC 12 at runtime). Indexed colors 16–255 always come from the fixed xterm
cube/ramp. In TUI mode `cols`/`rows` are ignored (the host terminal owns its
size), and `font`/`font-size` apply only to `--gui`.

#### Window backend controls (`--gui`)

| Input | Action |
|-------|--------|
| Left-drag | Select text (highlighted by inversion). |
| Ctrl+Shift+C | Copy the selection to the system clipboard. |
| Ctrl+Shift+V | Paste the clipboard into the shell (bracketed-paste aware). |

The window draws a block cursor and closes when the shell exits. Mouse
*reporting* to applications (so TUI apps see clicks), OSC 52 programmatic
clipboard, and IME are not yet wired.

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

- a **`channel`** meta-protocol for **version negotiation** (`initialize` agrees
  a version with the client and advertises per-protocol capabilities) and
  **schema discovery** (`describe` returns the machine-readable contract);
- an **MCP** server exposing the terminal to agents both as **tools**
  (`get_screen`, `get_scrollback`, `get_cwd`, `get_title`, `get_dimensions`,
  `get_cursor`) and as **resources** (`resources/list` + `resources/read` over
  `terminal://screen`, `terminal://scrollback`, `terminal://cursor`,
  `terminal://exit`, `terminal://command`, …), with live **change
  notifications** — `resources/subscribe` pushes `notifications/resources/updated`
  when a subscribed resource changes (cwd, title, terminal size on resize, and
  the captured output text of each finished command). Completing the OSC 133
  lifecycle, finishing a
  command (OSC 133;C…D) emits a typed `notifications/command_finished` carrying
  the **exit code** in the push itself — no follow-up read;
- a **`render`** protocol for terminal-owned UI the renderer composites
  independent of the child's output stream — `set_status` / `clear_status` drive
  a status-line overlay across the bottom row, honored by all three render paths;
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
  runtime/           the tokio I/O loop
    tokio_rt.rs        async reactor: Unix AsyncFd / Windows ConPTY bridge
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
