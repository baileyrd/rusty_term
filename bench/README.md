# Benchmark harness

Two complementary pieces, both driven off the same generated workloads:

| | What it measures | Needs other terminals installed? | Needs a display? |
|---|---|:---:|:---:|
| `cargo run --release --bin bench_vt_throughput` | rusty_term's own parser+grid throughput (MB/s) | no | no |
| `python3 run_bench.py` | end-to-end wall-clock time across rusty_term *and* other terminal emulators | no (skips what's missing) | only for windowed terminals |

Start here:

```sh
python3 bench/gen_workloads.py                 # write bench/workloads/*.vt (gitignored, regenerate any time)
cargo build --release                            # for the Rust-only bench
cargo run --release --bin bench_vt_throughput    # rusty_term parser+grid throughput, no process spawn

cargo build --release                            # rusty_term needs to be built for run_bench.py to find it too
python3 bench/run_bench.py --list                # see what's installed and would run
python3 bench/run_bench.py                       # run everything available, write results.json + results.md
```

## Workloads (`gen_workloads.py`)

Each workload is a deterministic (seeded), self-contained VT/ANSI byte
stream targeting a different part of the terminal pipeline:

- **`ascii_throughput`** — plain-text scroll (the `cat a-log-file` case).
- **`unicode_heavy`** — wide CJK, combining marks, emoji: width/grapheme handling.
- **`sgr_churn`** — every line changes fg/bg/bold/italic/underline: style-run pressure.
- **`cursor_thrash`** — scattered absolute cursor positioning, like a redraw-storm TUI app (vim, htop, tmux).
- **`scroll_heavy`** — many short lines: continuous line-at-a-time scroll.
- **`alt_screen_flicker`** — repeated alt-screen enter/clear/exit: full-screen apps flipping in and out (less, fzf, editors).

`--size {quick,default,large}` controls file size (200 KB / 4 MB / 16 MB);
`--only NAME [NAME...]` regenerates a subset. Regeneration is deterministic
— same size, same bytes — so numbers are comparable across runs as long as
you don't change the size. Generated files aren't committed (see
`.gitignore`); the generator is the source of truth.

## `bench_vt_throughput` (Rust, rusty_term-only)

Feeds each `bench/workloads/*.vt` file through `AnsiParser::advance` +
`Grid` directly, in 4 KB chunks (a realistic PTY `read()` size), with one
untimed warmup pass per file. No process spawn, no PTY, no display — just
the parser and grid doing their job. This is the piece that's actually safe
to run in CI as a regression smoke test (see the `bench-smoke` CI job),
since it has no external dependencies and nothing to skip.

```sh
cargo run --release --bin bench_vt_throughput                 # all workloads, 20 iterations each
cargo run --release --bin bench_vt_throughput -- 200          # 200 iterations each
cargo run --release --bin bench_vt_throughput -- 50 bench/workloads/sgr_churn.vt
```

## `run_bench.py` (cross-terminal)

Drives rusty_term and any other terminal emulators listed in
`terminals.json` — ghostty, alacritty, kitty, wezterm, foot, xterm, konsole,
plus a `cat`-only "floor" baseline with zero VT parsing — as black-box
processes, timing each one running `cat <workload>; exit` inside a shell,
start to exit.

```sh
python3 bench/run_bench.py                                        # everything installed, 5 timed + 1 warmup iteration each
python3 bench/run_bench.py -t rusty_term -t ghostty -t alacritty  # just these
python3 bench/run_bench.py -w ascii_throughput -w cursor_thrash   # just these workloads
python3 bench/run_bench.py --iterations 10 --warmup 3 --timeout 60
python3 bench/run_bench.py --list                                  # dry run: show resolved/skipped terminals only
```

Terminals with a missing binary, or a windowed terminal with no display and
no `xvfb-run` on PATH, are **skipped, not failed** — the run continues and
the report notes what didn't run. Nothing here requires having every
terminal installed; install whichever ones you want compared.

### Methodology (read before trusting the numbers)

For each (terminal, workload) pair, the harness launches the terminal
fresh — pointed at a shell running `cat <workload file>; exit` — and times
the whole process from spawn to exit, the same approach tools like
`hyperfine` use. `--warmup` iterations run first and are discarded, so
window-creation/font-loading/first-run cache effects don't dominate the
timed iterations.

What this actually measures is **how long it takes the terminal to read and
keep up with a burst of VT output too large for the kernel's PTY buffer
(tens of KB) to just absorb** — i.e., read+parse+redraw throughput under
backpressure. That's a solid proxy for "does this terminal keep up with a
noisy build / a busy TUI app / `cat`-ing a big file", but it is **not** a
measurement of true pixel-level frame latency: a renderer can report its
child as exited slightly before the very last frame actually lands on
screen (double-buffered/vsync-throttled paint loops), and two terminals
finishing in the same time didn't necessarily look identical while getting
there. Treat this as a throughput benchmark, not a frame-latency one.

Two run modes, chosen per terminal in `terminals.json`:

- **`mode: "pty"`** — the harness allocates its own PTY and runs the
  terminal attached to it as the controlling terminal, draining the master
  side continuously in a background thread. rusty_term's TUI/passthrough
  mode needs this: it relays to its own stdout rather than opening a
  window, so it calls `tcgetattr`/raw-mode setup on stdin, which requires a
  real controlling tty.
- **`mode: "subprocess"`** — a plain child process. Used for windowed
  terminals (including `rusty_term --gui`), which open their own display
  connection and don't care about inherited stdio.
- **`mode: "direct"`** — no shell wrapper at all; used only for the
  `cat-baseline` reference entry.

### Headless / CI environments

Windowed terminals need a display. If `$DISPLAY` and `$WAYLAND_DISPLAY` are
both unset, the harness automatically wraps X11 terminals in `xvfb-run -a`
when it's on PATH (`sudo apt-get install xvfb`); otherwise they're skipped
with a clear reason. Wayland-only terminals (`foot`) can't be run under
Xvfb at all — they need a real (or nested, e.g. `sway --unsupported-gpu`)
Wayland compositor. rusty_term's TUI/passthrough mode needs neither, since
it never opens a window.

### Adding a terminal

Add an entry to `terminals.json`: `id`, `label`, `binary` (looked up via
`PATH`), `mode`, `args` (with a literal `"{cmd}"` element marking where the
shell command goes — omit for `mode: "direct"`), and `needs_display`. A few
gotchas already documented inline there:

- **Konsole** forks to a background instance and returns immediately unless
  you pass `--nofork` — without it, the harness would time "how long it
  takes to fork", not the actual run.
- **gnome-terminal** is D-Bus-activated (single-instance server + client)
  and can't be timed as a plain child process at all — not included.
- Check `<terminal> --help` for the exact "run this command and exit
  when it's done" flag; it varies (`-e`, `start --`, or no flag at all).

### Output

`--out` (default `bench/results.json`) gets the raw per-iteration samples
plus median/mean/stdev/MB⁄s for every terminal × workload pair that ran.
`--report` (default `bench/results.md`) gets a human-readable Markdown
table plus a per-terminal MB/s summary. Both are gitignored — generate them
fresh for each comparison you care about, and note in your write-up which
terminal *versions* you tested (`ghostty --version`, etc.), since none of
that is captured automatically.
