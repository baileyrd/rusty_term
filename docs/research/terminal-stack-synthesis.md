# Building a Multiplatform Terminal: A Synthesis

A walking tour of what a terminal actually is, the standards stack underneath
it, and what it takes to build one in Rust. This document is the *narrative*
companion to the spec tree HTML reference — read this for understanding, refer
to the spec tree for the catalog of standards.

---

## Table of Contents

0. [Where we started](#0-where-we-started)
1. [The three things people conflate](#1-the-three-things-people-conflate)
2. [The lowest level: what a PTY actually is](#2-the-lowest-level-what-a-pty-actually-is)
3. [The standards stack at a glance](#3-the-standards-stack-at-a-glance)
4. [POSIX, ANSI, OSC — who owns what](#4-posix-ansi-osc--who-owns-what)
5. [Modernization approaches](#5-modernization-approaches)
6. [The OSC zoo](#6-the-osc-zoo)
7. [Building the whole stack in Rust](#7-building-the-whole-stack-in-rust)
8. [The libc question](#8-the-libc-question)
9. [How `nix` is organized](#9-how-nix-is-organized)
10. [The `mio` / `tokio` / `nix` relationship](#10-the-mio--tokio--nix-relationship)
11. [The concrete code pattern: PTY → AsyncFd](#11-the-concrete-code-pattern-pty--asyncfd)
12. [Architecture of a real Rust terminal](#12-architecture-of-a-real-rust-terminal)
13. [Where the gaps actually bite](#13-where-the-gaps-actually-bite)
14. [Realistic scope](#14-realistic-scope)

---

## 0. Where we started

The premise was: "I want to understand shells and REPLs, and I want to build a
multiplatform Linux-like terminal." That's actually three different questions
hiding in one sentence. Untangling them is step one.

---

## 1. The three things people conflate

**Terminal emulator** — the GUI application that draws characters on screen and
captures keyboard input. It owns a *pseudo-terminal* (PTY) and pipes bytes
to/from whatever process runs inside it. Examples: Alacritty, WezTerm, iTerm2,
Windows Terminal, gnome-terminal.

**Shell** — the program *running inside* the terminal that reads commands,
parses them, and executes other programs. Examples: bash, zsh, fish, nushell,
PowerShell. A shell is a specialized REPL where "evaluate" means "run a
program."

**REPL** — Read-Eval-Print Loop. The generic pattern of any interactive prompt:
read input, evaluate it, print the result, loop. Python's `>>>` is one. A
shell is another. Jupyter is a remoted REPL.

The relationship: a *terminal emulator* runs a *shell* inside it; the *shell*
is a *REPL* over OS commands.

These three layers move bytes between each other, and the bytes are the only
contract. The shell doesn't know what kind of terminal it's running in; the
terminal doesn't know what shell is inside. They communicate through a
byte-stream pretending to be a 1970s serial line.

---

## 2. The lowest level: what a PTY actually is

A pair of file descriptors that the kernel pretends is a serial line to a
teletype.

That's the whole foundation. Everything else is built on top of that fiction.

### The history

A **TTY** (teleTYpewriter) was a physical machine — keyboard plus printer,
connected to a mainframe by serial cable. The OS talked to it by writing bytes
down a wire. When physical terminals disappeared in the 1980s, Unix kept the
abstraction because too much already depended on it.

### The modern PTY

A **PTY** (pseudo-terminal) is the software incarnation: two file descriptors,
called **master** and **slave**.

- Anything the master writes appears as input on the slave.
- Anything the slave writes appears as output on the master.
- The kernel sits between them faking a serial line.

On Linux, you allocate a PTY pair via `/dev/ptmx` (multiplexer) and get a slave
device at `/dev/pts/N`. The four-call dance is `posix_openpt`, `grantpt`,
`unlockpt`, `ptsname`.

### The line discipline

The kernel runs a **line discipline** on the slave side that converts the raw
byte stream into civilized terminal behavior:

- Echo keystrokes back to the user
- Buffer input until Enter is pressed (canonical mode)
- Turn `Ctrl-C` into `SIGINT` to the foreground process
- Turn `Ctrl-Z` into `SIGTSTP`
- Fire `SIGWINCH` when the window resizes

All of this is configured via the **`termios`** API. The `struct termios` has
four flag fields:

- `c_iflag` — input processing (CR→NL, parity)
- `c_oflag` — output processing (NL→CRNL, tab expansion)
- `c_cflag` — hardware control (baud rate, byte size — meaningless on a PTY
  but kept for ABI compatibility)
- `c_lflag` — local modes (canonical, echo, signal generation)

Plus `c_cc[]` — the control character array. `c_cc[VINTR] == 0x03` is why
Ctrl-C generates SIGINT. Not magic — just a byte match.

### Why the whole stack is shaped like RS-232

PTYs simulate RS-232 serial connections. That's why `termios` still has fields
for baud rate and parity — they're meaningless on a software PTY but the API
surfaces them because programs from the 1980s expected to find them. The
ghost of RS-232 haunts every layer above.

### The terminal-emulator loop

When you build a terminal emulator, the basic loop is:

1. Open a PTY pair (`openpty()` on Unix, `CreatePseudoConsole` on Windows)
2. `fork()` + `exec()` a shell with the slave as its stdin/stdout/stderr
3. Your GUI reads bytes from the master (what the shell wants to display)
4. Your GUI writes bytes to the master (what the user typed)

Everything above that — ANSI parsing, grid rendering, fonts, scrollback — is
just interpretation of those byte streams.

---

## 3. The standards stack at a glance

The full stack has 14 layers. The HTML spec tree catalogs every standard at
every layer; here's a one-line summary of each:

| Layer | Name | What it does |
|-------|------|--------------|
| L00 | Physical / Historical | RS-232, ASCII, ISO 2022 — the substrate |
| L01 | OS Interface | POSIX, SUS, Windows API |
| L02 | Pseudo-Terminal | Unix98 PTY (`/dev/ptmx`), ConPTY |
| L03 | Line Discipline | termios, kernel TTY layer |
| L04 | File Descriptor / I/O | read/write, epoll/kqueue/IOCP, signals |
| L05 | Character Encoding | UTF-8, Unicode UAX algorithms |
| L06 | Control Sequences | ECMA-48, ANSI X3.64, xterm extensions |
| L07 | Graphics Protocols | Sixel, Kitty graphics, iTerm2 inline images |
| L08 | OSC Extensions | OSC 7/8/52/133/633/1337 — semantic features |
| L09 | Input Protocols | Mouse, bracketed paste, Kitty keyboard |
| L10 | Identification | terminfo, DA1/DA2, XTVERSION, `$TERM` |
| L11 | Shell | POSIX sh, bash, zsh, nushell, PowerShell |
| L12 | Utilities | POSIX utils, GNU coreutils, busybox, uutils |
| L13 | Adjacent Protocols | LSP, DAP, MCP, JSON-RPC, Jupyter |

L00 is empty in terms of "things you implement" — it's the historical/physical
substrate. L13 isn't strictly terminal but informs modernization design.

---

## 4. POSIX, ANSI, OSC — who owns what

The three big standards bodies you'll touch govern *different* things:

**POSIX** defines the *API surface* — how programs interact with the OS, what
syscalls do, how shells parse their language, what utilities like `ls` and
`grep` behave like. POSIX says how *bytes flow*, not what they *mean*.

**ANSI / ECMA-48** defines what those bytes *mean* once they hit the terminal.
Escape sequences. CSI for cursor movement, SGR for colors, OSC for vendor
extensions. POSIX doesn't dictate this — ECMA-48 does. Confusingly, ANSI X3.64
(the original American version) was withdrawn in 1997, so "ANSI escape codes"
is technically a misnomer — they're ECMA-48 codes now — but the name stuck.

**OSC codes** are the modern frontier inside ECMA-48. Operating System
Commands are the namespace where terminal vendors add semantic features:
hyperlinks (OSC 8), clipboard access (OSC 52), prompt marks (OSC 133),
notifications (OSC 9), iTerm2's grab-bag (OSC 1337). Some are standardized
de-facto; some are vendor-private.

**The key insight:** these three layers are independent. You can be POSIX-
compatible without supporting any escape codes. You can support ANSI escape
codes without being POSIX (Windows did this with ConPTY). You can use OSC
codes nobody else recognizes, and graceful-degrade to plain text.

This independence is what makes the "all three" terminal possible — see next
section.

---

## 5. Modernization approaches

There are three philosophical camps for "modernizing" the terminal:

### Wrap-and-extend

Keep POSIX, ANSI/ECMA-48, and the PTY substrate. Add new OSC codes on top.

- *OSC 8* gives you hyperlinks
- *OSC 133* lets terminals know where commands start and end → unlocks
  block-based UI (Warp's pitch)
- *Kitty graphics protocol* gets you real pixel images
- *Semantic prompts* let your terminal understand the shell state

This is the **additive** approach. Programs that don't know about new codes
ignore them; programs that do get richer rendering. Warp, Wave, VSCode's
terminal, iTerm2 all live here.

**Strengths:** backward compatible. Every existing Unix program keeps working.
**Weaknesses:** you're contributing to the OSC sequence zoo. Every terminal
vendor invents their own codes. Fragmentation.

### Replace-the-substrate

Throw out the byte-stream-pretending-to-be-RS-232 model. Build a structured
protocol from the shell up.

- *Nushell* — pipelines pass typed structured records, not bytes
- *PowerShell* — pipelines pass .NET objects
- *DomTerm* — HTML/CSS as the native output format
- *Notty* — explicit protocol redesign (mostly experimental)

**Strengths:** the model is genuinely better. Structured output composes.
**Weaknesses:** you lose POSIX compatibility. Every existing Unix script
either breaks or needs a translation layer. The ecosystem cost is enormous.

### Dual-channel (the unsolved problem)

Run both at once. Byte stream for legacy programs (vim, htop, ssh), structured
side-channel for native ones (your custom commands, your structured shell).

The architecture:

1. **POSIX PTY base** — full backward compat
2. **ANSI + modern OSC middle** — wrap-and-extend benefits
3. **Private OSC code carrying structured payloads** — the novelty

Programs aware of your terminal emit `OSC <your-code> ; <JSON-or-msgpack> ST`.
Programs that aren't emit normal text. Same byte stream, two readers —
a dumb terminal sees plain text, yours renders a structured widget.

That's the graceful-degradation trick. Same protocol, different fidelity
depending on who's listening.

Nobody has the perfect synthesis yet because the structured-protocol design
is genuinely hard — it's where terminal meets GUI toolkit meets RPC system,
and you have to make all three feel native.

Warp comes closest. Wave too. Neither has nailed it.

---

## 6. The OSC zoo

The modern OSC code landscape, grouped by purpose:

### Title and window
- **OSC 0 / 1 / 2** — window title and icon name

### Color
- **OSC 4 / 10 / 11 / 12** — palette and default fg/bg/cursor color
- **OSC 104 / 110 / 111 / 112** — reset variants

### Semantic
- **OSC 7** — working directory reporting (powers "new tab in same dir")
- **OSC 8** — hyperlinks (gnome-terminal led adoption)
- **OSC 52** — clipboard access (critical for SSH sessions)
- **OSC 133** — semantic prompt marks (FinalTerm origin, the block-UI enabler)
- **OSC 633** — VSCode's extensions to OSC 133

### Notifications
- **OSC 9** — iTerm2/ConEmu notifications
- **OSC 777** — urxvt notifications

### Vendor private
- **OSC 1337** — iTerm2's grab-bag namespace (inline images, marks, badges,
  attention, file transfer)

If you build a modern terminal, the minimum semantic OSC support is roughly:
0/1/2 (titles), 4/10/11/12 (colors), 7 (cwd), 8 (hyperlinks), 52 (clipboard),
133 (prompt marks). That covers ~95% of what modern programs actually emit.

For your private structured-channel OSC code: pick a number nobody else uses
(7777 is a reasonable pick; or claim a 4-digit number from the unallocated
range). Document it. The protocol body can be JSON, MessagePack, CBOR, or
whatever you prefer.

---

## 7. Building the whole stack in Rust

Yes, completely. The Rust ecosystem has full coverage of every layer. In fact,
the most prominent modern terminal projects are already Rust: Alacritty,
WezTerm, Zellij, Nushell, uutils, Rio, Warp's core.

### The hub crates

A few crates show up at multiple layers and form the spine of any real
implementation:

- **`nix`** — safe POSIX wrapper. Spans L01–L04.
- **`vte`** — Alacritty's ECMA-48 parser. Spans L06–L10.
- **`termwiz`** — WezTerm's terminal feature library. Spans L06–L09.
- **`tokio`** — async runtime. Owns L04 for application code.
- **`mio`** — cross-platform event abstraction underneath tokio.
- **`portable-pty`** — cross-platform PTY abstraction.

### Layer-by-layer mapping

```
L13  Adjacent Protocols
     tower-lsp · rmcp · jsonrpsee · dap-types · jupyter-protocol

L12  Utilities
     uutils/coreutils (uu_ls, uu_cat, uu_grep, ~100 binaries)

L11  Shell
     nu · reedline · rustyline
     std::process::Command (to spawn bash/zsh/fish/pwsh)

L10  Identification
     terminfo · term · vte (parse responses)

L09  Input
     termwiz::input · crossterm::event

L08  OSC Extensions
     vte (dispatch) · arboard · linkify · notify-rust

L07  Graphics
     sixela · icy_sixel · viuer · image

L06  Control Sequences
     vte · termwiz · ansi-parser

L05  Character Encoding
     std::str · unicode-segmentation · unicode-width
     unicode-bidi · unicode-normalization · icu4x

L04  File Descriptor / I/O
     tokio · mio · tokio-uring
     nix::unistd · nix::fcntl · nix::sys::signal · nix::sys::wait

L03  Line Discipline
     nix::sys::termios

L02  Pseudo-Terminal
     portable-pty (wraps nix::pty + windows for cross-platform)
     nix::pty · nix::sys::ioctl

L01  Operating System Interface
     libc · nix · rustix · windows · windows-sys

L00  Physical / Historical
     (no crates — pre-API substrate)
```

### Interconnection patterns

**The libc spine** — most paths funnel through `nix → libc → kernel`. The
exceptions are `rustix` and `tokio-uring`, which issue raw Linux syscalls.

**vte is the parsing hub** — most L08–L10 crates consume vte's parsed events.

**portable-pty is the cross-platform bridge** — wraps `nix::pty` (Unix) and
`windows::Console` (Windows) behind one trait-based API.

**tokio dominates async** — every L13 protocol crate runs on tokio. tokio
uses mio, which calls libc/windows-sys directly.

**The dashed line** is `std::process::Command` shelling out to external
bash/zsh/fish/pwsh — the one place where Rust-only purity breaks. You either
ship a C-shell binary or commit to Nushell.

---

## 8. The libc question

**Why can't libc itself be done in Rust?**

It can be — partial Rust libcs exist (relibc, used by Redox OS). The blockers
aren't "technically impossible," they're "boring infrastructure":

1. **Bootstrap dependency.** `rustc` links against glibc via LLVM. To compile
   any Rust code, you need a working libc already.

2. **ABI freeze.** glibc maintains binary compatibility to 1997. Symbol
   versioning (`@GLIBC_2.34`), weak symbols, struct layout stability. Rust has
   no equivalent forever-ABI culture.

3. **Async-signal-safety.** Many libc functions must be callable from signal
   handlers without deadlocking. Requires `unsafe` either way, which removes
   a big chunk of Rust's pitch.

4. **The dynamic linker IS libc.** On Linux, `ld-linux.so.2` is part of glibc.
   Replacing libc means replacing the linker.

5. **Thread-local everything.** errno is per-thread. So is locale state, FPU
   environment. TLS initialization is done by libc-provided startup code
   (`crt1.o`, `crti.o`, `crtn.o`).

6. **Performance lineage.** glibc has decades of hand-tuned assembly for
   `memcpy`/`strlen`/`memcmp`. SSE2, AVX, AVX-512, NEON, SVE code paths.

7. **The std problem.** Rust's stdlib is built on libc. Replacing libc means
   `#![no_std]` + `alloc` + custom everything.

### The three Rust positions

- **`nix`** — wraps libc with safe APIs. libc still loaded. *(Most apps.)*
- **`rustix`** — can skip libc on Linux by issuing raw syscalls directly.
  *(Servers, minimal binaries.)*
- **`relibc`** — IS libc, written in Rust. Replaces `libc.so`. *(Redox.)*

For application-level work, the realistic stack is: use `nix` (or `rustix` if
you want the option of statically linking with `musl` later), ship libc as a
host dependency, don't worry about it. The "fully Rust" goal stops at the
syscall boundary — below that is the kernel.

---

## 9. How `nix` is organized

The `nix` crate is one comprehensive Unix systems-programming wrapper. The
double-colon (`::`) is Rust's module separator — not a separate crate.

```
nix
├── pty             ← L02   openpty, forkpty, Winsize, PtyMaster
├── fcntl           ← L04   open(), file flags, locks
├── unistd          ← L04   read, write, close, dup, fork, execve
├── poll            ← L04   poll()
└── sys
    ├── ioctl       ← L02/L03  ioctl macros (TIOCGWINSZ, etc.)
    ├── termios     ← L03   tcgetattr, tcsetattr, struct termios
    ├── signal      ← L04   sigaction, kill, signals
    ├── wait        ← L04   waitpid, WaitStatus
    ├── select      ← L04   select() (legacy)
    ├── epoll       ← L04   Linux epoll (raw)
    └── event       ← L04   BSD/macOS kqueue (raw)
```

One Cargo dependency:

```toml
[dependencies]
nix = "0.29"
```

And in your code:

```rust
use nix::pty::{openpty, OpenptyResult};
use nix::sys::termios::{tcgetattr, tcsetattr};
use nix::unistd::{fork, ForkResult};
```

The crate itself lives at L01 (it's POSIX bindings), but it exposes
functionality across L01–L04 via its modules.

---

## 10. The `mio` / `tokio` / `nix` relationship

This is a place where it's easy to draw the dependency wrong.

`mio`, `tokio`, and `nix` are **siblings**, not stacked. They all sit on top
of `libc` directly:

```
tokio  ──►  mio  ──►  libc  ──►  kernel
                       ▲
                       │
                      nix  ←── parallel path to mio
```

- **`mio`** has its own thin wrappers over `epoll_ctl` / `kevent` / IOCP. It
  does NOT route through `nix::sys::epoll`. It calls libc directly.
- **`tokio`** uses `mio` for OS event registration, plus its own
  work-stealing scheduler.
- **`nix`** is a separate path into libc, used for one-time setup syscalls.

In your terminal stack, you'd typically use:

- **`nix`** for setup: opening the PTY, configuring termios, fork+exec.
- **`tokio`** for the hot path: async reads from the PTY master fd.

They coexist in the same binary without depending on each other. Both
eventually call libc.

---

## 11. The concrete code pattern: PTY → AsyncFd

A natural question: which crate "handles" Unix98 PTYs — `nix`, `tokio`, or
`mio`?

**None of them — and that's the right answer.** PTYs are a kernel construct
exposed as file descriptors. `nix` knows how to *create* a PTY (it wraps the
PTY-creation syscalls). Once you have the fds back, they're just regular file
descriptors. `tokio` and `mio` don't know or care that they're PTYs.

This is the general Unix principle: **everything is a file descriptor**. Once
you have the fd, the layers above don't need to know what kind of thing it
represents.

The handoff in code:

```rust
use nix::pty::openpty;
use tokio::io::unix::AsyncFd;

// L02: nix creates the PTY
let pair = openpty(None, None)?;
let master_fd = pair.master;

// L04: tokio wraps the fd for async use
let async_fd = AsyncFd::new(master_fd)?;

// Now: read bytes asynchronously
loop {
    let mut guard = async_fd.readable().await?;
    let mut buf = [0u8; 4096];
    match guard.try_io(|inner| inner.get_ref().read(&mut buf)) {
        Ok(Ok(n))  => { /* got n bytes, feed to vte parser */ }
        Ok(Err(e)) => return Err(e.into()),
        Err(_would_block) => continue,
    }
}
```

Two crates, two layers, clean handoff via the file descriptor.

---

## 12. Architecture of a real Rust terminal

A workspace of focused crates, with the standards-layer they cover noted:

```
terminal/
├── Cargo.toml                  (workspace root)
└── crates/
    ├── tcore-pty/              # L02 — PTY creation and child spawning
    ├── tcore-termios/          # L03 — line discipline config
    ├── tcore-io/               # L04 — async I/O event loop
    ├── tcore-parser/           # L06 — ECMA-48 + OSC dispatch
    ├── tcore-grid/             # L05 + L06 — cell model + scrollback
    ├── tcore-input/            # L09 — keyboard/mouse encoding
    ├── tcore-graphics/         # L07 — Sixel/Kitty image dispatch
    ├── tcore-shell/            # L11 — shell spawn + lifecycle
    ├── tcore-protocol/         # L13 — private structured OSC
    ├── tcore-font/             # glyph shaping + rasterization
    └── tcore-app/              # window + GPU + composition
```

### What each crate does

**tcore-pty** (L02) — owns master/slave fds, fork+exec, resize ioctls.
Uses `nix::pty`, `nix::unistd`, `nix::sys::ioctl`.

**tcore-termios** (L03) — sets raw mode for the slave at shell-start.
Uses `nix::sys::termios`.

**tcore-io** (L04) — async event loop reading bytes from PTY masters.
Uses `tokio::io::unix::AsyncFd`, `tokio::signal::unix`, `nix::sys::wait`.

**tcore-parser** (L06) — wraps `vte::Parser` with a custom `Perform` impl
that emits a `TerminalEvent` enum. OSC dispatch routes 7/8/52/133/633/1337
and your private code to handlers.

**tcore-grid** (L05+L06) — 2D grid of `Cell`, scrollback, cursor state.
Uses `unicode-segmentation`, `unicode-width` for proper CJK/emoji handling.

**tcore-input** (L09) — encodes keyboard/mouse events back to the PTY master
as escape sequences. Kitty keyboard protocol, SGR mouse 1006, bracketed paste.

**tcore-graphics** (L07) — decodes Sixel/Kitty/iTerm2 image payloads.
Uses `image`, `sixela`.

**tcore-shell** (L11) — spawns `$SHELL` via tcore-pty. Handles lifecycle.

**tcore-protocol** (L13) — private OSC code + `rmcp` server on the structured
channel.

**tcore-font** — `cosmic-text` + `swash` for shaping/rasterization, or
`harfbuzz_rs` for closer-to-FreeType behavior.

**tcore-app** — `winit` + `wgpu`, or `tauri` for webview composition.

### Data flow

```
[shell child] ──write──► [PTY slave] ──kernel──► [PTY master]
                                                       │
                                                       │ tokio AsyncFd::readable
                                                       ▼
                                                 [tcore-io: poll loop]
                                                       │
                                                       │ feed bytes
                                                       ▼
                                                 [tcore-parser: vte::Parser]
                                                       │
                                                       ├─ CSI → screen ops
                                                       ├─ SGR → style updates
                                                       ├─ OSC → tcore-protocol
                                                       └─ text → grid cells
                                                       │
                                                       ▼
                                                 [tcore-grid: mutate state]
                                                       │
                                                       ▼
                                                 [tcore-app: redraw GPU]
```

---

## 13. Where the gaps actually bite

The Rust ecosystem covers most of the stack, but there are real gaps:

1. **POSIX shell semantics — biggest gap.** No production-grade Rust bash.
   Workaround: spawn bash/zsh and accept the C dependency for the shell
   process. Or commit to Nushell. No third path.

2. **Font rendering edge cases.** `cosmic-text` handles Latin + CJK well;
   Arabic, Devanagari, Burmese, certain emoji ZWJ sequences are rough.
   Workaround: most serious Rust terminals ship `harfbuzz_rs` (FFI to
   HarfBuzz C library).

3. **IME (CJK input methods).** `winit` has IME hooks but Linux IBus/Fcitx
   integration is fragile.

4. **Accessibility.** `accesskit` is the Rust answer for screen readers;
   coverage is incomplete. Most Rust terminals skip this entirely.

5. **Kitty graphics protocol completeness.** No off-the-shelf complete Rust
   implementation. You'd write significant code yourself.

6. **Job control** (`Ctrl-Z`, `fg`, `bg`, `jobs`). Bash relies on careful
   PGID + `tcsetpgrp` choreography. `nix` exposes the syscalls but composing
   them race-free is hard.

7. **Async-signal-safety in SIGWINCH handlers.** Use `tokio::signal::unix`
   rather than rolling your own — it handles this for you.

8. **Wayland primary selection** (middle-click paste). `arboard` covers the
   standard clipboard; primary selection is fiddly.

9. **vte parser throughput.** For `cat largefile.txt`, vte is sometimes the
   bottleneck. Alacritty and Ghostty have custom parsers.

10. **ConPTY edge cases.** Newline conversion, color depth detection.
    `portable-pty` covers ~95%; the `windows` crate covers the rest.

11. **terminfo for exotic terminals.** Pure-Rust `terminfo` works for
    xterm-256color family. Falls back to `ncurses-rs` for obscure terminals.

12. **Locale-dependent edge cases.** POSIX `LC_*` state is process-global.
    `icu4x` doesn't fully replace it.

---

## 14. Realistic scope

Starting from "you have a working PTY layer and rmcp wired" (which is roughly
where Nexus Forge is):

- **Weekend** — termios + IO loop reading from PTY into stdout. Working
  "dumb terminal" passthrough.
- **Week** — parser + grid + input. A working TUI-mode terminal you could
  run inside another terminal.
- **Month** — font + app. Windowed terminal you can show people.
- **Three months** — graphics + protocol + production polish (clipboard,
  hyperlinks, OSC 133 block UI). Useful to others.
- **Year** — feature parity with WezTerm/Alacritty on common workflows.

The middle two layers — parser + grid + input — are the cathedral's chapel.
Once those are done, everything else is glass painting. The real labor isn't
writing Rust; it's understanding what each escape sequence is supposed to do
in 50 historical contexts. The `vte` crate gives you parsing; semantic
behavior is on you.

The one decision worth a week of paper design before any code: **the
structured side-channel protocol**. OSC code, payload schema, capability
advertisement. That decision shapes the next ten years of the project, and
changing it later breaks every tool you've shipped.

---

## Companion document

The full standards catalog with custom SVG diagrams (PTY architecture, OSC
sequence anatomy, dual-channel architecture, dependency graph) lives in
`terminal-stack-spec-tree.html`. That file is the reference; this document is
the narrative. Use them together.
