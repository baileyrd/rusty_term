//! Native keyboard encoding: winit key events → terminal byte sequences.
//!
//! In TUI mode the host terminal does this and we relay; with our own window we
//! finally encode keys ourselves (the native side of L09). This maps winit's
//! logical keys + modifiers to the conventional xterm byte sequences, honoring
//! DECCKM (application cursor keys) and xterm modifier parameters — or, when
//! the child has requested it via the Kitty keyboard protocol (`CSI > flags
//! u`, tracked in `Grid::kitty_flags_stack`), the disambiguating `CSI u`
//! encoding instead for the keys that are otherwise ambiguous.

use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Kitty keyboard protocol flag bits this encoder implements: disambiguate
/// escape codes (1), report event types (2, press/repeat/release), report
/// alternate keys (4, the shifted key as a sub-parameter), and report
/// associated text (16, the text a keypress would insert). Flag 8 (report
/// *all* keys as escape codes) is not implemented — plain text keys keep
/// sending text, so their releases are not reported, per the spec's
/// fallback behavior.
const KITTY_DISAMBIGUATE: u8 = 1;
const KITTY_EVENT_TYPES: u8 = 2;
const KITTY_ALTERNATE: u8 = 4;
const KITTY_ASSOC_TEXT: u8 = 16;

/// One key event's phase, from winit's `state` + `repeat`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum KeyPhase {
    Press,
    Repeat,
    Release,
}

impl KeyPhase {
    /// The Kitty event-type sub-parameter (`1` press is the omittable default).
    fn code(self) -> u8 {
        match self {
            KeyPhase::Press => 1,
            KeyPhase::Repeat => 2,
            KeyPhase::Release => 3,
        }
    }
}

/// Encode a key press to the bytes to write to the PTY, or `None` if the key
/// produces no input (e.g. a lone modifier). `app_cursor` is DECCKM state;
/// `kitty_flags` is the child's current Kitty keyboard protocol enhancement
/// flags (`0` for legacy encoding). Presses only — the full-fidelity entry
/// point with repeat/release, alternate keys, and associated text is
/// [`encode_full`].
#[cfg(test)]
pub(crate) fn encode(key: &Key, mods: ModifiersState, app_cursor: bool, kitty_flags: u8) -> Option<Vec<u8>> {
    encode_full(key, mods, app_cursor, kitty_flags, KeyPhase::Press, None, None)
}

/// The full encoder: `phase` distinguishes press/repeat/release (repeat and
/// release only produce output under Kitty flag 2), `alternate` is the
/// shifted key when it differs (flag 4), and `text` is the text this press
/// would insert (flag 16, attached to `CSI u`-encoded keys).
pub(crate) fn encode_full(
    key: &Key,
    mods: ModifiersState,
    app_cursor: bool,
    kitty_flags: u8,
    phase: KeyPhase,
    alternate: Option<char>,
    text: Option<&str>,
) -> Option<Vec<u8>> {
    // Without event-type reporting, releases are silent and repeats are
    // ordinary presses.
    let phase = if kitty_flags & KITTY_EVENT_TYPES == 0 {
        if phase == KeyPhase::Release {
            return None;
        }
        KeyPhase::Press
    } else {
        phase
    };
    let ctx = KittyCtx { flags: kitty_flags, phase, alternate, text };
    match key {
        Key::Named(named) => encode_named(*named, mods, app_cursor, &ctx),
        Key::Character(s) => encode_text(s, mods, &ctx),
        _ => None,
    }
}

/// The Kitty-protocol context threaded through the per-key encoders.
struct KittyCtx<'t> {
    flags: u8,
    phase: KeyPhase,
    alternate: Option<char>,
    text: Option<&'t str>,
}

impl KittyCtx<'_> {
    /// The `mods[:event]` field for a CSI sequence, `None` when the whole
    /// field is omittable (no modifiers, plain press).
    fn mods_event(&self, m: Option<u8>) -> Option<String> {
        let event = (self.flags & KITTY_EVENT_TYPES != 0 && self.phase != KeyPhase::Press)
            .then(|| self.phase.code());
        match (m, event) {
            (None, None) => None,
            (m, None) => Some(format!("{}", m.unwrap_or(1))),
            (m, Some(e)) => Some(format!("{}:{}", m.unwrap_or(1), e)),
        }
    }

    /// The `;text-codepoints` suffix (flag 16): press/repeat only — a
    /// release inserts nothing.
    fn text_suffix(&self) -> String {
        if self.flags & KITTY_ASSOC_TEXT == 0 || self.phase == KeyPhase::Release {
            return String::new();
        }
        let Some(text) = self.text.filter(|t| !t.is_empty()) else { return String::new() };
        // Control characters are never "associated text".
        if text.chars().any(char::is_control) {
            return String::new();
        }
        let cps: Vec<String> = text.chars().map(|c| (c as u32).to_string()).collect();
        format!(";{}", cps.join(":"))
    }

    /// The key-code field with the flag-4 alternate (`code:shifted`).
    fn key_field(&self, code: u32) -> String {
        match self.alternate {
            Some(alt) if self.flags & KITTY_ALTERNATE != 0 && alt as u32 != code => {
                format!("{code}:{}", alt as u32)
            }
            _ => code.to_string(),
        }
    }
}

/// Encode a disambiguated key as `CSI u`: the key code (with a flag-4
/// alternate), the `mods[:event]` field when needed, and flag-16 associated
/// text — the format the Kitty keyboard protocol spec assigns whether or
/// not a key has a legacy sequence.
fn kitty_u(code: u32, m: Option<u8>, ctx: &KittyCtx) -> Vec<u8> {
    let key = ctx.key_field(code);
    let text = ctx.text_suffix();
    match ctx.mods_event(m) {
        Some(field) => format!("\x1b[{key};{field}{text}u").into_bytes(),
        None if text.is_empty() => format!("\x1b[{key}u").into_bytes(),
        None => format!("\x1b[{key};1{text}u").into_bytes(),
    }
}

/// Encode printable text, applying Ctrl (→ control byte, or `CSI u` when the
/// child asked to disambiguate it from other keys) and Alt (→ ESC prefix).
/// Shift is already reflected in the text winit produced. Repeats and
/// releases only produce output for `CSI u`-encoded combinations — without
/// flag 8 a plain text key sends text on press and nothing on release.
fn encode_text(text: &str, mods: ModifiersState, ctx: &KittyCtx) -> Option<Vec<u8>> {
    if mods.control_key()
        && let Some(c) = text.chars().next()
        && c.is_ascii()
    {
        if ctx.flags & KITTY_DISAMBIGUATE != 0 {
            // The base (lowercase) letter's own codepoint, not the C0 control
            // byte — disambiguates e.g. Ctrl+I from a literal Tab keypress,
            // which both produce 0x09 in legacy encoding.
            return Some(kitty_u(c.to_ascii_lowercase() as u32, modifier_param(mods), ctx));
        }
        if ctx.phase == KeyPhase::Release {
            return None; // no legacy encoding for a release
        }
        // C0 control byte: 'a'/'A' → 0x01 … '_' → 0x1f, Space/@ → 0x00.
        let b = c.to_ascii_uppercase() as u8 & 0x1f;
        return Some(if mods.alt_key() { vec![0x1b, b] } else { vec![b] });
    }
    if ctx.phase == KeyPhase::Release {
        return None; // plain text keys have no release encoding (needs flag 8)
    }
    if mods.alt_key() {
        let mut v = vec![0x1b];
        v.extend_from_slice(text.as_bytes());
        return Some(v);
    }
    Some(text.as_bytes().to_vec())
}

fn encode_named(named: NamedKey, mods: ModifiersState, app_cursor: bool, ctx: &KittyCtx) -> Option<Vec<u8>> {
    let m = modifier_param(mods);
    if ctx.flags & KITTY_DISAMBIGUATE != 0 {
        // These four collide with C0 control bytes / plain text in legacy
        // encoding (Escape vs. the start of any other escape sequence, Enter/
        // Tab/Backspace vs. their literal bytes elsewhere); the Kitty spec's
        // functional-key table assigns them these fixed codepoints.
        let code = match named {
            NamedKey::Escape => Some(27),
            NamedKey::Enter => Some(13),
            NamedKey::Tab if !mods.shift_key() => Some(9),
            NamedKey::Backspace => Some(127),
            _ => None,
        };
        if let Some(code) = code {
            return Some(kitty_u(code, m, ctx));
        }
    }
    // The legacy single-byte keys (Enter/Tab/Backspace/Escape/Space as
    // text) have no release/repeat encoding without their CSI-u form.
    let functional = !matches!(
        named,
        NamedKey::Enter | NamedKey::Backspace | NamedKey::Escape | NamedKey::Space | NamedKey::Tab
    );
    if ctx.phase == KeyPhase::Release && !functional {
        return None;
    }
    // Functional keys carry the event type in the mods field's sub-parameter
    // (`CSI 1;mods:event A`), so a release needs the field present.
    let field = ctx.mods_event(m);
    let seq = match named {
        NamedKey::Enter => b"\r".to_vec(),
        NamedKey::Backspace => b"\x7f".to_vec(),
        NamedKey::Escape => b"\x1b".to_vec(),
        NamedKey::Space => b" ".to_vec(),
        NamedKey::Tab => {
            if mods.shift_key() {
                b"\x1b[Z".to_vec() // back-tab (CBT)
            } else {
                b"\t".to_vec()
            }
        }
        NamedKey::ArrowUp => cursor_seq(b'A', field.as_deref(), app_cursor),
        NamedKey::ArrowDown => cursor_seq(b'B', field.as_deref(), app_cursor),
        NamedKey::ArrowRight => cursor_seq(b'C', field.as_deref(), app_cursor),
        NamedKey::ArrowLeft => cursor_seq(b'D', field.as_deref(), app_cursor),
        NamedKey::Home => cursor_seq(b'H', field.as_deref(), app_cursor),
        NamedKey::End => cursor_seq(b'F', field.as_deref(), app_cursor),
        NamedKey::Insert => tilde_seq(2, field.as_deref()),
        NamedKey::Delete => tilde_seq(3, field.as_deref()),
        NamedKey::PageUp => tilde_seq(5, field.as_deref()),
        NamedKey::PageDown => tilde_seq(6, field.as_deref()),
        NamedKey::F1 => fn_seq(b'P', field.as_deref()),
        NamedKey::F2 => fn_seq(b'Q', field.as_deref()),
        NamedKey::F3 => fn_seq(b'R', field.as_deref()),
        NamedKey::F4 => fn_seq(b'S', field.as_deref()),
        NamedKey::F5 => tilde_seq(15, field.as_deref()),
        NamedKey::F6 => tilde_seq(17, field.as_deref()),
        NamedKey::F7 => tilde_seq(18, field.as_deref()),
        NamedKey::F8 => tilde_seq(19, field.as_deref()),
        NamedKey::F9 => tilde_seq(20, field.as_deref()),
        NamedKey::F10 => tilde_seq(21, field.as_deref()),
        NamedKey::F11 => tilde_seq(23, field.as_deref()),
        NamedKey::F12 => tilde_seq(24, field.as_deref()),
        _ => return None,
    };
    Some(seq)
}

/// Encode a numpad key under application keypad mode (DECKPAM `ESC =` /
/// DECNKM `?66`) as its `SS3` sequence, per xterm's PC-keyboard extension of
/// the VT220 table (`SS3 j`–`o` for the operators, `p`–`y` for the digits,
/// `X` for `=`, `M` for Enter). Returns `None` — falling back to the normal
/// text/named encoding — when the mode is off, the key isn't a keypad
/// character, or modifiers are held (a modified keypad key keeps its legacy
/// encoding, which carries the xterm modifier parameter).
///
/// The caller is responsible for only routing keys whose winit `KeyLocation`
/// is `Numpad` here; this function can't tell a top-row `5` from KP-5.
pub(crate) fn encode_numpad(key: &Key, mods: ModifiersState, app_keypad: bool) -> Option<Vec<u8>> {
    if !app_keypad || mods.control_key() || mods.alt_key() || mods.shift_key() {
        return None;
    }
    let f = match key {
        Key::Named(NamedKey::Enter) => b'M',
        Key::Character(s) => match s.as_str() {
            "0" => b'p',
            "1" => b'q',
            "2" => b'r',
            "3" => b's',
            "4" => b't',
            "5" => b'u',
            "6" => b'v',
            "7" => b'w',
            "8" => b'x',
            "9" => b'y',
            "*" => b'j',
            "+" => b'k',
            "," => b'l',
            "-" => b'm',
            "." => b'n',
            "/" => b'o',
            "=" => b'X',
            _ => return None,
        },
        _ => return None,
    };
    Some(vec![0x1b, b'O', f])
}

/// xterm modifier parameter: `1 + Shift + 2·Alt + 4·Ctrl + 8·Super`. `None` when
/// no modifiers are held (so the base sequence is emitted without it).
fn modifier_param(mods: ModifiersState) -> Option<u8> {
    let mut n = 0u8;
    if mods.shift_key() {
        n += 1;
    }
    if mods.alt_key() {
        n += 2;
    }
    if mods.control_key() {
        n += 4;
    }
    if mods.super_key() {
        n += 8;
    }
    (n != 0).then_some(n + 1)
}

/// Cursor/edit keys: `CSI 1 ; <mod> <final>` when modified, else `SS3 <final>`
/// in application-cursor mode or `CSI <final>` otherwise.
fn cursor_seq(final_byte: u8, field: Option<&str>, app_cursor: bool) -> Vec<u8> {
    match field {
        Some(f) => {
            let mut v = format!("\x1b[1;{f}").into_bytes();
            v.push(final_byte);
            v
        }
        None if app_cursor => vec![0x1b, b'O', final_byte],
        None => vec![0x1b, b'[', final_byte],
    }
}

/// `CSI <n> ~` keys (PageUp/Down, Insert/Delete, F5+): `CSI <n> ; <mod> ~` when
/// modified.
fn tilde_seq(n: u8, field: Option<&str>) -> Vec<u8> {
    match field {
        Some(f) => format!("\x1b[{n};{f}~").into_bytes(),
        None => format!("\x1b[{n}~").into_bytes(),
    }
}

/// F1–F4: `SS3 <final>` unmodified, `CSI 1 ; <mod> <final>` when modified.
fn fn_seq(final_byte: u8, field: Option<&str>) -> Vec<u8> {
    match field {
        Some(f) => {
            let mut v = format!("\x1b[1;{f}").into_bytes();
            v.push(final_byte);
            v
        }
        None => vec![0x1b, b'O', final_byte],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k_named(n: NamedKey) -> Key {
        Key::Named(n)
    }
    fn k_char(s: &str) -> Key {
        Key::Character(s.into())
    }
    const NONE: ModifiersState = ModifiersState::empty();

    #[test]
    fn plain_text_and_named_basics() {
        assert_eq!(encode(&k_char("a"), NONE, false, 0).unwrap(), b"a");
        assert_eq!(encode(&k_char("A"), ModifiersState::SHIFT, false, 0).unwrap(), b"A");
        assert_eq!(encode(&k_named(NamedKey::Enter), NONE, false, 0).unwrap(), b"\r");
        assert_eq!(encode(&k_named(NamedKey::Backspace), NONE, false, 0).unwrap(), b"\x7f");
        assert_eq!(encode(&k_named(NamedKey::Escape), NONE, false, 0).unwrap(), b"\x1b");
        assert_eq!(encode(&k_named(NamedKey::Tab), NONE, false, 0).unwrap(), b"\t");
        assert_eq!(encode(&k_named(NamedKey::Tab), ModifiersState::SHIFT, false, 0).unwrap(), b"\x1b[Z");
    }

    #[test]
    fn ctrl_and_alt_modifiers_on_text() {
        assert_eq!(encode(&k_char("c"), ModifiersState::CONTROL, false, 0).unwrap(), b"\x03"); // Ctrl-C
        assert_eq!(encode(&k_char("C"), ModifiersState::CONTROL, false, 0).unwrap(), b"\x03"); // case-insensitive
        assert_eq!(encode(&k_char("a"), ModifiersState::ALT, false, 0).unwrap(), b"\x1ba"); // Alt-a = ESC a
        // Ctrl+Alt-c → ESC + control byte
        assert_eq!(
            encode(&k_char("c"), ModifiersState::CONTROL | ModifiersState::ALT, false, 0).unwrap(),
            b"\x1b\x03"
        );
    }

    #[test]
    fn arrows_respect_app_cursor_and_modifiers() {
        assert_eq!(encode(&k_named(NamedKey::ArrowUp), NONE, false, 0).unwrap(), b"\x1b[A");
        assert_eq!(encode(&k_named(NamedKey::ArrowUp), NONE, true, 0).unwrap(), b"\x1bOA"); // DECCKM
        // Ctrl+Left → CSI 1;5 D
        assert_eq!(
            encode(&k_named(NamedKey::ArrowLeft), ModifiersState::CONTROL, false, 0).unwrap(),
            b"\x1b[1;5D"
        );
        // Shift+Up → CSI 1;2 A (modifier wins over app-cursor mode)
        assert_eq!(
            encode(&k_named(NamedKey::ArrowUp), ModifiersState::SHIFT, true, 0).unwrap(),
            b"\x1b[1;2A"
        );
    }

    #[test]
    fn function_and_tilde_keys() {
        assert_eq!(encode(&k_named(NamedKey::F1), NONE, false, 0).unwrap(), b"\x1bOP");
        assert_eq!(encode(&k_named(NamedKey::F5), NONE, false, 0).unwrap(), b"\x1b[15~");
        assert_eq!(encode(&k_named(NamedKey::F12), NONE, false, 0).unwrap(), b"\x1b[24~");
        assert_eq!(encode(&k_named(NamedKey::PageUp), NONE, false, 0).unwrap(), b"\x1b[5~");
        assert_eq!(encode(&k_named(NamedKey::Delete), NONE, false, 0).unwrap(), b"\x1b[3~");
        // Shift+F5 → CSI 15;2 ~
        assert_eq!(encode(&k_named(NamedKey::F5), ModifiersState::SHIFT, false, 0).unwrap(), b"\x1b[15;2~");
    }

    #[test]
    fn kitty_disambiguate_encodes_escape_enter_tab_backspace_as_csi_u() {
        assert_eq!(encode(&k_named(NamedKey::Escape), NONE, false, 1).unwrap(), b"\x1b[27u");
        assert_eq!(encode(&k_named(NamedKey::Enter), NONE, false, 1).unwrap(), b"\x1b[13u");
        assert_eq!(encode(&k_named(NamedKey::Tab), NONE, false, 1).unwrap(), b"\x1b[9u");
        assert_eq!(encode(&k_named(NamedKey::Backspace), NONE, false, 1).unwrap(), b"\x1b[127u");
        // Shift+Tab still takes the legacy back-tab shorthand, not CSI 9u —
        // there's no ambiguity to resolve for it.
        assert_eq!(encode(&k_named(NamedKey::Tab), ModifiersState::SHIFT, false, 1).unwrap(), b"\x1b[Z");
    }

    #[test]
    fn kitty_disambiguate_adds_modifier_param_to_csi_u() {
        assert_eq!(
            encode(&k_named(NamedKey::Escape), ModifiersState::CONTROL, false, 1).unwrap(),
            b"\x1b[27;5u"
        );
    }

    #[test]
    fn kitty_disambiguate_encodes_ctrl_letters_by_base_codepoint_not_control_byte() {
        // Ctrl+I would collide with a literal Tab (both 0x09) in legacy
        // encoding; disambiguated, it's CSI 105 (lowercase 'i') ; 5 (ctrl) u.
        assert_eq!(encode(&k_char("i"), ModifiersState::CONTROL, false, 1).unwrap(), b"\x1b[105;5u");
        // Case-insensitive, same as the legacy control-byte path.
        assert_eq!(encode(&k_char("I"), ModifiersState::CONTROL, false, 1).unwrap(), b"\x1b[105;5u");
        // Ctrl+Shift+C: shift folds into the modifier param, not the codepoint.
        assert_eq!(
            encode(&k_char("c"), ModifiersState::CONTROL | ModifiersState::SHIFT, false, 1).unwrap(),
            b"\x1b[99;6u"
        );
    }

    #[test]
    fn kitty_flags_zero_is_legacy_encoding_even_when_arg_present() {
        // Arrow keys and plain text are untouched by the disambiguate flag —
        // only the specifically ambiguous keys route through CSI u.
        assert_eq!(encode(&k_named(NamedKey::ArrowUp), NONE, false, 1).unwrap(), b"\x1b[A");
        assert_eq!(encode(&k_char("a"), NONE, false, 1).unwrap(), b"a");
    }

    #[test]
    fn numpad_application_mode_encodes_ss3() {
        // Digits map to SS3 p..y, operators to their xterm PC-keyboard slots.
        assert_eq!(encode_numpad(&k_char("0"), NONE, true).unwrap(), b"\x1bOp");
        assert_eq!(encode_numpad(&k_char("9"), NONE, true).unwrap(), b"\x1bOy");
        assert_eq!(encode_numpad(&k_char("+"), NONE, true).unwrap(), b"\x1bOk");
        assert_eq!(encode_numpad(&k_char("."), NONE, true).unwrap(), b"\x1bOn");
        assert_eq!(encode_numpad(&k_named(NamedKey::Enter), NONE, true).unwrap(), b"\x1bOM");
    }

    #[test]
    fn numpad_falls_back_when_mode_off_or_modified() {
        // Mode off: no SS3 — the caller falls through to normal text encoding.
        assert_eq!(encode_numpad(&k_char("5"), NONE, false), None);
        // Modifiers keep the legacy encoding (which carries the mod param).
        let ctrl = ModifiersState::CONTROL;
        assert_eq!(encode_numpad(&k_char("5"), ctrl, true), None);
        // A non-keypad key at the numpad location (NumLock etc.) encodes normally.
        assert_eq!(encode_numpad(&k_named(NamedKey::Home), NONE, true), None);
    }

    #[test]
    fn kitty_event_types_encode_repeat_and_release() {
        // Flag 1|2: Ctrl+A repeat and release carry the event sub-parameter.
        let ctrl = ModifiersState::CONTROL;
        let press = encode_full(&k_char("a"), ctrl, false, 3, KeyPhase::Press, None, None);
        assert_eq!(press.unwrap(), b"\x1b[97;5u");
        let rep = encode_full(&k_char("a"), ctrl, false, 3, KeyPhase::Repeat, None, None);
        assert_eq!(rep.unwrap(), b"\x1b[97;5:2u");
        let rel = encode_full(&k_char("a"), ctrl, false, 3, KeyPhase::Release, None, None);
        assert_eq!(rel.unwrap(), b"\x1b[97;5:3u");
        // Functional keys ride the mods field of their legacy form.
        let arrow = encode_full(&k_named(NamedKey::ArrowUp), NONE, false, 3, KeyPhase::Release, None, None);
        assert_eq!(arrow.unwrap(), b"\x1b[1;1:3A");
        // Without flag 2 a release is silent; a repeat is a plain press.
        assert_eq!(encode_full(&k_char("a"), ctrl, false, 1, KeyPhase::Release, None, None), None);
        let rep1 = encode_full(&k_char("a"), ctrl, false, 1, KeyPhase::Repeat, None, None);
        assert_eq!(rep1.unwrap(), b"\x1b[97;5u");
        // Plain text keys have no release encoding (that needs flag 8).
        assert_eq!(encode_full(&k_char("a"), NONE, false, 3, KeyPhase::Release, None, None), None);
    }

    #[test]
    fn kitty_alternate_and_associated_text() {
        // Flag 1|4: Ctrl+Shift+A reports base:shifted.
        let cs = ModifiersState::CONTROL | ModifiersState::SHIFT;
        let alt = encode_full(&k_char("A"), cs, false, 5, KeyPhase::Press, Some('A'), None);
        assert_eq!(alt.unwrap(), b"\x1b[97:65;6u");
        // Flag 1|16: Alt+a carries its associated text codepoints...
        let am = ModifiersState::ALT | ModifiersState::CONTROL;
        let txt = encode_full(&k_char("a"), am, false, 17, KeyPhase::Press, None, Some("a"));
        assert_eq!(txt.unwrap(), b"\x1b[97;7;97u");
        // ...but never on release, and control characters are filtered.
        let rel = encode_full(&k_char("a"), am, false, 19, KeyPhase::Release, None, Some("a"));
        assert_eq!(rel.unwrap(), b"\x1b[97;7:3u");
        let ctl = encode_full(&k_char("a"), am, false, 17, KeyPhase::Press, None, Some("\u{1}"));
        assert_eq!(ctl.unwrap(), b"\x1b[97;7u");
    }
}
