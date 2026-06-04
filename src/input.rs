//! Host-stdin classification shared by both runtimes (L09 relay side).
//!
//! Raw bytes arriving on the host terminal's stdin are mostly forwarded
//! verbatim to the child, except the scrollback-browse keys (Shift+PageUp /
//! Shift+PageDown), which are intercepted here and never reach the child. The
//! split is pure (no I/O, no grid), so the runtime applies identical semantics
//! and a single unit test pins the behaviour.

/// Shift+PageUp — browse one page up into scrollback (`CSI 5 ; 2 ~`).
pub(crate) const SCROLL_UP_KEY: &[u8] = b"\x1b[5;2~";
/// Shift+PageDown — browse one page down toward the live view (`CSI 6 ; 2 ~`).
pub(crate) const SCROLL_DN_KEY: &[u8] = b"\x1b[6;2~";
/// Ctrl+Shift+PageUp — jump to the previous shell prompt (`CSI 5 ; 6 ~`).
pub(crate) const PROMPT_PREV_KEY: &[u8] = b"\x1b[5;6~";
/// Ctrl+Shift+PageDown — jump to the next shell prompt (`CSI 6 ; 6 ~`).
pub(crate) const PROMPT_NEXT_KEY: &[u8] = b"\x1b[6;6~";

/// A scrollback-view movement requested by an intercepted key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scroll {
    /// Page the viewport up into history.
    Up,
    /// Page the viewport down toward the live bottom.
    Down,
    /// Jump to the previous shell prompt (OSC 133 mark) above the view.
    PrevPrompt,
    /// Jump to the next shell prompt (OSC 133 mark) below the view.
    NextPrompt,
}

/// Split a raw stdin chunk into the bytes to forward to the child and the
/// ordered scrollback movements to apply locally.
///
/// Scroll keys are removed from the forwarded stream; every other byte is kept
/// in order. A scroll key split across two reads is not recognized and its
/// bytes are forwarded verbatim — harmless, since keypresses arrive atomically
/// in raw mode.
pub(crate) fn split_input(buf: &[u8]) -> (Vec<u8>, Vec<Scroll>) {
    let mut forward = Vec::with_capacity(buf.len());
    let mut scrolls = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        if buf[i..].starts_with(SCROLL_UP_KEY) {
            scrolls.push(Scroll::Up);
            i += SCROLL_UP_KEY.len();
        } else if buf[i..].starts_with(SCROLL_DN_KEY) {
            scrolls.push(Scroll::Down);
            i += SCROLL_DN_KEY.len();
        } else if buf[i..].starts_with(PROMPT_PREV_KEY) {
            scrolls.push(Scroll::PrevPrompt);
            i += PROMPT_PREV_KEY.len();
        } else if buf[i..].starts_with(PROMPT_NEXT_KEY) {
            scrolls.push(Scroll::NextPrompt);
            i += PROMPT_NEXT_KEY.len();
        } else {
            forward.push(buf[i]);
            i += 1;
        }
    }
    (forward, scrolls)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_input_is_forwarded_unchanged() {
        let (forward, scrolls) = split_input(b"ls -la\r");
        assert_eq!(forward, b"ls -la\r");
        assert!(scrolls.is_empty());
    }

    #[test]
    fn scroll_keys_are_intercepted_not_forwarded() {
        let (forward, scrolls) = split_input(SCROLL_UP_KEY);
        assert!(forward.is_empty(), "a scroll key must not reach the child");
        assert_eq!(scrolls, vec![Scroll::Up]);

        let (forward, scrolls) = split_input(SCROLL_DN_KEY);
        assert!(forward.is_empty());
        assert_eq!(scrolls, vec![Scroll::Down]);
    }
    #[test]
    fn prompt_nav_keys_are_intercepted_not_forwarded() {
        let (forward, scrolls) = split_input(PROMPT_PREV_KEY);
        assert!(
            forward.is_empty(),
            "a prompt-nav key must not reach the child"
        );
        assert_eq!(scrolls, vec![Scroll::PrevPrompt]);

        let (forward, scrolls) = split_input(PROMPT_NEXT_KEY);
        assert!(forward.is_empty());
        assert_eq!(scrolls, vec![Scroll::NextPrompt]);
    }

    #[test]
    fn interleaved_keys_keep_order_and_split() {
        // "a" + ScrollUp + "b" + ScrollDown + "c"
        let mut chunk = Vec::new();
        chunk.push(b'a');
        chunk.extend_from_slice(SCROLL_UP_KEY);
        chunk.push(b'b');
        chunk.extend_from_slice(SCROLL_DN_KEY);
        chunk.push(b'c');
        let (forward, scrolls) = split_input(&chunk);
        assert_eq!(forward, b"abc");
        assert_eq!(scrolls, vec![Scroll::Up, Scroll::Down]);
    }

    #[test]
    fn partial_scroll_key_is_forwarded_verbatim() {
        // A truncated scroll key (missing the final `~`) is not a match and must
        // pass through untouched.
        let partial = &SCROLL_UP_KEY[..SCROLL_UP_KEY.len() - 1];
        let (forward, scrolls) = split_input(partial);
        assert_eq!(forward, partial);
        assert!(scrolls.is_empty());
    }
}
