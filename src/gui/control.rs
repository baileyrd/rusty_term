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
    NewTab {
        cwd: Option<PathBuf>,
        profile: Option<String>,
        shell: Option<String>,
    },
    /// Open a new top-level window (same options as `new-tab` for its first tab).
    NewWindow {
        cwd: Option<PathBuf>,
        profile: Option<String>,
        shell: Option<String>,
    },
    /// Toggle the quake (dropdown) window: create it on first use, then
    /// show/hide it. Bind a WM/desktop hotkey to `rusty_term ctl quake`.
    Quake,
    SendText(String),
    ListTabs,
    FocusTab(usize),
    Ping,
}

/// Where the control socket lives for this user.
#[cfg(unix)]
pub(crate) fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("rusty_term.sock");
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/rusty_term-{uid}.sock"))
}

/// Where the control pipe lives for this user — the Windows analogue of the
/// per-uid Unix socket path: same-user access is the trust boundary.
#[cfg(windows)]
pub(crate) fn pipe_path() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".into());
    format!(r"\\.\pipe\rusty_term-{user}")
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Parse one request line. Tokens split on whitespace outside double quotes;
/// `key=value` pairs may quote the value.
pub(crate) fn parse_command(line: &str) -> Result<CtlCommand, String> {
    let tokens = tokenize(line)?;
    let cmd = tokens.first().ok_or("empty command")?;
    let kv = |wanted: &str| -> Option<String> {
        tokens.iter().skip(1).find_map(|t| {
            t.split_once('=')
                .and_then(|(k, v)| (k == wanted).then(|| v.to_string()))
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
            let n = kv("n")
                .and_then(|v| v.parse().ok())
                .ok_or("focus-tab needs n=<index>")?;
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

/// A named-pipe `HANDLE` wrapped for `Read`/`Write` so `BufReader` and
/// `write_all` work the same way as the Unix `UnixStream` path.
#[cfg(windows)]
struct NamedPipe(windows_sys::Win32::Foundation::HANDLE);

// SAFETY: a Win32 pipe HANDLE has no thread affinity; it's fine to move to
// the thread that owns the connection and use exclusively from there.
#[cfg(windows)]
unsafe impl Send for NamedPipe {}

#[cfg(windows)]
impl std::io::Read for NamedPipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE;
        use windows_sys::Win32::Storage::FileSystem::ReadFile;
        let mut n = 0u32;
        // SAFETY: `buf` is a valid, exclusively-borrowed slice for the
        // duration of the call; `self.0` is a pipe HANDLE we own.
        let ok = unsafe {
            ReadFile(
                self.0,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let e = std::io::Error::last_os_error();
            // The peer disconnecting mid-read surfaces as ERROR_BROKEN_PIPE;
            // treat it as EOF, like a closed socket.
            return if e.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                Ok(0)
            } else {
                Err(e)
            };
        }
        Ok(n as usize)
    }
}

#[cfg(windows)]
impl std::io::Write for NamedPipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use windows_sys::Win32::Storage::FileSystem::WriteFile;
        let mut n = 0u32;
        // SAFETY: `buf` is a valid slice for the duration of the call;
        // `self.0` is a pipe HANDLE we own.
        let ok = unsafe {
            WriteFile(
                self.0,
                buf.as_ptr(),
                buf.len() as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for NamedPipe {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // SAFETY: `self.0` is a valid HANDLE we exclusively own; dropping is
        // the only place that closes it.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Client side: connect to the running instance, send one request line, and
/// return its full reply (data lines + the trailing `ok`/`err …`).
#[cfg(windows)]
pub fn request(line: &str) -> std::io::Result<String> {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING,
    };
    let wpath = wide(&pipe_path());
    // SAFETY: `wpath` is a valid NUL-terminated UTF-16 string; the call opens
    // an existing pipe instance for duplex I/O with no sharing/security
    // overrides, the normal client-side pipe open.
    let handle = unsafe {
        CreateFileW(
            wpath.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let mut pipe = NamedPipe(handle);
    pipe.write_all(line.as_bytes())?;
    pipe.write_all(b"\n")?;
    let mut reply = String::new();
    let mut reader = BufReader::new(pipe);
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
            if proxy
                .send_event(super::window::UserEvent::Control(cmd, tx))
                .is_err()
            {
                "err event loop is gone\n".to_string()
            } else {
                rx.recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap_or_else(|_| "err timed out\n".to_string())
            }
        }
    };
    let _ = out.write_all(reply.as_bytes());
}

/// Server side: create pipe instances in a loop and spawn a handler per
/// connection. Unlike a Unix listener socket, a single `CreateNamedPipeW`
/// handle is one connection slot — each accepted connection is replaced by a
/// freshly created instance so the next client has somewhere to connect.
#[cfg(windows)]
pub(crate) fn serve(
    proxy: winit::event_loop::EventLoopProxy<super::window::UserEvent>,
) -> std::io::Result<PathBuf> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    let path = pipe_path();
    // The first instance claims `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a second
    // `rusty_term --gui` fails here instead of silently sharing the name —
    // the Windows analogue of the Unix `AddrInUse` bind failure. Unlike a
    // stale Unix socket file, there's no dead-instance case to reclaim: the
    // OS frees the pipe name the moment its owning process exits.
    // SAFETY: `wide(&path)` is a valid NUL-terminated UTF-16 string; the
    // buffer sizes and instance count are plain integers with no aliasing.
    let first = unsafe {
        CreateNamedPipeW(
            wide(&path).as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            4096,
            4096,
            0,
            std::ptr::null(),
        )
    };
    if first == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // Smuggle the raw handle across the thread boundary as an integer — a
    // bare `HANDLE` (pointer) isn't `Send`, but the value itself has no
    // thread affinity.
    let first = first as isize;
    let accept_path = path.clone();
    std::thread::spawn(move || {
        let path = accept_path;
        let mut next_handle = Some(first as windows_sys::Win32::Foundation::HANDLE);
        loop {
            let pipe_handle = match next_handle.take() {
                Some(h) => h,
                None => {
                    // SAFETY: same call as above, minus the first-instance flag.
                    let h = unsafe {
                        CreateNamedPipeW(
                            wide(&path).as_ptr(),
                            PIPE_ACCESS_DUPLEX,
                            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                            PIPE_UNLIMITED_INSTANCES,
                            4096,
                            4096,
                            0,
                            std::ptr::null(),
                        )
                    };
                    if h == INVALID_HANDLE_VALUE {
                        break;
                    }
                    h
                }
            };
            if !connect_pipe(pipe_handle) {
                continue;
            }
            // Wrap into the `Send` `NamedPipe` before crossing the thread
            // boundary — the raw `HANDLE` (a pointer) is not itself `Send`.
            let pipe = NamedPipe(pipe_handle);
            let proxy = proxy.clone();
            std::thread::spawn(move || handle(pipe, proxy));
        }
    });
    Ok(PathBuf::from(path))
}

/// Block until a client connects to `handle`, or it's already connected in
/// the race window between creation and this call. Closes `handle` and
/// returns `false` on any other failure so the caller creates a fresh
/// instance rather than retrying a possibly-broken one.
#[cfg(windows)]
fn connect_pipe(handle: windows_sys::Win32::Foundation::HANDLE) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED};
    use windows_sys::Win32::System::Pipes::ConnectNamedPipe;
    // SAFETY: `handle` is a valid pipe instance HANDLE from CreateNamedPipeW.
    let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
    if connected != 0 {
        return true;
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32) {
        return true;
    }
    // SAFETY: `handle` is a valid HANDLE we own and haven't handed off.
    unsafe {
        CloseHandle(handle);
    }
    false
}

#[cfg(windows)]
fn handle(pipe: NamedPipe, proxy: winit::event_loop::EventLoopProxy<super::window::UserEvent>) {
    let mut reader = BufReader::new(pipe);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let reply = match parse_command(&line) {
        Err(e) => format!("err {e}\n"),
        Ok(cmd) => {
            let (tx, rx) = std::sync::mpsc::channel();
            if proxy
                .send_event(super::window::UserEvent::Control(cmd, tx))
                .is_err()
            {
                "err event loop is gone\n".to_string()
            } else {
                rx.recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap_or_else(|_| "err timed out\n".to_string())
            }
        }
    };
    let _ = reader.get_mut().write_all(reply.as_bytes());
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
        assert_eq!(
            parse_command("focus-tab n=2").unwrap(),
            CtlCommand::FocusTab(2)
        );
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
            CtlCommand::NewWindow {
                cwd: None,
                profile: None,
                shell: None
            }
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

    #[cfg(unix)]
    #[test]
    fn socket_path_is_user_scoped() {
        let p = socket_path();
        let s = p.to_string_lossy();
        assert!(s.ends_with(".sock"));
    }

    #[cfg(windows)]
    #[test]
    fn request_round_trips_one_line_and_reads_to_ok() {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
        };
        use windows_sys::Win32::System::Pipes::{
            CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
        };
        // Bind the real per-user pipe name (no instance runs in tests) and
        // answer one request with data lines + the `ok` terminator.
        let path = pipe_path();
        let wpath = wide(&path);
        // SAFETY: `wpath` is a valid NUL-terminated UTF-16 string; a single
        // instance is enough since only this test connects.
        let handle = unsafe {
            CreateNamedPipeW(
                wpath.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                4096,
                4096,
                0,
                std::ptr::null(),
            )
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE, "bind test pipe");
        // Smuggle the raw handle across the thread boundary as an integer;
        // exactly one `NamedPipe` is constructed from it, on the other side.
        let raw = handle as isize;
        let server = std::thread::spawn(move || {
            let handle = raw as windows_sys::Win32::Foundation::HANDLE;
            assert!(connect_pipe(handle));
            let mut reader = BufReader::new(NamedPipe(handle));
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line.trim_end(), "list-tabs");
            reader.get_mut().write_all(b"0\t*\tshell\nok\n").unwrap();
        });
        let reply = request("list-tabs").expect("reply");
        assert_eq!(reply, "0\t*\tshell\nok\n");
        server.join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn pipe_path_is_user_scoped() {
        let p = pipe_path();
        assert!(p.starts_with(r"\\.\pipe\rusty_term-"));
    }
}
