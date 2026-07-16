//! User-configurable keybindings for the windowed front-end.
//!
//! Terminal-owned shortcuts — copy/paste, tab management, scrollback browsing,
//! opening the config — are bound to key *chords* here instead of to
//! compile-time constants, so the `[keys]` config section can rebind them. This
//! layer is deliberately toolkit-free (no `winit`): the window backend maps its
//! keys onto [`Key`] for lookup, which keeps the parser and config unit-testable
//! without a GUI.

/// A terminal-owned action a key chord can trigger (everything else is encoded
/// and sent to the child).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Action {
    Copy,
    Paste,
    NewTab,
    NewWindow,
    FoldOutput,
    CloseTab,
    NextTab,
    PrevTab,
    OpenConfig,
    OpenSettings,
    Search,
    OpenLinks,
    CopyMode,
    Broadcast,
    SplitRight,
    SplitDown,
    FocusNext,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    ResizeLeft,
    ResizeRight,
    ResizeUp,
    ResizeDown,
    ZoomPane,
    ScrollPageUp,
    ScrollPageDown,
    ScrollPromptUp,
    ScrollPromptDown,
    /// Toggle fullscreen at runtime (F11 / the near-universal terminal/
    /// browser convention). Previously only reachable via `launch_mode` at
    /// startup — there was no way to flip it once the window was open.
    ToggleFullscreen,
    /// Grow/shrink/reset the font size (Ctrl+=/Ctrl+-/Ctrl+0, matching
    /// Chrome/VS Code/iTerm2/Windows Terminal). Previously only reachable
    /// through the settings overlay.
    FontSizeUp,
    FontSizeDown,
    FontSizeReset,
}

/// The non-modifier key of a chord, independent of any windowing toolkit.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Key {
    /// A printable key, stored lowercased (letters, digits, `,`, `.`, …).
    Char(char),
    Tab,
    PageUp,
    PageDown,
    Left,
    Right,
    Up,
    Down,
    Space,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    Home,
    End,
    Enter,
    Insert,
    Delete,
    Escape,
    Backspace,
}

/// A key chord: modifier state plus one [`Key`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct Chord {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: Key,
}

impl Chord {
    pub fn new(ctrl: bool, shift: bool, alt: bool, key: Key) -> Self {
        Self { ctrl, shift, alt, key }
    }
}

/// The active binding table (chord → action). Starts from [`Keymap::default`]
/// (the built-in bindings); the config overrides individual actions.
#[derive(Clone, Debug, PartialEq)]
pub struct Keymap {
    binds: Vec<(Chord, Action)>,
}

impl Default for Keymap {
    fn default() -> Self {
        use Action::*;
        use Key::{Char, PageDown, PageUp, Tab};
        // `c(ctrl, shift, key)` — all built-in chords are alt-free.
        let c = |ctrl, shift, key| Chord::new(ctrl, shift, false, key);
        Self {
            binds: vec![
                (c(true, true, Char('c')), Copy),
                (c(true, true, Char('v')), Paste),
                (c(true, true, Char('t')), NewTab),
                (c(true, true, Char('n')), NewWindow),
                (c(true, true, Char('u')), FoldOutput),
                (c(true, true, Char('w')), CloseTab),
                (c(true, false, Tab), NextTab),
                (c(true, true, Tab), PrevTab),
                (c(true, true, Char(',')), OpenConfig),
                (c(true, false, Char(',')), OpenSettings),
                (c(true, true, Char('f')), Search),
                (c(true, true, Char('o')), OpenLinks),
                (c(true, true, Char('d')), SplitRight),
                (c(true, true, Char('e')), SplitDown),
                (c(true, true, Char('j')), FocusNext),
                (Chord::new(true, false, true, Key::Left), FocusLeft),
                (Chord::new(true, false, true, Key::Right), FocusRight),
                (Chord::new(true, false, true, Key::Up), FocusUp),
                (Chord::new(true, false, true, Key::Down), FocusDown),
                (Chord::new(true, true, false, Key::Left), ResizeLeft),
                (Chord::new(true, true, false, Key::Right), ResizeRight),
                (Chord::new(true, true, false, Key::Up), ResizeUp),
                (Chord::new(true, true, false, Key::Down), ResizeDown),
                (c(true, true, Char('z')), ZoomPane),
                (Chord::new(true, true, false, Key::Space), CopyMode),
                (c(true, true, Char('b')), Broadcast),
                (c(false, true, PageUp), ScrollPageUp),
                (c(false, true, PageDown), ScrollPageDown),
                (c(true, true, PageUp), ScrollPromptUp),
                (c(true, true, PageDown), ScrollPromptDown),
                (c(false, false, Key::F11), ToggleFullscreen),
                (c(true, false, Char('=')), FontSizeUp),
                // Ctrl+Shift+= (i.e. Ctrl+Plus without needing the shift-less
                // `=`/`+` key some layouts split across two physical keys) —
                // the same binding Chrome/VS Code offer alongside Ctrl+=.
                (c(true, true, Char('=')), FontSizeUp),
                (c(true, false, Char('-')), FontSizeDown),
                (c(true, false, Char('0')), FontSizeReset),
            ],
        }
    }
}

impl Keymap {
    /// Rebind `action` to `chord`, dropping any previous binding for that action
    /// and any other action currently on that chord (a chord triggers one
    /// thing).
    pub fn set(&mut self, action: Action, chord: Chord) {
        self.binds.retain(|(ch, a)| *a != action && *ch != chord);
        self.binds.push((chord, action));
    }

    /// The action bound to `chord`, if any.
    #[cfg(any(test, feature = "gui"))]
    pub fn action(&self, chord: Chord) -> Option<Action> {
        self.binds.iter().find(|(ch, _)| *ch == chord).map(|(_, a)| *a)
    }
}

/// Parse a config action name (a `[keys]` key) into an [`Action`].
pub fn parse_action(name: &str) -> Option<Action> {
    Some(match name {
        "copy" => Action::Copy,
        "paste" => Action::Paste,
        "new_tab" => Action::NewTab,
        "new_window" => Action::NewWindow,
        "fold_output" => Action::FoldOutput,
        "close_tab" => Action::CloseTab,
        "next_tab" => Action::NextTab,
        "prev_tab" => Action::PrevTab,
        "open_config" => Action::OpenConfig,
        "open_settings" => Action::OpenSettings,
        "search" => Action::Search,
        "open_links" => Action::OpenLinks,
        "copy_mode" => Action::CopyMode,
        "broadcast" => Action::Broadcast,
        "focus_left" => Action::FocusLeft,
        "focus_right" => Action::FocusRight,
        "focus_up" => Action::FocusUp,
        "focus_down" => Action::FocusDown,
        "resize_left" => Action::ResizeLeft,
        "resize_right" => Action::ResizeRight,
        "resize_up" => Action::ResizeUp,
        "resize_down" => Action::ResizeDown,
        "zoom_pane" => Action::ZoomPane,
        "split_right" => Action::SplitRight,
        "split_down" => Action::SplitDown,
        "focus_next" => Action::FocusNext,
        "scroll_page_up" => Action::ScrollPageUp,
        "scroll_page_down" => Action::ScrollPageDown,
        "scroll_prompt_up" => Action::ScrollPromptUp,
        "scroll_prompt_down" => Action::ScrollPromptDown,
        "toggle_fullscreen" => Action::ToggleFullscreen,
        "font_size_up" => Action::FontSizeUp,
        "font_size_down" => Action::FontSizeDown,
        "font_size_reset" => Action::FontSizeReset,
        _ => return None,
    })
}

/// Parse a chord string like `"Ctrl+Shift+C"` or `"Shift+PageUp"`. Modifiers are
/// `ctrl`/`control`, `shift`, `alt`/`option` (case-insensitive); the final token
/// is the key — a single printable character, one of `comma`, `tab`,
/// `pageup`/`pgup`, `pagedown`/`pgdn`, `home`, `end`, `enter`/`return`,
/// `insert`/`ins`, `delete`/`del`, `escape`/`esc`, `backspace`, or `f1`-`f12`.
pub fn parse_chord(s: &str) -> Result<Chord, String> {
    let (mut ctrl, mut shift, mut alt) = (false, false, false);
    let mut key = None;
    for tok in s.split('+') {
        let t = tok.trim();
        if t.is_empty() {
            return Err(format!("malformed key chord `{s}`"));
        }
        match t.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "shift" => shift = true,
            "alt" | "option" => alt = true,
            "comma" => key = Some(Key::Char(',')),
            "tab" => key = Some(Key::Tab),
            "pageup" | "pgup" => key = Some(Key::PageUp),
            "pagedown" | "pgdn" => key = Some(Key::PageDown),
            "left" => key = Some(Key::Left),
            "right" => key = Some(Key::Right),
            "up" => key = Some(Key::Up),
            "down" => key = Some(Key::Down),
            "space" => key = Some(Key::Space),
            "home" => key = Some(Key::Home),
            "end" => key = Some(Key::End),
            "enter" | "return" => key = Some(Key::Enter),
            "insert" | "ins" => key = Some(Key::Insert),
            "delete" | "del" => key = Some(Key::Delete),
            "escape" | "esc" => key = Some(Key::Escape),
            "backspace" => key = Some(Key::Backspace),
            "f1" => key = Some(Key::F1),
            "f2" => key = Some(Key::F2),
            "f3" => key = Some(Key::F3),
            "f4" => key = Some(Key::F4),
            "f5" => key = Some(Key::F5),
            "f6" => key = Some(Key::F6),
            "f7" => key = Some(Key::F7),
            "f8" => key = Some(Key::F8),
            "f9" => key = Some(Key::F9),
            "f10" => key = Some(Key::F10),
            "f11" => key = Some(Key::F11),
            "f12" => key = Some(Key::F12),
            other => {
                let mut chars = other.chars();
                let c = chars.next().unwrap(); // `t` is non-empty
                if chars.next().is_some() {
                    return Err(format!(
                        "unknown key `{t}` in chord `{s}` (expected a single character, or one \
                         of: comma, tab, pageup/pgup, pagedown/pgdn, left, right, up, down, \
                         space, home, end, enter/return, insert/ins, delete/del, escape/esc, \
                         backspace, f1-f12)"
                    ));
                }
                key = Some(Key::Char(c));
            }
        }
    }
    match key {
        Some(k) => Ok(Chord::new(ctrl, shift, alt, k)),
        None => Err(format!("key chord `{s}` names no key")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_bind_the_builtin_shortcuts() {
        let km = Keymap::default();
        assert_eq!(km.action(Chord::new(true, true, false, Key::Char('c'))), Some(Action::Copy));
        assert_eq!(km.action(Chord::new(true, false, false, Key::Tab)), Some(Action::NextTab));
        assert_eq!(km.action(Chord::new(true, true, false, Key::Tab)), Some(Action::PrevTab));
        assert_eq!(km.action(Chord::new(false, true, false, Key::PageUp)), Some(Action::ScrollPageUp));
        assert_eq!(km.action(Chord::new(true, false, false, Key::Char(','))), Some(Action::OpenSettings));
        assert_eq!(km.action(Chord::new(true, true, false, Key::Char('n'))), Some(Action::NewWindow));
        assert_eq!(km.action(Chord::new(true, true, false, Key::Char('u'))), Some(Action::FoldOutput));
        // Unbound chords (plain typing, Ctrl+C) fall through to the child.
        assert_eq!(km.action(Chord::new(false, false, false, Key::Char('a'))), None);
        assert_eq!(km.action(Chord::new(true, false, false, Key::Char('c'))), None);
    }

    #[test]
    fn defaults_bind_fullscreen_and_font_zoom() {
        // These were previously unreachable at runtime at all — fullscreen
        // was launch_mode-only, and font size only adjustable via the
        // settings overlay.
        let km = Keymap::default();
        assert_eq!(km.action(Chord::new(false, false, false, Key::F11)), Some(Action::ToggleFullscreen));
        assert_eq!(km.action(Chord::new(true, false, false, Key::Char('='))), Some(Action::FontSizeUp));
        assert_eq!(km.action(Chord::new(true, false, false, Key::Char('-'))), Some(Action::FontSizeDown));
        assert_eq!(km.action(Chord::new(true, false, false, Key::Char('0'))), Some(Action::FontSizeReset));
    }

    #[test]
    fn set_rebinds_and_clears_old_chord_and_action() {
        let mut km = Keymap::default();
        let new = Chord::new(true, false, true, Key::Char('y'));
        km.set(Action::Copy, new);
        assert_eq!(km.action(new), Some(Action::Copy));
        // The old Ctrl+Shift+C no longer copies.
        assert_eq!(km.action(Chord::new(true, true, false, Key::Char('c'))), None);
        // Rebinding Paste onto Copy's new chord moves it off Copy.
        km.set(Action::Paste, new);
        assert_eq!(km.action(new), Some(Action::Paste));
        // Copy was on `new`, which Paste just took, so Copy is now unbound.
        assert!(km.binds.iter().all(|&(_, a)| a != Action::Copy));
    }

    #[test]
    fn parse_chord_handles_modifiers_and_keys() {
        assert_eq!(parse_chord("Ctrl+Shift+C"), Ok(Chord::new(true, true, false, Key::Char('c'))));
        assert_eq!(parse_chord("shift+pageup"), Ok(Chord::new(false, true, false, Key::PageUp)));
        assert_eq!(parse_chord("Ctrl+Tab"), Ok(Chord::new(true, false, false, Key::Tab)));
        assert_eq!(parse_chord("Ctrl+Shift+Comma"), Ok(Chord::new(true, true, false, Key::Char(','))));
        assert_eq!(parse_chord("Ctrl+Shift+,"), Ok(Chord::new(true, true, false, Key::Char(','))));
        assert_eq!(parse_chord("Alt+x"), Ok(Chord::new(false, false, true, Key::Char('x'))));
    }

    #[test]
    fn parse_chord_rejects_garbage() {
        assert!(parse_chord("Ctrl+").is_err()); // empty key token
        assert!(parse_chord("Ctrl+Foo").is_err()); // unsupported named key
        assert!(parse_chord("Ctrl+Shift").is_err()); // modifiers only, no key
    }

    #[test]
    fn parse_chord_handles_newly_supported_named_keys() {
        // F-keys, navigation keys, and editing keys were previously
        // unbindable at all (only letters/digits/comma/tab/pageup/pagedown/
        // arrows/space worked) — `[keys] copy = "Ctrl+F5"` failed with
        // "unknown key" and there was no escape hatch to spare function keys.
        assert_eq!(parse_chord("Ctrl+F5"), Ok(Chord::new(true, false, false, Key::F5)));
        assert_eq!(parse_chord("F11"), Ok(Chord::new(false, false, false, Key::F11)));
        assert_eq!(parse_chord("Ctrl+Home"), Ok(Chord::new(true, false, false, Key::Home)));
        assert_eq!(parse_chord("Shift+End"), Ok(Chord::new(false, true, false, Key::End)));
        assert_eq!(parse_chord("Ctrl+Enter"), Ok(Chord::new(true, false, false, Key::Enter)));
        assert_eq!(parse_chord("Alt+Return"), Ok(Chord::new(false, false, true, Key::Enter)));
        assert_eq!(parse_chord("Ctrl+Delete"), Ok(Chord::new(true, false, false, Key::Delete)));
        assert_eq!(parse_chord("Ctrl+Insert"), Ok(Chord::new(true, false, false, Key::Insert)));
        assert_eq!(parse_chord("Ctrl+Escape"), Ok(Chord::new(true, false, false, Key::Escape)));
        assert_eq!(parse_chord("Ctrl+Backspace"), Ok(Chord::new(true, false, false, Key::Backspace)));
    }

    #[test]
    fn parse_action_maps_names() {
        assert_eq!(parse_action("new_tab"), Some(Action::NewTab));
        assert_eq!(parse_action("new_window"), Some(Action::NewWindow));
        assert_eq!(parse_action("fold_output"), Some(Action::FoldOutput));
        assert_eq!(parse_action("scroll_prompt_down"), Some(Action::ScrollPromptDown));
        assert_eq!(parse_action("open_settings"), Some(Action::OpenSettings));
        assert_eq!(parse_action("toggle_fullscreen"), Some(Action::ToggleFullscreen));
        assert_eq!(parse_action("font_size_up"), Some(Action::FontSizeUp));
        assert_eq!(parse_action("font_size_down"), Some(Action::FontSizeDown));
        assert_eq!(parse_action("font_size_reset"), Some(Action::FontSizeReset));
        assert_eq!(parse_action("nonsense"), None);
    }

    #[test]
    fn pane_and_copy_mode_bindings_parse_and_default() {
        let km = Keymap::default();
        assert_eq!(
            km.action(Chord::new(true, false, true, Key::Left)),
            Some(Action::FocusLeft)
        );
        assert_eq!(
            km.action(Chord::new(true, true, false, Key::Right)),
            Some(Action::ResizeRight)
        );
        assert_eq!(km.action(Chord::new(true, true, false, Key::Char('z'))), Some(Action::ZoomPane));
        assert_eq!(km.action(Chord::new(true, true, false, Key::Space)), Some(Action::CopyMode));
        // Config strings for the new keys/actions round-trip.
        assert_eq!(parse_action("zoom_pane"), Some(Action::ZoomPane));
        assert_eq!(parse_action("copy_mode"), Some(Action::CopyMode));
        assert_eq!(
            parse_chord("Ctrl+Alt+Up"),
            Ok(Chord::new(true, false, true, Key::Up))
        );
        assert_eq!(
            parse_chord("Ctrl+Shift+Space"),
            Ok(Chord::new(true, true, false, Key::Space))
        );
    }
}
