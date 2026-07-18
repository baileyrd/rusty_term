//! Pure in-process throughput benchmark for rusty_term's parser+grid layer.
//!
//! Feeds pre-generated VT/ANSI byte-stream workloads (see
//! `bench/gen_workloads.py`) through `AnsiParser::advance` in realistic
//! PTY-read-sized chunks and reports MB/s. No process spawn, no PTY, no
//! display — this is the rusty_term-only half of the benchmark harness in
//! `bench/`, useful as a fast regression signal in CI. The other half
//! (`bench/run_bench.py`) drives rusty_term and other terminal emulators as
//! black-box processes for cross-terminal comparison.
//!
//! Usage:
//!   python3 bench/gen_workloads.py
//!   cargo run --release --bin bench_vt_throughput               # all workloads, 20 iterations each
//!   cargo run --release --bin bench_vt_throughput -- 100         # 100 iterations each
//!   cargo run --release --bin bench_vt_throughput -- 50 bench/workloads/sgr_churn.vt

use std::env;
use std::fs;
use std::time::Instant;

use rusty_term::core::{AnsiParser, Grid};

const CHUNK: usize = 4096; // typical PTY read() size
const COLS: usize = 120;
const ROWS: usize = 40;
const DEFAULT_ITERATIONS: u32 = 20;
const DEFAULT_WORKLOAD_DIR: &str = "bench/workloads";

fn feed(data: &[u8], iterations: u32) {
    for _ in 0..iterations {
        let mut grid = Grid::new(COLS, ROWS);
        let mut parser = AnsiParser::new();
        for chunk in data.chunks(CHUNK) {
            parser.advance(&mut grid, chunk);
            parser.take_responses();
            grid.take_host_out();
        }
        std::hint::black_box(&grid);
    }
}

fn bench_one(path: &str, iterations: u32) {
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skip {path}: {e}");
            return;
        }
    };
    if data.is_empty() {
        eprintln!("skip {path}: empty file");
        return;
    }

    feed(&data, 1); // warmup

    let start = Instant::now();
    feed(&data, iterations);
    let elapsed = start.elapsed();

    let total_bytes = data.len() as u64 * iterations as u64;
    let secs = elapsed.as_secs_f64();
    let mb_per_s = (total_bytes as f64 / 1_000_000.0) / secs.max(1e-9);
    let name = std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(path);

    println!(
        "{name:22} {:>10} bytes  x{iterations:<5} {:>9.1} ms total  {:>9.1} MB/s",
        data.len(),
        elapsed.as_secs_f64() * 1000.0,
        mb_per_s,
    );
}

fn discover_workloads(dir: &str) -> Vec<String> {
    let mut found: Vec<String> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("vt"))
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    found.sort();
    found
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    let (iterations, rest): (u32, &[String]) =
        match args.first().and_then(|s| s.parse::<u32>().ok()) {
            Some(n) => (n, &args[1..]),
            None => (DEFAULT_ITERATIONS, &args[..]),
        };

    let paths: Vec<String> = if rest.is_empty() {
        let found = discover_workloads(DEFAULT_WORKLOAD_DIR);
        if found.is_empty() {
            eprintln!(
                "no workload files given and `{DEFAULT_WORKLOAD_DIR}` has none — run \
                 `python3 bench/gen_workloads.py` first, or pass file paths explicitly."
            );
            std::process::exit(1);
        }
        found
    } else {
        rest.to_vec()
    };

    println!(
        "--- rusty_term parser+grid throughput ({COLS}x{ROWS} grid, {CHUNK}B chunks, x{iterations} each) ---"
    );
    for path in &paths {
        bench_one(path, iterations);
    }
}
