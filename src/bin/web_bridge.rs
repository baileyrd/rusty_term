//! `rusty_term_web_bridge` — the WebSocket PTY bridge behind the Nebula web
//! frontend (`web/`). Thin CLI over [`rusty_term::web_bridge`]; build with
//! `cargo build --features web-bridge`.

use rusty_term::config::flag_value;
use rusty_term::web_bridge::{BridgeConfig, DEFAULT_LISTEN, run};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "rusty_term_web_bridge — WebSocket PTY bridge for the web frontend\n\n\
             USAGE:\n    rusty_term_web_bridge [--listen ADDR:PORT] [--shell CMD]\n\n\
             OPTIONS:\n    \
             --listen ADDR:PORT   bind address (default {DEFAULT_LISTEN}; loopback only —\n                         \
             front it with an authenticating proxy to expose it further)\n    \
             --shell CMD          shell per session (default $SHELL / %COMSPEC%)\n    \
             -h, --help           print this help\n\n\
             Serve the web UI (cd web && npm run dev), then open it with\n\
             ?ws=ws://{DEFAULT_LISTEN} to attach it to this bridge."
        );
        return;
    }
    // Refuse unknown flags rather than silently ignoring a typo'd --listne.
    let known = ["--listen", "--shell"];
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if known.contains(&a.as_str()) {
            it.next(); // its value
        } else if a.starts_with("--") && !known.iter().any(|k| a.starts_with(&format!("{k}="))) {
            eprintln!("rusty_term_web_bridge: unknown flag `{a}` (try --help)");
            std::process::exit(64); // EX_USAGE
        }
    }
    let cfg = BridgeConfig {
        listen: flag_value(&args, "--listen")
            .unwrap_or(DEFAULT_LISTEN)
            .to_string(),
        shell: flag_value(&args, "--shell").map(str::to_string),
    };
    if let Err(e) = run(cfg) {
        eprintln!("rusty_term_web_bridge: {e}");
        std::process::exit(1);
    }
}
