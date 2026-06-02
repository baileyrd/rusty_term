
pub trait Backend {
    fn spawn_shell(&self) -> Result<Box<dyn BackendHandle>, std::io::Error>;
    fn set_raw_mode(&self, enabled: bool) -> Result<(), std::io::Error>;
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
}

pub mod unix;
pub mod windows;

pub use unix::UnixBackend;
pub use windows::WindowsBackend;
