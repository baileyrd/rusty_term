pub trait Backend {
    /// Spawn the child shell on a new PTY sized to `cols`×`rows`.
    fn spawn_shell(&self, cols: u16, rows: u16) -> Result<Box<dyn BackendHandle>, std::io::Error>;
    fn set_raw_mode(&self, enabled: bool) -> Result<(), std::io::Error>;
    /// Best-effort query of the controlling terminal's size as `(cols, rows)`.
    fn terminal_size(&self) -> Option<(u16, u16)>;
}

pub trait BackendHandle: Send {
    /// Read whatever bytes are currently available from the child (blocking).
    ///
    /// An `Ok` value containing an empty slice signals end-of-file: the child
    /// has exited and its side of the PTY is closed. Used by the windowed (gui)
    /// reader thread and by the Windows runtime's bridge thread; the Unix runtime
    /// drives the raw [`pty_fd`](Self::pty_fd) through its reactor instead, so the
    /// method is unused on a Unix terminal-only build.
    #[cfg_attr(unix, allow(dead_code))]
    fn read(&mut self) -> Result<Vec<u8>, std::io::Error>;

    /// Write `data` to the child's input (blocking). Used by the windowed (gui)
    /// backend and the Windows runtime's bridge thread; the Unix runtime writes
    /// the raw [`pty_fd`](Self::pty_fd) through its reactor instead.
    #[cfg_attr(unix, allow(dead_code))]
    fn write(&mut self, data: &[u8]) -> Result<(), std::io::Error>;

    /// Produce an independent handle referring to the same child, so the
    /// read side and write side can live on separate threads without sharing
    /// a lock. The clone does not own the child (only the original reaps it).
    ///
    /// Descriptors are released via the handle's `Drop` impl.
    fn try_clone(&self) -> Result<Box<dyn BackendHandle>, std::io::Error>;

    /// Inform the child of a new window size (`cols`×`rows`), so it can reflow
    /// and so applications receive `SIGWINCH`.
    fn set_winsize(&mut self, cols: u16, rows: u16) -> Result<(), std::io::Error>;

    /// A blocking closure that returns once the child has exited, for front-ends
    /// that can't rely on read-EOF to detect it. `None` when read-EOF already
    /// signals exit (the Unix PTY) or this handle doesn't own the child; `Some`
    /// only on the owning Windows ConPTY handle (whose output pipe EOFs at
    /// teardown, not on child exit — without this, teardown would deadlock
    /// waiting for an EOF that only teardown itself produces). The windowed
    /// backend runs it on a watcher thread to close the window when the shell
    /// quits; the Windows console runtime uses it to stop its event loop.
    #[cfg_attr(unix, allow(dead_code))]
    fn exit_token(&self) -> Option<Box<dyn FnOnce() + Send>> {
        None
    }

    /// The PTY master descriptor backing this handle, for the Unix runtime to
    /// drive through a readiness reactor (tokio's `AsyncFd`). The fd stays owned
    /// by the handle — the caller registers it without closing it. Unix-only; the
    /// Windows ConPTY handle has no equivalent pollable fd (its runtime bridges
    /// the blocking [`read`](Self::read)/[`write`](Self::write) instead).
    #[cfg(unix)]
    fn pty_fd(&self) -> std::os::unix::io::RawFd;
}

// Each backend only compiles on its own platform — the Unix one leans on
// platform-specific libc (openpty, termios, …), the Windows one on ConPTY.
#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

#[cfg(unix)]
pub use unix::UnixBackend;
#[cfg(windows)]
pub use windows::WindowsBackend;
