//! Tokio runtime: a single async reactor drives the terminal on every platform.
//!
//! On **Unix** the PTY master and a fresh `/dev/tty` open are registered with
//! the reactor via [`AsyncFd`]; `SIGWINCH` arrives as a [`tokio::signal`] stream.
//! On **Windows** ConPTY's pipes are synchronous (no pollable fd), so blocking
//! reader / writer / stdin threads bridge into tokio channels and a timer polls
//! the console size for resizes. Both paths share [`run`] and the
//! [`Notify`]-driven render loop.
//!
//! Locking note: the grid uses a `parking_lot::Mutex` (sync), and a guard is
//! **never** held across an `.await` — every critical section locks, mutates or
//! snapshots, and drops the guard before any await. So a sync mutex is correct
//! here, not `tokio::sync::Mutex`.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::backend::Backend;
use crate::core::{AnsiParser, Grid};
use crate::input::{Scroll, split_input};
use crate::render::{FRAME_BUDGET, RawModeGuard, RenderState, render_once, restore_host_modes};

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use tokio::io::unix::AsyncFd;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};

#[cfg(windows)]
use std::io::Read;
#[cfg(windows)]
use std::time::Duration;

/// Signals the render loop to stop when a task ends — on its own, the task's
/// `running.store(false)` + `frame.notify_one()` only run after the loop body,
/// so a panic anywhere inside (e.g. in the untrusted-byte parser) would skip
/// them and leave the main `select!` waiting forever with the host terminal
/// stuck in raw mode. As a `Drop` guard this fires on every exit, panic
/// included. (Unix only — the Windows path bridges through blocking threads
/// and channels rather than these reactor-driven tasks.)
#[cfg(unix)]
struct ShutdownOnDrop {
    running: Arc<AtomicBool>,
    frame: Arc<Notify>,
}

#[cfg(unix)]
impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        self.frame.notify_one();
    }
}

/// A PTY master descriptor registered with the reactor that does **not** close
/// the fd on drop — ownership stays with the `BackendHandle` that produced it,
/// which closes it. `AsyncFd<FdRef>` only deregisters from the reactor on drop.
#[cfg(unix)]
struct FdRef(RawFd);

#[cfg(unix)]
impl AsRawFd for FdRef {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// Put `fd`'s open file description into non-blocking mode, as `AsyncFd`
/// requires. Dups (from `try_clone`) share the description, so one call covers
/// every clone of the master.
#[cfg(unix)]
fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: fcntl on a valid fd; no memory operands.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Open the controlling terminal as its own non-blocking descriptor.
///
/// A fresh `/dev/tty` open has an independent open file description, so the
/// `O_NONBLOCK` we need for `AsyncFd` never leaks onto the shared stdout (fds
/// 0/1/2 usually share one description). Raw mode set on stdin still applies —
/// it is a property of the tty *device*, not of any one fd.
#[cfg(unix)]
fn open_tty_nonblocking() -> std::io::Result<OwnedFd> {
    // SAFETY: opening a path; the returned fd is owned and wrapped immediately.
    let fd = unsafe {
        libc::open(
            c"/dev/tty".as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is a freshly opened descriptor we exclusively own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Read whatever is available from a non-blocking fd. `Ok(empty)` signals EOF
/// (a zero-length read, or `EIO` on a PTY master after the slave closed);
/// `EAGAIN` surfaces as `WouldBlock` so the caller's `AsyncFd` guard re-arms.
#[cfg(unix)]
fn read_nonblocking(fd: RawFd) -> std::io::Result<Vec<u8>> {
    let mut buf = [0u8; 4096];
    // SAFETY: writing into a stack buffer of the given length.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        // The shell closing the slave surfaces as EIO on the master: clean EOF.
        if err.raw_os_error() == Some(libc::EIO) {
            return Ok(Vec::new());
        }
        return Err(err);
    }
    Ok(buf[..n as usize].to_vec())
}

/// Issue one `write`, retrying `EINTR`. `EAGAIN` surfaces as `WouldBlock`.
#[cfg(unix)]
fn write_some(fd: RawFd, data: &[u8]) -> std::io::Result<usize> {
    loop {
        // SAFETY: reading `data.len()` bytes from a valid slice.
        let n = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }
        return Ok(n as usize);
    }
}

/// Write every byte of `data` to the PTY master, awaiting writability between
/// short writes. The slave's input queue is finite, so partial writes are
/// normal.
#[cfg(unix)]
async fn write_all(afd: &AsyncFd<FdRef>, data: &[u8]) -> std::io::Result<()> {
    let mut written = 0;
    while written < data.len() {
        let mut guard = afd.writable().await?;
        match guard.try_io(|inner| write_some(inner.as_raw_fd(), &data[written..])) {
            Ok(Ok(0)) => return Err(std::io::Error::other("PTY write returned 0")),
            Ok(Ok(n)) => written += n,
            Ok(Err(e)) => return Err(e),
            Err(_would_block) => continue,
        }
    }
    Ok(())
}

/// Build a small multi-threaded tokio runtime and drive the terminal on it.
pub fn run(
    backend: Box<dyn Backend>,
    grid: Arc<Mutex<Grid>>,
    init_cols: u16,
    init_rows: u16,
    config: crate::config::Config,
) -> std::io::Result<()> {
    // Spawn the shell *before* the runtime exists. On Unix this keeps the
    // fork single-threaded: POSIX only guarantees async-signal-safe calls
    // between fork and exec in a multithreaded process, and a worker thread
    // holding e.g. the malloc lock at fork time could otherwise deadlock the
    // child. `reader` owns the master/ConPTY + child and reaps it on drop.
    let reader = backend.spawn_shell(
        init_cols,
        init_rows,
        config.shell.as_deref(),
        &config.command_args,
        config.cwd.as_deref(),
    )?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(run_async(backend, reader, grid, config))
}

/// Live config reload for the TUI: watch the config file and, on each save,
/// re-read it and apply what can change live — theme (parser palette + grid
/// recolor) and the scrollback cap. Shell/font/window-size remain launch-time
/// choices. Wakes the render loop through `frame` for a repaint; reload
/// warnings go to stderr like startup ones (cosmetic in raw mode, but real).
fn watch_config(parser: Arc<Mutex<AnsiParser>>, grid: Arc<Mutex<Grid>>, frame: Arc<Notify>) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = crate::config::Config::file_path(&args) else {
        return;
    };
    let reload_args = vec!["--config".to_string(), path.to_string_lossy().into_owned()];
    crate::config::watch(path, move || {
        let (new, warnings) = crate::config::Config::load(&reload_args);
        for w in &warnings {
            eprintln!("rusty_term: {w}");
        }
        let mut g = grid.lock();
        let old = parser.lock().retheme(new.theme);
        if old != new.theme {
            g.retheme(&old, &new.theme);
        }
        g.set_scrollback_max(new.scrollback.unwrap_or(crate::core::SCROLLBACK_MAX));
        drop(g);
        frame.notify_one();
    });
}

#[cfg(unix)]
async fn run_async(
    backend: Box<dyn Backend>,
    reader: Box<dyn crate::backend::BackendHandle>,
    grid: Arc<Mutex<Grid>>,
    config: crate::config::Config,
) -> std::io::Result<()> {
    // Raw mode for the host terminal; restored on drop (any exit path).
    let _raw_guard = RawModeGuard::enable(backend.as_ref())?;

    // `reader` (the shell, spawned before the runtime started) owns the master
    // fd + child and reaps it on drop; keeping it in this top-level future
    // means the child is reaped on every exit path. Independent dups feed the
    // read, write, and resize paths.
    let read_handle = reader.try_clone()?;
    let write_handle = reader.try_clone()?;
    let mut resizer = reader.try_clone()?;

    // One O_NONBLOCK on the shared open file description covers every dup; the
    // resize dup only issues ioctls, so it is unaffected.
    set_nonblocking(reader.pty_fd())?;
    let read_fd = read_handle.pty_fd();
    let write_fd = write_handle.pty_fd();

    let running = Arc::new(AtomicBool::new(true));
    let frame = Arc::new(Notify::new());

    {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[2J");
        let _ = out.flush();
    }

    // Shared parser: the parser task feeds it; the config watcher thread takes
    // it briefly on a config save to retheme (see `watch_config`).
    let parser = Arc::new(Mutex::new(AnsiParser::with_theme(config.theme)));
    watch_config(Arc::clone(&parser), Arc::clone(&grid), Arc::clone(&frame));

    // --- Parser task: PTY master -> Grid (+ DA/DSR replies) ---
    let parser_task = {
        let grid = Arc::clone(&grid);
        let running = Arc::clone(&running);
        let frame = Arc::clone(&frame);
        let parser = Arc::clone(&parser);
        tokio::spawn(async move {
            // Wakes the render loop on every exit from this task — including a
            // panic in `parser.advance` on hostile child bytes.
            let _shutdown = ShutdownOnDrop {
                running: Arc::clone(&running),
                frame: Arc::clone(&frame),
            };
            // Keep the read dup alive for the task; `master` deregisters before
            // it (declared after, dropped first) so the fd is still open then.
            let _read_handle = read_handle;
            let master = match AsyncFd::new(FdRef(read_fd)) {
                Ok(m) => m,
                Err(_) => return,
            };
            loop {
                let mut guard = match master.readable().await {
                    Ok(g) => g,
                    Err(_) => break,
                };
                match guard.try_io(|inner| read_nonblocking(inner.as_raw_fd())) {
                    Ok(Ok(data)) if !data.is_empty() => {
                        let (responses, should_notify) = {
                            let mut g = grid.lock();
                            let mut parser = parser.lock();
                            parser.advance(&mut g, &data);
                            g.epoch += 1;
                            (parser.take_responses(), !g.sync_output_active())
                        };
                        // New output landed in the grid — ask for a repaint,
                        // unless a synchronized-output window is suppressing it.
                        if should_notify {
                            frame.notify_one();
                        }
                        if !responses.is_empty() && write_all(&master, &responses).await.is_err() {
                            break;
                        }
                    }
                    Ok(Ok(_)) => break,            // empty read == EOF
                    Ok(Err(_)) => break,           // hard read error
                    Err(_would_block) => continue, // readiness was spurious
                }
            }
            // `_shutdown` signals the render loop on drop.
        })
    };

    // --- Input task: host stdin (/dev/tty) -> PTY ---
    let input_task = {
        let grid = Arc::clone(&grid);
        let running = Arc::clone(&running);
        let frame = Arc::clone(&frame);
        tokio::spawn(async move {
            let _shutdown = ShutdownOnDrop {
                running: Arc::clone(&running),
                frame: Arc::clone(&frame),
            };
            let _write_handle = write_handle;
            let tty = match open_tty_nonblocking().and_then(AsyncFd::new) {
                Ok(t) => t,
                Err(_) => return,
            };
            let writer = match AsyncFd::new(FdRef(write_fd)) {
                Ok(w) => w,
                Err(_) => return,
            };
            loop {
                let mut guard = match tty.readable().await {
                    Ok(g) => g,
                    Err(_) => break,
                };
                match guard.try_io(|inner| read_nonblocking(inner.as_raw_fd())) {
                    Ok(Ok(data)) if !data.is_empty() => {
                        // Shift+PageUp/Down browse scrollback; the rest forwards.
                        let (forward, scrolls) = split_input(&data);
                        if !scrolls.is_empty() {
                            let mut moved = false;
                            {
                                let mut g = grid.lock();
                                let page = g.rows.saturating_sub(1).max(1);
                                for s in &scrolls {
                                    moved |= match s {
                                        Scroll::Up => g.scroll_view_up(page),
                                        Scroll::Down => g.scroll_view_down(page),
                                        Scroll::PrevPrompt => g.scroll_to_prev_prompt(),
                                        Scroll::NextPrompt => g.scroll_to_next_prompt(),
                                    };
                                }
                            }
                            if moved {
                                frame.notify_one();
                            }
                        }
                        if !forward.is_empty() {
                            // Real input snaps to the live bottom; wake only if
                            // the viewport actually moved.
                            let snapped = grid.lock().reset_view();
                            if snapped {
                                frame.notify_one();
                            }
                            if write_all(&writer, &forward).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(Ok(_)) => break, // EOF on the tty
                    Ok(Err(_)) => break,
                    Err(_would_block) => continue,
                }
            }
            // `_shutdown` signals the render loop on drop.
        })
    };

    // --- Render loop (this future) ---
    let mut sigwinch = signal(SignalKind::window_change())?;
    let mut render_state = RenderState::new();

    // A pinned `Notified` future re-armed in place: no await sits between its
    // completion and the next arm, so a `notify_one` in that gap is not lost,
    // and the `sigwinch` branch never drops a pending notification.
    let notified = frame.notified();
    tokio::pin!(notified);

    loop {
        tokio::select! {
            _ = &mut notified => {
                notified.set(frame.notified());
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                // Coalesce bursts: hold off until the frame budget since the
                // last paint has elapsed, so a flood repaints at ~60 Hz.
                let since = render_state.last_frame.elapsed();
                if since < FRAME_BUDGET {
                    tokio::time::sleep(FRAME_BUDGET - since).await;
                }
                render_once(&grid, &mut render_state);
            }
            _ = sigwinch.recv() => {
                // Reflow the grid, tell the child its new size, and clear the
                // screen for a clean full repaint. Only act on a real change so a
                // spurious SIGWINCH doesn't blank the screen.
                if let Some((cols, rows)) = backend.terminal_size() {
                    let changed = {
                        let mut g = grid.lock();
                        let changed = g.cols != cols as usize || g.rows != rows as usize;
                        if changed {
                            g.resize(cols as usize, rows as usize);
                        }
                        changed
                    };
                    if changed {
                        let _ = resizer.set_winsize(cols, rows);
                        // Tell a subscribed structured client the size changed
                        // (the child also gets SIGWINCH). The master fd is
                        // non-blocking; a best-effort write is fine for a small
                        // frame and never blocks the reactor.
                        #[cfg(feature = "l13")]
                        {
                            let notif = grid.lock().resize_notification();
                            if let Some(bytes) = notif {
                                let _ = resizer.write(&bytes);
                            }
                        }
                        let mut out = std::io::stdout();
                        let _ = out.write_all(b"\x1b[2J");
                        let _ = out.flush();
                        // Repaint the reflowed grid on the next render wake.
                        frame.notify_one();
                    }
                }
            }
        }
    }

    // Stop the tasks; their dropped fds deregister and close. `reader` (this
    // scope) reaps the child; `_raw_guard` restores cooked mode on return.
    parser_task.abort();
    input_task.abort();
    restore_host_modes();
    Ok(())
}

/// Windows ConPTY driver: the pipes are synchronous (no readiness-pollable fd),
/// so dedicated blocking threads bridge PTY output, PTY input, and host stdin
/// into tokio channels, while the async render loop coalesces repaints and polls
/// the console size for resizes (there is no `SIGWINCH`).
#[cfg(windows)]
async fn run_async(
    backend: Box<dyn Backend>,
    reader: Box<dyn crate::backend::BackendHandle>,
    grid: Arc<Mutex<Grid>>,
    config: crate::config::Config,
) -> std::io::Result<()> {
    let _raw_guard = RawModeGuard::enable(backend.as_ref())?;

    // `reader` (the shell, spawned before the runtime started) owns the
    // ConPTY + child and reaps it on drop; independent dups feed the read,
    // write, and resize paths.
    let read_handle = reader.try_clone()?;
    let write_handle = reader.try_clone()?;
    let mut resizer = reader.try_clone()?;

    let running = Arc::new(AtomicBool::new(true));
    let frame = Arc::new(Notify::new());

    // ConPTY's output pipe does NOT EOF when the child exits — it EOFs when the
    // pseudoconsole is torn down, which only happens once *we* drop `reader`.
    // Relying on read-EOF alone would deadlock: we'd wait forever for an EOF
    // that only our own teardown produces. So a watcher thread blocks on the
    // child process handle and stops the event loop when the shell exits.
    if let Some(wait_for_exit) = reader.exit_token() {
        let running = Arc::clone(&running);
        let frame = Arc::clone(&frame);
        std::thread::spawn(move || {
            wait_for_exit();
            running.store(false, Ordering::Relaxed);
            frame.notify_one();
        });
    }

    {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[2J");
        let _ = out.flush();
    }

    // PTY output: a blocking `ReadFile` thread feeds the async side.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    {
        let running = Arc::clone(&running);
        std::thread::spawn(move || {
            let mut read_handle = read_handle;
            loop {
                match read_handle.read() {
                    Ok(data) if !data.is_empty() => {
                        if out_tx.send(data).is_err() {
                            break;
                        }
                    }
                    _ => break, // empty read == EOF, or a hard error
                }
            }
            running.store(false, Ordering::Relaxed);
            // Dropping `out_tx` closes the channel so the render loop sees EOF.
        });
    }

    // PTY input: the async side queues bytes; a blocking `WriteFile` thread
    // drains them so reply/keystroke writes never block the reactor.
    let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    {
        let mut in_rx = in_rx;
        std::thread::spawn(move || {
            let mut write_handle = write_handle;
            while let Some(data) = in_rx.blocking_recv() {
                if write_handle.write(&data).is_err() {
                    break;
                }
            }
        });
    }

    // Host stdin: a blocking read thread (Shift+PageUp/Down browse scrollback;
    // everything else forwards to the child).
    {
        let grid = Arc::clone(&grid);
        let running = Arc::clone(&running);
        let frame = Arc::clone(&frame);
        let in_tx = in_tx.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut stdin = stdin.lock();
            let mut buf = [0u8; 1024];
            loop {
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let (forward, scrolls) = split_input(&buf[..n]);
                        if !scrolls.is_empty() {
                            let mut moved = false;
                            {
                                let mut g = grid.lock();
                                let page = g.rows.saturating_sub(1).max(1);
                                for s in &scrolls {
                                    moved |= match s {
                                        Scroll::Up => g.scroll_view_up(page),
                                        Scroll::Down => g.scroll_view_down(page),
                                        Scroll::PrevPrompt => g.scroll_to_prev_prompt(),
                                        Scroll::NextPrompt => g.scroll_to_next_prompt(),
                                    };
                                }
                            }
                            if moved {
                                frame.notify_one();
                            }
                        }
                        if !forward.is_empty() {
                            let snapped = grid.lock().reset_view();
                            if snapped {
                                frame.notify_one();
                            }
                            if in_tx.send(forward.to_vec()).is_err() {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            running.store(false, Ordering::Relaxed);
            frame.notify_one();
        });
    }

    // Render loop: parse incoming output, coalesce repaints, and poll for resize.
    // The parser is shared with the config watcher thread, which takes it
    // briefly on a config save to retheme (see `watch_config`).
    let parser = Arc::new(Mutex::new(AnsiParser::with_theme(config.theme)));
    watch_config(Arc::clone(&parser), Arc::clone(&grid), Arc::clone(&frame));
    let mut render_state = RenderState::new();
    let mut resize_poll = tokio::time::interval(Duration::from_millis(150));
    let notified = frame.notified();
    tokio::pin!(notified);

    loop {
        tokio::select! {
            data = out_rx.recv() => {
                match data {
                    Some(data) => {
                        let (responses, should_notify) = {
                            let mut g = grid.lock();
                            let mut parser = parser.lock();
                            parser.advance(&mut g, &data);
                            g.epoch += 1;
                            (parser.take_responses(), !g.sync_output_active())
                        };
                        if should_notify {
                            frame.notify_one();
                        }
                        if !responses.is_empty() {
                            let _ = in_tx.send(responses);
                        }
                    }
                    None => break, // PTY EOF: the shell exited
                }
            }
            _ = &mut notified => {
                notified.set(frame.notified());
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                let since = render_state.last_frame.elapsed();
                if since < FRAME_BUDGET {
                    tokio::time::sleep(FRAME_BUDGET - since).await;
                }
                render_once(&grid, &mut render_state);
            }
            _ = resize_poll.tick() => {
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                // No SIGWINCH on Windows: poll the console size and act on change.
                if let Some((cols, rows)) = backend.terminal_size() {
                    let changed = {
                        let mut g = grid.lock();
                        let changed = g.cols != cols as usize || g.rows != rows as usize;
                        if changed {
                            g.resize(cols as usize, rows as usize);
                        }
                        changed
                    };
                    if changed {
                        let _ = resizer.set_winsize(cols, rows);
                        // Notify a subscribed structured client; routed through the
                        // write bridge so it serializes with other child writes.
                        #[cfg(feature = "l13")]
                        {
                            let notif = grid.lock().resize_notification();
                            if let Some(bytes) = notif {
                                let _ = in_tx.send(bytes);
                            }
                        }
                        let mut out = std::io::stdout();
                        let _ = out.write_all(b"\x1b[2J");
                        let _ = out.flush();
                        frame.notify_one();
                    }
                }
            }
        }
    }

    restore_host_modes();
    Ok(())
}
