# rusty_term — Code Review & Improvement Analysis

Scope: full source tree at commit `78a5a92` (~19k lines). Method: subsystem-by-subsystem
review (core emulation, image codecs, GUI, platform/infra), with every high-severity
finding empirically verified against the actual code (several by running targeted tests).
Baseline health: all 308 tests pass; clippy is clean on `gui` and `gui-gpu` and shows only
8 dead-code warnings on the default build.

Overall assessment: this is a carefully written codebase. Saturating arithmetic, size
caps on untrusted input, RAII raw-mode restoration, and sound `unsafe` in both pty
backends are pervasive, and the comments explain intent unusually well. The findings
below are mostly the gaps that pattern-based discipline missed — a handful of real
panics reachable from untrusted child output, two protocol-conformance bugs that affect
real programs, and structural debt in the GUI layer.

---

## Status (resolved vs. deferred)

**Fixed**, each with a regression test where testable, verified against the default
build, `--features gui`/`gui-gpu`, and the `x86_64-pc-windows-gnu` target:

- All five high-severity bugs (H1–H3 here, plus the bracketed-paste sanitizer bypass
  and the scrolled-history selection mismatch).
- Core protocol/robustness: colon-form (ISO 8613-6) SGR, CSI parameter-buffer cap,
  `CSI 3 J` scrollback erase, JPEG SOS-vs-SOF validation, Kitty over-cap transmission
  reporting failure instead of rendering a truncated buffer.
- Runtime/backend (Unix): `ShutdownOnDrop` guard so a task panic can't wedge the
  terminal in raw mode; shell spawned before the runtime starts (single-threaded
  fork); failed `execvp` now reports to the terminal and exits 127; `FD_CLOEXEC` on
  the PTY master and dups.
- Config: warn on an unreadable default-path config; gui-only write-back code gated
  (default build is now warning-clean); atomic settings write (temp + rename).
- Windows backend: HRESULT→`io::Error` mapping, pipe-leak on the second `CreatePipe`
  failure, stale resize comment.
- GUI: glyph atlas recycles when full instead of rendering blank forever; settings
  font-size step rounds a fractional seed; shared search-highlight constants.

**Deferred** (need visual verification or a large refactor, tracked below): the
dirty-tracking GUI repaint optimization (M1), the `window.rs` split (ST1), the
CPU/GPU per-cell resolution dedup beyond the shared constants (ST2), and the minor
cosmetic GUI items (wide chrome-glyph clipping, tiny-window row math). The `l13`
resize-write path (M4) is gated behind a feature that can't build without the private
`rusty_lsp` sibling crate, so it's left for when that dependency is resolved (B1).

---

## 1. Build & packaging

### B1 (high) — A fresh clone cannot build at all
`Cargo.toml:36` declares `rusty_lsp = { path = "../rusty_lsp", optional = true }`.
Cargo resolves path dependencies' manifests unconditionally, even when the feature is
off, so `cargo build` on a standalone checkout fails with *"failed to load source for
dependency rusty_lsp"*. Verified in this environment (the sibling crate is absent).

**Fix:** make `rusty_lsp` a workspace member that ships with the repo, vendor the small
set of types `core/channel.rs` needs, or move to a git/registry dependency. At minimum,
document the sibling-checkout requirement in the README.

### B2 (medium) — The `l13` feature has no build guard and can silently rot
Nothing in the default build or tests touches the `#[cfg(feature = "l13")]` code in
`channel.rs`, `grid.rs`, `parser.rs`, `osc.rs`, and `tokio_rt.rs`. Without a CI job
building `--features l13` (against a real `rusty_lsp`), any drift goes unnoticed.

### B3 (low) — Dead-code warnings on the default build
`config.rs` (`template`, `open_in_editor`, `save_settings`, `toml_string`,
`SettingEdit`, `upsert`, `header_name`) are gui-only but not gated, producing 8
warnings on `cargo build`. Gate them with `#[cfg(feature = "gui")]`.

---

## 2. High-severity bugs (all verified)

### H1 — JPEG: crafted scan header panics the terminal
`src/core/jpeg.rs:370-371` stores the SOS Huffman-table selectors unclamped
(`td = tdta >> 4`, `ta = tdta & 0x0F`, each 0..=15); `decode_scan` then indexes them
into `[Huff; 4]` at `jpeg.rs:425-426`. A JPEG with `tdta = 0xFF` in the scan header —
deliverable by any program writing an iTerm2 inline image to the pty — panics the
process. Notably, the quantization-table index a few lines up *is* masked (`& 3`);
only td/ta were missed.

**Fix:** in `parse_sos`, `return None` when `(tdta >> 4) > 3 || (tdta & 0x0F) > 3`.

### H2 — Grid: wide glyph in a 1-column grid panics
`src/core/grid.rs:1144` (`left_base`): after a width-2 glyph wraps in a `cols == 1`
grid, `cursor.0` reaches 2 and the next `put_char` indexes `cells[cy*cols + (cx-1)]`
out of the row (and potentially off the end of the buffer). Verified:
`Grid::new(1, 3)` + `"世界世界"` panics. A 1-column window is reachable via resize.

**Fix:** guard the autowrap-of-wide-glyph branch so `cursor.0` never exceeds `cols`
(drop/replace the glyph when `width > cols`), and bound-check `left < self.cols` in
`left_base`.

### H3 — Parser: overlong UTF-8 decodes to real control bytes (escape injection)
`src/core/parser.rs:212-229, 559-606`: the UTF-8 decoder accepts any sequence whose
accumulated scalar passes `char::from_u32`, never rejecting overlong encodings.
Verified: `E0 80 9B` (overlong ESC) decodes to a real `U+001B` stored in `cell.ch`,
which the TUI renderer then emits verbatim to the host terminal (`render.rs:149`) —
an escape-sequence injection vector from untrusted child output. (Surrogates are
correctly rejected; only overlong forms slip through.)

**Fix:** after assembling the scalar, reject values below the minimum for the byte
length (`< 0x80` for 2-byte, `< 0x800` for 3-byte, `< 0x10000` for 4-byte) and emit
`U+FFFD