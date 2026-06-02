//! Unix PTY backend: forks `/bin/bash` onto a freshly allocated pseudo-terminal
//! and exposes the master side as a [`BackendHandle`].

use std::os::unix::io::RawFd;
use std::sync::Mutex;

/// The host terminal's `termios` captured when raw mode was enabled, so it can
/// be restored exactly on exit.
static ORIGINAL_TERMIOS: Mutex<Option<libc::termios>> = Mutex::new(None);

/// Unit type implementing [`Backend`](crate::backend::Backend) for Unix-likes.
pub struct UnixBackend;

impl crate::backend::Backend for UnixBackend {
    fn spawn_shell(&self, cols: u16, rows: u16) -> Result<Box<dyn crate::backend::BackendHandle>, std::io::Error> {
        unsafe {
            // Seed the PTY with the initial window size so the child shell and
            // its children start out knowing the geometry.
            let ws = libc::winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };

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
                &ws,
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
                // Child: start a new session, then claim the slave as this
                // session's controlling terminal. Without TIOCSCTTY the kernel
                // never sets the slave's foreground process group, so Ctrl-C
                // wouldn't reach the shell and TIOCSWINSZ wouldn't deliver
                // SIGWINCH to it (a dup'd fd does not auto-acquire a ctty).
                libc::setsid();
                libc::ioctl(slave_fd, libc::TIOCSCTTY, 0);

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
            if enabled {
                let mut termios = std::mem::zeroed();
                if libc::tcgetattr(libc::STDIN_FILENO, &mut termios) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                // Stash the original so it can be restored verbatim on exit.
                *ORIGINAL_TERMIOS.lock().unwrap() = Some(termios);

                // Full raw mode: this clears ISIG (so Ctrl-C/Z/\ are forwarded
                // as bytes to the child instead of signalling us), IXON, ICRNL,
                // OPOST, ICANON, and ECHO. Without ISIG cleared, Ctrl-C would
                // kill the emulator instead of the running command.
                let mut raw = termios;
                libc::cfmakeraw(&mut raw);
                raw.c_cc[libc::VMIN] = 1;
                raw.c_cc[libc::VTIME] = 0;
                if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if let Some(orig) = ORIGINAL_TERMIOS.lock().unwrap().take()
                && libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &orig) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn terminal_size(&self) -> Option<(u16, u16)> {
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            let rc = libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws);
            if rc == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
                Some((ws.ws_col, ws.ws_row))
            } else {
                None
            }
        }
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
        // Short writes are normal on a PTY master (the slave's input queue is
        // finite), so loop until everything is accepted. Retry on EINTR; treat
        // a real error as fatal.
        let mut written = 0;
        while written < data.len() {
            let n = unsafe {
                libc::write(
                    self.fd,
                    data[written..].as_ptr() as *const libc::c_void,
                    data.len() - written,
                )
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(err);
            }
            if n == 0 {
                return Err(std::io::Error::other("PTY write returned 0"));
            }
            written += n as usize;
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

    fn set_winsize(&mut self, cols: u16, rows: u16) -> Result<(), std::io::Error> {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(self.fd, libc::TIOCSWINSZ, &ws) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for UnixHandle {
    fn drop(&mut self) {
        unsafe {
            // Reap the child if we own it. SIGHUP first, but escalate: a shell
            // that traps/ignores HUP must not wedge us in a blocking waitpid.
            if let Some(pid) = self.child.take() {
                let mut status = 0;
                let reaped = |status: &mut libc::c_int| {
                    libc::waitpid(pid, status, libc::WNOHANG) == pid
                };
                libc::kill(pid, libc::SIGHUP);
                let mut done = false;
                for _ in 0..25 {
                    // ~250ms grace
                    if reaped(&mut status) {
                        done = true;
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                if !done {
                    // SIGKILL cannot be trapped, so the final wait is bounded.
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, &mut status, 0);
                }
            }
            if self.fd >= 0 {
                libc::close(self.fd);
                self.fd = -1;
            }
        }
    }
}
