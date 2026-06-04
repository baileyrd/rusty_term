# rusty_term — Repository Analysis

> Source-grounded technical review of the whole repository.
> Date: 2026-06-03. Scope: full tree at `C:/dev/rusty_term`.
> Verification basis: default build compiles; `cargo test` = **231 passing**
> (228 at review time + 3 adversarial added with the fixes in section 4);
> `cargo clippy` = 4 benign style nits; the binary decoders were traced by hand.

## 1. What it is

A from-scratch terminal emulator in Rust (edition 2024), **~10.3k LOC across 32
`.rs` files** (~6.5k production, ~3.8k tests). It owns the PTY, an ANSI/VT
parser, an in-memory `Grid`, and a protocol surface, and renders two ways:

- **TUI / passthrough mode (default):** re-emits ANSI to its own stdout, running
  *inside* a host terminal (tmux-like). The host paints pixels.
- **Native window backend (`gui` / `gui-gpu`):** a `winit` window with
  `softbuffer` (CPU) or `wgpu` (GPU) renderers and `ab_glyph` glyphs.

The defining constraint is a **tiny dependency surface**: the core depends only
on `libc`, `parking_lot`, `unicode-width`, `unicode-segmentation`. base64,
zlib/DEFLATE, PNG, Sixel, and the Kitty graphics stack are all hand-rolled.

## 2. Architecture

Two independent axes, both cleanly abstracted behind traits/features:

| Axis | Options | Seam |
|------|---------|------|
| **Runtime** | `threaded` (3 OS threads + condvar) / `tokio-runtime` (single `AsyncFd`/epoll reactor, Unix-only) | `runtime::run(Box<dyn Backend>, Arc<Mutex<Grid>>, ...)` — `main` is agnostic (`runtime/mod.rs:43`) |
| **OS backend** | `UnixBackend` (openpty+fork) / `WindowsBackend` (ConPTY) | `Backend` + `BackendHandle` traits (`backend/mod.rs`) |
| **Front-end** | TUI relay / native window | shared `core::Grid` |

The decomposition is genuinely good. `core/` is platform- and runtime-agnostic;
the parser drives the grid through a semantic API (`put_char`, `scroll_up`,
`set_cursor_abs`, ...) and produces a `DirtyFrame` snapshot for the renderer.
Query replies (DA/DSR/Kitty/MCP) ride a `responses` buffer back to the PTY
master, drained identically by both runtimes (`render.rs:206`).

Worth singling out:

- **Locking discipline (tokio):** the guard is *never* held across `.await` —
  documented and enforced, so a sync `parking_lot::Mutex` is correct
  (`tokio_rt.rs:9-12`). The repaint `Notify` uses a pinned, in-place re-armed
  `Notified` future to avoid lost wakeups (`tokio_rt.rs:292-301`). Subtle,
  correct concurrency.
- **fork/exec safety (Unix):** the `$SHELL` `CString` is allocated *pre-fork* so
  the post-fork path is async-signal-safe; `setsid` + `TIOCSCTTY` + `dup2` +
  `execvp` only (`backend/unix.rs:20-92`). Child reaping escalates
  SIGHUP -> (250ms grace) -> SIGKILL (`unix.rs:219-250`).
- **Raw-mode safety:** `RawModeGuard` restores cooked mode on every exit path
  including panic (`render.rs:24-41`); `restore_host_modes()` un-sticks relayed
  mouse/paste modes on shutdown.
- **~60 Hz frame coalescing:** producers signal damage; the renderer holds off
  to `FRAME_BUDGET` (16ms) so a `cat bigfile` flood repaints smoothly rather
  than per-read (`threaded.rs:226-234`, `render.rs:22`).

## 3. Code quality

Unusually disciplined for a hobby-scale emulator:

- **Bounded everything in the core:** scrollback (`SCROLLBACK_MAX=10k`),
  interned links (`LINK_MAX=4096`), grapheme clusters (`CLUSTER_MAX=8192`),
  prompt marks (1024), OSC buffer (4 KiB), DCS/APC buffers (4 MiB). Tables are
  append-only so ids stay stable for cells held in scrollback.
- **Memory-safe hand-rolled decoders.** All traced:
  - `inflate.rs` bounds output at `max_out`, validates LEN/NLEN, rejects
    out-of-range distance/length symbols, and decodes Huffman through
    `symbols.get(..).copied()` (no panic on a bad table).
  - `png.rs` uses `checked_mul`/`checked_add` for stride/size, caps at
    `MAX_PIXELS=4M`, and every scanline access is provably within the verified
    `expected` length (`png.rs:90-104`).
  - `kitty.rs` caps payload (8 MiB) and decoded pixels (16 MiB); `raw_pixels`
    only reaches `Vec::with_capacity(count)` after `raw.len() >= count*ch`, so
    capacity is bounded by the (capped) input (`kitty.rs:150-154`).
- **Pragmatic, well-cited VT coverage:** clean VT500-style state machine
  (`parser.rs:24-63`); C0 controls execute in-place inside CSI preserving params
  (`parser.rs:324`); cursor motion is scroll-region-aware (`grid.rs:300-322`);
  `set_cell` repairs orphaned wide-glyph halves (`grid.rs:260-276`);
  IRM/DECOM/DECAWM/alt-screen semantics are correct.
- **Comments explain *why*, not *what*** — and they are load-bearing (the
  EIO-as-EOF rationale, the `1049`-only cursor-save rule, the deferred-wrap
  approximation).
- **Clean static analysis:** default build compiles, **231 tests pass** (228
  pre-existing + 3 new adversarial), clippy = **4 style nits** (collapsible
  `if`/`match`, one counter loop in `osc.rs`, two in `parser.rs`).

## 4. Correctness & robustness findings

Ranked by severity. All parse **untrusted child output** and all run under the
held `Grid` mutex inside `parser.advance()`, so any hang freezes the *entire* UI
(render + input cannot acquire the lock).

> **Status (2026-06-03):** the three DoS clamps below are now **fixed** and
> covered by adversarial tests (`sixel_huge_repeat_count_is_bounded`,
> `rep_huge_count_is_bounded_to_capacity`,
> `su_huge_count_clears_region_without_flooding_scrollback`). Suite: **231
> passing**. The findings are retained for the record.

### HIGH — Sixel `!` repeat is an unbounded loop (denial-of-service)

`src/core/sixel.rs:95-98` reads the run-length via `parse_num`, which
**saturates to `usize::MAX`** on absurd input (`sixel.rs:184-194`). The
data-byte handler then does `for _ in 0..repeat.max(1) { ...; self.x += 1; }`
(`sixel.rs:81-90`). `set_pixel` early-returns once `x >= MAX_DIM` (2000), but the
loop body keeps running for the full count. A ~25-byte payload —
`DCS q !99999999999999999999 ~ ST` — drives a loop of ~1.8x10^19 iterations: an
effectively permanent hang of the whole terminal.

**Fix (applied):** `repeat` is clamped to `MAX_DIM` (the column ceiling) where it
is read in the `!` handler (`sixel.rs`).

### MEDIUM — SU (`CSI Pn S`) loops `n` times with no clamp

`scroll_up_n` is `for _ in 0..n { scroll_up() }` (`grid.rs:913-916`), and `n` is
taken straight from the parsed parameter (`parser.rs:684`). `CSI 9999999999 S`
=> ~10^10 full-screen memmoves + scrollback pushes — a multi-minute freeze. Note
the asymmetry: `scroll_down_n` (`grid.rs:927`), `insert_lines` (`grid.rs:852`),
and `delete_lines` (`grid.rs:882`) all correctly clamp `n` to the region height;
only SU was left as a per-row loop (to reuse `scroll_up`'s scrollback capture).

**Fix (applied):** `n` is clamped to the region height in `scroll_up_n`
(`grid.rs`); over-scrolling clears the region and pushes only the region's rows
to scrollback.

### MEDIUM — REP (`CSI Pn b`) loops `count` times

`parser.rs:664-671` does `for _ in 0..count { g.put_char(ch, pen) }` with `count`
from the parameter (up to ~10^10 before `usize::parse` fails). Each call is real
work, but a huge count still hangs under the lock.

**Fix (applied):** `count` is clamped to the addressable capacity
(`(rows + SCROLLBACK_MAX) * cols`, saturating) in the REP handler (`parser.rs`).

*(These three are the same class — unclamped attacker-controlled repeat counts —
and a real terminal like xterm clamps all of them.)*

### LOW / fidelity gaps (by design, but worth knowing)

- **Resize is wrap-aware** *(reflowed 2026-06-03)*. `reflow_history()` rejoins
  soft-wrapped runs across scrollback + screen into logical lines (tracked by a
  per-row `wrapped` bit set on DECAWM autowrap) and re-wraps them to the new
  width, carrying the cursor, OSC-133 prompt marks, and per-row `line_attrs`
  through the reflow without splitting a double-width glyph across the margin.
  Narrowing pushes the overflow into scrollback; widening pulls continuations
  (and history lines) back. The alternate screen is still clipped/extended, since
  its full-screen apps repaint on resize. (Earlier this was a top-left clip that
  truncated long lines and dropped prompt marks + line attrs.)
- **GPU glyph atlas caps at 1024 distinct glyphs** (`SLOTS_PER_ROW^2`,
  `gpu.rs:22,282`); the 1025th+ glyph silently renders as blank (slot 0). Fine
  for ASCII+symbols; a heavy-CJK session would show gaps. Graceful, not a crash.
- **`FontCache` glyph cache is unbounded** (`font.rs:51`) — bounded in practice
  by distinct scalars emitted; glyphs are small.
- **`split_input` can eat a literal scroll-key byte sequence** appearing inside
  pasted/forwarded data (`input.rs:38`) — acknowledged in the comment; very
  unlikely in raw mode.
- **DEFLATE does not validate Huffman-table completeness** and **does not verify
  Adler-32/CRC** (`inflate.rs:62-81`, documented). Memory-safe; a malformed
  table yields wrong pixels or `None`, never UB.

## 5. Documentation accuracy

Docs are extensive and mostly accurate, but two drift points:

- **`backend/windows.rs` header** — *(resolved 2026-06-03)* previously said the
  ConPTY path was *"type-checked ... but not run"*, contradicting `README.md:22-30`
  and the status doc (run & verified on Windows 11, build 26200). The header now
  matches the verified status and notes the resize-propagation gap.
- The status doc and README are otherwise faithful: the relay-vs-native-encoding
  split, the documented gaps (in-window mouse reporting, OSC 52 query, IME,
  DECCKM-in-`gui`, Windows resize propagation) all match the code — e.g. `gui`
  key encoding is called with `app_cursor=false` hard-coded (`window.rs:279`),
  exactly as documented.

The project is honest about its scope: gaps are labeled gaps, not stubs dressed
as features.

## 6. Test coverage

**257 `#[test]` functions** (226 in `core/tests.rs`), plus per-module suites in
`cpu.rs` (8), `input.rs` / `gui/input.rs` / `gui/window.rs` / `term.rs` (4-5
each), `threaded.rs` (FrameSignal), `gpu.rs`, `font.rs`. Coverage skews correctly
toward behavior that can break: cursor/scroll-region edge cases, charset
toggling, grapheme clustering, double-width lines, alt-screen cursor semantics,
key-encoding tables, paste-injection guarding (`encode_paste` strips embedded
`ESC[201~`, `window.rs:326-337`), the `FrameSignal` condvar, and headless GPU
pipeline/atlas creation. The live window and real-Vulkan submit cannot run
headless and are documented as such.

**Gap (now closed for the repeat-count class):** the three pathological
Sixel/REP/SU repeat counts are now covered by adversarial tests (see section 4).
The other hand-rolled decoders (`inflate`/`png`/`base64`) still have no
coverage-guided fuzzing, though they were traced by hand and shown memory-safe.

## 7. Verdict

A high-quality, tastefully engineered codebase: clean layering, careful
concurrency, genuinely from-scratch and memory-safe binary decoders, honest
docs, and a strong behavior-focused test suite. The one substantive weakness —
a family of **unclamped repeat counts in untrusted-input paths** (Sixel `!`, SU,
REP) that let a tiny malicious byte sequence hang the whole emulator under the
grid lock — has been **fixed and regression-tested** (section 4). The remaining
items are intentional fidelity simplifications (cell-resolution images,
1024-glyph atlas) appropriate to a passthrough terminal.

### Follow-ups

1. ~~Clamp `repeat` in `sixel.rs` (HIGH).~~ **Done.**
2. ~~Clamp `n` in `scroll_up_n` (`grid.rs`) (MEDIUM).~~ **Done.**
3. ~~Clamp `count` for REP in `parser.rs` (MEDIUM).~~ **Done.**
4. ~~Add adversarial tests covering the pathological repeat counts.~~ **Done.**
5. ~~Refresh the stale `backend/windows.rs` header comment.~~ **Done.**
6. *(Open)* Coverage-guided fuzzing of `inflate`/`png`/`base64`/`kitty` decoders.
7. ~~Wrap-aware reflow on resize; preserve prompt marks and line attrs.~~ **Done.**
