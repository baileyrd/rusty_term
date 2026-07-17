//! WebSocket PTY bridge for the Nebula web frontend (`web/`).
//!
//! A small server that spawns a shell through the platform [`Backend`] and
//! shuttles bytes between its PTY and a browser `WebSocket` — the "later
//! phase" the web prototype's `TerminalTransport` was shaped around. Like
//! the rest of the tree it adds no dependencies: the RFC 6455 handshake and
//! framing are hand-rolled in [`ws`] (SHA-1 included), the HTTP part is the
//! two lines a websocket upgrade actually needs, and the async runtime is
//! the tokio the terminal already links.
//!
//! ## Wire protocol (also documented in `web/README.md`)
//!
//! Text frames carry control, binary frames carry PTY bytes:
//!
//! * client → server, first frame: `start <cols> <rows>` — spawns the shell.
//! * client → server: `resize <cols> <rows>` — [`BackendHandle::set_winsize`].
//! * client → server: `cwd <path>` — the shell's OSC 7 working directory,
//!   relayed by the page so the stats push can carry git facts for it.
//! * client → server: `ping <token>` → server: `pong <token>` — an
//!   application-level RTT probe (browsers can't send WS Ping frames).
//! * client → server, binary: keystrokes/pastes, written to the PTY verbatim.
//! * server → client, binary: PTY output, verbatim.
//! * server → client, text: `stats <json>` every [`STATS_INTERVAL`] — system
//!   load, memory pressure, and the cwd's git branch/counts (see
//!   [`stats::stats_json`] for the shape; fields the host can't provide are
//!   `null`).
//! * server → client, text: `exit <code>` when the shell exits (code per
//!   [`BackendHandle::reap_exit_status`]: the exit code, or 128+signal),
//!   followed by a Close frame.
//!
//! ## Security posture
//!
//! This hands a shell to whoever can complete a websocket handshake, so it
//! binds `127.0.0.1` by default and refuses browser origins other than
//! localhost (see [`ws::origin_allowed`]) — a random web page you happen to
//! have open must not be able to drive a PTY on your machine. Exposing it
//! beyond localhost is deliberately not a flag; put a real reverse proxy
//! with authentication in front instead.

mod stats;
mod ws;

use std::io::{Error, ErrorKind};
use std::path::PathBuf;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::backend::{Backend, BackendHandle};

#[cfg(unix)]
use crate::backend::UnixBackend as PlatformBackend;
#[cfg(windows)]
use crate::backend::WindowsBackend as PlatformBackend;

/// Where the bridge listens when `--listen` isn't given. Loopback only —
/// see the module docs' security posture.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7703";

/// How often each session pushes a `stats <json>` frame.
const STATS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Ceiling on how long the HTTP-upgrade handshake may take. The header-block
/// read loop below is bounded in *size* (8192 bytes) but not otherwise in
/// time, so a peer trickling one byte occasionally could otherwise hold a
/// spawned task (and its socket) open indefinitely — a loopback-only
/// slowloris variant. Ten seconds is generous for a same-machine handshake.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Bridge configuration, filled from the binary's CLI flags.
#[derive(Clone, Debug)]
pub struct BridgeConfig {
    /// `address:port` to bind (default [`DEFAULT_LISTEN`]).
    pub listen: String,
    /// Shell to spawn per session, `None` for the platform default
    /// (`$SHELL` / `%COMSPEC%`).
    pub shell: Option<String>,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        BridgeConfig { listen: DEFAULT_LISTEN.to_string(), shell: None }
    }
}

/// Run the bridge until the process is killed: bind, accept, one independent
/// PTY session per websocket connection.
pub fn run(cfg: BridgeConfig) -> Result<(), Error> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let listener = TcpListener::bind(&cfg.listen).await?;
        eprintln!(
            "rusty_term web bridge: listening on ws://{} (shell: {})",
            listener.local_addr()?,
            cfg.shell.as_deref().unwrap_or("platform default"),
        );
        loop {
            let (stream, peer) = listener.accept().await?;
            let cfg = cfg.clone();
            tokio::spawn(async move {
                if let Err(e) = session(stream, cfg).await {
                    eprintln!("rusty_term web bridge: session {peer}: {e}");
                }
            });
        }
    })
}

/// What the PTY reader thread hands the session task.
enum PtyEvent {
    Data(Vec<u8>),
    Eof,
}

/// Drive one websocket connection: handshake, `start`, then shuttle bytes
/// until either side goes away. Dropping the PTY handle on any exit path
/// hangs up the child's terminal, so an abandoned session doesn't leak a
/// shell.
async fn session(mut stream: TcpStream, cfg: BridgeConfig) -> Result<(), Error> {
    // --- HTTP upgrade. Read the header block (bounded in size and time),
    // validate, 101. ---
    let head = match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_handshake_head(&mut stream)).await
    {
        Ok(Ok(Some(head))) => head,
        Ok(Ok(None)) => return refuse(&mut stream, "431 Request Header Fields Too Large").await,
        Ok(Err(e)) => return Err(e),
        // The peer never finished sending a handshake within the budget —
        // drop the connection rather than holding the task open forever.
        Err(_) => return Err(Error::new(ErrorKind::TimedOut, "handshake timed out")),
    };
    let head = String::from_utf8_lossy(&head);
    let Some((key, origin)) = ws::parse_upgrade(&head) else {
        return refuse(&mut stream, "400 Bad Request").await;
    };
    if !ws::origin_allowed(origin.as_deref()) {
        return refuse(&mut stream, "403 Forbidden").await;
    }
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        ws::accept_key(&key)
    );
    stream.write_all(response.as_bytes()).await?;

    // --- Framed session. ---
    let (mut rd, mut wr) = stream.into_split();
    let mut buf: Vec<u8> = Vec::new();
    let mut handle: Option<Box<dyn BackendHandle>> = None;
    // Keystrokes/pastes are handed off to spawn_writer's dedicated thread
    // rather than written here directly — see its doc comment.
    let mut write_tx: Option<mpsc::UnboundedSender<Vec<u8>>> = None;
    let (tx, mut rx) = mpsc::unbounded_channel::<PtyEvent>();
    // Stats side-channel state: the cwd the client relays from OSC 7, and
    // the per-session git cache behind it.
    let mut cwd: Option<PathBuf> = None;
    let mut git_cache = stats::GitCache::new();
    let mut stats_tick = tokio::time::interval(STATS_INTERVAL);
    stats_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // Drain complete frames before reading more from the socket.
        while let Some((frame, used)) = ws::decode_frame(&buf).map_err(protocol_err)? {
            buf.drain(..used);
            match frame.opcode {
                ws::Opcode::Text => {
                    let text = String::from_utf8_lossy(&frame.payload).into_owned();
                    match parse_command(&text) {
                        Some(Command::Start(cols, rows)) if handle.is_none() => {
                            let h = spawn_session_shell(&cfg, cols, rows)?;
                            spawn_reader(h.try_clone()?, tx.clone());
                            let (wtx, wrx) = mpsc::unbounded_channel::<Vec<u8>>();
                            spawn_writer(h.try_clone()?, wrx);
                            write_tx = Some(wtx);
                            handle = Some(h);
                        }
                        Some(Command::Resize(cols, rows)) => {
                            if let Some(h) = handle.as_mut() {
                                let _ = h.set_winsize(cols, rows);
                            }
                        }
                        Some(Command::Cwd(path)) => cwd = Some(path),
                        Some(Command::Ping(token)) => {
                            let pong = format!("pong {token}");
                            wr.write_all(&ws::encode_frame(ws::Opcode::Text, pong.as_bytes()))
                                .await?;
                        }
                        // A second `start`, or anything unrecognized: a
                        // client bug. Close rather than guess.
                        _ => {
                            wr.write_all(&ws::close_frame(1002)).await?;
                            return Ok(());
                        }
                    }
                }
                ws::Opcode::Binary => {
                    // Handed off to spawn_writer's dedicated thread rather
                    // than written here: frames run up to MAX_FRAME (1 MB —
                    // a large paste, not just a keystroke), and writing that
                    // directly would block whichever tokio worker thread
                    // services this session, stalling any other session
                    // sharing it. A write failure surfaces independently
                    // through the reader thread's EOF once the PTY is gone.
                    if let Some(tx) = write_tx.as_ref() {
                        let _ = tx.send(frame.payload);
                    }
                }
                ws::Opcode::Ping => {
                    wr.write_all(&ws::encode_frame(ws::Opcode::Pong, &frame.payload)).await?;
                }
                ws::Opcode::Pong => {}
                ws::Opcode::Close => {
                    let _ = wr.write_all(&ws::close_frame(1000)).await;
                    return Ok(());
                }
                // Vanilla browser WebSockets fragment only huge messages,
                // which our size cap refuses anyway; treat as protocol error.
                ws::Opcode::Continuation => {
                    wr.write_all(&ws::close_frame(1002)).await?;
                    return Ok(());
                }
            }
        }

        let mut chunk = [0u8; 4096];
        tokio::select! {
            read = rd.read(&mut chunk) => {
                let n = read?;
                if n == 0 {
                    return Ok(()); // client went away; drop hangs up the PTY
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            event = rx.recv() => match event {
                Some(PtyEvent::Data(data)) => {
                    wr.write_all(&ws::encode_frame(ws::Opcode::Binary, &data)).await?;
                }
                Some(PtyEvent::Eof) | None => {
                    return finish(&mut wr, handle.as_mut()).await;
                }
            },
            _ = stats_tick.tick() => {
                let git = match &cwd {
                    Some(dir) => git_cache.info(dir),
                    None => stats::GitInfo::default(),
                };
                let json = stats::stats_json(
                    stats::system_load(),
                    stats::memory_used(),
                    cwd.as_deref(),
                    &git,
                );
                let msg = format!("stats {json}");
                wr.write_all(&ws::encode_frame(ws::Opcode::Text, msg.as_bytes())).await?;
            }
        }
    }
}

/// The shell exited: reap its status, tell the client, close cleanly.
async fn finish(
    wr: &mut OwnedWriteHalf,
    handle: Option<&mut Box<dyn BackendHandle>>,
) -> Result<(), Error> {
    let code = handle.and_then(|h| h.reap_exit_status()).unwrap_or(0);
    let msg = format!("exit {code}");
    wr.write_all(&ws::encode_frame(ws::Opcode::Text, msg.as_bytes())).await?;
    wr.write_all(&ws::close_frame(1000)).await?;
    Ok(())
}

/// Answer a bad handshake with a bare HTTP error and close.
async fn refuse(stream: &mut TcpStream, status: &str) -> Result<(), Error> {
    let _ = stream.write_all(format!("HTTP/1.1 {status}\r\n\r\n").as_bytes()).await;
    Ok(())
}

/// Read the HTTP upgrade request's header block a byte at a time, up to and
/// including the blank line that ends it. `Ok(None)` means the block grew
/// past 8192 bytes without ending (caller sends 431); real read errors
/// (including a clean EOF mid-handshake) propagate as `Err`. Bounded only in
/// *size* here — [`session`] wraps the call in [`HANDSHAKE_TIMEOUT`] to bound
/// it in time too.
async fn read_handshake_head(stream: &mut TcpStream) -> Result<Option<Vec<u8>>, Error> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 {
            return Ok(None);
        }
        if stream.read(&mut byte).await? == 0 {
            return Err(Error::new(ErrorKind::UnexpectedEof, "closed during handshake"));
        }
        head.push(byte[0]);
    }
    Ok(Some(head))
}

fn protocol_err(e: ws::FrameError) -> Error {
    Error::new(ErrorKind::InvalidData, format!("websocket protocol error: {e:?}"))
}

/// Spawn the session's shell sized `cols × rows`.
fn spawn_session_shell(
    cfg: &BridgeConfig,
    cols: u16,
    rows: u16,
) -> Result<Box<dyn BackendHandle>, Error> {
    PlatformBackend.spawn_shell(cols, rows, cfg.shell.as_deref(), &[], None)
}

/// Blocking PTY reader on its own thread (reads have no async form — the
/// windowed front-end runs the same loop): forward output until EOF/error.
fn spawn_reader(mut clone: Box<dyn BackendHandle>, tx: mpsc::UnboundedSender<PtyEvent>) {
    std::thread::spawn(move || {
        loop {
            match clone.read() {
                Ok(data) if data.is_empty() => break,
                Ok(data) => {
                    if tx.send(PtyEvent::Data(data)).is_err() {
                        return; // session task gone; nothing to notify
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(PtyEvent::Eof);
    });
}

/// Blocking PTY writer on its own thread, fed by a channel from the async
/// session task (mirrors [`spawn_reader`]'s split). A single frame can carry
/// up to `MAX_FRAME` (1 MB — a large paste, not just a keystroke), and
/// writing that inline in the session task's `select!` would block whichever
/// tokio worker thread services it for the duration, stalling any other
/// session sharing that thread. Ends quietly once the channel closes (the
/// session task is done with this connection) or a write fails (the PTY
/// went away — the reader thread's EOF already tells the session task that
/// independently).
fn spawn_writer(mut clone: Box<dyn BackendHandle>, mut rx: mpsc::UnboundedReceiver<Vec<u8>>) {
    std::thread::spawn(move || {
        while let Some(data) = rx.blocking_recv() {
            if clone.write(&data).is_err() {
                break;
            }
        }
    });
}

/// A parsed client control message (text frame).
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Start(u16, u16),
    Resize(u16, u16),
    /// The shell's OSC 7 working directory, relayed by the page (already
    /// URI-decoded to a plain path client-side).
    Cwd(PathBuf),
    /// RTT probe; the token echoes back verbatim in a `pong`.
    Ping(String),
}

/// Parse a control frame. `start`/`resize` carry two dimensions, clamped to
/// something a terminal can be (so a hostile client can't request a
/// pathological grid); `cwd` takes the rest of the line as a path; `ping` a
/// single opaque token.
fn parse_command(text: &str) -> Option<Command> {
    let (verb, rest) = match text.split_once(' ') {
        Some((v, r)) => (v, r.trim()),
        None => (text, ""),
    };
    match verb {
        "start" | "resize" => {
            let mut parts = rest.split_ascii_whitespace();
            let cols: u16 = parts.next()?.parse().ok()?;
            let rows: u16 = parts.next()?.parse().ok()?;
            if parts.next().is_some() {
                return None;
            }
            let (cols, rows) = (cols.clamp(2, 1000), rows.clamp(2, 1000));
            match verb {
                "start" => Some(Command::Start(cols, rows)),
                _ => Some(Command::Resize(cols, rows)),
            }
        }
        // Bound the path (it lands in filesystem walks) and require it to be
        // absolute — OSC 7 always reports one.
        "cwd" if !rest.is_empty() && rest.len() < 4096 && rest.starts_with('/') => {
            Some(Command::Cwd(PathBuf::from(rest)))
        }
        "ping" if !rest.is_empty() && rest.len() < 64 && !rest.contains(char::is_whitespace) => {
            Some(Command::Ping(rest.to_string()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A [`BackendHandle`] double for the writer-thread test: `write`
    /// simulates a slow PTY (a sleep) and records what arrived, in order,
    /// into a shared log — no real child process involved.
    struct MockHandle {
        log: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        write_delay: std::time::Duration,
    }

    impl BackendHandle for MockHandle {
        fn read(&mut self) -> Result<Vec<u8>, Error> {
            Ok(Vec::new())
        }
        fn write(&mut self, data: &[u8]) -> Result<(), Error> {
            std::thread::sleep(self.write_delay);
            self.log.lock().unwrap().push(data.to_vec());
            Ok(())
        }
        fn try_clone(&self) -> Result<Box<dyn BackendHandle>, Error> {
            Ok(Box::new(MockHandle { log: self.log.clone(), write_delay: self.write_delay }))
        }
        fn set_winsize(&mut self, _cols: u16, _rows: u16) -> Result<(), Error> {
            Ok(())
        }
        #[cfg(unix)]
        fn pty_fd(&self) -> std::os::unix::io::RawFd {
            -1
        }
    }

    /// Routing writes through spawn_writer's channel must decouple the
    /// caller from a slow underlying write — the exact property this fix
    /// exists for (a large paste's write no longer blocks the tokio worker
    /// thread the async session task runs on).
    #[tokio::test]
    async fn writer_thread_decouples_slow_writes_from_the_caller() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let handle: Box<dyn BackendHandle> = Box::new(MockHandle {
            log: log.clone(),
            write_delay: std::time::Duration::from_millis(200),
        });
        let (wtx, wrx) = mpsc::unbounded_channel::<Vec<u8>>();
        spawn_writer(handle, wrx);

        let start = std::time::Instant::now();
        wtx.send(b"hello".to_vec()).unwrap();
        wtx.send(b"world".to_vec()).unwrap();
        // Sending must return immediately regardless of how long the
        // background thread's writes take.
        assert!(
            start.elapsed() < std::time::Duration::from_millis(50),
            "send() blocked on the (200ms-per-write) mock"
        );

        // Give the background thread time to drain both writes, then check
        // they arrived, and in order.
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert_eq!(*log.lock().unwrap(), vec![b"hello".to_vec(), b"world".to_vec()]);
    }

    /// A peer that never finishes the header block must eventually be cut
    /// off rather than holding the read open forever — the mechanism
    /// `session()` relies on ([`HANDSHAKE_TIMEOUT`] wrapping this same call)
    /// to bound a loopback-only slowloris variant. Uses a short test-local
    /// timeout rather than the production constant so the test itself stays
    /// fast.
    #[tokio::test]
    async fn handshake_read_times_out_on_a_stalled_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client = tokio::spawn(async move {
            let mut sock = TcpStream::connect(addr).await.unwrap();
            // Send part of a request line, then stall — never the blank
            // line that would end the header block.
            sock.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        });

        let (mut server_stream, _) = listener.accept().await.unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            read_handshake_head(&mut server_stream),
        )
        .await;
        assert!(result.is_err(), "a stalled peer must time out rather than hang forever");

        client.abort();
    }

    #[test]
    fn commands_parse_and_clamp() {
        assert_eq!(parse_command("start 80 24"), Some(Command::Start(80, 24)));
        assert_eq!(parse_command("resize 132 43"), Some(Command::Resize(132, 43)));
        assert_eq!(parse_command("start 1 90000"), None, "u16 overflow refused");
        assert_eq!(parse_command("start 1 0"), Some(Command::Start(2, 2)), "floor clamp");
        assert_eq!(parse_command("resize 4000 4000"), Some(Command::Resize(1000, 1000)));
        assert_eq!(parse_command("start 80"), None);
        assert_eq!(parse_command("start 80 24 zsh"), None, "no trailing args");
        assert_eq!(parse_command("kill 1 2"), None);
        assert_eq!(parse_command(""), None);
    }

    #[test]
    fn cwd_and_ping_commands_parse_with_bounds() {
        assert_eq!(
            parse_command("cwd /home/user/my project"),
            Some(Command::Cwd(PathBuf::from("/home/user/my project"))),
            "paths keep their spaces"
        );
        assert_eq!(parse_command("cwd relative/path"), None, "must be absolute");
        assert_eq!(parse_command("cwd"), None);
        assert_eq!(parse_command(&format!("cwd /{}", "a".repeat(5000))), None, "bounded");
        assert_eq!(parse_command("ping 1752712345"), Some(Command::Ping("1752712345".into())));
        assert_eq!(parse_command("ping a b"), None, "one token only");
        assert_eq!(parse_command("ping"), None);
    }
}
