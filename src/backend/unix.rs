//! Unix PTY backend: forks `/bin/bash` onto a freshly allocated pseudo-terminal
//! and exposes the master side as a [`BackendHandle`].

use parking_lot::Mutex;
use std::os::unix::io::RawFd;

/// The host terminal's `termios` captured when raw mode was enabled, so it can
/// be restored exactly on exit.
static ORIGINAL_TERMIOS: Mutex<Option<libc::termios>> = Mutex::new(None);

/// Unit type implementing [`Backend`](crate::backend::Backend) for Unix-likes.
pub struct UnixBackend;

impl crate::backend::Backend for UnixBackend {
    fn spawn_shell(
        &self,
        cols: u16,
        rows: u16,
        shell: Option<&str>,
        args: &[String],
        cwd: Option<&std::path::Path>,
    ) -> Result<Box<dyn crate::backend::BackendHandle>, std::io::Error> {
        // The configured shell wins; else honor the user's `$SHELL`, falling
        // back to /bin/bash.
        let shell = shell
            .map(str::to_owned)
            .or_else(|| std::env::var("SHELL").ok())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/bash".to_string());

        // Resolve program + argv: explicit `args` (the caller's pre-split
        // argv, e.g. a trailing `-- prog arg...`) win outright — `shell` is
        // then the bare program. With no explicit args, split any embedded
        // args out of the `shell` string itself (a config `shell = "bash
        // --login -i"`), matching how Windows' CreateProcessW already parses
        // its whole command-line string.
        let mut words: Vec<String> = if !args.is_empty() {
            std::iter::once(shell.clone())
                .chain(args.iter().cloned())
                .collect()
        } else {
            split_shell_words(&shell)
        };
        if words.is_empty() {
            words.push(shell.clone());
        }
        // CString allocation happens here in the parent, before fork, so the
        // child's post-fork, pre-exec path (only async-signal-safe work is
        // sound there) never allocates. An interior NUL (impossible for a
        // real program/arg) falls back to /bin/bash bare.
        let argv: Vec<std::ffi::CString> = words
            .iter()
            .map(|s| std::ffi::CString::new(s.as_str()))
            .collect::<Result<_, _>>()
            .unwrap_or_else(|_| vec![std::ffi::CString::new("/bin/bash").unwrap()]);
        let program = argv[0].clone();

        let cwd = cwd.map(|p| {
            use std::os::unix::ffi::OsStrExt;
            std::ffi::CString::new(p.as_os_str().as_bytes())
        });
        // An unrepresentable cwd (interior NUL, impossible for a real path)
        // is treated the same as "not given" rather than failing the spawn.
        let cwd = cwd.and_then(Result::ok);

        // Diagnostic for a failed exec, assembled here (pre-fork) so the
        // child's post-fork path only does async-signal-safe work (write(2)).
        // CRLF because the host terminal is already in raw mode by now.
        let exec_err = {
            let mut m = b"rusty_term: failed to start shell '".to_vec();
            m.extend_from_slice(shell.as_bytes());
            m.extend_from_slice(b"'\r\n");
            m
        };

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

            // Don't leak the master into shells spawned later (e.g. the GUI's
            // "open in editor"): a held dup would keep this PTY alive past exit.
            // The child closes master explicitly and uses dup2 for std{in,out,
            // err} (dup2 clears CLOEXEC on its targets), so this is safe.
            libc::fcntl(master_fd, libc::F_SETFD, libc::FD_CLOEXEC);

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

                if let Some(cwd) = &cwd
                    && libc::chdir(cwd.as_ptr()) != 0
                {
                    libc::_exit(1);
                }

                let mut argv_ptrs: Vec<*const libc::c_char> =
                    argv.iter().map(|a| a.as_ptr()).collect();
                argv_ptrs.push(std::ptr::null());
                libc::execvp(program.as_ptr(), argv_ptrs.as_ptr());

                // Only reached if exec failed. Tell the user on the terminal
                // (write(2) is async-signal-safe) rather than vanishing with a
                // bare exit, then use 127 — the conventional "command not
                // found" code — so the wait status is diagnosable.
                libc::write(
                    libc::STDERR_FILENO,
                    exec_err.as_ptr() as *const libc::c_void,
                    exec_err.len(),
                );
                libc::_exit(127);
            }

            // Parent: close the slave, keep the master.
            libc::close(slave_fd);
            Ok(Box::new(UnixHandle {
                fd: master_fd,
                child: Some(pid),
            }))
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
                *ORIGINAL_TERMIOS.lock() = Some(termios);

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
            } else if let Some(orig) = ORIGINAL_TERMIOS.lock().take()
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

/// Splits a shell command string into a program + argv, honoring single and
/// double quotes and backslash escapes — a small POSIX-subset word splitter
/// (not a full shell grammar) sufficient for a config `shell = "bash --login
/// -i"` style value. Windows needs no equivalent: `CreateProcessW` already
/// parses the whole command-line string itself.
fn split_shell_words(s: &str) -> Vec<String> {
    #[derive(Clone, Copy, PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut has_cur = false;
    let mut quote = Quote::None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Quote::None, ' ' | '\t') => {
                if has_cur {
                    words.push(std::mem::take(&mut cur));
                    has_cur = false;
                }
            }
            (Quote::None, '\'') => {
                quote = Quote::Single;
                has_cur = true;
            }
            (Quote::None, '"') => {
                quote = Quote::Double;
                has_cur = true;
            }
            (Quote::None, '\\') => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                    has_cur = true;
                }
            }
            (Quote::Single, '\'') => quote = Quote::None,
            (Quote::Double, '"') => quote = Quote::None,
            (Quote::Double, '\\') if matches!(chars.peek(), Some('\\') | Some('"')) => {
                cur.push(chars.next().unwrap());
                has_cur = true;
            }
            (_, c) => {
                cur.push(c);
                has_cur = true;
            }
        }
    }
    if has_cur {
        words.push(cur);
    }
    words
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
        loop {
            let n =
                unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                // The shell closing the slave surfaces as EIO on the master; treat
                // that as a clean end-of-file rather than a hard error.
                if err.raw_os_error() == Some(libc::EIO) {
                    return Ok(Vec::new());
                }
                // A signal landing on this thread mid-syscall (a profiler, a
                // debugger, anything installing a non-`SA_RESTART` handler) is
                // not a real read error; the GUI reader thread maps any `Err`
                // here to "child exited" and closes the tab, so an unrelated
                // signal used to be able to kill a perfectly healthy pane.
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(err);
            }
            buf.truncate(n as usize);
            return Ok(buf);
        }
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
        // Keep the no-leak-into-later-children property across dups too (dup(2)
        // does not copy the flag).
        unsafe { libc::fcntl(dup_fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        Ok(Box::new(UnixHandle {
            fd: dup_fd,
            child: None,
        }))
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

    fn pty_fd(&self) -> RawFd {
        self.fd
    }

    fn reap_exit_status(&mut self) -> Option<i32> {
        let pid = self.child?;
        // A bounded, mostly-non-blocking wait: the caller already knows the
        // child exited (read-EOF, or a SIGCHLD this handle's watcher won the
        // race for) before calling this, so the very first `WNOHANG` poll is
        // expected to succeed — the sleep loop only guards the rare case
        // where the two events land in the opposite order.
        for _ in 0..50 {
            let mut status: libc::c_int = 0;
            // SAFETY: WNOHANG never blocks.
            let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if r == pid {
                self.child = None; // reaped: Drop must not try again
                return Some(exit_code_from_wait_status(status));
            }
            if r < 0 {
                // ECHILD: something else (the SIGCHLD watcher) already
                // reaped it and won the race. Nothing left to report here.
                self.child = None;
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }

    fn child_pid(&self) -> Option<libc::pid_t> {
        self.child
    }
}

/// Convert a `waitpid` status into a `std::process::exit`-compatible value:
/// the child's own exit code on a normal exit, or 128+signal (the sh/bash
/// convention) on a signal death.
pub(crate) fn exit_code_from_wait_status(status: libc::c_int) -> i32 {
    if status & 0x7f == 0 {
        (status >> 8) & 0xff
    } else {
        128 + (status & 0x7f)
    }
}

impl Drop for UnixHandle {
    fn drop(&mut self) {
        unsafe {
            // Reap the child if we own it. SIGHUP first, but escalate: a shell
            // that traps/ignores HUP must not wedge us in a blocking waitpid.
            if let Some(pid) = self.child.take() {
                let mut status = 0;
                let reaped =
                    |status: &mut libc::c_int| libc::waitpid(pid, status, libc::WNOHANG) == pid;
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

#[cfg(test)]
mod tests {
    use super::{exit_code_from_wait_status, split_shell_words};

    #[test]
    fn bare_program_is_a_single_word() {
        assert_eq!(split_shell_words("zsh"), vec!["zsh"]);
    }

    #[test]
    fn splits_on_whitespace() {
        assert_eq!(
            split_shell_words("bash --login -i"),
            vec!["bash", "--login", "-i"]
        );
        assert_eq!(split_shell_words("  bash   -i  "), vec!["bash", "-i"]);
    }

    #[test]
    fn honors_quotes_and_escapes() {
        assert_eq!(
            split_shell_words(r#"bash -lc "echo hi there""#),
            vec!["bash", "-lc", "echo hi there"]
        );
        assert_eq!(
            split_shell_words("bash -lc 'echo hi'"),
            vec!["bash", "-lc", "echo hi"]
        );
        assert_eq!(split_shell_words(r"a\ b c"), vec!["a b", "c"]);
    }

    #[test]
    fn empty_string_yields_no_words() {
        assert!(split_shell_words("").is_empty());
        assert!(split_shell_words("   ").is_empty());
    }

    #[test]
    fn wait_status_decodes_a_normal_exit_code() {
        // A normal exit packs the code into bits 8-15, low 7 bits (the signal
        // field) all zero; libc's WEXITSTATUS-equivalent shift/mask.
        assert_eq!(exit_code_from_wait_status(0), 0);
        assert_eq!(exit_code_from_wait_status(42 << 8), 42);
        assert_eq!(exit_code_from_wait_status(255 << 8), 255);
    }

    #[test]
    fn wait_status_decodes_a_signal_death_as_128_plus_signal() {
        // Killed by a signal: the low 7 bits carry the signal number (bit
        // 0x80, set on a core dump, must not leak into the reported code).
        assert_eq!(
            exit_code_from_wait_status(libc::SIGKILL),
            128 + libc::SIGKILL
        );
        assert_eq!(
            exit_code_from_wait_status(libc::SIGSEGV | 0x80),
            128 + libc::SIGSEGV
        );
    }

    /// Smoke test: a real child spawns on a real PTY, its output reaches the
    /// reader, and `reap_exit_status` reports the exit code it actually used
    /// — the same shape of coverage `backend::windows` already has for
    /// ConPTY, previously missing on the Unix side.
    #[test]
    fn spawned_child_output_reaches_the_reader_and_its_exit_code_is_reaped() {
        use crate::backend::Backend as _;
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        let mut h = super::UnixBackend
            .spawn_shell(
                80,
                24,
                Some("/bin/sh"),
                &["-c".into(), "echo boot_ok; exit 7".into()],
                None,
            )
            .expect("spawn");
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut rh = h.try_clone().expect("clone read handle");
        let out2 = Arc::clone(&out);
        let reader = std::thread::spawn(move || {
            loop {
                match rh.read() {
                    Ok(b) if b.is_empty() => break, // EOF: the child exited
                    Ok(b) => out2.lock().unwrap().extend_from_slice(&b),
                    Err(_) => break,
                }
            }
        });

        reader.join().expect("reader thread panicked");
        let buf = out.lock().unwrap().clone();
        assert!(
            buf.windows(7).any(|w| w == b"boot_ok"),
            "no child output: {:?}",
            String::from_utf8_lossy(&buf)
        );

        // `reap_exit_status` polls WNOHANG internally, so this alone bounds
        // the wait rather than needing our own deadline loop.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(code) = h.reap_exit_status() {
                assert_eq!(code, 7);
                return;
            }
            assert!(
                Instant::now() < deadline,
                "reap_exit_status never reported an exit"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A cloned handle round-trips bytes written to the child back out
    /// through the read side — `try_clone` gives the read/write split its
    /// own independent fd, not just a second reference to the same one.
    #[test]
    fn a_cloned_handle_can_write_and_read_back_through_the_same_child() {
        use crate::backend::Backend as _;
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        let h = super::UnixBackend
            .spawn_shell(80, 24, Some("/bin/cat"), &[], None)
            .expect("spawn /bin/cat");
        let mut read_handle = h.try_clone().expect("clone read handle");
        let mut write_handle = h.try_clone().expect("clone write handle");
        // NOTE: `h` must stay alive until the assertions pass — the owning
        // handle's Drop SIGHUPs (then SIGKILLs) the child, so dropping it
        // early races cat's echo and makes the test flaky.

        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let out2 = Arc::clone(&out);
        std::thread::spawn(move || {
            loop {
                match read_handle.read() {
                    Ok(b) if b.is_empty() => break,
                    Ok(b) => out2.lock().unwrap().extend_from_slice(&b),
                    Err(_) => break,
                }
            }
        });

        write_handle.write(b"hello cat\n").expect("write");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if out.lock().unwrap().windows(9).any(|w| w == b"hello cat") {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "cat never echoed the input back: {:?}",
                String::from_utf8_lossy(&out.lock().unwrap())
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
