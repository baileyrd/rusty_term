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

use crate::core::Theme;

/// Parsed configuration with everything optional; `None` / the [`Theme`]
/// defaults mean "keep the built-in behavior".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    /// Shell command to spawn instead of `$SHELL` / `%COMSPEC%`.
    pub shell: Option<String>,
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
    /// Startup colors: default fg/bg/cursor and the 16-color ANSI palette.
    pub theme: Theme,
}

impl Config {
    /// Load the config: resolve the path (CLI flag > env var > platform
    /// default), read it, parse it. Returns the config plus human-readable
    /// warnings for anything skipped. A missing file yields pure defaults and
    /// no warnings; an unreadable *explicitly requested* file yields a warning.
    pub fn load(args: &[String]) -> (Config, Vec<String>) {
        let (path, explicit) = match resolve_path(args) {
            Some(p) => p,
            None => return (Config::default(), Vec::new()),
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let (cfg, mut warnings) = parse(&text);
                for w in &mut warnings {
                    *w = format!("{}: {}", path.display(), w);
                }
                (cfg, warnings)
            }
            Err(e) if explicit => (
                Config::default(),
                vec![format!("config {}: {}", path.display(), e)],
            ),
            Err(_) => (Config::default(), Vec::new()), // default path absent: fine
        }
    }
}

/// The config file to read, if any, and whether the user *named* it (CLI/env)
/// rather than us probing the platform default. Explicit paths are returned
/// even if unreadable so the caller can warn; the default path only when it
/// exists.
fn resolve_path(args: &[String]) -> Option<(PathBuf, bool)> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--config" {
            return it.next().map(|p| (PathBuf::from(p), true));
        }
        if let Some(p) = a.strip_prefix("--config=") {
            return Some((PathBuf::from(p), true));
        }
    }
    if let Some(p) = std::env::var_os("RUSTY_TERM_CONFIG") {
        return Some((PathBuf::from(p), true));
    }
    let base = default_config_dir()?;
    let p = base.join("rusty_term").join("config.toml");
    p.exists().then_some((p, false))
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

/// One parsed `key = value` payload.
#[derive(Debug, PartialEq)]
enum Value {
    Str(String),
    Int(i64),
    Float(f64),
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
        ("", "scrollback") => {
            cfg.scrollback = Some(expect_int(key, value)?.clamp(0, 10_000_000) as usize)
        }
        ("", "theme") => {
            let name = expect_str(key, value)?;
            cfg.theme = preset(&name)
                .ok_or_else(|| format!("unknown theme `{name}` (try {})", PRESET_NAMES))?;
        }
        ("window", "cols") => cfg.cols = Some(expect_dim(key, value)?),
        ("window", "rows") => cfg.rows = Some(expect_dim(key, value)?),
        ("window", "font") => cfg.font = Some(PathBuf::from(expect_str(key, value)?)),
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

/// The preset names, for the unknown-theme warning.
const PRESET_NAMES: &str =
    "default, gruvbox-dark, dracula, solarized-dark, solarized-light, nord, one-dark";

/// A built-in theme preset by (case/sep-insensitive) name, or `None`. Colors
/// are the published palettes of each scheme. `theme = "name"` seeds the whole
/// [`Theme`]; explicit `[colors]` keys still override individual entries when
/// they appear after it in the file.
fn preset(name: &str) -> Option<Theme> {
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
        _ => return None,
    })
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
    fn unknown_keys_warn_but_do_not_fail() {
        let (cfg, warns) = parse("shell = \"sh\"\nbogus = 1\n[weird]\nx = 2\n");
        assert_eq!(cfg.shell.as_deref(), Some("sh"));
        assert_eq!(warns.len(), 2);
        assert!(warns[0].contains("unknown key `bogus`"));
        assert!(warns[1].contains("[weird]"));
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
        assert_eq!(resolve_path(&args), Some((PathBuf::from("x.toml"), true)));
        let args = vec!["--config=y.toml".to_string()];
        assert_eq!(resolve_path(&args), Some((PathBuf::from("y.toml"), true)));
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
}
