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
| server → client  | text   | `stats <json>` every 2s: system load, memory, and the cwd's git branch/counts (`null` where the host can't say) |
| client → server  | text   | `cwd <path>` — the shell's OSC 7 directory, relayed by the page |
| client → server  | text   | `ping <token>` → `pong <token>` — app-level RTT probe |

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
- **The command cards**, when the shell emits OSC 133 shell-integration
  marks: each command becomes a card the moment it starts running and
  resolves to success/error with its real output (first 30 lines), exit
  code, and duration. The tracker
  ([`commandTracker.ts`](src/components/terminal/commandTracker.ts)) reads
  the same A/B/C/D marks rusty_term's native gutter marks and command dock
  consume, via xterm's OSC handler + buffer markers — it observes the
  rendered stream and never rewrites it, so terminal fidelity is untouched
  (the native renderer's "semantic features are additive" rule).
- The side dock's *recent commands*, which mirror the live card stream.
- **The ribbon and dock stats**: the bridge pushes `stats` frames with the
  host's load average and memory pressure (Linux `/proc`; `null` elsewhere)
  and git facts — branch from `.git/HEAD`, added/modified/deleted counts
  from `git status --porcelain` — for the directory the shell reports via
  OSC 7 (the page decodes the URI and relays it as a `cwd` message). The
  environment pill flips to `live`, the sparkline is a rolling window of
  real load samples, and latency is a measured app-level ping RTT.
- The input line: submits write into the same PTY, and the resulting card
  arrives through the same OSC 133 path as a hand-typed command.
- **Pinned snippets**: hover a command card and hit its pin (⌖) to keep the
  command in the dock; clicking a snippet (or a *recent commands* row) runs
  it through the same submit path. Pins persist in `localStorage` and can
  be unpinned with the row's ✕.
- **The AI orb**: opens an assist sheet fed by *local heuristics* over the
  real session — a session summary, diagnoses of the latest failures
  (permission denied → suggest `sudo`, command not found → suggest
  `command -v`, missing paths, plain non-zero exits) with runnable/copyable
  suggested commands, and a repeated-failure nudge. The badge counts
  failures that arrived since the sheet was last opened. The local rules
  live behind the `AssistProvider` interface in
  [`assist/heuristics.ts`](src/assist/heuristics.ts) and always run.
- **Claude assist** ([`assist/llmProvider.ts`](src/assist/llmProvider.ts)):
  paste an Anthropic API key into the sheet's *connect* bar and a
  Claude-generated section (model `claude-opus-4-8`, adaptive thinking,
  JSON-schema structured output so the reply is always renderable insights)
  appears above the local rules, re-analyzing whenever a command finishes
  while the sheet is open. The response **streams**: each insight card
  renders the moment its object completes in the SSE stream (an incremental
  scanner in `llmProvider.ts` pulls finished array elements out of the
  partial JSON), with a pulse line marking the in-flight tail.
- **Chat tab**: the same sheet holds a streaming conversation with Claude
  about the session. Each send ships the whole thread plus a fresh snapshot
  of the recent command cards on the latest turn (earlier turns stay plain
  text, so the model always reasons over the cards as they are *now*); the
  reply streams token-by-token into the assistant bubble and the input
  locks until it settles. The thread lives in memory only — closing the
  sheet keeps it, disconnecting or reloading clears it. The key is held in `sessionStorage` only —
  never `localStorage`, never the bundle — and *disconnect* wipes it. The
  `@anthropic-ai/sdk` chunk is lazy-loaded, so nothing is fetched until a
  key is connected. For tests, `sessionStorage["nebula.assistBaseUrl"]`
  points the SDK at a mock Messages endpoint.

  > A key pasted into a browser page is visible to that page; use a
  > scoped/disposable key. A production deployment should proxy the
  > Messages API server-side instead of shipping keys to the client.

To emit the marks from bash, drop this in the profile of the shell the
bridge spawns (zsh/fish equivalents exist; VS Code and WezTerm ship the
same integration):

```sh
PS0='\033]133;C\007'
PROMPT_COMMAND='printf "\033]133;D;%s\007\033]133;A\007" "$?"'
PS1='\$ \[\033]133;B\007\]'
```

Without the marks the cards simply stay empty in live mode — semantic
features degrade to a plain terminal, never break it.

Demo/stub:
- `LoopbackTransport` (the default without `?ws`) echoes input locally (try
  `help`, `size`, `clear`, `exit`) — no real shell behind it.
- Without `?ws`, the command cards in `App.tsx` are hardcoded demo data and
  the input line appends a fake "executed locally" card.
- Without `?ws`, the ribbon's load/latency/git numbers and the dock's
  CPU/RAM bars are hardcoded (they're live with the bridge — see above),
  and cards/submits are the loopback fakes.
- Without a connected API key the assist panel is pattern rules, not a
  model — and says so in its header.
- `theme="cyberpunk" | "minimal"` are accepted per the spec but currently
  render the Nebula skin.
