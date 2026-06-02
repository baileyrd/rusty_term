
use std::io::{Read, Write};
use std::os::unix::io::RawFd;

pub struct UnixBackend;

impl crate::backend::Backend for UnixBackend {
    fn spawn_shell(&self) -> Result<Box<dyn crate::backend::BackendHandle>, std::io::Error> {
        unsafe {
            let mut slave_fd: libc::c_int = 0;
            let master_fd = libc::openpty(&mut slave_fd, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
            if master_fd < 0 {
                return Err(std::io::Error::last_os_error());
            }

            let pid = libc::fork();
            if pid == -1 {
                libc::close(master_fd);
                libc::close(slave_fd);
                return Err(std::io::Error::last_os_error());
            }

            if pid == 0 {
                // Child: Attach to PTY slave
                libc::setpgid(0, 0);
                libc::setsid();
                
                // Redirect stdin, stdout, stderr to slave
                libc::dup2(slave_fd, libc::STDIN_FILENO);
                libc::dup2(slave_fd, libc::STDOUT_FILENO);
                libc::dup2(slave_fd, libc::STDERR_FILENO);
                
                libc::close(slave_fd);
                libc::close(master_fd);

                let shell = "/bin/bash\0".as_ptr() as *const libc::c_char;
                let args = [shell, std::ptr::null()];
                libc::execvp(shell, args.as_ptr() as *const *const libc::c_char);
                
                libc::_exit(1);
            }

            // Parent: Close slave, keep master
            libc::close(slave_fd);
            Ok(Box::new(UnixHandle {
                fd: master_fd,
            }))
        }
    }

    fn set_raw_mode(&self, enabled: bool) -> Result<(), std::io::Error> {
        unsafe {
            let mut termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut termios) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            
            if enabled {
                // Disable canonical mode (buffered input) and echo
                termios.c_lflag &= !(libc::ICANON | libc::ECHO);
                // Set read timeout to 0, minimum characters to 1
                termios.c_cc[libc::VMIN] = 1;
                termios.c_cc[libc::VTIME] = 0;
            } else {
                termios.c_lflag |= (libc::ICANON | libc::ECHO);
            }
            
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &termios) == -1 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

struct UnixHandle {
    fd: RawFd,
}

impl crate::backend::BackendHandle for UnixHandle {
    fn read(&mut self) -> Result<Vec<u8>, std::io::Error> {
        let mut buf = vec![0u8; 4096];
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        buf.truncate(n as usize);
        Ok(buf)
    }

    fn write(&mut self, data: &[u8]) -> Result<(), std::io::Error> {
        let n = unsafe { libc::write(self.fd, data.as_ptr() as *const libc::c_void, data.len()) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if (n as usize) < data.len() {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "Partial write to PTY"));
        }
        Ok(())
    }

    fn close(&mut self) -> Result<(), std::io::Error> {
        if unsafe { libc::close(self.fd) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}
