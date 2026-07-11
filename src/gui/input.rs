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

/// Kitty keyboard protocol flag bit: disambiguate escape codes — the only
/// enhancement level this encoder implements. The other bits (event types,
/// alternate keys, associated text) need release/repeat events and IME-layout
/// data this encoder doesn't have plumbed through; a client that requested
/// them still gets legacy-shaped input rather than a wrong answer.
const KITTY_DISAMBIGUATE: u8 = 1;

/// Encode a key press to the bytes to write to the PTY, or `None` if the key
/// produces no input (e.g. a lone modifier). `app_cursor` is DECCKM state;
/// `kitty_flags` is the child's current Kitty keyboard protocol enhancement
/// flags (`0` for legacy encoding).
pub(crate) fn encode(key: &Key, mods: ModifiersState, app_cursor: bool, kitty_flags: u8) -> Option<Vec<u8>> {
    match key {
        Key::Named(named) => encode_named(*named, mods, app_cursor, kitty_flags),
        Key::Character(s) => Some(encode_text(s, mods, kitty_flags)),
        _ => None,
    }
}

/// Encode a disambiguated key as `CSI u`: `CSI <unicode-key-code> u`, or
/// `CSI <unicode-key-code> ; <mod> u` with modifiers — the format the Kitty
/// keyboard protocol spec assigns whether or not a key has a legacy sequence.
fn kitty_u(code: u32, m: Option<u8>) -> Vec<u8> {
    match m {
        Some(n) => format!("\x1b[{code};{n}u").into_bytes(),
        None => format!("\x1b[{code}u").into_bytes(),
    }
}

/// Encode printable text, applying Ctrl (→ control byte, or `CSI u` when the
/// child asked to disambiguate it from other keys) and Alt (→ ESC prefix).
/// Shift is already reflected in the text winit produced.
fn encode_text(text: &str, mods: ModifiersState, kitty_flags: u8) -> Vec<u8> {
    if mods.control_key()
        && let Some(c) = text.chars().next()
        && c.is_ascii()
    {
        if kitty_flags & KITTY_DISAMBIGUATE != 0 {
            // The base (lowercase) letter's own codepoint, not the C0 control
            // byte — disambiguates e.g. Ctrl+I from a literal Tab keypress,
            // which both produce 0x09 in legacy encoding.
            return kitty_u(c.to_ascii_lowercase() as u32, modifier_param(mods));
        }
        // C0 control byte: 'a'/'A' → 0x01 … '_' → 0x1f, Space/@ → 0x00.
        let b = c.to_ascii_uppercase() as u8 & 0x1f;
        return if mods.alt_key() { vec![0x1b, b] } else { vec![b] };
    }
    if mods.alt_key() {
        let mut v = vec![0x1b];
        v.extend_from_slice(text.as_bytes());
        return v;
    }
    text.as_bytes().to_vec()
}

fn encode_named(named: NamedKey, mods: ModifiersState, app_cursor: bool, kitty_flags: u8) -> Option<Vec<u8>> {
    let m = modifier_param(mods);
    if kitty_flags & KITTY_DISAMBIGUATE != 0 {
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
            return Some(kitty_u(code, m));
        }
    }
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
        NamedKey::ArrowUp => cursor_seq(b'A', m, app_cursor),
        NamedKey::ArrowDown => cursor_seq(b'B', m, app_cursor),
        NamedKey::ArrowRight => cursor_seq(b'C', m, app_cursor),
        NamedKey::ArrowLeft => cursor_seq(b'D', m, app_cursor),
        NamedKey::Home => cursor_seq(b'H', m, app_cursor),
        NamedKey::End => cursor_seq(b'F', m, app_cursor),
        NamedKey::Insert => tilde_seq(2, m),
        NamedKey::Delete => tilde_seq(3, m),
        NamedKey::PageUp => tilde_seq(5, m),
        NamedKey::PageDown => tilde_seq(6, m),
        NamedKey::F1 => fn_seq(b'P', m),
        NamedKey::F2 => fn_seq(b'Q', m),
        NamedKey::F3 => fn_seq(b'R', m),
        NamedKey::F4 => fn_seq(b'S', m),
        NamedKey::F5 => tilde_seq(15, m),
        NamedKey::F6 => tilde_seq(17, m),
        NamedKey::F7 => tilde_seq(18, m),
        NamedKey::F8 => tilde_seq(19, m),
        NamedKey::F9 => tilde_seq(20, m),
        NamedKey::F10 => tilde_seq(21, m),
        NamedKey::F11 => tilde_seq(23, m),
        NamedKey::F12 => tilde_seq(24, m),
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
fn cursor_seq(final_byte: u8, m: Option<u8>, app_cursor: bool) -> Vec<u8> {
    match m {
        Some(n) => {
            let mut v = format!("\x1b[1;{n}").into_bytes();
            v.push(final_byte);
            v
        }
        None if app_cursor => vec![0x1b, b'O', final_byte],
        None => vec![0x1b, b'[', final_byte],
    }
}

/// `CSI <n> ~` keys (PageUp/Down, Insert/Delete, F5+): `CSI <n> ; <mod> ~` when
/// modified.
fn tilde_seq(n: u8, m: Option<u8>) -> Vec<u8> {
    match m {
        Some(modn) => format!("\x1b[{n};{modn}~").into_bytes(),
        None => format!("\x1b[{n}~").into_bytes(),
    }
}

/// F1–F4: `SS3 <final>` unmodified, `CSI 1 ; <mod> <final>` when modified.
fn fn_seq(final_byte: u8, m: Option<u8>) -> Vec<u8> {
    match m {
        Some(n) => {
            let mut v = format!("\x1b[1;{n}").into_bytes();
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
}
