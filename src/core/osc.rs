//! OSC (Operating System Command) dispatch (L08).
//!
//! Acts on a completed OSC string collected by the parser: window title (0/2),
//! working directory (7), hyperlinks (8), and clipboard (52). Other OSC codes
//! (4, 133, …) are recognized as well-formed and ignored for now.

use super::grid::Grid;

/// Act on the OSC string in `osc_buffer` (the bytes between `ESC ]` and its
/// `BEL`/`ST` terminator). The payload is `<code> ; <text>`.
pub(crate) fn dispatch(osc_buffer: &[u8], g: &mut Grid) {
    let payload = String::from_utf8_lossy(osc_buffer);
    let Some((code, text)) = payload.split_once(';') else {
        return; // no separator — nothing actionable
    };
    match code {
        // 0 sets icon name *and* window title; 2 sets the window title.
        "0" | "2" => g.title = text.to_string(),
        // 7 reports the working directory (usually a file:// URI).
        "7" => g.cwd = text.to_string(),
        // 8 sets/clears the active hyperlink: `8 ; params ; URI`. An empty
        // URI (the `8 ; ;` close form) ends the link.
        "8" => {
            let uri = text.split_once(';').map(|(_, u)| u).unwrap_or("");
            g.set_link(if uri.is_empty() { None } else { Some(uri) });
        }
        // 52 sets the clipboard: `52 ; <selection> ; <base64>`. We have no
        // clipboard of our own yet, so forward set requests verbatim to the
        // host terminal, which performs the write via its own OSC 52. Query
        // (`?`) forms aren't forwarded — the reply path isn't wired.
        "52" => {
            let is_query = text.rsplit(';').next() == Some("?");
            if !is_query {
                g.host_out.push(0x1b);
                g.host_out.push(b']');
                g.host_out.extend_from_slice(osc_buffer);
                g.host_out.push(0x07);
            }
        }
        _ => {}
    }
}
