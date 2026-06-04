mod backend;
mod config;
mod core;
#[cfg(feature = "gui")]
mod gui;
mod input;
mod render;
mod runtime;
mod term;

use std::sync::Arc;

use parking_lot::Mutex;

use crate::backend::Backend;
#[cfg(unix)]
use crate::backend::UnixBackend;
#[cfg(windows)]
use crate::backend::WindowsBackend;
use crate::config::Config;
use crate::core::Grid;

fn main() -> Result<(), std::io::Error> {
    // Load the config file (`--config <path>` > `$RUSTY_TERM_CONFIG` > the
    // platform default location). Warnings (unknown keys, bad values, an
    // unreadable explicit path) go to stderr — visible with the TUI not yet
    // drawing and harmless behind a detached gui window — and never abort:
    // the terminal always starts, with defaults filling any gap.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (config, warnings) = Config::load(&args);
    for w in &warnings {
        eprintln!("rusty_term: {w}");
    }

    #[cfg(unix)]
    let backend: Box<dyn Backend> = Box::new(UnixBackend);
    #[cfg(windows)]
    let backend: Box<dyn Backend> = Box::new(WindowsBackend);

    // `--gui` launches the native window backend (the tcore-app fork) instead of
    // rendering into the host terminal. Requires the `gui` feature.
    #[cfg(feature = "gui")]
    if args.iter().any(|a| a == "--gui") {
        return gui::run(backend.as_ref(), &config)
            .map_err(|e| std::io::Error::other(e.to_string()));
    }

    // Start at the host terminal's actual size, falling back to 80x24. The
    // configured cols/rows are a *window* size; the TUI renders into the host
    // terminal, whose size is not ours to choose.
    let (init_cols, init_rows) = backend.terminal_size().unwrap_or((80, 24));
    let mut grid = Grid::new(init_cols as usize, init_rows as usize);
    if let Some(max) = config.scrollback {
        grid.set_scrollback_max(max);
    }
    grid.apply_theme(&config.theme);
    let grid = Arc::new(Mutex::new(grid));

    // The child renders through rusty_term, not the host terminal, so it should
    // see *our* identity and capabilities. We advertise our self-describing
    // `rusty_term` terminfo entry when it's installed, else the near-universal
    // `xterm-256color` (the repertoire we implement), plus truecolor (the
    // renderer always emits 24-bit SGR). Set before spawning so the child
    // inherits it. We're still single-threaded here, so this process-wide env
    // mutation races with nothing.
    unsafe {
        std::env::set_var("TERM", term::resolve_term());
        std::env::set_var("COLORTERM", "truecolor");
    }

    // Hand off to the tokio runtime — a single async reactor driving the grid
    // and backend (Unix via AsyncFd, Windows by bridging ConPTY's blocking pipes).
    runtime::run(backend, grid, init_cols, init_rows, config)
}
