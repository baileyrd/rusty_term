# Windows Work Handoff — July 2026

You are working on **rusty_term**, a from-scratch Rust terminal emulator
(edition 2024). All cross-platform work is done; this session is the
Windows-specific backlog, which needs a real Windows machine (ConPTY, real
GPU, DWM, taskbar). You are on that machine.

## Project ground rules (non-negotiable)

- **Minimal dependencies.** No new crates without a very strong reason —
  this codebase hand-rolls its own PNG/JPEG/GIF/WebP decoders, inflate,
  base64, UAX #9 bidi, and regex engine rather than adding deps.
  Hand-rolled `unsafe` COM/Win32 FFI is acceptable where it's small and
  contained; a `windows-sys` dependency is a last resort — ask the user
  first.
- **Workflow: every chunk of work = branch → PR → merge → sync main.**
  Small, reviewable PRs, one theme each.
- **Quality gates before every PR:**
  `cargo test --workspace --all-features` all green,
  `cargo clippy --workspace --all-features --tests` zero warnings,
  plain `cargo build` (no features) zero warnings.
- **Honesty in reporting.** Paste real test output. If something can't be
  verified, say so explicitly in the PR body. Never claim a feature works
  on Windows without exercising it against a real child process.
- **Update the tracking doc** (`docs/research/gap-analysis-2026-07.md`)
  status line for each item you complete, same style as the existing
  "**Status: done.**" entries.

## Environment notes

- The `l13` feature pulls a git dependency; if fetch fails, use
  `CARGO_NET_GIT_FETCH_WITH_CLI=true`.
- The `fuzz/` directory is a workspace-excluded cargo-fuzz crate;
  libFuzzer doesn't build under MSVC — skip it entirely on Windows.
- Feature flags: default = TUI passthrough; `gui` = winit + softbuffer
  window; `gui-gpu` = wgpu renderer. Run the app with
  `cargo run --features gui -- --gui`.

## Work items, in priority order

### 1. Baseline: make the suite green on Windows (do this first)

The test suite (597 tests) has never run on Windows. Run
`cargo test --workspace --all-features` and fix what breaks — expect
path-separator assumptions, `#[cfg(unix)]` gaps, and the PTY e2e helpers
(they use python `pty.fork`, Unix-only — gate them, don't delete them).
`src/shells.rs` already has Windows candidates; `src/backend/` has the
ConPTY implementation (reader EOF semantics differ from Unix — see the
`exit_token` machinery in `src/gui/window.rs`). This PR is pure fixes, no
features.

### 2. G10 — win32-input-mode (the headline item)

See the G10 entry in `docs/research/gap-analysis-2026-07.md`. Spec:
`win32-input-mode.md` in the microsoft/terminal repo. Three pieces:

- **Parser/grid**: track DEC private mode `?9001` per grid
  (`src/core/parser.rs`, alongside 1004/2031/2501; make it
  DECRQM-answerable like the others — see `dec_private_mode_state`).
- **GUI input encoding**: when the child has `?9001` set, encode key
  events as `CSI Vk;Sc;Uc;Kd;Cs;Rc _` instead of VT sequences
  (`src/gui/input.rs` — follow the existing `encode_full`/kitty-flags
  pattern: the grid state is read in `src/gui/window.rs`'s
  `KeyboardInput` handler and passed in). Both press and release events;
  winit gives you virtual keycodes and scancodes.
- **TUI passthrough**: relay the mode to the host like other input modes
  (`is_host_input_mode` in parser.rs).

Verify against a **real ConPTY child**: PowerShell and `cmd.exe` under
Windows Terminal-era conhost request this mode; test that arrow keys,
Ctrl combinations, and release events round-trip correctly. Unit-test the
encoder exhaustively (the encoding is pure); e2e what you can.

### 3. G31 Windows half — control channel over named pipes

`src/gui/control.rs` (line protocol: `new-tab`, `new-window`, `quake`,
`send-text`, `list-tabs`, `focus-tab`, `ping`) is `#[cfg(unix)]` over a
Unix socket. Add a Windows transport: named pipe
`\\.\pipe\rusty_term-<username>`, same line protocol, same serve/request
API surface, and un-gate `rusty_term ctl` in `src/main.rs` (`run_ctl`).
Hand-rolled `CreateNamedPipeW`/`CreateFileW` FFI is fine and small. The
parse/dispatch layer is transport-agnostic and already fully tested —
only the transport is new. Test with two real processes.

### 4. Quake window + optional global hotkey (G30 Windows polish)

`rusty_term ctl quake` should work once item 3 lands — verify the
dropdown window behaves on Windows (position, always-on-top, show/hide
focus). Optional stretch: `RegisterHotKey` (Win32) so a global hotkey
toggles it without an external tool — that's the per-platform code the
G30 write-up deferred. Keep it behind a config key (e.g.
`quake_hotkey = "win+grave"`), parse failures warn, and unregister on
exit.

### 5. G01 Windows half — taskbar progress (stretch)

OSC 9;4 progress state already lives in the grid (`Grid::set_progress`)
and shows in the tab strip. Surface it via `ITaskbarList3::SetProgressValue`
/ `SetProgressState` on the window handle (winit exposes the HWND).
That's COM: hand-rolled vtable FFI is acceptable if it stays ~100 lines;
otherwise report back and skip.

### 6. On-hardware verification sweep (no code, one report)

Things built headlessly that have never been seen on a real screen:
- `cargo test --all-features -- --ignored gpu_renders_to_texture` on the
  real GPU (it's `#[ignore]` because software Vulkan crashes headless).
- Visual checks with `--gui` and `--gui` + `gui-gpu`: ligatures, wide
  CJK, Kitty image placement/animation (kitten icat), inline GIF/WebP
  (imgcat), bidi with `bidi = "auto"` (echo Arabic/Hebrew), Nerd Font
  icon constraining, fold summaries (Ctrl+Shift+U after a command),
  cursor trail (`cursor_trail = true`), HTML copy-paste into Word/
  OneNote (CF_HTML flavor via arboard).
- Known gap to confirm and file: Windows' Segoe UI Emoji is **COLR/CPAL**,
  and the emoji decoder only does CBDT/sbix PNG strikes — color emoji
  will likely render monochrome on Windows. Don't fix it in this session;
  document what you observe in the tracking doc as a follow-up item.

## Key file map

| Area | Files |
|---|---|
| VT parser + modes | `src/core/parser.rs` (DECRQM: `report_ansi_mode`, `dec_private_mode_state`) |
| Grid / screen model | `src/core/grid.rs` |
| GUI window/event loop | `src/gui/window.rs` (`App` router + `WindowState` per window) |
| Key encoding | `src/gui/input.rs` |
| Control channel | `src/gui/control.rs`, CLI entry in `src/main.rs` (`run_ctl`) |
| ConPTY backend | `src/backend/` |
| Renderers | `src/gui/cpu.rs` (softbuffer), `src/gui/gpu.rs` (wgpu) |
| Tracking doc | `docs/research/gap-analysis-2026-07.md` |

Start with item 1, and open its PR before moving on.
