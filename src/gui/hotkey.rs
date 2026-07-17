//! Windows global hotkey (G30 stretch): `RegisterHotKey` plus a raw
//! message-loop hook so `quake_hotkey = "win+grave"` toggles the quake
//! window without an external binding tool. Unix desktops bind
//! `rusty_term ctl quake` at the WM/DE level instead — there's no
//! cross-desktop equivalent to hook into here.
#![cfg(windows)]

use std::cell::Cell;
use std::rc::Rc;

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{MSG, WM_HOTKEY};

/// Fixed id: this app registers at most one global hotkey.
const HOTKEY_ID: i32 = 1;

/// A parsed hotkey: Win32 `MOD_*` modifier bits and a virtual-key code.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Hotkey {
    modifiers: u32,
    vk: u32,
}

/// Parse `"win+grave"`, `"ctrl+alt+f12"`, etc.: `+`-separated tokens,
/// case-insensitive, the last of which is the key and the rest modifiers.
pub(crate) fn parse(spec: &str) -> Result<Hotkey, String> {
    let tokens: Vec<&str> = spec
        .split('+')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    let Some((key, mods)) = tokens.split_last() else {
        return Err("empty hotkey spec".to_string());
    };
    // MOD_NOREPEAT: one WM_HOTKEY per press, not one per auto-repeat tick.
    let mut modifiers = MOD_NOREPEAT;
    for m in mods {
        modifiers |= match m.to_ascii_lowercase().as_str() {
            "win" | "super" | "cmd" => MOD_WIN,
            "ctrl" | "control" => MOD_CONTROL,
            "alt" => MOD_ALT,
            "shift" => MOD_SHIFT,
            other => return Err(format!("unknown modifier `{other}`")),
        };
    }
    Ok(Hotkey {
        modifiers,
        vk: parse_key(key)?,
    })
}

/// Map a key name to its Win32 virtual-key code. Letters/digits use their
/// ASCII value directly (`VK_A..VK_Z == b'A'..=b'Z'`, `VK_0..VK_9 ==
/// b'0'..=b'9'` by Win32 convention); a handful of named keys round out the
/// set most quake-hotkey bindings actually use.
fn parse_key(key: &str) -> Result<u32, String> {
    let lower = key.to_ascii_lowercase();
    if lower.len() == 1 {
        let c = lower.as_bytes()[0];
        if c.is_ascii_alphanumeric() {
            return Ok(c.to_ascii_uppercase() as u32);
        }
    }
    if let Some(n) = lower.strip_prefix('f')
        && let Ok(n) = n.parse::<u32>()
        && (1..=24).contains(&n)
    {
        return Ok(0x70 + (n - 1)); // VK_F1 = 0x70, sequential through VK_F24
    }
    Ok(match lower.as_str() {
        "grave" | "`" => 0xC0,      // VK_OEM_3
        "space" => 0x20,            // VK_SPACE
        "tab" => 0x09,              // VK_TAB
        "escape" | "esc" => 0x1B,   // VK_ESCAPE
        "enter" | "return" => 0x0D, // VK_RETURN
        "backspace" => 0x08,        // VK_BACK
        "up" => 0x26,               // VK_UP
        "down" => 0x28,             // VK_DOWN
        "left" => 0x25,             // VK_LEFT
        "right" => 0x27,            // VK_RIGHT
        other => return Err(format!("unknown key `{other}`")),
    })
}

/// Register `hotkey` on the calling thread — must be the thread that later
/// pumps the event loop, since `RegisterHotKey`'s queue association is
/// thread-specific. Returns whether it succeeded; the caller warns and
/// carries on without a hotkey rather than treating failure as fatal.
pub(crate) fn register(hotkey: Hotkey) -> bool {
    // SAFETY: `hwnd = null` associates the hotkey with this thread's message
    // queue rather than a window; `id`/`modifiers`/`vk` are plain integers.
    let ok =
        unsafe { RegisterHotKey(std::ptr::null_mut(), HOTKEY_ID, hotkey.modifiers, hotkey.vk) };
    ok != 0
}

/// Unregister the hotkey registered by [`register`]. Call on the same
/// thread, on the way out.
pub(crate) fn unregister() {
    // SAFETY: matches the `register` call; a no-op if nothing is registered.
    unsafe {
        UnregisterHotKey(std::ptr::null_mut(), HOTKEY_ID);
    }
}

/// A `winit` `with_msg_hook` callback: sets `pressed` when `WM_HOTKEY`
/// arrives for our id. Runs synchronously on the main thread inside winit's
/// own message pump, so a plain `Cell` (no locking) is enough.
pub(crate) fn make_msg_hook(
    pressed: Rc<Cell<bool>>,
) -> impl FnMut(*const std::ffi::c_void) -> bool {
    move |msg_ptr| {
        if msg_ptr.is_null() {
            return false;
        }
        // SAFETY: `with_msg_hook` guarantees `msg_ptr` points to a valid
        // `MSG` for the duration of the callback.
        let msg = unsafe { &*msg_ptr.cast::<MSG>() };
        if msg.message == WM_HOTKEY && msg.wParam as i32 == HOTKEY_ID {
            pressed.set(true);
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifiers_and_key_case_insensitively() {
        let hk = parse("Win+Grave").unwrap();
        assert_eq!(hk.modifiers, MOD_WIN | MOD_NOREPEAT);
        assert_eq!(hk.vk, 0xC0);
    }

    #[test]
    fn parses_multiple_modifiers_and_function_key() {
        let hk = parse("ctrl+alt+f12").unwrap();
        assert_eq!(hk.modifiers, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT);
        assert_eq!(hk.vk, 0x70 + 11);
    }

    #[test]
    fn parses_bare_letter_key() {
        let hk = parse("shift+q").unwrap();
        assert_eq!(hk.modifiers, MOD_SHIFT | MOD_NOREPEAT);
        assert_eq!(hk.vk, b'Q' as u32);
    }

    #[test]
    fn rejects_unknown_modifier_and_key() {
        assert!(parse("meta+grave").is_err());
        assert!(parse("win+nosuchkey").is_err());
    }

    #[test]
    fn rejects_empty_spec() {
        assert!(parse("").is_err());
        assert!(parse("  ").is_err());
    }
}
