#!/usr/bin/env python3
"""Generates deterministic VT/ANSI byte-stream workloads used by both halves
of the benchmark harness:

- `bench/run_bench.py` cats each file into a shell running inside a real
  terminal emulator (rusty_term, ghostty, alacritty, ...) and times it.
- `cargo run --release --bin bench_vt_throughput` feeds the same files
  straight through rusty_term's own parser+grid, no terminal/process
  involved, for a rusty_term-only regression signal.

Each workload is a self-contained byte stream (its own clear-screen prologue)
so files can be `cat`-ed independently and in any order. Generation is seeded
so re-running this script reproduces byte-identical files.

Usage:
    python3 bench/gen_workloads.py                  # default size, all workloads
    python3 bench/gen_workloads.py --size quick      # smaller files, fast iteration
    python3 bench/gen_workloads.py --only sgr_churn cursor_thrash
"""

import argparse
import os
import random

ESC = b"\x1b"
CSI = ESC + b"["


def sgr(*codes):
    return CSI + b";".join(str(c).encode() for c in codes) + b"m"


def cup(row, col):
    return CSI + f"{row};{col}H".encode()


WORDS = (
    "the quick brown fox jumps over the lazy dog while packets stream "
    "through the pty buffer and the parser walks the byte stream one "
    "codepoint at a time before the grid repaints the affected cells"
).split()


def gen_ascii_throughput(target_bytes, seed):
    """Plain-text scroll: the common case (`cat`ing a log, build output)."""
    rng = random.Random(seed)
    out = bytearray(CSI + b"2J" + CSI + b"H")
    while len(out) < target_bytes:
        line = " ".join(rng.choice(WORDS) for _ in range(rng.randint(6, 14)))
        out += line.encode() + b"\r\n"
    return bytes(out)


def gen_unicode_heavy(target_bytes, seed):
    """Wide CJK, combining marks, and emoji: width/grapheme handling."""
    rng = random.Random(seed)
    pool = (
        list("日本語漢字あいうえおカタカナ")
        + list("한국어테스트문자")
        + list("😀😁😂🤣😃😄😅😆👍🎉")
        + list("éàüñô")
        + list("Ω≈ç√∫˜µ≤≥÷")
    )
    out = bytearray(CSI + b"2J" + CSI + b"H")
    while len(out) < target_bytes:
        line = "".join(rng.choice(pool) for _ in range(rng.randint(20, 60)))
        out += line.encode("utf-8") + b"\r\n"
    return bytes(out)


def gen_sgr_churn(target_bytes, seed):
    """Every line changes fg/bg/bold/italic/underline: style-run pressure."""
    rng = random.Random(seed)
    out = bytearray(CSI + b"2J" + CSI + b"H")
    fgs = list(range(30, 38)) + list(range(90, 98))
    bgs = list(range(40, 48))
    attrs = [1, 3, 4, 7, 9]
    while len(out) < target_bytes:
        out += sgr(0, rng.choice(fgs), rng.choice(bgs), rng.choice(attrs))
        out += " ".join(rng.choice(WORDS) for _ in range(rng.randint(3, 8))).encode()
        out += sgr(0) + b"\r\n"
    return bytes(out)


def gen_cursor_thrash(target_bytes, seed):
    """Scattered absolute cursor positioning: redraw-storm TUI apps (vim,
    htop, tmux) do this constantly."""
    rng = random.Random(seed)
    out = bytearray(CSI + b"2J")
    while len(out) < target_bytes:
        out += cup(rng.randint(1, 45), rng.randint(1, 110))
        out += rng.choice(WORDS).encode()
    return bytes(out)


def gen_scroll_heavy(target_bytes, seed):
    """Many short lines: continuous line-at-a-time scroll."""
    rng = random.Random(seed)
    out = bytearray(CSI + b"2J" + CSI + b"H")
    while len(out) < target_bytes:
        out += f"{rng.randint(0, 999999):>7} ".encode()
        out += rng.choice(WORDS).encode() + b"\r\n"
    return bytes(out)


def gen_alt_screen_flicker(target_bytes, seed):
    """Repeated alt-screen enter/clear/exit: full-screen app churn (less,
    fzf, editors flipping in and out)."""
    rng = random.Random(seed)
    out = bytearray()
    enter_alt = CSI + b"?1049h"
    exit_alt = CSI + b"?1049l"
    while len(out) < target_bytes:
        out += enter_alt + CSI + b"2J" + CSI + b"H"
        for _ in range(rng.randint(5, 20)):
            out += rng.choice(WORDS).encode() + b" "
        out += exit_alt
    return bytes(out)


WORKLOADS = {
    "ascii_throughput": gen_ascii_throughput,
    "unicode_heavy": gen_unicode_heavy,
    "sgr_churn": gen_sgr_churn,
    "cursor_thrash": gen_cursor_thrash,
    "scroll_heavy": gen_scroll_heavy,
    "alt_screen_flicker": gen_alt_screen_flicker,
}

SIZES = {"quick": 200_000, "default": 4_000_000, "large": 16_000_000}
SEED_BASE = 1337


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument(
        "--out-dir",
        default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "workloads"),
        help="directory to write <name>.vt files into (default: bench/workloads)",
    )
    ap.add_argument("--size", choices=sorted(SIZES), default="default", help="target file size per workload")
    ap.add_argument(
        "--only",
        nargs="+",
        choices=sorted(WORKLOADS),
        help="only generate these workloads (default: all)",
    )
    args = ap.parse_args()

    os.makedirs(args.out_dir, exist_ok=True)
    target = SIZES[args.size]
    names = args.only or sorted(WORKLOADS)
    for i, name in enumerate(names):
        data = WORKLOADS[name](target, SEED_BASE + i)
        path = os.path.join(args.out_dir, f"{name}.vt")
        with open(path, "wb") as f:
            f.write(data)
        print(f"{name:20s} {len(data):>10} bytes -> {path}")


if __name__ == "__main__":
    main()
