
pub struct WindowsBackend;

impl crate::backend::Backend for WindowsBackend {
    fn spawn_shell(&self) -> Result<Box<dyn crate::backend::BackendHandle>, std::io::Error> {
        Err(std::io::Error::new(std::io::ErrorKind::Other, "Windows driver not yet implemented"))
    }

    fn set_raw_mode(&self, _enabled: bool) -> Result<(), std::io::Error> {
        Err(std::io::Error::new(std::io::ErrorKind::Other, "Windows driver not yet implemented"))
    }
}

struct WindowsHandle;

impl crate::backend::BackendHandle for WindowsHandle {
    fn read(&mut self) -> Result<Vec<u8>, std::io::Error> {
        Ok(vec![])
    }

    fn write(&mut self, _data: &[u8]) -> Result<(), std::io::Error> {
        Ok(())
    }

    fn close(&mut self) -> Result<(), std::io::Error> {
        Ok(())
    }
}
