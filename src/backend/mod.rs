
pub trait Backend {
    /// Spawn the child shell on a new PTY sized to `cols`×`rows`.
    fn spawn_shell(&self, cols: u16, rows: u16) -> Result<Box<dyn BackendHandle>, std::io::Error>;
    fn set_raw_mode(&self, enabled: bool) -> Result<(), std::io::Error>;
    /// Best-effort query of the controlling terminal's size as `(cols, rows)`.
    fn terminal_size(&self) -> Option<(u16, u16)>;
}

pub trait BackendHandle: Send {
    /// Read whatever bytes are currently available from the child.
    ///
    /// An `Ok` value containing an empty slice signals end-of-file: the child
    /// has exited and its side of the PTY is closed.
    fn read(&mut self) -> Result<Vec<u8>, std::io::Error>;

    /// Write `data` to the child's input.
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
}

pub mod unix;
pub mod windows;

pub use unix::UnixBackend;
pub use windows::WindowsBackend;
