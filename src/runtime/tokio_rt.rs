//! Tokio runtime (Unix-only): a single async reactor replaces the three OS
//! threads of the [`threaded`](super::threaded) runtime.
//!
//! The PTY master and a fresh `/dev/tty` open are registered with the reactor
//! via [`AsyncFd`]; `SIGWINCH` arrives as a [`tokio::signal`] stream; and a
//! [`Notify`] coalesces repaints. Two spawned tasks (parser, input pump) feed
//! the grid and the child, while the render loop lives in the top-level future.
//!
//! Locking note: the grid uses a `parking_lot::Mutex` (sync), and a guard is
//! **never** held across an `.await` — every critical section locks, mutates or
//! snapshots, and drops the guard before any await. So a sync mutex is correct
//! here, not `tokio::sync::Mutex`.

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use tokio::io::unix::AsyncFd;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Notify;

use crate::backend::Backend;
use crate::core::{AnsiParser, Grid};
use crate::input::{Scroll, split_input};
use crate::render::{FRAME_BUDGET, RawModeGuard, RenderState, render_once, restore_host_modes};

/// A PTY master descriptor registered with the reactor that does **not** close
/// the fd on drop — ownership stays with the `BackendHandle` that produced it,
/// which closes it. `AsyncFd<FdRef>` only deregisters from the reactor on drop.
struct FdRef(RawFd);

impl AsRawFd for FdRef {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// Put `fd`'s open file description into non-blocking mode, as `AsyncFd`
/// requires. Dups (from `try_clone`) share the description, so one call covers
/// every clone of the master.
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
) -> std::io::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(run_async(backend, grid, init_cols, init_rows))
}

async fn run_async(
    backend: Box<dyn Backend>,
    grid: Arc<Mutex<Grid>>,
    init_cols: u16,
    init_rows: u16,
) -> std::io::Result<()> {
    // Raw mode for the host terminal; restored on drop (any exit path).
    let _raw_guard = RawModeGuard::enable(backend.as_ref())?;

    // `reader` owns the master fd + child and reaps it on drop; keeping it in
    // this top-level future means the child is reaped on every exit path.
    // Independent dups feed the read, write, and resize paths.
    let reader = backend.spawn_shell(init_cols, init_rows)?;
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

    // --- Parser task: PTY master -> Grid (+ DA/DSR replies) ---
    let parser_task = {
        let grid = Arc::clone(&grid);
        let running = Arc::clone(&running);
        let frame = Arc::clone(&frame);
        tokio::spawn(async move {
            // Keep the read dup alive for the task; `master` deregisters before
            // it (declared after, dropped first) so the fd is still open then.
            let _read_handle = read_handle;
            let master = match AsyncFd::new(FdRef(read_fd)) {
                Ok(m) => m,
                Err(_) => return,
            };
            let mut parser = AnsiParser::new();
            loop {
                let mut guard = match master.readable().await {
                    Ok(g) => g,
                    Err(_) => break,
                };
                match guard.try_io(|inner| read_nonblocking(inner.as_raw_fd())) {
                    Ok(Ok(data)) if !data.is_empty() => {
                        let responses = {
                            let mut g = grid.lock();
                            parser.advance(&mut g, &data);
                            g.epoch += 1;
                            parser.take_responses()
                        };
                        // New output landed in the grid — ask for a repaint.
                        frame.notify_one();
                        if !responses.is_empty() && write_all(&master, &responses).await.is_err() {
                            break;
                        }
                    }
                    Ok(Ok(_)) => break,            // empty read == EOF
                    Ok(Err(_)) => break,           // hard read error
                    Err(_would_block) => continue, // readiness was spurious
                }
            }
            running.store(false, Ordering::Relaxed);
            frame.notify_one();
        })
    };

    // --- Input task: host stdin (/dev/tty) -> PTY ---
    let input_task = {
        let grid = Arc::clone(&grid);
        let running = Arc::clone(&running);
        let frame = Arc::clone(&frame);
        tokio::spawn(async move {
            let _write_handle = write_handle;
            let tty = match open_tty_nonblocking().and_then(AsyncFd::new) {
                Ok(t) => t,
                Err(_) => {
                    running.store(false, Ordering::Relaxed);
                    frame.notify_one();
                    return;
                }
            };
            let writer = match AsyncFd::new(FdRef(write_fd)) {
                Ok(w) => w,
                Err(_) => {
                    running.store(false, Ordering::Relaxed);
                    frame.notify_one();
                    return;
                }
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
            running.store(false, Ordering::Relaxed);
            frame.notify_one();
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
