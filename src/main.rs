use rusty_term::{backend, config, core, runtime, shells, term};
#[cfg(feature = "gui")]
use rusty_term::gui;

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;

use backend::Backend;
#[cfg(unix)]
use backend::UnixBackend;
#[cfg(windows)]
use backend::WindowsBackend;
use config::{Config, LaunchMode, flag_value};
use core::Grid;

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

/// Every flag rusty_term itself understands, paired with whether it consumes
/// a following argument (`--cwd /tmp` is two tokens; `--fullscreen` is one).
/// Used both for `--help`'s listing and for flagging typos below — kept as
/// one list so the two can't drift apart.
const KNOWN_FLAGS: &[(&str, bool)] = &[
    ("--help", false),
    ("-h", false),
    ("--version", false),
    ("--list-shells", false),
    ("--config", true),
    ("--cwd", true),
    ("--starting-directory", true),
    ("--title", true),
    ("--fullscreen", false),
    ("--maximized", false),
    ("--opacity", true),
    ("--profile", true),
    ("--session", true),
    ("--gui", false),
    ("--single-instance", false),
];

fn print_help() {
    println!("rusty_term {}", env!("CARGO_PKG_VERSION"));
    println!("A terminal emulator written from scratch in Rust.");
    println!();
    println!("USAGE:");
    println!("    rusty_term [FLAGS] [-- <command> [args...]]");
    println!("    rusty_term ctl <command> [key=value...]");
    println!();
    println!("FLAGS:");
    println!("    -h, --help                    Print this help message and exit");
    println!("        --version                 Print the version and exit");
    println!("        --list-shells             Print detected shells and exit");
    println!("        --config <path>           Load config from this path");
    println!("        --cwd <dir>                Starting directory for the shell");
    println!("        --starting-directory <dir> Alias for --cwd");
    println!("        --title <title>           Window/tab title override");
    println!("        --fullscreen              Launch fullscreen (--gui only)");
    println!("        --maximized               Launch maximized (--gui only)");
    println!("        --opacity <0.0-1.0>       Window background opacity (--gui only)");
    println!("        --profile <name>          Apply a named [profile.<name>] bundle");
    println!("        --session <path>          Restore a session file (--gui only)");
    println!("        --gui                     Launch the windowed front-end");
    println!("        --single-instance         Hand off to a running --gui instance");
    println!();
    println!("Anything after `--`, `-e`, or `--command` is run as the child command,");
    println!("replacing the configured shell, e.g. `rusty_term -- htop`.");
}

/// Every `--`-prefixed token in `args` that isn't in [`KNOWN_FLAGS`], skipping
/// the value token of flags that consume one. Pulled out as a pure function
/// (rather than inlined into an eprintln loop) so the scan logic — which flag
/// takes a value, `--flag=value` vs. `--flag value` — is unit-testable
/// without capturing stderr.
fn unrecognized_flags(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(name) = a.split('=').next().filter(|_| a.starts_with("--")) {
            match KNOWN_FLAGS.iter().find(|(f, _)| *f == name) {
                Some((_, takes_value)) => {
                    // `--flag=value` already carries its value in this token;
                    // only a separate `--flag value` pair consumes the next one.
                    if *takes_value && !a.contains('=') {
                        i += 1;
                    }
                }
                None => out.push(a.as_str()),
            }
        }
        i += 1;
    }
    out
}

/// Warn (to stderr) about any unrecognized flag. Typos like `--fullscren`
/// would otherwise be silently ignored rather than doing what the user
/// asked.
fn warn_unrecognized_flags(args: &[String]) {
    for a in unrecognized_flags(args) {
        eprintln!("rusty_term: unrecognized flag `{a}` (ignored)");
    }
}

/// Layer every CLI-only override onto the config file's `Config`: `--cwd`/
/// `--starting-directory` (G1, first match wins), `--title` (G3),
/// `--maximized`/`--fullscreen` (G6, fullscreen taking priority if both are
/// given), `--opacity`, `--profile`, `--session`, and the trailing `command`
/// override (`-- prog arg...` / `-e` / `--command`) — which replaces `shell`
/// outright rather than merely supplying args, so it wins over both the
/// config file and shell auto-detection.
///
/// Pulled out as a pure function (rather than inlined into `main`'s
/// eprintln-as-you-go style) so this whole layering pass — order of
/// precedence included — is unit-testable without a real process/config
/// file. Returns any warnings (bad `--opacity`, unknown `--profile`) instead
/// of printing them directly, for the same reason.
fn apply_cli_overrides(
    config: &mut Config,
    args: &[String],
    command: Option<(String, Vec<String>)>,
) -> Vec<String> {
    let mut warnings = Vec::new();
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
    if let Some(raw) = flag_value(args, "--opacity") {
        match raw.parse::<f32>() {
            Ok(v) => config.opacity = Some(v.clamp(0.0, 1.0)),
            Err(_) => warnings.push(format!(
                "--opacity `{raw}` is not a number between 0.0 and 1.0; ignored"
            )),
        }
    }
    // `--profile <name>`: layer the named `[profile.<name>]` bundle onto the
    // top-level config, so both front-ends inherit its shell/cwd/theme.
    // `Config::apply_profile` is the single source of truth for this so a
    // live config reload can reapply the same override (see
    // `runtime::tokio_rt::watch_config` / `gui::window::reload_config`)
    // instead of silently reverting it to the file's top-level defaults.
    if let Some(name) = flag_value(args, "--profile")
        && let Some(w) = config.apply_profile(name)
    {
        warnings.push(w);
    }
    if let Some(path) = flag_value(args, "--session") {
        config.session = Some(PathBuf::from(path));
    }
    if let Some((prog, cmd_args)) = command {
        config.shell = Some(prog);
        config.command_args = cmd_args;
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn all_known_flags_pass_through_unrecognized_flags_cleanly() {
        let args = s(&[
            "--cwd", "/tmp", "--title", "hi", "--fullscreen", "--opacity", "0.5", "--profile",
            "dev", "--session", "/x", "--gui", "--single-instance", "--list-shells", "--help",
            "-h", "--version",
        ]);
        assert!(unrecognized_flags(&args).is_empty());
    }

    #[test]
    fn typo_d_flag_is_reported() {
        let args = s(&["--fullscren"]);
        assert_eq!(unrecognized_flags(&args), vec!["--fullscren"]);
    }

    #[test]
    fn value_token_of_a_known_flag_is_not_mistaken_for_a_flag() {
        // `--cwd` consumes the next token; `--title` looks like a flag but is
        // really the *value* of `--cwd` here, so it must not be misdetected as
        // an unknown flag in its own right nor its own value skipped.
        let args = s(&["--cwd", "--title", "--title", "hi"]);
        assert!(unrecognized_flags(&args).is_empty());
    }

    #[test]
    fn equals_form_does_not_swallow_the_next_token() {
        // `--opacity=0.5` carries its value inline, so the following
        // `--fullscreen` is a separate flag, not a consumed value.
        let args = s(&["--opacity=0.5", "--fullscreen"]);
        assert!(unrecognized_flags(&args).is_empty());
    }

    #[test]
    fn multiple_typos_are_all_reported_in_order() {
        let args = s(&["--fulscreen", "--cwd", "/tmp", "--tittle", "x"]);
        assert_eq!(unrecognized_flags(&args), vec!["--fulscreen", "--tittle"]);
    }

    #[test]
    fn split_command_still_hides_child_args_from_the_flag_scan() {
        // A child command that happens to look like our flags (even
        // literally `--config`) must never reach the "our flags" scan.
        let all = s(&["--fullscreen", "--", "--config", "--bogus"]);
        let (ours, command) = split_command(&all);
        assert!(unrecognized_flags(ours).is_empty());
        assert_eq!(command, Some(("--config".to_string(), s(&["--bogus"]))));
    }

    #[test]
    fn cli_overrides_cwd_title_and_opacity() {
        let args = s(&["--cwd", "/work", "--title", "hi", "--opacity", "0.5"]);
        let mut config = Config::default();
        let warnings = apply_cli_overrides(&mut config, &args, None);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.cwd, Some(PathBuf::from("/work")));
        assert_eq!(config.title.as_deref(), Some("hi"));
        assert_eq!(config.opacity, Some(0.5));
    }

    #[test]
    fn cli_overrides_prefers_cwd_over_starting_directory() {
        let args = s(&["--cwd", "/a", "--starting-directory", "/b"]);
        let mut config = Config::default();
        apply_cli_overrides(&mut config, &args, None);
        assert_eq!(config.cwd, Some(PathBuf::from("/a")));
    }

    #[test]
    fn cli_overrides_falls_back_to_starting_directory_when_cwd_is_absent() {
        let args = s(&["--starting-directory", "/b"]);
        let mut config = Config::default();
        apply_cli_overrides(&mut config, &args, None);
        assert_eq!(config.cwd, Some(PathBuf::from("/b")));
    }

    #[test]
    fn cli_overrides_fullscreen_wins_over_maximized_when_both_given() {
        let args = s(&["--maximized", "--fullscreen"]);
        let mut config = Config::default();
        apply_cli_overrides(&mut config, &args, None);
        assert_eq!(config.launch_mode, Some(LaunchMode::Fullscreen));
    }

    #[test]
    fn cli_overrides_maximized_alone_applies() {
        let args = s(&["--maximized"]);
        let mut config = Config::default();
        apply_cli_overrides(&mut config, &args, None);
        assert_eq!(config.launch_mode, Some(LaunchMode::Maximized));
    }

    #[test]
    fn cli_overrides_invalid_opacity_warns_and_leaves_opacity_unset() {
        let args = s(&["--opacity", "not-a-number"]);
        let mut config = Config::default();
        let warnings = apply_cli_overrides(&mut config, &args, None);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("--opacity"), "{warnings:?}");
        assert_eq!(config.opacity, None);
    }

    #[test]
    fn cli_overrides_opacity_is_clamped_to_the_unit_range() {
        let args = s(&["--opacity", "5.0"]);
        let mut config = Config::default();
        assert!(apply_cli_overrides(&mut config, &args, None).is_empty());
        assert_eq!(config.opacity, Some(1.0));
    }

    #[test]
    fn cli_overrides_unknown_profile_warns_and_leaves_config_untouched() {
        let args = s(&["--profile", "nope"]);
        let mut config = Config::default();
        let warnings = apply_cli_overrides(&mut config, &args, None);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("nope"), "{warnings:?}");
    }

    #[test]
    fn cli_overrides_session_sets_the_session_path() {
        let args = s(&["--session", "/tabs.toml"]);
        let mut config = Config::default();
        apply_cli_overrides(&mut config, &args, None);
        assert_eq!(config.session, Some(PathBuf::from("/tabs.toml")));
    }

    #[test]
    fn cli_overrides_trailing_command_replaces_shell_and_args() {
        let mut config = Config::default();
        config.shell = Some("/bin/bash".to_string());
        let command = Some(("htop".to_string(), s(&["-d", "10"])));
        apply_cli_overrides(&mut config, &[], command);
        assert_eq!(config.shell.as_deref(), Some("htop"));
        assert_eq!(config.command_args, s(&["-d", "10"]));
    }
}

fn main() -> Result<(), std::io::Error> {
    // Load the config file (`--config <path>` > `$RUSTY_TERM_CONFIG` > the
    // platform default location). Warnings (unknown keys, bad values, an
    // unreadable explicit path) go to stderr — visible with the TUI not yet
    // drawing and harmless behind a detached gui window — and never abort:
    // the terminal always starts, with defaults filling any gap.
    let all_args: Vec<String> = std::env::args().skip(1).collect();
    let (args, command) = split_command(&all_args);

    // `--help`/`-h`/`--version`: the usual "answer and exit before touching
    // anything else" diagnostics, ahead of config loading and `ctl` dispatch
    // so they work even with a broken config or no running instance.
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    if args.iter().any(|a| a == "--version") {
        println!("rusty_term {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // `--list-shells`: print what's installed and exit. Runs before config
    // loading so a broken config can't get in the way of the diagnostic.
    if args.iter().any(|a| a == "--list-shells") {
        shells::print_detected();
        return Ok(());
    }

    // `rusty_term ctl <command…>`: talk to a running instance's control
    // socket and print its reply — the whole scripting surface (G33).
    if args.first().map(String::as_str) == Some("ctl") {
        return run_ctl(&args[1..]);
    }

    // Typo'd or unknown flags (`--fullscren`, `--tittle foo`) would otherwise
    // be silently swallowed as no-ops; flag them so the user notices before
    // wondering why the option "didn't work".
    warn_unrecognized_flags(args);

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
    for w in apply_cli_overrides(&mut config, args, command) {
        eprintln!("rusty_term: {w}");
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
        // Single-instance: if a control socket/pipe answers, hand this
        // launch to the running instance as a new tab and exit instead of
        // opening a second window.
        if config.single_instance.unwrap_or(false)
            || args.iter().any(|a| a == "--single-instance")
        {
            let mut req = String::from("new-tab");
            if let Some(cwd) = &config.cwd {
                req.push_str(&format!(" cwd=\"{}\"", cwd.display()));
            }
            if let Some(name) = flag_value(args, "--profile") {
                req.push_str(&format!(" profile=\"{name}\""));
            }
            if let Ok(reply) = gui::control::request(&req) {
                if reply.trim_end().ends_with("ok") {
                    return Ok(());
                }
                eprintln!("rusty_term: running instance refused: {}", reply.trim_end());
            }
            // No (or dead) instance: fall through and become it.
        }
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
    // `run` returns the child shell's own exit status; `main`'s `Result`
    // return only ever maps to 0 or 1, so mirror the child's code exactly via
    // an explicit `process::exit` instead of just propagating the `Result`.
    let code = runtime::run(backend, grid, init_cols, init_rows, config)?;
    std::process::exit(code);
}

/// `rusty_term ctl <command> [key=value]…`: forward one control request to
/// the running instance and print its reply. Exits nonzero on `err`.
fn run_ctl(args: &[String]) -> std::io::Result<()> {
    #[cfg(feature = "gui")]
    {
        if args.is_empty() {
            eprintln!(
                "usage: rusty_term ctl <new-tab|new-window|quake|send-text|list-tabs|focus-tab|ping> [key=value]…"
            );
            return Err(std::io::Error::other("no control command"));
        }
        // Re-quote each token so values with spaces survive the round trip.
        let line = args
            .iter()
            .map(|a| match a.split_once('=') {
                Some((k, v)) if !v.starts_with('"') => {
                    format!("{k}=\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
                }
                _ => a.clone(),
            })
            .collect::<Vec<_>>()
            .join(" ");
        let reply = gui::control::request(&line).map_err(|e| {
            std::io::Error::other(format!("no running rusty_term instance ({e})"))
        })?;
        print!("{reply}");
        if reply.lines().last().is_some_and(|l| l.starts_with("err")) {
            return Err(std::io::Error::other("control command failed"));
        }
        Ok(())
    }
    #[cfg(not(feature = "gui"))]
    {
        let _ = args;
        eprintln!("rusty_term: `ctl` needs the `gui` feature");
        Err(std::io::Error::other("ctl unsupported on this build"))
    }
}
