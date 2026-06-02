//! Unix PTY backend: forks `/bin/bash` onto a freshly allocated pseudo-terminal
//! and exposes the master side as a [`BackendHandle`].

use std::os::unix::io::RawFd;

/// Unit type implementing [`Backend`](crate::backend::Backend) for Unix-likes.
pub struct UnixBackend;

impl crate::backend::Backend for UnixBackend {
    fn spawn_shell(&self) -> Result<Box<dyn crate::backend::BackendHandle>, std::io::Error> {
        unsafe {
            // openpty writes the master fd to the first out-param and the slave
            // fd to the second, returning 0 on success / -1 on error. Both
            // pointers must be valid — passing null for the slave segfaults.
            let mut master_fd: libc::c_int = -1;
            let mut slave_fd: libc::c_int = -1;
            let rc = libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if rc < 0 {
                return Err(std::io::Error::last_os_error());
            }

            let pid = libc::fork();
            if pid == -1 {
                libc::close(master_fd);
                libc::close(slave_fd);
                return Err(std::io::Error::last_os_error());
            }

            if pid == 0 {
                // Child: detach from the controlling terminal and attach to the
                // PTY slave as stdin/stdout/stderr, then exec the shell.
                libc::setsid();

                libc::dup2(slave_fd, libc::STDIN_FILENO);
                libc::dup2(slave_fd, libc::STDOUT_FILENO);
                libc::dup2(slave_fd, libc::STDERR_FILENO);

                libc::close(slave_fd);
                libc::close(master_fd);

                let shell = c"/bin/bash";
                let args = [shell.as_ptr(), std::ptr::null()];
                libc::execvp(shell.as_ptr(), args.as_ptr());

                // Only reached if exec failed.
                libc::_exit(1);
            }

            // Parent: close the slave, keep the master.
            libc::close(slave_fd);
            Ok(Box::new(UnixHandle { fd: master_fd, child: Some(pid) }))
        }
    }

    fn set_raw_mode(&self, enabled: bool) -> Result<(), std::io::Error> {
        unsafe {
            let mut termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut termios) == -1 {
                return Err(std::io::Error::last_os_error());
            }

            if enabled {
                // Disable canonical mode (line buffering) and echo.
                termios.c_lflag &= !(libc::ICANON | libc::ECHO);
                // Block until at least one byte is available, no inter-byte timer.
                termios.c_cc[libc::VMIN] = 1;
                termios.c_cc[libc::VTIME] = 0;
            } else {
                termios.c_lflag |= libc::ICANON | libc::ECHO;
            }

            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &termios) == -1 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

/// Owns the master side of the PTY. The handle created by `spawn_shell` also
/// owns the child PID and reaps it on drop; cloned handles do not.
struct UnixHandle {
    fd: RawFd,
    child: Option<libc::pid_t>,
}

impl crate::backend::BackendHandle for UnixHandle {
    fn read(&mut self) -> Result<Vec<u8>, std::io::Error> {
        let mut buf = vec![0u8; 4096];
        let n = unsafe {
            libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            // The shell closing the slave surfaces as EIO on the master; treat
            // that as a clean end-of-file rather than a hard error.
            if err.raw_os_error() == Some(libc::EIO) {
                return Ok(Vec::new());
            }
            return Err(err);
        }
        buf.truncate(n as usize);
        Ok(buf)
    }

    fn write(&mut self, data: &[u8]) -> Result<(), std::io::Error> {
        let n = unsafe {
            libc::write(self.fd, data.as_ptr() as *const libc::c_void, data.len())
        };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if (n as usize) < data.len() {
            return Err(std::io::Error::other("Partial write to PTY"));
        }
        Ok(())
    }

    fn try_clone(&self) -> Result<Box<dyn crate::backend::BackendHandle>, std::io::Error> {
        let dup_fd = unsafe { libc::dup(self.fd) };
        if dup_fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Box::new(UnixHandle { fd: dup_fd, child: None }))
    }
}

impl Drop for UnixHandle {
    fn drop(&mut self) {
        unsafe {
            // Reap the child if we own it, hanging it up first so an idle shell
            // doesn't linger as a zombie.
            if let Some(pid) = self.child.take() {
                libc::kill(pid, libc::SIGHUP);
                let mut status = 0;
                libc::waitpid(pid, &mut status, 0);
            }
            if self.fd >= 0 {
                libc::close(self.fd);
                self.fd = -1;
            }
        }
    }
}
