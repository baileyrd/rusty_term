# rusty_term web — "Nebula" frontend prototype

A web-based frontend prototype for [rusty_term](../README.md), implementing the
**Nebula** design system: a dark, card-based command stream with a status
ribbon, side dock, raw xterm.js panel, and an AI-assistant orb.

This is a UI prototype. It does **not** talk to rusty_term yet — see
[Bridging plan](#bridging-plan-connecting-to-rusty_term) below.

## Stack

- React 18 + TypeScript, built with Vite
- Tailwind CSS, extended with Nebula design tokens (namespace `nebula`,
  source of truth in [`src/theme/tokens.ts`](src/theme/tokens.ts))
- [@xterm/xterm](https://xtermjs.org/) + `@xterm/addon-fit` for the raw
  terminal panel, themed with the Nebula ANSI palette

Fonts (JetBrains Mono / Cascadia Code / Inter) are referenced with
`ui-monospace` / `system-ui` fallbacks only — no CDN imports, so the app
renders offline. Ship the font files as local assets in a later pass if
pixel-exact typography matters.

## Running

```sh
cd web
npm install
npm run dev      # dev server at http://localhost:5173
npm run build    # type-check + production bundle in dist/
```

Without URL parameters the page runs the offline loopback demo. To attach it
to a **real shell**, build and start the websocket PTY bridge from the repo
root, then open the page with `?ws`:

```sh
cargo run --features web-bridge --bin rusty_term_web_bridge   # ws://127.0.0.1:7703
# then browse to http://localhost:5173/?ws
# or with an explicit endpoint: http://localhost:5173/?ws=ws://127.0.0.1:7703
```

## Layout

```
src/
  theme/tokens.ts                  Nebula design tokens (colors, radii,
                                   shadows, motion, fonts, ANSI palette)
  transport/bridge.ts              TerminalTransport interface, offline
                                   LoopbackTransport demo, and the live
                                   WebSocketTransport (rusty_term bridge)
  components/terminal/
    types.ts                       Shared prop types per the design spec
    TerminalShell.tsx              Layout root: ribbon / stream / dock / orb
    StatusRibbon.tsx               Load sparkline, latency dot, env pill,
                                   git branch chip + stats
    CommandStream.tsx              CommandCard list + input line + raw panel
    CommandCard.tsx                One executed command (idle/running/
                                   success/error states)
    TerminalView.tsx               xterm.js panel wired to a transport
    SideDock.tsx                   Process bars, recent commands, snippets
    AiOrb.tsx                      Pulsing cyan orb with unread badge
  App.tsx                          Demo data + shell wiring
```

## Bridging plan: connecting to rusty_term

The UI is written against a small transport interface,
[`TerminalTransport`](src/transport/bridge.ts):

```
connect(url) · write(data) · resize(cols, rows) · onData(cb) · onExit(cb) · dispose()
```

This deliberately mirrors the shape of the repo's PTY abstraction in
`src/backend/mod.rs` — `BackendHandle::write`, `set_winsize`, read-as-events,
and `reap_exit_status` — so the bridge is a thin adapter, not a redesign.

**The bridge exists**: `rusty_term_web_bridge` (repo root, `src/web_bridge/`,
built with `cargo build --features web-bridge`). True to the repo's ethos it
adds **zero dependencies** — the RFC 6455 handshake (SHA-1 included) and
frame codec are hand-rolled and unit-tested, and the runtime is the tokio
the terminal already links. Per websocket connection it spawns a shell
through the platform `Backend::spawn_shell` and shuttles bytes.

Wire protocol (text = control, binary = PTY bytes):

| direction        | frame  | meaning                                        |
|------------------|--------|------------------------------------------------|
| client → server  | text   | `start <cols> <rows>` (first message), then `resize <cols> <rows>` |
| client → server  | binary | keystrokes/pastes, written to the PTY verbatim |
| server → client  | binary | PTY output, verbatim                           |
| server → client  | text   | `exit <code>` when the shell exits, then Close |

Security posture: the bridge hands a shell to whoever completes a handshake,
so it binds `127.0.0.1` only and refuses browser `Origin`s other than
localhost. Exposing it further is deliberately not a flag — put an
authenticating reverse proxy in front instead.

On this side, `WebSocketTransport` in
[`transport/bridge.ts`](src/transport/bridge.ts) implements
`TerminalTransport` over that protocol, and `transportFromLocation` picks it
(vs. the loopback demo) from the page's `?ws` parameter — nothing in the
component tree changed. Structured features (command cards populated from
real command boundaries, git stats, system load) can ride the same socket
later, plausibly reusing the repo's L13 side-channel concepts for framing.

## What is demo/stub vs real

Real:
- The component architecture, prop contracts, and design-token pipeline
  (tokens → Tailwind theme → components).
- The xterm.js terminal: a genuine `Terminal` instance with the Nebula ANSI
  theme, fit addon, and resize handling, driven through the transport
  interface it will use in production.
- The `TerminalTransport` interface.

Real (with the bridge running, `?ws`):
- The xterm panel is a live shell: PTY output, resize (SIGWINCH), and the
  exit code all round-trip through `rusty_term_web_bridge`.

Demo/stub:
- `LoopbackTransport` (the default without `?ws`) echoes input locally (try
  `help`, `size`, `clear`, `exit`) — no real shell behind it.
- The command cards in `App.tsx`, the ribbon's load/latency/git numbers, the
  dock's CPU/RAM bars, recent commands, and snippets are hardcoded demo data.
- Submitting on the input line appends a fake "executed locally" card.
- The AI orb is presentational; clicking it only clears the badge.
- `theme="cyberpunk" | "minimal"` are accepted per the spec but currently
  render the Nebula skin.
