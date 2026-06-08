//! The in-app settings page model (windowed front-end).
//!
//! A small, mostly-pure state machine: the editable settings, their current
//! values seeded from the live [`Config`](crate::config::Config), how ←/→
//! change each, how each row renders, and how the set persists to the config
//! file. The window owns *applying* a change to the running terminal (retheme,
//! font rebuild, …) and drawing the page; this module owns the *values* and
//! their transitions, so the cycling/clamping/formatting/persistence logic
//! stays unit-testable without a live window.

use crate::config::{self, SettingEdit};
use crate::core::{CursorShape, SCROLLBACK_MAX, Theme};
use crate::shells::DetectedShell;

/// Font-size adjuster bounds. Tighter than the config's 4–512 clamp so the
/// +/- steps stay on a sane on-screen range.
const FONT_MIN: f32 = 6.0;
const FONT_MAX: f32 = 72.0;
/// Scrollback adjuster step and ceiling (the config clamps to 10M; the page
/// steps in readable increments).
const SCROLLBACK_STEP: usize = 1000;
const SCROLLBACK_CEIL: usize = 1_000_000;
/// The built-in font size when the config names none (mirrors `window::FONT_PX`).
pub(crate) const DEFAULT_FONT_PX: f32 = 18.0;

/// Which setting a row edits; also the page's row order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Field {
    Theme,
    FontSize,
    Cursor,
    Blink,
    Ligatures,
    Scrollback,
    Shell,
}

/// Rows in display order. `LEN` is the row count for selection wrapping.
const FIELDS: [Field; 7] = [
    Field::Theme,
    Field::FontSize,
    Field::Cursor,
    Field::Blink,
    Field::Ligatures,
    Field::Scrollback,
    Field::Shell,
];

/// The settings page state: the current values plus the highlighted row.
pub(crate) struct Settings {
    /// Highlighted row, an index into [`FIELDS`].
    pub sel: usize,
    /// Whether any value changed since the page opened (gates the save).
    pub dirty: bool,
    /// Index into [`config::PRESETS`].
    theme: usize,
    font_size: f32,
    cursor: CursorShape,
    blink: bool,
    ligatures: bool,
    scrollback: usize,
    /// Detected shells as `(name, launch-path)`. Index `0` of the row's choice
    /// is "(default)"; `1..` map to `shells[choice - 1]`.
    shells: Vec<(String, String)>,
    shell: usize,
}

impl Settings {
    /// Seed the page from the live configuration.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        theme: &Theme,
        font_size: f32,
        cursor: CursorShape,
        blink: bool,
        ligatures: bool,
        scrollback: usize,
        shell: Option<&str>,
        detected: &[DetectedShell],
    ) -> Self {
        let shells: Vec<(String, String)> = detected
            .iter()
            .map(|s| (s.name.to_string(), s.path.to_string_lossy().into_owned()))
            .collect();
        // Match the configured theme to a preset by its colors, else "default".
        let theme = config::PRESETS
            .iter()
            .position(|&n| config::preset(n).as_ref() == Some(theme))
            .unwrap_or(0);
        // Match the configured shell to a detected one (by friendly name or path).
        let shell = shell
            .and_then(|s| shells.iter().position(|(n, p)| n == s || p == s).map(|i| i + 1))
            .unwrap_or(0);
        Settings {
            sel: 0,
            dirty: false,
            theme,
            font_size,
            cursor,
            blink,
            ligatures,
            scrollback,
            shells,
            shell,
        }
    }

    /// Number of rows (for the renderer and hit-testing).
    pub(crate) fn len(&self) -> usize {
        FIELDS.len()
    }

    /// Move the highlight by one row, wrapping.
    pub(crate) fn move_sel(&mut self, forward: bool) {
        self.sel = wrap(self.sel, FIELDS.len(), forward);
    }

    /// Highlight row `i` directly (a mouse click); ignores an out-of-range row.
    pub(crate) fn select(&mut self, i: usize) {
        if i < FIELDS.len() {
            self.sel = i;
        }
    }

    /// Change the highlighted row's value (`forward` = ←/→ direction; toggles
    /// ignore it). Returns the field changed so the window can apply it live.
    pub(crate) fn change(&mut self, forward: bool) -> Field {
        let field = FIELDS[self.sel];
        match field {
            Field::Theme => self.theme = wrap(self.theme, config::PRESETS.len(), forward),
            Field::FontSize => {
                let step = if forward { 1.0 } else { -1.0 };
                self.font_size = (self.font_size + step).clamp(FONT_MIN, FONT_MAX);
            }
            Field::Cursor => {
                const ORDER: [CursorShape; 3] =
                    [CursorShape::Block, CursorShape::Underline, CursorShape::Bar];
                let i = ORDER.iter().position(|&c| c == self.cursor).unwrap_or(0);
                self.cursor = ORDER[wrap(i, ORDER.len(), forward)];
            }
            Field::Blink => self.blink = !self.blink,
            Field::Ligatures => self.ligatures = !self.ligatures,
            Field::Scrollback => {
                self.scrollback = if forward {
                    (self.scrollback + SCROLLBACK_STEP).min(SCROLLBACK_CEIL)
                } else {
                    self.scrollback.saturating_sub(SCROLLBACK_STEP)
                };
            }
            Field::Shell => self.shell = wrap(self.shell, self.shells.len() + 1, forward),
        }
        self.dirty = true;
        field
    }

    // --- live-value getters the window applies on a change ---

    pub(crate) fn theme_name(&self) -> &'static str {
        config::PRESETS[self.theme]
    }
    pub(crate) fn font_size(&self) -> f32 {
        self.font_size
    }
    pub(crate) fn cursor(&self) -> CursorShape {
        self.cursor
    }
    pub(crate) fn blink(&self) -> bool {
        self.blink
    }
    pub(crate) fn ligatures(&self) -> bool {
        self.ligatures
    }
    pub(crate) fn scrollback(&self) -> usize {
        self.scrollback
    }
    /// The chosen shell's launch path, or `None` for the platform default.
    pub(crate) fn shell_path(&self) -> Option<String> {
        self.shell.checked_sub(1).map(|i| self.shells[i].1.clone())
    }

    /// The rows as `(label, value)` pairs in display order, for the renderer.
    pub(crate) fn display(&self) -> Vec<(&'static str, String)> {
        let shell = match self.shell {
            0 => "(default)".to_string(),
            i => self.shells[i - 1].0.clone(),
        };
        vec![
            ("Theme", self.theme_name().to_string()),
            ("Font size", format!("{} px", fmt_px(self.font_size))),
            ("Cursor", cursor_name(self.cursor).to_string()),
            ("Cursor blink", on_off(self.blink).to_string()),
            ("Ligatures", on_off(self.ligatures).to_string()),
            ("Scrollback", self.scrollback.to_string()),
            ("Default shell", shell),
        ]
    }

    /// Persistence edits for the whole managed set. A value equal to its
    /// built-in default is written only if the file already names it, so the
    /// page never clutters a minimal config with defaults; the "(default)"
    /// shell removes any existing `shell` line.
    pub(crate) fn edits(&self) -> Vec<SettingEdit> {
        vec![
            SettingEdit {
                section: "",
                key: "theme",
                value: Some(config::toml_string(self.theme_name())),
                insert: self.theme != 0,
            },
            SettingEdit {
                section: "window",
                key: "font_size",
                value: Some(fmt_px(self.font_size)),
                insert: self.font_size != DEFAULT_FONT_PX,
            },
            SettingEdit {
                section: "",
                key: "cursor_style",
                value: Some(config::toml_string(cursor_name(self.cursor))),
                insert: self.cursor != CursorShape::default(),
            },
            SettingEdit {
                section: "",
                key: "cursor_blink",
                value: Some(self.blink.to_string()),
                insert: self.blink,
            },
            SettingEdit {
                section: "window",
                key: "ligatures",
                value: Some(self.ligatures.to_string()),
                insert: !self.ligatures, // default is on
            },
            SettingEdit {
                section: "",
                key: "scrollback",
                value: Some(self.scrollback.to_string()),
                insert: self.scrollback != SCROLLBACK_MAX,
            },
            match self.shell_path() {
                Some(path) => SettingEdit {
                    section: "",
                    key: "shell",
                    value: Some(config::toml_string(&path)),
                    insert: true,
                },
                None => SettingEdit { section: "", key: "shell", value: None, insert: false },
            },
        ]
    }
}

/// Step `i` by one within `0..len`, wrapping. `len == 0` stays at `0`.
fn wrap(i: usize, len: usize, forward: bool) -> usize {
    if len == 0 {
        0
    } else if forward {
        (i + 1) % len
    } else {
        (i + len - 1) % len
    }
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

fn cursor_name(c: CursorShape) -> &'static str {
    match c {
        CursorShape::Block => "block",
        CursorShape::Underline => "underline",
        CursorShape::Bar => "bar",
    }
}

/// Format a font size: an integral value as a bare integer, else one decimal.
fn fmt_px(px: f32) -> String {
    if px.fract() == 0.0 { format!("{}", px as i32) } else { format!("{px:.1}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn shells() -> Vec<DetectedShell> {
        vec![
            DetectedShell { name: "pwsh", path: PathBuf::from("/x/pwsh") },
            DetectedShell { name: "bash", path: PathBuf::from("/bin/bash") },
        ]
    }

    fn seeded() -> Settings {
        Settings::new(&Theme::default(), 18.0, CursorShape::Block, false, true, SCROLLBACK_MAX, None, &shells())
    }

    #[test]
    fn new_matches_configured_theme_and_shell() {
        let dracula = config::preset("dracula").unwrap();
        let s = Settings::new(
            &dracula, 18.0, CursorShape::Block, false, true, SCROLLBACK_MAX, Some("bash"), &shells(),
        );
        assert_eq!(s.theme_name(), "dracula", "theme matched by color");
        assert_eq!(s.shell_path().as_deref(), Some("/bin/bash"), "shell matched by name");
    }

    #[test]
    fn unknown_theme_and_shell_fall_back_to_defaults() {
        // A custom (non-preset) theme shows as "default"; an unrecognized shell
        // resolves to "(default)".
        let custom = Theme { fg: 0x123456, ..Theme::default() };
        let s = Settings::new(
            &custom, 18.0, CursorShape::Block, false, true, SCROLLBACK_MAX, Some("zsh"), &shells(),
        );
        assert_eq!(s.theme_name(), "default");
        assert_eq!(s.shell_path(), None);
    }

    #[test]
    fn theme_change_wraps_both_directions() {
        let mut s = seeded(); // sel defaults to Theme (row 0)
        assert_eq!(s.theme_name(), "default");
        assert_eq!(s.change(true), Field::Theme);
        assert_eq!(s.theme_name(), config::PRESETS[1]);
        // Wrap backward off the start to the last preset.
        let mut s = seeded();
        s.change(false);
        assert_eq!(s.theme_name(), *config::PRESETS.last().unwrap());
        assert!(s.dirty);
    }

    #[test]
    fn cursor_cycles_block_underline_bar() {
        let mut s = seeded();
        s.sel = 2; // Cursor row
        assert_eq!(s.cursor(), CursorShape::Block);
        s.change(true);
        assert_eq!(s.cursor(), CursorShape::Underline);
        s.change(true);
        assert_eq!(s.cursor(), CursorShape::Bar);
        s.change(true);
        assert_eq!(s.cursor(), CursorShape::Block, "wraps");
    }

    #[test]
    fn toggles_ignore_direction() {
        let mut s = seeded();
        s.sel = 3; // Blink
        assert!(!s.blink());
        s.change(false); // a toggle flips regardless of direction
        assert!(s.blink());
        s.sel = 4; // Ligatures (default on)
        assert!(s.ligatures());
        s.change(true);
        assert!(!s.ligatures());
    }

    #[test]
    fn font_and_scrollback_clamp_at_bounds() {
        let mut s = seeded();
        s.sel = 1; // Font size
        for _ in 0..100 {
            s.change(false);
        }
        assert_eq!(s.font_size(), FONT_MIN, "font clamps to the floor");
        s.sel = 5; // Scrollback
        // already at SCROLLBACK_MAX (1_000_000 ceil shares it); step down then floor.
        let start = s.scrollback();
        s.change(false);
        assert_eq!(s.scrollback(), start - SCROLLBACK_STEP);
        for _ in 0..10_000 {
            s.change(false);
        }
        assert_eq!(s.scrollback(), 0, "scrollback floors at 0 without underflow");
    }

    #[test]
    fn shell_choice_wraps_through_default() {
        let mut s = seeded();
        s.sel = 6; // Default shell
        assert_eq!(s.shell_path(), None, "starts at (default)");
        s.change(true);
        assert_eq!(s.shell_path().as_deref(), Some("/x/pwsh"));
        s.change(true);
        assert_eq!(s.shell_path().as_deref(), Some("/bin/bash"));
        s.change(true);
        assert_eq!(s.shell_path(), None, "wraps back to (default)");
    }

    #[test]
    fn display_rows_match_field_count() {
        let s = seeded();
        assert_eq!(s.display().len(), s.len());
        assert_eq!(s.display()[1], ("Font size", "18 px".to_string()));
    }

    #[test]
    fn edits_omit_defaults_but_persist_overrides() {
        // All defaults: every managed edit is a non-inserting (or removing) one.
        let s = seeded();
        let edits = s.edits();
        assert!(
            edits.iter().all(|e| !e.insert),
            "nothing inserted when everything is at its default",
        );
        // Change theme + font: those become inserting edits with the new value.
        let mut s = seeded();
        s.sel = 0;
        s.change(true); // theme -> PRESETS[1]
        s.sel = 1;
        s.change(true); // font 18 -> 19
        let edits = s.edits();
        let theme = edits.iter().find(|e| e.key == "theme").unwrap();
        assert!(theme.insert && theme.value == Some(config::toml_string(config::PRESETS[1])));
        let font = edits.iter().find(|e| e.key == "font_size").unwrap();
        assert!(font.insert && font.value.as_deref() == Some("19"));
    }

    #[test]
    fn default_shell_edit_removes_the_key() {
        let s = seeded(); // shell at "(default)"
        let shell = s.edits().into_iter().find(|e| e.key == "shell").unwrap();
        assert!(shell.value.is_none() && !shell.insert, "default shell removes any existing line");
    }
}
