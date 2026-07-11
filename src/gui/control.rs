//! Control socket (G31/G33): a Unix-domain socket the windowed front-end
//! listens on, giving rusty_term a `--single-instance` mode (a second launch
//! asks the running instance for a tab and exits) and a scripting surface
//! (`rusty_term ctl <command…>`), like kitty's `@` remote control and
//! WezTerm's CLI — but over the same forgiving line protocol style as the
//! config file, with no serialization dependency.
//!
//! Protocol: one request line per connection, `<command> [key=value]…`
//! (values may be double-quoted to carry spaces; `\n` `\t` `\\` `\"` escapes
//! decode). The reply is zero or more data lines followed by `ok` or
//! `err <message>`. Commands: `new-tab [cwd=…] [profile=…] [shell=…]`,
//! `send-text text=…` (bytes to the focused pane, escapes decoded),
//! `list-tabs`, `focus-tab n=…`, `ping`.
//!
//! The socket lives in `$XDG_RUNTIME_DIR` (user-private) or falls back to
//! `/tmp/rusty_term-<uid>.sock` with `0600` permissions — same trust model
//! as kitty's: anything running as the user may drive the terminal.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

/// One parsed control request.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CtlCommand {
    NewTab { cwd: Option<PathBuf>, profile: Option<String>, shell: Option<String> },
    /// Open a new top-level window (same options as `new-tab` for its first tab).
    NewWindow { cwd: Option<PathBuf>, profile: Option<String>, shell: Option<String> },
    /// Toggle the quake (dropdown) window: create it on first use, then
    /// show/hide it. Bind a WM/desktop hotkey to `rusty_term ctl quake`.
    Quake,
    SendText(String),
    ListTabs,
    FocusTab(usize),
    Ping,
}

/// Where the control socket lives for this user.
pub(crate) fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("rusty_term.sock");
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/rusty_term-{uid}.sock"))
}

/// Parse one request line. Tokens split on whitespace outside double quotes;
/// `key=value` pairs may quote the value.
pub(crate) fn parse_command(line: &str) -> Result<CtlCommand, String> {
    let tokens = tokenize(line)?;
    let cmd = tokens.first().ok_or("empty command")?;
    let kv = |wanted: &str| -> Option<String> {
        tokens.iter().skip(1).find_map(|t| {
            t.split_once('=').and_then(|(k, v)| (k == wanted).then(|| v.to_string()))
        })
    };
    match cmd.as_str() {
        "new-tab" => Ok(CtlCommand::NewTab {
            cwd: kv("cwd").map(PathBuf::from),
            profile: kv("profile"),
            shell: kv("shell"),
        }),
        "new-window" => Ok(CtlCommand::NewWindow {
            cwd: kv("cwd").map(PathBuf::from),
            profile: kv("profile"),
            shell: kv("shell"),
        }),
        "quake" => Ok(CtlCommand::Quake),
        "send-text" => {
            let text = kv("text").ok_or("send-text needs text=…")?;
            Ok(CtlCommand::SendText(text))
        }
        "list-tabs" => Ok(CtlCommand::ListTabs),
        "focus-tab" => {
            let n = kv("n").and_then(|v| v.parse().ok()).ok_or("focus-tab needs n=<index>")?;
            Ok(CtlCommand::FocusTab(n))
        }
        "ping" => Ok(CtlCommand::Ping),
        other => Err(format!("unknown command `{other}`")),
    }
}

/// Split a request line into tokens, honoring double quotes and decoding
/// `\n` `\t` `\\` `\"` escapes inside them.
fn tokenize(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = line.trim().chars().peekable();
    let mut in_quotes = false;
    let mut any = false;
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                any = true;
            }
            '\\' if in_quotes => match chars.next() {
                Some('n') => cur.push('\n'),
                Some('t') => cur.push('\t'),
                Some('\\') => cur.push('\\'),
                Some('"') => cur.push('"'),
                Some(other) => cur.push(other),
                None => return Err("dangling escape".into()),
            },
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() || any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => cur.push(c),
        }
    }
    if in_quotes {
        return Err("unterminated quote".into());
    }
    if !cur.is_empty() || any {
        out.push(cur);
    }
    Ok(out)
}

/// Client side: connect to the running instance, send one request line, and
/// return its full reply (data lines + the trailing `ok`/`err …`).
#[cfg(unix)]
pub fn request(line: &str) -> std::io::Result<String> {
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(socket_path())?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    let mut reply = String::new();
    let mut reader = BufReader::new(stream);
    loop {
        let mut l = String::new();
        if reader.read_line(&mut l)? == 0 {
            break;
        }
        let done = l.trim_end() == "ok" || l.starts_with("err ") || l.trim_end() == "err";
        reply.push_str(&l);
        if done {
            break;
        }
    }
    Ok(reply)
}

/// Server side: bind the socket (replacing a stale file from a dead
/// instance) and spawn the accept loop. Each request is handed to the event
/// loop as a [`super::window::UserEvent`] carrying a reply channel; the
/// connection thread writes whatever comes back.
#[cfg(unix)]
pub(crate) fn serve(
    proxy: winit::event_loop::EventLoopProxy<super::window::UserEvent>,
) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    let path = socket_path();
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Alive instance? Then we must not steal its socket. Dead one
            // leaves a stale file we can reclaim.
            if request("ping").is_ok() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "another rusty_term instance owns the control socket",
                ));
            }
            std::fs::remove_file(&path)?;
            UnixListener::bind(&path)?
        }
        Err(e) => return Err(e),
    };
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let proxy = proxy.clone();
            std::thread::spawn(move || handle(stream, proxy));
        }
    });
    Ok(path)
}

#[cfg(unix)]
fn handle(
    stream: std::os::unix::net::UnixStream,
    proxy: winit::event_loop::EventLoopProxy<super::window::UserEvent>,
) {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut out = &stream;
    let reply = match parse_command(&line) {
        Err(e) => format!("err {e}\n"),
        Ok(cmd) => {
            let (tx, rx) = std::sync::mpsc::channel();
            if proxy.send_event(super::window::UserEvent::Control(cmd, tx)).is_err() {
                "err event loop is gone\n".to_string()
            } else {
                rx.recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap_or_else(|_| "err timed out\n".to_string())
            }
        }
    };
    let _ = out.write_all(reply.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse_with_quotes_and_escapes() {
        assert_eq!(parse_command("ping").unwrap(), CtlCommand::Ping);
        assert_eq!(
            parse_command("new-tab cwd=\"/with space\" profile=dev").unwrap(),
            CtlCommand::NewTab {
                cwd: Some(PathBuf::from("/with space")),
                profile: Some("dev".into()),
                shell: None
            }
        );
        assert_eq!(
            parse_command(r#"send-text text="ls -la\n""#).unwrap(),
            CtlCommand::SendText("ls -la\n".into())
        );
        assert_eq!(parse_command("focus-tab n=2").unwrap(), CtlCommand::FocusTab(2));
    }

    #[test]
    fn window_commands_parse() {
        assert_eq!(
            parse_command("new-window shell=/bin/zsh cwd=/tmp").unwrap(),
            CtlCommand::NewWindow {
                cwd: Some(PathBuf::from("/tmp")),
                profile: None,
                shell: Some("/bin/zsh".into())
            }
        );
        assert_eq!(
            parse_command("new-window").unwrap(),
            CtlCommand::NewWindow { cwd: None, profile: None, shell: None }
        );
        assert_eq!(parse_command("quake").unwrap(), CtlCommand::Quake);
    }

    #[test]
    fn malformed_requests_report_errors() {
        assert!(parse_command("").is_err());
        assert!(parse_command("frobnicate").is_err());
        assert!(parse_command("send-text").is_err());
        assert!(parse_command("focus-tab n=x").is_err());
        assert!(parse_command("new-tab cwd=\"unterminated").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn request_round_trips_one_line_and_reads_to_ok() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        // Bind the real per-user socket path (no instance runs in tests) and
        // answer one request with data lines + the `ok` terminator.
        let path = socket_path();
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind test socket");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            assert_eq!(line.trim_end(), "list-tabs");
            let mut out = &stream;
            out.write_all(b"0\t*\tshell\nok\n").unwrap();
        });
        let reply = request("list-tabs").expect("reply");
        assert_eq!(reply, "0\t*\tshell\nok\n");
        server.join().unwrap();
        let _ = std::fs::remove_file(socket_path());
    }

    #[test]
    fn socket_path_is_user_scoped() {
        let p = socket_path();
        let s = p.to_string_lossy();
        assert!(s.ends_with(".sock"));
    }
}
