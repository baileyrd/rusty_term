# rusty_term — Architecture Decision & Handoff

**Topic:** parser ↔ renderer concurrency model for a from-scratch terminal emulator (Rust, std-only)
**Outcome:** rejected the proposed lock-free event pipeline; adopted a snapshot handoff (two variants), with a runnable 3-thread skeleton built and verified.

---

## 1. Starting point

`rusty_term` runs three threads — input capture, shell-output (PTY) reader, and a
main render thread — sharing a single `Arc<Mutex<TerminalBuffer>>`.

## 2. The proposal under review

A prior review diagnosed **mutex contention** as the bottleneck ("stop-the-world"
lock) and proposed replacing the shared buffer with a **lock-free SPSC event
pipeline**: the output thread pushes per-cell events (`SetChar`, `MoveCursor`)
into a ring buffer; the renderer drains and replays them onto its own local
screen copy. It also proposed parallel ANSI parsing and parallel vertex
generation for the later GUI phase.

## 3. Verdict

One instinct correct, two overcorrections.

**Right:** the render thread must never block the PTY reader. Decoupling
parse-from-render is a real goal.

**Wrong diagnosis:** the mutex is not the toll booth — *holding the lock across
the entire draw* is. Alacritty is among the fastest terminals in existence and
runs on an `Arc<FairMutex<Term>>`; it is fast because its critical section is
tiny, not because it avoids locks.

**Wrong granularity:** per-cell event streaming is a poor fit for a terminal.

| Concern | Event pipeline | Snapshot handoff (chosen) |
|---|---|---|
| Handoff cost | O(mutations) — millions of events for a 10 MB dump | O(dirty rows) clone, or O(1) Arc swap |
| State | duplicated on both sides | single source of truth |
| Coalescing | none without rebuilding a grid by hand | free — renderer reads newest only |
| Backpressure | bounded queue stalls parser; unbounded blows up memory | none on the read path |

**Mostly wrong:** parallel ANSI parsing — VT parsing is inherently sequential
(byte N's meaning depends on accumulated state; escapes straddle chunk
boundaries). Make the single-threaded parser fast first (`vte` is SIMD-fast).

**Premature:** parallel vertex generation — even a 4K grid is ~75K cells;
instanced rendering makes CPU-side vertex gen negligible. Quadrant threading
adds coordination cost that dwarfs the gain.

**Process note:** the whole memo assumed the bottleneck rather than measuring it.
Profile under a `cat bigfile` / `yes` flood before re-architecting — time
usually goes to glyph rasterization, atlas management, or GPU upload, not lock
contention.

## 4. Decision

Keep the lock; take it **off the draw path**. Two variants, both std-only:

- **Variant A (ship first):** keep `Arc<Mutex<Grid>>`. The renderer locks only
  long enough to clone the dirty rows and clear the damage set, then releases
  and draws from that snapshot. Smallest diff from today; single source of truth.
- **Variant B (GUI phase):** parser owns the live grid and publishes an immutable
  `Arc<GridSnapshot>` via `Mutex<Arc<GridSnapshot>>`. Renderer reads are an Arc
  clone (a refcount bump); intermediate snapshots are dropped → automatic
  coalescing. Lets the parser run fully ahead of the renderer.

Truly wait-free reads (seqlock / left-right buffering) are possible std-only but
require `unsafe`; deferred until a flamegraph proves the Arc clone is hot.

## 5. Deliverables in this bundle

- `rusty_term_snapshot_handoff.rs` — reference sketch of Variant A and Variant B
  side by side, with an inline diff against the event-pipeline proposal. Types
  and handoff shapes are compile-plausible; render/parse bodies are stubs.
- `rusty_term_skeleton.rs` — complete, **compiling and runnable** 3-thread
  program around Variant A. Spawns a real `sh` child and reads its piped stdout
  (the std-only stand-in for a PTY), with a small but real stateful parser
  (printable bytes, `\r` `\n` `\t`, scroll-up, CSI stripping across read
  boundaries). Verified output:

  ```
  $ printf 'echo hello from rusty_term\nuname -s\nseq 1 3\nexit\n' | ./rusty_term
  [e   3 r 0] hello from rusty_term
  [e   3 r 1] Linux
  [e   3 r 2] 1
  [e   3 r 3] 2
  [e   3 r 4] 3
  ```

  Build/run:
  ```
  rustc --edition 2021 -O rusty_term_skeleton.rs -o rusty_term
  printf 'ls\nuname -a\nexit\n' | ./rusty_term   # scripted
  ./rusty_term                                    # interactive; 'quit' or Ctrl-D
  ```

## 6. Next steps (in priority order)

1. **PTY seam.** Replace the `sh` + piped-stdout handle with a PTY master fd
   (`openpty` + `fork`/`exec`, plus `TIOCSWINSZ` for resize and `ONLCR` from the
   line discipline). This is the one thing std cannot do — first reach for
   `nix`/`libc`/`rustix`. Everything above the fd is unchanged. *(In the skeleton,
   `\n` is manually treated as CR+LF to emulate ONLCR; the real PTY provides it.)*
2. **Real parser.** Swap the toy `Parser` for the `vte` state machine. The
   `advance(&mut Grid, &[u8])` signature survives.
3. **Profile**, then decide whether Variant B and any data parallelism are
   warranted.

## 7. Known rough edges in the skeleton

- The input thread can park on `stdin().lines()` after the shell exits; process
  teardown reaps it rather than a clean join. Goes away in the GUI phase when the
  input source becomes window events.
- The toy parser ignores SGR/cursor-move effects (it only strips them), so color
  and absolute cursor positioning are not yet rendered — by design, pending the
  `vte` swap.
