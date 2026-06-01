
pub trait Backend {
    fn spawn_shell(&self) -> Result<Box<dyn BackendHandle>, std::io::Error>;
    fn set_raw_mode(&self, enabled: bool) -> Result<(), std::io::Error>;
}

pub trait BackendHandle {
    fn read(&mut self) -> Result<Vec<u8>, std::io::Error>;
    fn write(&mut self, data: &[u8]) -> Result<(), std::io::Error>;
    fn close(&mut self) -> Result<(), std::io::Error>;
}

pub mod unix;
pub mod windows;
