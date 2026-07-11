//! Native keyboard encoding: winit key events → terminal byte sequences.
//!
//! In TUI mode the host terminal does this and we relay; with our own window we
//! finally encode keys ourselves (the native side of L09). This maps winit's
//! logical keys + modifiers to the conventional xterm byte sequences, honoring
//! DECCKM (application cursor keys) and xterm modifier parameters — or, when
//! the child has requested it via the Kitty keyboard protocol (`CSI > flags
//! u`, tracked in `Grid::kitty_flags_stack`), the disambiguating `CSI u`
//! encoding instead for the keys that are otherwise ambiguous.

use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};

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

// ---------------------------------------------------------------------------
// win32-input-mode (DEC `?9001`, microsoft/terminal `win32-input-mode.md`):
// every key event — press *and* release, modifiers included — is sent as a
// serialized Win32 `KEY_EVENT_RECORD`, `CSI Vk;Sc;Uc;Kd;Cs;Rc _`, and conhost
// on the far side reconstitutes real console input records from it.

/// Win32 `ControlKeyState` bits carried in the `Cs` field. winit's
/// `ModifiersState` doesn't distinguish left/right, so the left-hand bits
/// stand in for both; lock-key states aren't observable through winit and are
/// reported unset.
const CS_LEFT_ALT: u16 = 0x0002;
const CS_LEFT_CTRL: u16 = 0x0008;
const CS_SHIFT: u16 = 0x0010;
const CS_ENHANCED: u16 = 0x0100;

/// Encode one key event as a win32-input-mode record. `None` only for keys
/// that have neither a known virtual-key code nor a character — per the spec
/// a record with `Vk`/`Sc` 0 but a real `Uc` is still meaningful, so unknown
/// physical keys that produce text fall back to that.
pub(crate) fn encode_win32(
    physical: PhysicalKey,
    logical: &Key,
    mods: ModifiersState,
    down: bool,
) -> Option<Vec<u8>> {
    let mapped = match physical {
        PhysicalKey::Code(code) => vk_sc(code),
        PhysicalKey::Unidentified(_) => None,
    };
    let uc = win32_uc(logical, mods);
    let (vk, sc, enhanced) = match mapped {
        Some(m) => m,
        None if uc != 0 => (0, 0, false),
        None => return None,
    };
    let mut cs = 0u16;
    if mods.shift_key() {
        cs |= CS_SHIFT;
    }
    if mods.control_key() {
        cs |= CS_LEFT_CTRL;
    }
    if mods.alt_key() {
        cs |= CS_LEFT_ALT;
    }
    if enhanced {
        cs |= CS_ENHANCED;
    }
    // A modifier key's own state is part of its own record (winit's
    // `ModifiersChanged` may not have fired yet when the key itself arrives).
    match vk {
        0x10 => cs = if down { cs | CS_SHIFT } else { cs & !CS_SHIFT },
        0x11 => cs = if down { cs | CS_LEFT_CTRL } else { cs & !CS_LEFT_CTRL },
        0x12 => cs = if down { cs | CS_LEFT_ALT } else { cs & !CS_LEFT_ALT },
        _ => {}
    }
    let kd = u8::from(down);
    Some(format!("\x1b[{vk};{sc};{uc};{kd};{cs};1_").into_bytes())
}

/// The `Uc` field: the UTF-16 code unit this key contributes, cooked the way
/// a Win32 console would see it (Ctrl+letter is the C0 byte, Enter is CR).
fn win32_uc(logical: &Key, mods: ModifiersState) -> u16 {
    match logical {
        Key::Named(named) => match named {
            NamedKey::Enter => 13,
            NamedKey::Tab => 9,
            NamedKey::Backspace => 8,
            NamedKey::Escape => 27,
            NamedKey::Space => 32,
            _ => 0,
        },
        Key::Character(s) => {
            let c = s.encode_utf16().next().unwrap_or(0);
            if mods.control_key() && !mods.alt_key() {
                match c {
                    // @ A–Z [ \ ] ^ _ and a–z fold to their C0 byte.
                    0x40..=0x5f | 0x61..=0x7a => c & 0x1f,
                    _ => 0,
                }
            } else {
                c
            }
        }
        _ => 0,
    }
}

/// Physical key → (virtual-key code, PC/AT set-1 scan code, enhanced-key
/// flag). The scan codes are the fixed per-key values Windows reports for a
/// standard layout — the same table winit uses in the other direction.
#[rustfmt::skip]
fn vk_sc(code: KeyCode) -> Option<(u16, u16, bool)> {
    use KeyCode as K;
    Some(match code {
        K::KeyA => (0x41, 0x1e, false), K::KeyB => (0x42, 0x30, false),
        K::KeyC => (0x43, 0x2e, false), K::KeyD => (0x44, 0x20, false),
        K::KeyE => (0x45, 0x12, false), K::KeyF => (0x46, 0x21, false),
        K::KeyG => (0x47, 0x22, false), K::KeyH => (0x48, 0x23, false),
        K::KeyI => (0x49, 0x17, false), K::KeyJ => (0x4a, 0x24, false),
        K::KeyK => (0x4b, 0x25, false), K::KeyL => (0x4c, 0x26, false),
        K::KeyM => (0x4d, 0x32, false), K::KeyN => (0x4e, 0x31, false),
        K::KeyO => (0x4f, 0x18, false), K::KeyP => (0x50, 0x19, false),
        K::KeyQ => (0x51, 0x10, false), K::KeyR => (0x52, 0x13, false),
        K::KeyS => (0x53, 0x1f, false), K::KeyT => (0x54, 0x14, false),
        K::KeyU => (0x55, 0x16, false), K::KeyV => (0x56, 0x2f, false),
        K::KeyW => (0x57, 0x11, false), K::KeyX => (0x58, 0x2d, false),
        K::KeyY => (0x59, 0x15, false), K::KeyZ => (0x5a, 0x2c, false),
        K::Digit1 => (0x31, 0x02, false), K::Digit2 => (0x32, 0x03, false),
        K::Digit3 => (0x33, 0x04, false), K::Digit4 => (0x34, 0x05, false),
        K::Digit5 => (0x35, 0x06, false), K::Digit6 => (0x36, 0x07, false),
        K::Digit7 => (0x37, 0x08, false), K::Digit8 => (0x38, 0x09, false),
        K::Digit9 => (0x39, 0x0a, false), K::Digit0 => (0x30, 0x0b, false),
        K::F1 => (0x70, 0x3b, false), K::F2 => (0x71, 0x3c, false),
        K::F3 => (0x72, 0x3d, false), K::F4 => (0x73, 0x3e, false),
        K::F5 => (0x74, 0x3f, false), K::F6 => (0x75, 0x40, false),
        K::F7 => (0x76, 0x41, false), K::F8 => (0x77, 0x42, false),
        K::F9 => (0x78, 0x43, false), K::F10 => (0x79, 0x44, false),
        K::F11 => (0x7a, 0x57, false), K::F12 => (0x7b, 0x58, false),
        K::Escape => (0x1b, 0x01, false),
        K::Backquote => (0xc0, 0x29, false),
        K::Minus => (0xbd, 0x0c, false),
        K::Equal => (0xbb, 0x0d, false),
        K::Backspace => (0x08, 0x0e, false),
        K::Tab => (0x09, 0x0f, false),
        K::BracketLeft => (0xdb, 0x1a, false),
        K::BracketRight => (0xdd, 0x1b, false),
        K::Enter => (0x0d, 0x1c, false),
        K::ControlLeft => (0x11, 0x1d, false),
        K::ControlRight => (0x11, 0x1d, true),
        K::Semicolon => (0xba, 0x27, false),
        K::Quote => (0xde, 0x28, false),
        K::ShiftLeft => (0x10, 0x2a, false),
        K::ShiftRight => (0x10, 0x36, false),
        K::Backslash => (0xdc, 0x2b, false),
        K::Comma => (0xbc, 0x33, false),
        K::Period => (0xbe, 0x34, false),
        K::Slash => (0xbf, 0x35, false),
        K::AltLeft => (0x12, 0x38, false),
        K::AltRight => (0x12, 0x38, true),
        K::Space => (0x20, 0x39, false),
        K::CapsLock => (0x14, 0x3a, false),
        K::NumLock => (0x90, 0x45, false),
        K::ScrollLock => (0x91, 0x46, false),
        K::PrintScreen => (0x2c, 0x37, true),
        K::Pause => (0x13, 0x45, false),
        K::Insert => (0x2d, 0x52, true),
        K::Delete => (0x2e, 0x53, true),
        K::Home => (0x24, 0x47, true),
        K::End => (0x23, 0x4f, true),
        K::PageUp => (0x21, 0x49, true),
        K::PageDown => (0x22, 0x51, true),
        K::ArrowUp => (0x26, 0x48, true),
        K::ArrowDown => (0x28, 0x50, true),
        K::ArrowLeft => (0x25, 0x4b, true),
        K::ArrowRight => (0x27, 0x4d, true),
        K::Numpad0 => (0x60, 0x52, false), K::Numpad1 => (0x61, 0x4f, false),
        K::Numpad2 => (0x62, 0x50, false), K::Numpad3 => (0x63, 0x51, false),
        K::Numpad4 => (0x64, 0x4b, false), K::Numpad5 => (0x65, 0x4c, false),
        K::Numpad6 => (0x66, 0x4d, false), K::Numpad7 => (0x67, 0x47, false),
        K::Numpad8 => (0x68, 0x48, false), K::Numpad9 => (0x69, 0x49, false),
        K::NumpadMultiply => (0x6a, 0x37, false),
        K::NumpadAdd => (0x6b, 0x4e, false),
        K::NumpadSubtract => (0x6d, 0x4a, false),
        K::NumpadDecimal => (0x6e, 0x53, false),
        K::NumpadDivide => (0x6f, 0x35, true),
        K::NumpadEnter => (0x0d, 0x1c, true),
        K::SuperLeft => (0x5b, 0x5b, true),
        K::SuperRight => (0x5c, 0x5c, true),
        K::ContextMenu => (0x5d, 0x5d, true),
        K::IntlBackslash => (0xe2, 0x56, false),
        _ => return None,
    })
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

    fn pk(code: KeyCode) -> PhysicalKey {
        PhysicalKey::Code(code)
    }

    #[test]
    fn win32_plain_and_shifted_letters() {
        // a: Vk=65 ('A'), Sc=0x1e, Uc='a', down, no mods.
        let b = encode_win32(pk(KeyCode::KeyA), &k_char("a"), NONE, true);
        assert_eq!(b.unwrap(), b"\x1b[65;30;97;1;0;1_");
        // Shift+A: Uc='A', SHIFT_PRESSED (0x10).
        let b = encode_win32(pk(KeyCode::KeyA), &k_char("A"), ModifiersState::SHIFT, true);
        assert_eq!(b.unwrap(), b"\x1b[65;30;65;1;16;1_");
        // Release is the same record with Kd=0.
        let b = encode_win32(pk(KeyCode::KeyA), &k_char("a"), NONE, false);
        assert_eq!(b.unwrap(), b"\x1b[65;30;97;0;0;1_");
    }

    #[test]
    fn win32_ctrl_combinations_cook_the_c0_byte() {
        // Ctrl+C: Uc=0x03, LEFT_CTRL_PRESSED (0x08).
        let ctrl = ModifiersState::CONTROL;
        let b = encode_win32(pk(KeyCode::KeyC), &k_char("c"), ctrl, true);
        assert_eq!(b.unwrap(), b"\x1b[67;46;3;1;8;1_");
        // Ctrl+Shift+A: Uc=0x01, ctrl|shift (0x18).
        let cs = ModifiersState::CONTROL | ModifiersState::SHIFT;
        let b = encode_win32(pk(KeyCode::KeyA), &k_char("A"), cs, true);
        assert_eq!(b.unwrap(), b"\x1b[65;30;1;1;24;1_");
        // Ctrl+[ folds to ESC; Ctrl+1 has no C0 mapping, Uc=0.
        let b = encode_win32(pk(KeyCode::BracketLeft), &k_char("["), ctrl, true);
        assert_eq!(b.unwrap(), b"\x1b[219;26;27;1;8;1_");
        let b = encode_win32(pk(KeyCode::Digit1), &k_char("1"), ctrl, true);
        assert_eq!(b.unwrap(), b"\x1b[49;2;0;1;8;1_");
        // Ctrl+Alt (AltGr-ish) does not cook: the char passes through.
        let ca = ModifiersState::CONTROL | ModifiersState::ALT;
        let b = encode_win32(pk(KeyCode::KeyC), &k_char("c"), ca, true);
        assert_eq!(b.unwrap(), b"\x1b[67;46;99;1;10;1_");
    }

    #[test]
    fn win32_named_and_enhanced_keys() {
        // Enter carries CR in Uc.
        let b = encode_win32(pk(KeyCode::Enter), &k_named(NamedKey::Enter), NONE, true);
        assert_eq!(b.unwrap(), b"\x1b[13;28;13;1;0;1_");
        // Arrows are enhanced keys: ENHANCED_KEY (0x100), Uc=0.
        let b = encode_win32(pk(KeyCode::ArrowLeft), &k_named(NamedKey::ArrowLeft), NONE, true);
        assert_eq!(b.unwrap(), b"\x1b[37;75;0;1;256;1_");
        let b = encode_win32(pk(KeyCode::ArrowUp), &k_named(NamedKey::ArrowUp), NONE, false);
        assert_eq!(b.unwrap(), b"\x1b[38;72;0;0;256;1_");
        // Numpad Enter is VK_RETURN with the enhanced flag.
        let b = encode_win32(pk(KeyCode::NumpadEnter), &k_named(NamedKey::Enter), NONE, true);
        assert_eq!(b.unwrap(), b"\x1b[13;28;13;1;256;1_");
        // F5, Tab, Escape, Backspace, Delete.
        let b = encode_win32(pk(KeyCode::F5), &k_named(NamedKey::F5), NONE, true);
        assert_eq!(b.unwrap(), b"\x1b[116;63;0;1;0;1_");
        let b = encode_win32(pk(KeyCode::Tab), &k_named(NamedKey::Tab), NONE, true);
        assert_eq!(b.unwrap(), b"\x1b[9;15;9;1;0;1_");
        let b = encode_win32(pk(KeyCode::Escape), &k_named(NamedKey::Escape), NONE, true);
        assert_eq!(b.unwrap(), b"\x1b[27;1;27;1;0;1_");
        let b = encode_win32(pk(KeyCode::Backspace), &k_named(NamedKey::Backspace), NONE, true);
        assert_eq!(b.unwrap(), b"\x1b[8;14;8;1;0;1_");
        let b = encode_win32(pk(KeyCode::Delete), &k_named(NamedKey::Delete), NONE, true);
        assert_eq!(b.unwrap(), b"\x1b[46;83;0;1;256;1_");
    }

    #[test]
    fn win32_modifier_keys_report_their_own_state() {
        // A lone Shift press includes SHIFT_PRESSED even if ModifiersChanged
        // hasn't fired yet; the release clears it.
        let b = encode_win32(pk(KeyCode::ShiftLeft), &k_named(NamedKey::Shift), NONE, true);
        assert_eq!(b.unwrap(), b"\x1b[16;42;0;1;16;1_");
        let b = encode_win32(pk(KeyCode::ShiftLeft), &k_named(NamedKey::Shift), ModifiersState::SHIFT, false);
        assert_eq!(b.unwrap(), b"\x1b[16;42;0;0;0;1_");
        // Right Ctrl is an enhanced key.
        let b = encode_win32(pk(KeyCode::ControlRight), &k_named(NamedKey::Control), NONE, true);
        assert_eq!(b.unwrap(), b"\x1b[17;29;0;1;264;1_");
        let b = encode_win32(pk(KeyCode::AltLeft), &k_named(NamedKey::Alt), NONE, true);
        assert_eq!(b.unwrap(), b"\x1b[18;56;0;1;2;1_");
    }

    #[test]
    fn win32_unknown_keys_fall_back_to_uc_or_nothing() {
        use winit::keyboard::NativeKeyCode;
        let unident = PhysicalKey::Unidentified(NativeKeyCode::Unidentified);
        // No VK but real text: Vk/Sc 0, Uc carries the char.
        let b = encode_win32(unident, &k_char("é"), NONE, true);
        assert_eq!(b.unwrap(), b"\x1b[0;0;233;1;0;1_");
        // No VK and no text: nothing to send.
        assert_eq!(encode_win32(unident, &k_named(NamedKey::F35), NONE, true), None);
        // Unmapped KeyCode with text still falls back.
        let b = encode_win32(pk(KeyCode::LaunchMail), &k_char("m"), NONE, true);
        assert_eq!(b.unwrap(), b"\x1b[0;0;109;1;0;1_");
    }

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
