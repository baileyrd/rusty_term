# Release Notes

What shipped, grouped by the theme of the work rather than a strict commit
list — see `git log` for that. Newest first. Every underlying claim here is
backed by a merged PR and (where the change has runtime behavior) a passing
test; nothing below is aspirational.

---

## 2026-07-23 — Accessibility tree (C20)

### ✨ Added
- **Screen-reader accessibility (C20)** — the native GUI window (`gui`
  feature) now exposes its visible screen text and cursor position through
  an `accesskit`-backed accessibility tree, feeding the platform's real
  assistive-tech API (AT-SPI/UIA/AX) rather than an in-house overlay. A new
  `gui/access.rs` module builds a `Terminal`-role root node carrying the
  full visible grid as text plus a polite live-region child announcing the
  cursor's 1-based row/column, refreshed on redraw only when the content,
  cursor, or title actually changed (a blink-only redraw doesn't
  re-announce). The AccessKit adapter is created before each window is
  first shown, as the library requires, and every window event is forwarded
  to it alongside the existing input handling. New dependencies:
  `accesskit`, `accesskit_winit`. Per the roadmap's own research, no major
  competitor (kitty/Ghostty/WezTerm/Alacritty) has meaningful screen-reader
  support either — this is differentiation, not catch-up. Known scope
  limit: AT-driven actions (e.g. a screen reader's scroll/focus requests)
  aren't yet acted on, only content exposure.

---

## 2026-07-17 — Web frontend hardening & CI

A full pass over the Nebula web frontend and its bridge: the review that
found these items, and every fix, PR, merge, and CI check, happened in this
pass.

### 🐛 Fixed
- **`origin_allowed` rejected valid bracketed IPv6 loopback origins** — the
  host-matching code split on `:` before checking for a `[...]` literal, so
  `http://[::1]:5173` never matched. Fixed and covered by new tests.
- **CommandCard's fallback key was position-based** (`cmd-${i}`) — filtering
  (failures-only) or regrouping could hand React a key pointing at a
  different card than before, causing avoidable unmount/remount churn. Now
  derived from the card's own `command` + `startedAt`.
- **New clippy lints were breaking CI on `main`** — the floating
  `dtolnay/rust-toolchain@stable` action picks up new lints over time;
  7 real findings plus a Windows-only `unused_mut` were fixed.
- **The `fmt` CI job had been red all session** — rustfmt formatting had
  drifted from the committed source (same floating-toolchain root cause).
  Reconciled with a single mechanical `cargo fmt --all` across the
  workspace — no logic changes, full test suite re-verified green after.

### 🔒 Hardened
- **The WebSocket handshake had no timeout** — a peer that opened a
  connection and never finished the HTTP upgrade held a task (and a PTY
  slot) open indefinitely. Now bounded by `tokio::time::timeout`.
- **PTY writes ran on the async runtime thread**, so a slow write to a
  wedged child could stall the whole reactor. Writes now go through a
  dedicated writer thread and channel, decoupling the caller from the I/O.
- **`bidi.rs`'s isolate/embedding status-stack unwraps** were tightened
  with bounds-checked pops and `debug_assert!` tripwires — loud in
  debug/CI, silent-degrade in release, so a future regression is caught
  by tests instead of surfacing as a user-visible panic.
- **The assist API key's exposure was undocumented in-app** — the settings
  sheet now shows a warning banner while a key is connected, explaining
  it's sent client-side straight to the Anthropic API and pointing at the
  safer production shape (proxy the Messages API server-side).

### 🛠 Improved
- **Every structured `localStorage` key now goes through a shared,
  versioned envelope** (`web/src/storage.ts`) instead of three hand-rolled
  parse/validate/save paths — a future shape change resets cleanly per key
  instead of a validator half-accepting a shape it wasn't written for.
- **Shared overlay hooks** (`useOverlayEscape`, `useOverlayLifecycle`)
  factored out of the command palette, search overlay, and settings sheet —
  one place for capture-phase Escape and open/close lifecycle instead of
  three copies.
- **The production bundle now splits into vendor chunks** — `react`,
  `@xterm/*`, and `@anthropic-ai/sdk` each get their own chunk via Vite's
  `manualChunks`, so an app-code deploy doesn't invalidate dependencies
  that didn't change.

### 🧪 CI
- **The web-bridge Rust feature is now linted and tested in CI** — it
  previously shipped with zero CI coverage.
- **A `web` job now runs the full Playwright E2E suite on every push and
  PR** — 55 assertions across the palette, search, settings, themes,
  panes, tabs, session restore, and the assist panel. Getting this green
  took three follow-up fixes to the suite's own preview-server bootstrap
  (a fixed sleep that raced a slow CI runner, `npx` resolution overhead,
  and finally an explicit `--host 127.0.0.1` bind to match what the suite
  polls) — all now committed alongside the job itself.
- **The web frontend's E2E test suite is committed to the repo** at all
  (`web/e2e/e2e.mjs`, `web/e2e/live-bridge.mjs`) — previously it existed
  only as a local, uncommitted artifact.

---

## Nebula web frontend

The web-based frontend prototype (`web/`) went from a scaffold to a
feature-complete session workspace, bridged to a real shell over a
hand-rolled WebSocket PTY server (`rusty_term_web_bridge`, zero new Rust
dependencies).

### ✨ New
- **Live command cards** driven by real OSC 133 shell-integration marks —
  each command becomes a card the moment it starts and resolves to
  success/error with its real output, exit code, and duration.
- **Session tabs and split panes** — up to four side-by-side terminals per
  tab, each an independent PTY session; tabs keep their own command-card
  history and pane layout.
- **Session restore** — the whole workspace (tabs, active tab, command
  cards, pane layout) survives a reload.
- **History search** (`Ctrl+Shift+F`) across every tab's commands, output,
  and meta, with match highlighting and jump-to-card.
- **Collapsible card groups** — idle gaps in activity fold the stream into
  bursts with a hairline summary header.
- **Transcript export** — a session's card history as Markdown (lab-notebook
  style) or JSON.
- **Card quick actions** — re-run, copy output, and pin, on hover.
- **Failures-only stream filter** with a dismissible ratio chip.
- **Background tab activity badges** for commands finishing out of view.
- **Command palette** (`Ctrl/Cmd+K`) — fuzzy filter over pinned snippets,
  recent commands, and shell actions, with a raw run-in-terminal fallback.
- **Theme switcher** — nebula / cyberpunk / minimal presets, live and
  persisted, with the xterm panel re-skinning its ANSI palette to match.
- **Settings sheet** (`Ctrl+,`) — one place for the theme picker, assist
  connection, pinned-snippet housekeeping, and a keyboard-shortcut
  reference, instead of scattered one-shot palette actions.
- **The AI orb**: local-heuristic session insights out of the box, and a
  live Claude-backed upgrade once an API key is connected — streaming
  insight cards, a full chat tab with runnable code blocks, all reasoning
  over the session's actual command history.
- **Live stats side-channel** — the bridge pushes system load, memory, and
  git branch/status over the same socket, driving the ribbon and dock.
- **Pinned snippets** — pin a command from its card, run it from the dock
  or palette.

See [`web/README.md`](web/README.md) for the full feature-by-feature
real-vs-demo breakdown, the wire protocol, and how to run it against a
real shell.

---

## Core terminal

The from-scratch Rust terminal emulator (`src/`) that the web frontend and
the native window backend both sit on top of — a hand-rolled VT/ANSI parser,
grid, and protocol surface, with a `gui`/`gui-gpu` native window backend and
an `l13` structured MCP/LSP side-channel. This has had many waves of work
(protocol conformance, bidi, image codecs, GPU rendering, Windows parity,
and more) predating the web frontend; the full catalog — features, dates,
and the tests backing each — lives in [`docs/FEATURES.md`](docs/FEATURES.md)
and [`docs/research/`](docs/research/). `CODE_REVIEW.md` has the point-in-time
security/robustness audit and its resolutions.
