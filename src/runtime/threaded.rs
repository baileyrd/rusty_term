//! Thread-based runtime (default): one OS thread each for parsing PTY output
//! into the grid, pumping host stdin to the PTY, and rendering. A condvar
//! ([`FrameSignal`]) wakes the renderer only on real damage; a `SIGWINCH`
//! handler sets an atomic flag the renderer drains on its idle tick.

use parking_lot::{Condvar, Mutex};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::backend::Backend;
use crate::core::{AnsiParser, Grid};
use crate::input::{Scroll, split_input};
use crate::render::{FRAME_BUDGET, RawModeGuard, RenderState, render_once, restore_host_modes};

/// Set by the `SIGWINCH` handler; the render loop drains it to resize the grid
/// and the PTY in step with the host terminal. A plain atomic store is the only
/// async-signal-safe work the handler does. Defined on all platforms so the
/// renderer can read it unconditionally; only the Unix handler ever sets it.
static RESIZE_PENDING: AtomicBool = AtomicBool::new(false);

/// How long the renderer parks when idle before waking to re-check for a
/// pending resize. Output and scrollback moves wake it immediately; this bare
/// tick exists only because the async-signal-safe `SIGWINCH` handler can set a
/// flag but cannot touch the condvar, so it bounds resize latency while quiet.
const IDLE_TICK: Duration = Duration::from_millis(100);

#[cfg(unix)]
extern "C" fn handle_sigwinch(_: libc::c_int) {
    RESIZE_PENDING.store(true, Ordering::Relaxed);
}

/// Wakes the render thread when there is something new to paint, so it can park
/// on a condvar instead of spinning on a fixed timer. The boolean is the
/// "frame pending" predicate: producers (the parser on new output, the input
/// pump on a scrollback move) set it and signal; the renderer clears it on each
/// wake. Many producer events between two repaints collapse into one frame via
/// the renderer's frame budget, so `notify` is deliberately cheap and idempotent.
struct FrameSignal {
    pending: Mutex<bool>,
    cv: Condvar,
}

impl FrameSignal {
    fn new() -> Self {
        Self {
            pending: Mutex::new(false),
            cv: Condvar::new(),
        }
    }

    /// Mark a frame pending and wake the renderer if it is parked.
    fn notify(&self) {
        *self.pending.lock() = true;
        self.cv.notify_one();
    }

    /// Park until a frame is pending or `timeout` elapses, then clear the
    /// predicate. Returns `true` when woken by a `notify` (real work), `false`
    /// on a bare timeout — the periodic tick that lets a `SIGWINCH`-driven
    /// resize be noticed even when no output is flowing. The timeout is the only
    /// reason the renderer wakes while idle.
    fn wait(&self, timeout: Duration) -> bool {
        let mut pending = self.pending.lock();
        if !*pending {
            let res = self.cv.wait_for(&mut pending, timeout);
            // A notify landing right at the timeout boundary still counts as work.
            if res.timed_out() && !*pending {
                return false;
            }
        }
        *pending = false;
        true
    }
}

/// Drive the terminal with three threads until the child exits or stdin ends.
pub fn run(
    backend: Box<dyn Backend>,
    grid: Arc<Mutex<Grid>>,
    init_cols: u16,
    init_rows: u16,
) -> std::io::Result<()> {
    let running = Arc::new(AtomicBool::new(true));
    let signal = Arc::new(FrameSignal::new());

    // Raw mode stays enabled for the lifetime of this guard; it is restored on
    // any exit path, including a panic.
    let _raw_guard = RawModeGuard::enable(backend.as_ref())?;

    // Install the SIGWINCH handler before spawning, so a resize during startup
    // isn't lost to the default (ignore) disposition. sigaction with SA_RESTART
    // keeps the parser's blocking read from being interrupted.
    #[cfg(unix)]
    unsafe {
        let handler: extern "C" fn(libc::c_int) = handle_sigwinch;
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGWINCH, &sa, std::ptr::null_mut());
    }

    // The reader owns the child (and reaps it on drop); the writer and resizer
    // are independent handles to the same PTY, so read, write, and winsize
    // updates never contend on a shared lock.
    let mut reader = backend.spawn_shell(init_cols, init_rows)?;
    let mut writer = reader.try_clone()?;
    let mut resizer = reader.try_clone()?;

    // --- Thread 1: Parser (PTY -> Grid) ---
    let grid_parser = Arc::clone(&grid);
    let running_parser = Arc::clone(&running);
    let signal_parser = Arc::clone(&signal);

    thread::spawn(move || {
        let mut parser = AnsiParser::new();
        loop {
            if !running_parser.load(Ordering::Relaxed) {
                break;
            }
            match reader.read() {
                // Non-empty data: fold it into the grid, then send any query
                // replies (DA/DSR) back to the child via the master fd.
                Ok(data) if !data.is_empty() => {
                    let responses = {
                        let mut g = grid_parser.lock();
                        parser.advance(&mut g, &data);
                        g.epoch += 1;
                        parser.take_responses()
                    };
                    // New output landed in the grid — ask for a repaint.
                    signal_parser.notify();
                    if !responses.is_empty() && reader.write(&responses).is_err() {
                        break;
                    }
                }
                // Empty read == EOF: the shell exited.
                Ok(_) => break,
                Err(_) => break,
            }
        }
        running_parser.store(false, Ordering::Relaxed);
        // Wake the renderer so it observes `running == false` and exits its park.
        signal_parser.notify();
        // On this thread's own exit path (shell EOF/error) `reader` drops here
        // and reaps the child. On the stdin-EOF path the process exits while
        // this thread is blocked in read(), so Drop may not run — but the child
        // now has a controlling tty, so the kernel sends it SIGHUP when the
        // master closes, and init reaps the orphan.
    });

    // --- Thread 2: Input pump (stdin -> PTY) ---
    let running_input = Arc::clone(&running);
    let grid_input = Arc::clone(&grid);
    let signal_input = Arc::clone(&signal);

    thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut stdin_handle = stdin.lock();
        let mut input_buf = [0u8; 1024];

        loop {
            if !running_input.load(Ordering::Relaxed) {
                break;
            }
            match stdin_handle.read(&mut input_buf) {
                Ok(0) => break,
                Ok(n) => {
                    // Shift+PageUp / Shift+PageDown browse scrollback instead of
                    // reaching the child; everything else is forwarded and snaps
                    // the view to the live bottom.
                    let (forward, scrolls) = split_input(&input_buf[..n]);
                    if !scrolls.is_empty() {
                        let mut moved = false;
                        {
                            let mut g = grid_input.lock();
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
                            signal_input.notify();
                        }
                    }
                    if !forward.is_empty() {
                        // Real input snaps the view to the live bottom; only wake
                        // the renderer when that actually moved the viewport.
                        let moved = grid_input.lock().reset_view();
                        if moved {
                            signal_input.notify();
                        }
                        if writer.write(&forward).is_err() {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        running_input.store(false, Ordering::Relaxed);
        // Wake the renderer so it observes `running == false` and exits its park.
        signal_input.notify();
    });

    // --- Thread 3: Renderer (this thread) ---
    print!("\x1b[2J");

    let mut render_state = RenderState::new();

    loop {
        // Park until a producer flags a pending frame, or the idle tick fires so
        // a SIGWINCH-driven resize is still picked up while output is quiet.
        let woke_for_work = signal.wait(IDLE_TICK);
        if !running.load(Ordering::Relaxed) {
            break;
        }
        // Coalesce bursts: when woken by output, hold off until the frame budget
        // since the last paint has elapsed, so a flood repaints at ~60 Hz rather
        // than once per read. Idle ticks are already older than the budget.
        if woke_for_work {
            let since = render_state.last_frame.elapsed();
            if since < FRAME_BUDGET {
                thread::sleep(FRAME_BUDGET - since);
            }
        }

        // Apply a pending resize before snapshotting: reflow the grid, tell the
        // child its new size, and clear the screen so the full repaint is clean.
        // Only act when the size actually changed — a spurious SIGWINCH must not
        // clear the screen (the clear would flush ahead of a dirty-rows-only
        // repaint and blank everything else).
        if RESIZE_PENDING.swap(false, Ordering::Relaxed)
            && let Some((cols, rows)) = backend.terminal_size()
        {
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
                // Tell a subscribed structured client the size changed (the child
                // also gets SIGWINCH from set_winsize). Best-effort: a dropped
                // frame on a full input queue is harmless.
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
            }
        }

        render_once(&grid, &mut render_state);
    }

    // Reset any host-terminal input modes we may have relayed on the child's
    // behalf, so a child that exited without disabling them can't leave the
    // host stuck emitting mouse escapes on every click.
    restore_host_modes();

    // `_raw_guard` drops here, restoring cooked mode.
    Ok(())
}

#[cfg(test)]
mod frame_signal_tests {
    use super::FrameSignal;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    // A notify issued before the wait must not be lost: the wait observes the
    // pending predicate and returns `true` without parking for the timeout.
    #[test]
    fn notify_before_wait_returns_immediately() {
        let signal = FrameSignal::new();
        signal.notify();
        let start = Instant::now();
        let woke = signal.wait(Duration::from_secs(5));
        assert!(woke, "a pre-set notify must report work");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "wait blocked despite a pending frame",
        );
    }

    // A notify from another thread must wake a parked wait and report work, well
    // before the (generous) timeout would fire.
    #[test]
    fn parked_wait_is_woken_by_cross_thread_notify() {
        let signal = Arc::new(FrameSignal::new());
        let notifier = Arc::clone(&signal);
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            notifier.notify();
        });
        let start = Instant::now();
        let woke = signal.wait(Duration::from_secs(5));
        let elapsed = start.elapsed();
        handle.join().unwrap();
        assert!(woke, "cross-thread notify must report work");
        assert!(
            elapsed < Duration::from_secs(2),
            "wait was not woken by the notify",
        );
    }

    // With no notify, wait must eventually report a bare timeout (`false`) — the
    // periodic tick the renderer relies on to poll for a pending resize. Retried
    // to absorb the rare spurious condvar wakeup, which the contract reports as
    // `true`.
    #[test]
    fn wait_reports_bare_timeout_without_notify() {
        let signal = FrameSignal::new();
        let mut timed_out = false;
        for _ in 0..40 {
            if !signal.wait(Duration::from_millis(25)) {
                timed_out = true;
                break;
            }
        }
        assert!(timed_out, "wait never reported a bare timeout");
    }
}
