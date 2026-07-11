mod backend;
mod config;
mod core;
#[cfg(feature = "gui")]
mod gui;
mod input;
mod keymap;
mod render;
mod runtime;
mod shells;
mod term;

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::backend::Backend;
#[cfg(unix)]
use crate::backend::UnixBackend;
#[cfg(windows)]
use crate::backend::WindowsBackend;
use crate::config::{Config, LaunchMode};
use crate::core::Grid;

/// Splits CLI args at the first `--`, `-e`, or `--command` token: everything
/// before is rusty_term's own flags; everything after is the child command to
/// run (naner's `CustomShell.ExecutablePath` + `Arguments`) — `[program,
/// args...]`. Keeping the child's tokens out of the "our flags" scan means a
/// child argument that happens to read like one of our flags (even literally
/// `--config`) is never misread as ours.
fn split_command(args: &[String]) -> (&[String], Option<(String, Vec<String>)>) {
    match args.iter().position(|a| a == "--" || a == "-e" || a == "--command") {
        Some(pos) => {
            let rest = &args[pos + 1..];
            let command = rest.split_first().map(|(p, a)| (p.clone(), a.to_vec()));
            (&args[..pos], command)
        }
        None => (args, None),
    }
}

/// A `--flag value` or `--flag=value` CLI argument's value, first match wins.
fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let eq_prefix = format!("{name}=");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().map(String::as_str);
        }
        if let Some(v) = a.strip_prefix(eq_prefix.as_str()) {
            return Some(v);
        }
    }
    None
}

fn main() -> Result<(), std::io::Error> {
    // Load the config file (`--config <path>` > `$RUSTY_TERM_CONFIG` > the
    // platform default location). Warnings (unknown keys, bad values, an
    // unreadable explicit path) go to stderr — visible with the TUI not yet
    // drawing and harmless behind a detached gui window — and never abort:
    // the terminal always starts, with defaults filling any gap.
    let all_args: Vec<String> = std::env::args().skip(1).collect();
    let (args, command) = split_command(&all_args);

    // `--list-shells`: print what's installed and exit. Runs before config
    // loading so a broken config can't get in the way of the diagnostic.
    if args.iter().any(|a| a == "--list-shells") {
        shells::print_detected();
        return Ok(());
    }

    let (mut config, warnings) = Config::load(args);
    for w in &warnings {
        eprintln!("rusty_term: {w}");
    }

    // CLI-only overrides layered on top of the config file: `--cwd` /
    // `--starting-directory` (G1), `--title` (G3), `--maximized` /
    // `--fullscreen` (G6, fullscreen taking priority if both are given), and
    // the trailing `-- prog arg...` / `-e` / `--command` child override (G2)
    // — which replaces `shell` outright rather than merely supplying args, so
    // it wins over both the config file and shell auto-detection below.
    if let Some(dir) = flag_value(args, "--cwd").or_else(|| flag_value(args, "--starting-directory"))
    {
        config.cwd = Some(PathBuf::from(dir));
    }
    if let Some(title) = flag_value(args, "--title") {
        config.title = Some(title.to_string());
    }
    if args.iter().any(|a| a == "--fullscreen") {
        config.launch_mode = Some(LaunchMode::Fullscreen);
    } else if args.iter().any(|a| a == "--maximized") {
        config.launch_mode = Some(LaunchMode::Maximized);
    }
    if let Some(v) = flag_value(args, "--opacity").and_then(|v| v.parse::<f32>().ok()) {
        config.opacity = Some(v.clamp(0.0, 1.0));
    }
    // `--profile <name>`: layer the named `[profile.<name>]` bundle onto the
    // top-level config, so both front-ends inherit its shell/cwd/theme.
    if let Some(name) = flag_value(args, "--profile") {
        match config.profile(name).cloned() {
            Some(p) => {
                if p.shell.is_some() {
                    config.shell = p.shell;
                }
                if p.cwd.is_some() {
                    config.cwd = p.cwd;
                }
                if let Some(t) = p.theme {
                    config.theme = t;
                }
            }
            None => eprintln!("rusty_term: no profile named `{name}` in the config"),
        }
    }
    if let Some(path) = flag_value(args, "--session") {
        config.session = Some(PathBuf::from(path));
    }
    if let Some((prog, cmd_args)) = command {
        config.shell = Some(prog);
        config.command_args = cmd_args;
    }

    // No shell configured: ask the detector for a better default than the
    // backend's last resort. On Windows that upgrades cmd.exe to PowerShell
    // when one is installed; on Unix it stays `None` ($SHELL already wins).
    // An explicitly set $COMSPEC is honored over the probe — someone who set
    // it chose their shell, same as $SHELL on Unix.
    if config.shell.is_none() {
        #[cfg(windows)]
        let env_choice = std::env::var("COMSPEC")
            .map(|c| !c.to_ascii_lowercase().ends_with("cmd.exe"))
            .unwrap_or(false);
        #[cfg(not(windows))]
        let env_choice = false;
        if !env_choice {
            config.shell = shells::detect_default();
        }
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

    // A session file describes tabs, which only the windowed front-end has.
    if config.session.is_some() {
        eprintln!("rusty_term: `session` requires the windowed front-end (--gui); ignored");
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
    grid.set_default_cursor(
        config.cursor_style.unwrap_or_default(),
        config.cursor_blink.unwrap_or(false),
    );
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
