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
the narrative design synthesis in
[`docs/research/terminal-stack-synthesis.md`](docs/research/terminal-stack-synthesis.md),
and the implemented-feature catalog (per-feature notes + test names) in
[`docs/FEATURES.md`](docs/FEATURES.md). A competitive audit against kitty,
Ghostty, WezTerm, Alacritty, Windows Terminal, and peers — 28 capabilities
evaluated, 27 recommended and sequenced, 1 explicitly rejected — lives in
[`docs/research/capability-assessment-2026.md`](docs/research/capability-assessment-2026.md).
See [`RELEASE_NOTES.md`](RELEASE_NOTES.md) for what shipped and when.

> **Platform support.** Unix (Linux/macOS) is the primary, fully exercised
> target. The Windows ConPTY backend has been run and verified on Windows 11
> (build 26200): shell spawn, child `TERM`/`COLORTERM` env, bidirectional relay,
> and OSC window-title capture all work; host resize is handled by polling the
> console size (there is no `SIGWINCH` equivalent — see `resize_poll` in
> `src/runtime/tokio_rt.rs`). The optional window
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
| `gui`          |         | Native window backend: a `winit` window with a `softbuffer` CPU renderer, `ab_glyph` glyph rasterization, and `ttf-parser`-driven GSUB ligature shaping. |
| `gui-gpu`      |         | Adds a `wgpu` GPU renderer (glyph atlas + instanced quads) alongside the CPU one. Implies `gui`. |
| `l13`          |         | L13 structured side-channel: a private-OSC JSON-RPC transport hosting MCP plus LSP/ACP negotiation. Lives in its own crate (`l13/`, see below). |
| `web-bridge`   |         | `rusty_term_web_bridge`: a hand-rolled WebSocket PTY bridge (RFC 6455 handshake + framing, zero new dependencies) for the [web frontend prototype](web/README.md). Binds `127.0.0.1` only. |

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

# Structured side-channel (see below).
cargo run --features l13

# WebSocket PTY bridge for the web frontend prototype (see web/README.md).
cargo run --features web-bridge --bin rusty_term_web_bridge
```

### Command-line flags & environment

| Flag    | Requires    | Effect |
|---------|-------------|--------|
| `--gui` | `gui`       | Launch the native window backend instead of TUI/passthrough mode. |
| `--gpu` | `gui-gpu`   | Use the `wgpu` GPU renderer in the window (CPU fallback on failure). |
| `--config <path>` | — | Read the configuration file from `<path>` (see below). |
| `--list-shells` | — | Print the shells detected on this machine and exit. |
| `--cwd <dir>` / `--starting-directory <dir>` | — | Starting working directory for the spawned shell. A missing directory fails the spawn cleanly (nonzero exit) rather than crashing. |
| `--title <t>` | `gui` | Seed the window's initial title; the child's own OSC 0/2 title still wins once it emits one. |
| `--maximized` / `--fullscreen` | `gui` | Open the window maximized or borderless-fullscreen. `--fullscreen` wins if both are given. |
| `-- <prog> [args...]` (aliases: `-e`, `--command`) | — | Run `<prog>` with the given args instead of the configured/detected shell — everything after the token is passed through untouched, so it's also where a child's own flags go, e.g. `rusty_term --gui -- bash -lc 'echo hi; exec bash'`. |

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
                         # solarized-dark, solarized-light, nord, one-dark,
                         # catppuccin-mocha, catppuccin-latte, tokyo-night,
                         # tokyo-night-storm, monokai, rose-pine,
                         # github-dark, kanagawa, nebula
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
font = "/path/to/mono.ttf"          # else $RUSTY_TERM_FONT, else system search
font-size = 16                      # pixels; default 18
font_bold = "/path/to/mono-bold.ttf"          # else derived from `font`
font_italic = "/path/to/mono-italic.ttf"
font_bold_italic = "/path/to/mono-bolditalic.ttf"
font_fallback = "/path/to/cjk.ttf"  # glyphs the main font lacks (CJK, symbols)
ligatures = true                    # GSUB liga/calt ligatures; default on
cursor_style = "bar"                # block (default) | bar | underline
cursor_blink = true                 # default off
status_bar = false                  # hide the bottom status ribbon; default on
command_marks = false               # hide the per-command gutter marks; default on

[keys]                   # rebind window shortcuts as  action = "chord"
search = "Ctrl+Shift+F"
split_right = "Ctrl+Shift+D"
new_tab = "Ctrl+Shift+T"

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
The windowed cursor is painted in the `cursor` color (and follows
OSC 12 at runtime). Indexed colors 16–255 always come from the fixed xterm
cube/ramp. In TUI mode `cols`/`rows` are ignored (the host terminal owns its
size), and `font`/`font-size` apply only to `--gui`.

The `[window]` block also configures the **font stack** and **cursor**:
`font_bold` / `font_italic` / `font_bold_italic` name explicit variant faces
(unset, they fall back to filename-derived siblings of `font`, then to the
regular face), `font_fallback` covers glyphs the main font lacks, and
`ligatures` toggles programming ligatures (the font's GSUB `liga`/`calt`
features — on by default, ignored for fonts without them). `cursor_style`
(`block`/`bar`/`underline`) and `cursor_blink` set the startup cursor; the
child can still override both at runtime via DECSCUSR.

The windowed front-end also draws a one-row **status ribbon** flush with the
window's bottom edge: the focused pane's working directory (OSC 7, home
shortened to `~`), its git branch (read straight from `.git/HEAD`, no `git`
subprocess), the last command's exit code as a green `✓` / red `✗` pill
(OSC 133 shell integration), the scrollback position while scrolled back,
and the grid size. `status_bar = false` under `[window]` (or the settings
page's Window → Status bar toggle) hides it and returns the row to the grid.

With OSC 133 shell integration, each command's output also gets a
**gutter mark**: a thin colored stripe just left of the pane's text spanning
the command's rows — green for exit 0, red for a non-zero exit, the accent
color while the command is still running. Marks follow the scrollback view
(including folded blocks, whose summary line keeps the block's mark), stay
out of full-screen (alt-screen) apps, and sit in the padding band so they
never overpaint text. `command_marks = false` under `[window]` (or the
settings page's Window → Command marks toggle) turns them off.

`Ctrl+Shift+K` (rebindable as `toggle_dock`) opens the **command dock**: a
right-hand panel listing the focused pane's commands newest-first — the
running one, then each finished block with its ✓/✗ exit glyph, the command
line it was typed on, and its runtime. Clicking an entry jumps the
scrollback to that command's output (unfolding it if collapsed). The pane
area re-tiles around the dock, and it auto-hides on windows too narrow to
fit it beside a usable terminal.

The `[keys]` section rebinds any window shortcut as `action = "chord"`. The
actions are `copy`, `paste`, `new_tab`, `new_window`, `fold_output`,
`close_tab`, `next_tab`, `prev_tab`, `open_config`, `open_settings`, `search`,
`open_links`, `copy_mode`, `broadcast`, `split_right`, `split_down`,
`focus_next`, `focus_left`, `focus_right`, `focus_up`, `focus_down`,
`resize_left`, `resize_right`, `resize_up`, `resize_down`, `zoom_pane`,
`scroll_page_up`, `scroll_page_down`, `scroll_prompt_up`, `scroll_prompt_down`,
`toggle_fullscreen`, `font_size_up`, `font_size_down`, `font_size_reset`,
`toggle_dock`; a
chord is `+`-separated modifiers (`ctrl`/`shift`/`alt`) plus one key — a
printable character, or `comma`/`tab`/`pageup`/`pagedown`/`left`/`right`/`up`/
`down`/`space`/`home`/`end`/`enter` (or `return`)/`insert` (or `ins`)/`delete`
(or `del`)/`escape` (or `esc`)/`backspace`/`f1`-`f12`.

#### Live reload

The config file is watched while rusty_term runs: saving it applies **theme**
and **scrollback** changes immediately — existing screen content, scrollback,
and even the stashed primary screen are recolored, while truecolor output and
colors the child set itself (OSC 4/10/11/12) are left alone. `shell`, `font`,
and `[window]` size are launch-time choices and take effect on the next start.
In the window backend, **Ctrl+Shift+,** opens the config file in your editor
(`$VISUAL`/`$EDITOR`, else the system opener), creating it from a commented
template on first use — edit, save, watch it apply.

#### Window backend controls (`--gui`)

The window is borderless and draws its own chrome: a one-row bar across the
top with the session **tabs** (sized to their titles, shrinking together as
the strip fills; truncated titles end in `…`, and the `×` close button shows
on the active and hovered tab), a `+` new-tab button, a `▾` shell-launcher /
settings dropdown, and minimize/maximize/close. Each tab holds one or more
shell sessions arranged as split panes. Drag the empty bar to move the
window (double-click it to toggle maximize), and drag the thin band at the
window edges to resize.

| Input | Action |
|-------|--------|
| Left-drag | Select text (highlighted by inversion); double/triple-click first to extend by word/line as you drag, and drag past a pane's top/bottom edge to auto-scroll. |
| Ctrl+hover / Ctrl+click | Underline + pointer cursor over a hyperlink (OSC 8 or a detected plain-text URL), Ctrl+click opens it. |
| Ctrl+Shift+C | Copy the selection to the system clipboard. |
| Ctrl+Shift+V | Paste the clipboard into the shell (bracketed-paste aware). |
| Ctrl+Shift+F | Open the in-window search bar (incremental match highlighting; Ctrl+R toggles regex, Alt+C toggles case sensitivity, Ctrl+V/middle-click pastes into the query). |
| Ctrl+Shift+, | Open the config file in your editor (created from a template on first use). |
| Ctrl+, / `▾` menu | Toggle the in-app **settings page** (it opens as its own tab in the strip): an Appearance / Terminal / Window sidebar covering theme, font, cursor, contrast, shell, scrollback, clipboard, padding, opacity, launch size, and more, each with a one-line description. `Tab` switches category, `↑`/`↓` pick a row, `←`/`→` (or a click on the value) change it — `Shift` steps numbers ×10, `Home`/`End` jump to the bound, `Enter` (or a typed digit) edits a number directly — type (or `/` / `Ctrl+F`) to search across every category, the wheel scrolls, `Esc` saves & closes; changes apply live. |
| Ctrl+Shift+T / `+` | Open a new tab with the configured shell; the `▾` dropdown launches any detected shell (PowerShell, cmd, WSL, bash, …) in a new tab instead. |
| Ctrl+Shift+W / tab `×` / tab middle-click | Close the focused pane, or the whole tab via its `×` button or a middle-click on the tab (the last pane / tab closes the window). |
| Ctrl+Tab / Ctrl+Shift+Tab | Cycle through tabs; when there are more tabs than fit, the strip scrolls to keep the active one visible and a `»N` indicator (click to advance) shows how many are hidden. |
| Ctrl+Alt+1…9 | Jump straight to that tab (9 = the last tab, browser-style). |
| Tab drag | Drag a tab along the strip to reorder it (it trades places as you cross a neighbor's midpoint); a plain click never reorders. |
| Ctrl+Shift+D / Ctrl+Shift+E | Split the focused pane right / down. |
| Ctrl+Shift+J | Move focus to the next pane. |
| Shift+PageUp / PageDown | Scroll the scrollback by a page. |
| Ctrl+Shift+PageUp / PageDown | Jump to the previous / next shell prompt (OSC 133). |
| F11 | Toggle fullscreen (not for the quake window, which docks to the monitor edge instead). |
| Ctrl+= / Ctrl+- / Ctrl+0 | Zoom the font size up / down / reset (matches Chrome/VS Code/iTerm2/Windows Terminal). |

Every shortcut above is rebindable in the `[keys]` config section (below).

The cursor shape (block / bar / underline) and blink follow the `cursor_style`
and `cursor_blink` config keys, and the child can change them at runtime via
DECSCUSR (`CSI Sp q`); a tab closes when its shell exits, and the window closes
with the last tab. The window is a full participant in the input and graphics
protocols: when a TUI app enables mouse tracking (`?1000`/`?1002`/`?1003`,
SGR/1006) clicks and the wheel are reported to it as SGR-encoded events.
Holding **Shift** during a click, drag, or scroll always bypasses app mouse
tracking for that event — the standard xterm/iTerm2 escape hatch for
selecting text or scrolling back while a full-screen app (vim, tmux, htop)
has grabbed the mouse. OSC 52 reads and writes the system clipboard; IME
pre-edit composes inline; OSC 9/777
desktop notifications are forwarded to the OS; and Sixel / Kitty / iTerm2
(`OSC 1337`) images render over the grid — pixel-for-pixel in the CPU renderer,
with a half-block fallback in the GPU and TUI paths.

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

It lives in its own workspace crate, `l13/` (package `rusty_term_l13`), addressed
against a narrow `TerminalState` trait rather than this repo's `Grid` type
directly — independently buildable and unit-tested with no dependency on
`rusty_term` itself (`cargo test -p rusty_term_l13`); `Grid` implements the
trait as a thin delegation layer (`src/core/grid.rs`). It reuses the JSON-RPC
model and LSP types from `rusty_lsp`, pinned as a `git` dependency (tag
`v0.1.0`) rather than a local path, so building `--features l13` needs
network access but no sibling checkout.

## Repository layout

```
src/
  main.rs            entry point + runtime/window dispatch
  config.rs          config-file parsing (TOML subset) + live reload
  shells.rs          shell detection (--list-shells)
  keymap.rs          window keybindings (rebindable via [keys])
  backend/           OS interface: PTY spawn, raw mode, resize
    unix.rs            openpty + fork/exec (libc)
    windows.rs         ConPTY (windows-sys)
  runtime/           the tokio I/O loop
    tokio_rt.rs        async reactor: Unix AsyncFd / Windows ConPTY bridge
  core/              parser + grid + protocol surface
    parser.rs          VT/ANSI state machine
    grid.rs            cells, scrollback, reflow, image + ligature plumbing
    charset.rs         G0–G3 + DEC line-drawing
    color.rs           palette + truecolor
    osc.rs             OSC dispatch (title, palette, hyperlinks, clipboard, …)
    sixel.rs           Sixel decoder
    kitty.rs           Kitty graphics (APC) protocol
    iterm.rs           iTerm2 inline images (OSC 1337)
    base64/inflate/png/jpeg.rs   from-scratch image-decode stack (no crates)
    tests.rs           the core test suite
  render.rs          TUI-mode ANSI re-emission
  input.rs           TUI-mode input handling + scrollback keys
  term.rs            TERM probe-and-fallback selection
  gui/               native window backend (features `gui` / `gui-gpu`)
    font.rs            ab_glyph glyph cache (variants + fallback chain)
    shape.rs           GSUB ligature shaper (ttf-parser)
    layout.rs          tab / split-pane tree
    cpu.rs             Grid → pixel-buffer compositor (+ image overlay)
    gpu.rs             wgpu glyph-atlas renderer
    render.rs          shared Renderer trait + CPU presenter
    input.rs           native winit-key → terminal-byte encoding
    mouse.rs           pointer → SGR mouse-report encoding
    window.rs          winit event loop
  bin/bench_metrics.rs   grid-handoff microbenchmark
l13/                 the `rusty_term_l13` workspace crate (feature `l13`)
  src/lib.rs           L13 structured side-channel, against a TerminalState trait
extra/
  rusty_term.terminfo
  shell-integration/   bash / zsh / fish / pwsh OSC 133 emitters
  gen_ligtest_font.py  regenerates the GSUB shaper test fixture
docs/
  FEATURES.md          implemented-feature catalog
  research/            spec tree, synthesis, implementation status
```

## Design notes

- **Small dependency surface.** The core depends only on `libc`, `parking_lot`,
  `unicode-width`, and `unicode-segmentation`; Sixel, the Kitty graphics stack
  (base64 → zlib/DEFLATE → PNG), the baseline JPEG decoder (iTerm2 images), and
  the GSUB ligature shaper are all hand-rolled — no image or text-shaping crates.
  Windowing, GPU, and font crates are pulled in only behind `gui` / `gui-gpu`;
  the ligature shaper reads the font's GSUB table through `ttf-parser`, which
  `ab_glyph` already depends on, so it adds no new compiled crate.
- **Runtime-agnostic protocol logic.** Everything in `core/` works identically
  under both runtimes; replies ride a response channel back to the PTY master,
  which both runtimes drain.
- **Relay, not re-encode (TUI mode).** Input-generating protocols (mouse,
  bracketed paste, cursor-key mode, Kitty keyboard, modifyOtherKeys) are relayed
  to the host terminal rather than natively encoded — the emulator sees encoded
  bytes, not key events. Native encoding lives in the window backend.
