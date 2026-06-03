//! Native keyboard encoding: winit key events → terminal byte sequences.
//!
//! In TUI mode the host terminal does this and we relay; with our own window we
//! finally encode keys ourselves (the native side of L09). This maps winit's
//! logical keys + modifiers to the conventional xterm byte sequences, honoring
//! DECCKM (application cursor keys) and xterm modifier parameters.

use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Encode a key press to the bytes to write to the PTY, or `None` if the key
/// produces no input (e.g. a lone modifier). `app_cursor` is DECCKM state.
pub(crate) fn encode(key: &Key, mods: ModifiersState, app_cursor: bool) -> Option<Vec<u8>> {
    match key {
        Key::Named(named) => encode_named(*named, mods, app_cursor),
        Key::Character(s) => Some(encode_text(s, mods)),
        _ => None,
    }
}

/// Encode printable text, applying Ctrl (→ control byte) and Alt (→ ESC prefix).
/// Shift is already reflected in the text winit produced.
fn encode_text(text: &str, mods: ModifiersState) -> Vec<u8> {
    if mods.control_key()
        && let Some(c) = text.chars().next()
        && c.is_ascii()
    {
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

fn encode_named(named: NamedKey, mods: ModifiersState, app_cursor: bool) -> Option<Vec<u8>> {
    let m = modifier_param(mods);
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
        assert_eq!(encode(&k_char("a"), NONE, false).unwrap(), b"a");
        assert_eq!(encode(&k_char("A"), ModifiersState::SHIFT, false).unwrap(), b"A");
        assert_eq!(encode(&k_named(NamedKey::Enter), NONE, false).unwrap(), b"\r");
        assert_eq!(encode(&k_named(NamedKey::Backspace), NONE, false).unwrap(), b"\x7f");
        assert_eq!(encode(&k_named(NamedKey::Escape), NONE, false).unwrap(), b"\x1b");
        assert_eq!(encode(&k_named(NamedKey::Tab), NONE, false).unwrap(), b"\t");
        assert_eq!(encode(&k_named(NamedKey::Tab), ModifiersState::SHIFT, false).unwrap(), b"\x1b[Z");
    }

    #[test]
    fn ctrl_and_alt_modifiers_on_text() {
        assert_eq!(encode(&k_char("c"), ModifiersState::CONTROL, false).unwrap(), b"\x03"); // Ctrl-C
        assert_eq!(encode(&k_char("C"), ModifiersState::CONTROL, false).unwrap(), b"\x03"); // case-insensitive
        assert_eq!(encode(&k_char("a"), ModifiersState::ALT, false).unwrap(), b"\x1ba"); // Alt-a = ESC a
        // Ctrl+Alt-c → ESC + control byte
        assert_eq!(
            encode(&k_char("c"), ModifiersState::CONTROL | ModifiersState::ALT, false).unwrap(),
            b"\x1b\x03"
        );
    }

    #[test]
    fn arrows_respect_app_cursor_and_modifiers() {
        assert_eq!(encode(&k_named(NamedKey::ArrowUp), NONE, false).unwrap(), b"\x1b[A");
        assert_eq!(encode(&k_named(NamedKey::ArrowUp), NONE, true).unwrap(), b"\x1bOA"); // DECCKM
        // Ctrl+Left → CSI 1;5 D
        assert_eq!(
            encode(&k_named(NamedKey::ArrowLeft), ModifiersState::CONTROL, false).unwrap(),
            b"\x1b[1;5D"
        );
        // Shift+Up → CSI 1;2 A (modifier wins over app-cursor mode)
        assert_eq!(
            encode(&k_named(NamedKey::ArrowUp), ModifiersState::SHIFT, true).unwrap(),
            b"\x1b[1;2A"
        );
    }

    #[test]
    fn function_and_tilde_keys() {
        assert_eq!(encode(&k_named(NamedKey::F1), NONE, false).unwrap(), b"\x1bOP");
        assert_eq!(encode(&k_named(NamedKey::F5), NONE, false).unwrap(), b"\x1b[15~");
        assert_eq!(encode(&k_named(NamedKey::F12), NONE, false).unwrap(), b"\x1b[24~");
        assert_eq!(encode(&k_named(NamedKey::PageUp), NONE, false).unwrap(), b"\x1b[5~");
        assert_eq!(encode(&k_named(NamedKey::Delete), NONE, false).unwrap(), b"\x1b[3~");
        // Shift+F5 → CSI 15;2 ~
        assert_eq!(encode(&k_named(NamedKey::F5), ModifiersState::SHIFT, false).unwrap(), b"\x1b[15;2~");
    }
}
