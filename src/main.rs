mod backend;
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
use crate::core::Grid;

fn main() -> Result<(), std::io::Error> {
    #[cfg(unix)]
    let backend: Box<dyn Backend> = Box::new(UnixBackend);
    #[cfg(windows)]
    let backend: Box<dyn Backend> = Box::new(WindowsBackend);

    // `--gui` launches the native window backend (the tcore-app fork) instead of
    // rendering into the host terminal. Requires the `gui` feature.
    #[cfg(feature = "gui")]
    if std::env::args().any(|a| a == "--gui") {
        return gui::run(backend.as_ref()).map_err(|e| std::io::Error::other(e.to_string()));
    }

    // Start at the host terminal's actual size, falling back to 80x24.
    let (init_cols, init_rows) = backend.terminal_size().unwrap_or((80, 24));
    let grid = Arc::new(Mutex::new(Grid::new(
        init_cols as usize,
        init_rows as usize,
    )));

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

    // Hand off to the selected runtime (threaded by default, tokio behind the
    // `tokio-runtime` feature). Both drive the same grid and backend.
    runtime::run(backend, grid, init_cols, init_rows)
}
