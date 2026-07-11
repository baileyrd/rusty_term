//! OSC (Operating System Command) dispatch (L08).
//!
//! Acts on a completed OSC string collected by the parser: window title (0/2),
//! icon name (1), working directory (7), hyperlinks (8), mouse pointer shape
//! (22), clipboard (52), desktop notifications (9/777), and the
//! color controls — palette (4/104) and the default fg/bg/cursor (10/11/12 and
//! their resets 110/111/112). Color *query* (`?`) forms reply to the child via
//! the parser's response buffer; sets mutate the shared [`Palette`], and default
//! fg/bg changes are mirrored into the grid so cleared regions pick up a new
//! background, and shell-integration command-lifecycle marks (133, and 633 —
//! VS Code's superset of it). Other OSC codes are recognized as well-formed
//! and ignored for now.

use super::cell::Pen;
use super::color::{Palette, format_color_spec, parse_color_spec};
use super::grid::Grid;

/// Act on the OSC string in `osc_buffer` (the bytes between `ESC ]` and its
/// `BEL`/`ST` terminator). The payload is `<code>` optionally followed by
/// `; <args>`. `palette` is the live color table; `responses` is the child-bound
/// reply channel (drained by the driver onto the PTY master).
pub(crate) fn dispatch(
    osc_buffer: &[u8],
    g: &mut Grid,
    palette: &mut Palette,
    responses: &mut Vec<u8>,
    pen: &mut Pen,
) {
    let payload = String::from_utf8_lossy(osc_buffer);
    // Most codes carry a `;`-separated argument; the reset forms (104/110/111/
    // 112) may stand alone, so the argument is optional.
    let (code, text) = match payload.split_once(';') {
        Some((c, t)) => (c, Some(t)),
        None => (payload.as_ref(), None),
    };

    match code {
        // 0 sets icon name *and* window title; 2 sets the window title.
        "0" | "2" => {
            if let Some(text) = text
                && g.title != text
            {
                g.title = text.to_string();
                #[cfg(feature = "l13")]
                rusty_term_l13::notify_resource_changed(g, rusty_term_l13::RES_TITLE, responses);
            }
        }
        // 1 sets the icon name only. We have no icon-name surface, so forward it
        // to the host terminal verbatim and let it update its own.
        "1" => {
            if text.is_some() {
                forward_to_host(osc_buffer, g);
            }
        }
        // 22 requests a mouse pointer shape, as a CSS `cursor` keyword — the
        // convention Kitty and others use. The windowed front-end maps it to a
        // platform cursor icon while the pointer is over pane content; TUI mode
        // has no pointer of its own, so this is a no-op there (harmlessly
        // ignored, the graceful-degradation path for an unaware terminal).
        "22" => {
            if let Some(text) = text {
                g.cursor_icon = (!text.is_empty()).then(|| text.to_string());
            }
        }
        // 7 reports the working directory (usually a file:// URI).
        "7" => {
            if let Some(text) = text
                && g.cwd != text
            {
                g.cwd = text.to_string();
                #[cfg(feature = "l13")]
                rusty_term_l13::notify_resource_changed(g, rusty_term_l13::RES_CWD, responses);
            }
        }
        // 8 sets/clears the active hyperlink: `8 ; params ; URI`. An empty URI
        // (the `8 ; ;` close form) ends the link.
        "8" => {
            if let Some(text) = text {
                let uri = text.split_once(';').map(|(_, u)| u).unwrap_or("");
                g.set_link(if uri.is_empty() { None } else { Some(uri) });
            }
        }
        // 52 sets the clipboard: `52 ; <selection> ; <base64>`. We have no
        // clipboard of our own, so forward set requests verbatim to the host
        // terminal, which performs the write via its own OSC 52. Query (`?`)
        // forms aren't forwarded — the reply path isn't wired.
        "52" => {
            if let Some(text) = text {
                // `<selection> ; <data>`; `?` for the data is a query.
                let data = text.rsplit(';').next().unwrap_or("");
                if data == "?" {
                    // The window backend answers from the system clipboard; the
                    // TUI relies on the host terminal for the reply.
                    g.clipboard_query = true;
                } else {
                    // Set: relay to the host (TUI) and decode for the window
                    // backend, which owns the clipboard.
                    forward_to_host(osc_buffer, g);
                    if let Some(bytes) = super::base64::decode(data.as_bytes())
                        && let Ok(s) = String::from_utf8(bytes)
                    {
                        g.clipboard_set = Some(s);
                    }
                }
            }
        }
        // 9 (iTerm2) posts a desktop notification with the given message; the
        // windowed front-end raises it and the TUI relays to the host. ConEmu
        // reuses OSC 9 with a numeric subcommand (progress, etc.) — those start
        // with a digit field and are not notifications, so they're left alone.
        "9" => {
            if let Some(body) = text {
                let mut fields = body.split(';');
                let head = fields.next().unwrap_or("");
                let conemu = !head.is_empty() && head.bytes().all(|b| b.is_ascii_digit());
                if head == "4" {
                    // ConEmu progress: `9 ; 4 ; st ; pr`. Missing/garbled
                    // fields read as 0, which clears — the safe direction.
                    let mut num =
                        || fields.next().and_then(|f| f.parse::<u8>().ok()).unwrap_or(0);
                    let (state, percent) = (num(), num());
                    g.set_progress(state, percent);
                    forward_to_host(osc_buffer, g);
                } else if !body.is_empty() && !conemu {
                    forward_to_host(osc_buffer, g);
                    g.push_notification(String::new(), body.to_string());
                }
            }
        }
        // 777 (rxvt) posts a notification: `777 ; notify ; <title> ; <body>`.
        "777" => {
            if let Some(text) = text {
                let mut it = text.splitn(3, ';');
                if it.next() == Some("notify") {
                    let title = it.next().unwrap_or("");
                    let body = it.next().unwrap_or("");
                    if !title.is_empty() || !body.is_empty() {
                        forward_to_host(osc_buffer, g);
                        g.push_notification(title.to_string(), body.to_string());
                    }
                }
            }
        }
        // 4 sets/queries palette entries: `4 ; n ; spec [ ; n ; spec ]…`.
        "4" => {
            if let Some(text) = text {
                set_palette_entries(text, palette, responses);
            }
        }
        // 104 resets palette entries: listed indices, or all when none given.
        "104" => {
            let indices: Vec<usize> = text
                .map(|t| t.split(';').filter_map(|s| s.parse().ok()).collect())
                .unwrap_or_default();
            palette.reset_colors(&indices);
        }
        // 10/11/12 set/query the default fg / bg / cursor. Specs cascade to the
        // later roles, so `10 ; fg ; bg` also sets the background (per xterm).
        "10" => set_dynamic_colors(10, text, g, palette, responses, pen),
        "11" => set_dynamic_colors(11, text, g, palette, responses, pen),
        "12" => set_dynamic_colors(12, text, g, palette, responses, pen),
        // 110/111/112 reset the default fg / bg / cursor to the built-in values.
        // A pen currently sitting on the old default follows the reset, so text
        // drawn next uses the restored color.
        "110" => {
            let old = palette.fg;
            palette.reset_fg();
            if pen.fg == old {
                pen.fg = palette.fg;
            }
            g.set_default_colors(palette.fg, palette.bg, palette.cursor);
        }
        "111" => {
            let old = palette.bg;
            palette.reset_bg();
            if pen.bg == old {
                pen.bg = palette.bg;
            }
            g.set_default_colors(palette.fg, palette.bg, palette.cursor);
        }
        "112" => {
            palette.reset_cursor();
            g.set_default_colors(palette.fg, palette.bg, palette.cursor);
        }
        // 133 (shell integration / FinalTerm): A=prompt start, B=prompt end,
        // C=command output start, D[;exit]=command end. We record prompt starts
        // for prompt-to-prompt scrollback navigation and, under `l13`, surface
        // the command-end exit code as the `terminal://exit` resource (pushing a
        // change notification to any subscriber). (Emitters live in
        // extra/shell-integration/.)
        "133" => {
            if let Some(text) = text {
                let mut parts = text.split(';');
                if let Some(sub) = parts.next() {
                    mark_command_lifecycle(sub, &mut parts, g, responses);
                }
            }
        }
        // 633 (VS Code shell integration) is a superset of 133: the same
        // A/B/C/D command-lifecycle letters have identical effects (VS Code's
        // own docs describe 633 as "a superset of the FinalTerm shell
        // integration sequences"), plus VS Code-specific property reports we
        // selectively honor — `P;Cwd=<path>` mirrors OSC 7's cwd tracking, the
        // one property with an existing surface to update. Other 633
        // subcommands (command-line text, IsWindows hints, …) are recognized
        // as well-formed and ignored.
        "633" => {
            if let Some(text) = text {
                let mut parts = text.split(';');
                if let Some(sub) = parts.next()
                    && !mark_command_lifecycle(sub, &mut parts, g, responses)
                    && sub == "P"
                    && let Some(cwd) = parts.next().and_then(|p| p.strip_prefix("Cwd="))
                    && g.cwd != cwd
                {
                    g.cwd = cwd.to_string();
                    #[cfg(feature = "l13")]
                    rusty_term_l13::notify_resource_changed(g, rusty_term_l13::RES_CWD, responses);
                }
            }
        }
        // 1337 (iTerm2) multiplexes several subcommands; we handle inline images
        // (`File=<args>:<base64>`) and ignore the rest.
        "1337" => {
            if let Some(text) = text {
                super::iterm::feed(text, g);
            }
        }
        _ => {}
    }
}

/// Apply the command-lifecycle mark `sub` (`A` prompt start, `C` command
/// output start, `D[;exit]` command end) shared by OSC 133 and OSC 633's
/// superset of it. `parts` is the remaining `;`-separated fields after `sub`
/// (only `D` consumes one, for the exit code, under `l13`). Returns whether
/// `sub` was one of these letters, so a 633-specific fallback (its `P`
/// property reports) only runs for subcommands that aren't already a
/// lifecycle mark. `C`/`D` drive two independent consumers: the `l13`
/// feature's command-output capture (for the `terminal://command` MCP
/// resource) and, under `gui`, the fold-block range tracked for the
/// scrollback-folding feature — neither depends on the other being enabled.
#[cfg_attr(not(feature = "l13"), allow(unused_variables, clippy::ptr_arg))]
fn mark_command_lifecycle(
    sub: &str,
    parts: &mut std::str::Split<'_, char>,
    g: &mut Grid,
    responses: &mut Vec<u8>,
) -> bool {
    match sub {
        "A" => {
            g.mark_prompt();
            true
        }
        "C" => {
            #[cfg(feature = "l13")]
            g.command_output_begin();
            #[cfg(any(test, feature = "gui"))]
            {
                g.fold_output_begin();
                g.command_timer_begin();
            }
            true
        }
        "D" => {
            let exit = parts.next().and_then(|s| s.parse::<i32>().ok());
            let _ = exit; // consumed only by the cfg-gated arms below
            #[cfg(feature = "l13")]
            {
                g.command_finished(exit);
                rusty_term_l13::notify_command_finished(g, exit, responses);
                rusty_term_l13::notify_resource_changed(g, rusty_term_l13::RES_COMMAND, responses);
            }
            #[cfg(any(test, feature = "gui"))]
            {
                g.fold_output_end();
                g.command_timer_end(exit);
            }
            true
        }
        _ => false,
    }
}

/// Forward a complete OSC string to the host terminal verbatim (re-wrapped in
/// `ESC ]` … `BEL`), via the grid's host-bound queue.
fn forward_to_host(osc_buffer: &[u8], g: &mut Grid) {
    g.host_out.push(0x1b);
    g.host_out.push(b']');
    g.host_out.extend_from_slice(osc_buffer);
    g.host_out.push(0x07);
}

/// Apply an OSC 4 argument string: `n ; spec` pairs, where `spec` is `?` (query
/// the current value) or an X11 color spec (set it).
fn set_palette_entries(text: &str, palette: &mut Palette, responses: &mut Vec<u8>) {
    let mut it = text.split(';');
    while let (Some(idx), Some(spec)) = (it.next(), it.next()) {
        let Ok(n) = idx.parse::<usize>() else {
            continue;
        };
        if spec == "?" {
            // Reply `OSC 4 ; n ; rgb:… ST` to the child that queried.
            responses.extend_from_slice(b"\x1b]4;");
            responses.extend_from_slice(idx.as_bytes());
            responses.push(b';');
            responses.extend_from_slice(format_color_spec(palette.index(n)).as_bytes());
            responses.extend_from_slice(b"\x1b\\");
        } else if let Some(rgb) = parse_color_spec(spec) {
            palette.set_index(n, rgb);
        }
    }
}

/// Apply an OSC 10/11/12 argument string starting at role `start` (10=fg,
/// 11=bg, 12=cursor). Each `;`-separated spec advances to the next role; `?`
/// queries, anything parseable sets. The grid's default colors are re-synced
/// afterwards so erases use any new background.
fn set_dynamic_colors(
    start: u8,
    text: Option<&str>,
    g: &mut Grid,
    palette: &mut Palette,
    responses: &mut Vec<u8>,
    pen: &mut Pen,
) {
    let Some(text) = text else { return };
    let mut role = start;
    for spec in text.split(';') {
        if role > 12 {
            break;
        }
        if spec == "?" {
            let value = match role {
                10 => palette.fg,
                11 => palette.bg,
                _ => palette.cursor,
            };
            responses.extend_from_slice(b"\x1b]");
            responses.extend_from_slice(role.to_string().as_bytes());
            responses.push(b';');
            responses.extend_from_slice(format_color_spec(value).as_bytes());
            responses.extend_from_slice(b"\x1b\\");
        } else if let Some(rgb) = parse_color_spec(spec) {
            // A pen currently showing the old default follows the change, so the
            // common "set OSC 10/11 then print" pattern recolors immediately.
            match role {
                10 => {
                    if pen.fg == palette.fg {
                        pen.fg = rgb;
                    }
                    palette.fg = rgb;
                }
                11 => {
                    if pen.bg == palette.bg {
                        pen.bg = rgb;
                    }
                    palette.bg = rgb;
                }
                _ => palette.cursor = rgb,
            }
        }
        role += 1;
    }
    g.set_default_colors(palette.fg, palette.bg, palette.cursor);
}
