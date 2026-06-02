//! Windows backend stub. ConPTY support is not yet implemented; all entry
//! points return an error so the binary still links and runs on Windows.

/// Unit type implementing [`Backend`](crate::backend::Backend) for Windows.
pub struct WindowsBackend;

impl crate::backend::Backend for WindowsBackend {
    fn spawn_shell(&self, _cols: u16, _rows: u16) -> Result<Box<dyn crate::backend::BackendHandle>, std::io::Error> {
        Err(std::io::Error::other("Windows driver not yet implemented"))
    }

    fn set_raw_mode(&self, _enabled: bool) -> Result<(), std::io::Error> {
        Err(std::io::Error::other("Windows driver not yet implemented"))
    }

    fn terminal_size(&self) -> Option<(u16, u16)> {
        None
    }
}

/// Placeholder handle for the future ConPTY implementation. Unconstructed on
/// non-Windows targets, where `spawn_shell` always errors.
#[allow(dead_code)]
struct WindowsHandle;

impl crate::backend::BackendHandle for WindowsHandle {
    fn read(&mut self) -> Result<Vec<u8>, std::io::Error> {
        Ok(Vec::new())
    }

    fn write(&mut self, _data: &[u8]) -> Result<(), std::io::Error> {
        Ok(())
    }

    fn try_clone(&self) -> Result<Box<dyn crate::backend::BackendHandle>, std::io::Error> {
        Ok(Box::new(WindowsHandle))
    }

    fn set_winsize(&mut self, _cols: u16, _rows: u16) -> Result<(), std::io::Error> {
        Ok(())
    }
}
