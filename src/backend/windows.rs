//! Windows ConPTY backend.
//!
//! Allocates a [pseudoconsole](https://learn.microsoft.com/windows/console/pseudoconsoles),
//! wires it to a pair of pipes, and spawns the shell (`%COMSPEC%`, default
//! `cmd.exe`) attached to it. The master side (our pipe ends plus the `HPCON`)
//! is exposed as a [`BackendHandle`], mirroring the Unix PTY backend.
//!
//! This module is only compiled on Windows (gated in `backend/mod.rs`).
//!
//! NOTE: run and verified on Windows 11 (build 26200) — shell spawn, child
//! `TERM`/`COLORTERM`, bidirectional relay, and OSC title capture all work.
//! Host resize propagation also works: there is no `SIGWINCH` equivalent to
//! wire, so `resize_poll` in `src/runtime/tokio_rt.rs` polls the console size
//! on a timer and calls [`BackendHandle::set_winsize`] on change.

use crate::backend::{Backend, BackendHandle};
use parking_lot::Mutex;

use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_BROKEN_PIPE, HANDLE,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, COORD, ClosePseudoConsole, CreatePseudoConsole, ENABLE_ECHO_INPUT,
    ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT,
    ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
    GetConsoleScreenBufferInfo, GetStdHandle, HPCON, ResizePseudoConsole, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE, SetConsoleMode,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
    InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
    STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

/// Unit type implementing [`Backend`](crate::backend::Backend) for Windows.
pub struct WindowsBackend;

/// Host console modes captured when raw mode was enabled, restored on exit.
/// Tuple is `(stdin_mode, stdout_mode)`.
static ORIGINAL_MODES: Mutex<Option<(u32, u32)>> = Mutex::new(None);

impl Backend for WindowsBackend {
    fn spawn_shell(
        &self,
        cols: u16,
        rows: u16,
        shell: Option<&str>,
        args: &[String],
        cwd: Option<&std::path::Path>,
    ) -> Result<Box<dyn BackendHandle>, std::io::Error> {
        use std::os::windows::ffi::OsStrExt;

        unsafe {
            // Two anonymous pipes. The child reads its input from `input_read`
            // and writes its output to `output_write`; we keep the opposite ends.
            let mut input_read: HANDLE = std::ptr::null_mut();
            let mut input_write: HANDLE = std::ptr::null_mut();
            let mut output_read: HANDLE = std::ptr::null_mut();
            let mut output_write: HANDLE = std::ptr::null_mut();
            if CreatePipe(&mut input_read, &mut input_write, std::ptr::null(), 0) == 0
                || CreatePipe(&mut output_read, &mut output_write, std::ptr::null(), 0) == 0
            {
                return Err(std::io::Error::last_os_error());
            }

            // Allocate the pseudoconsole over the child-side pipe ends.
            let size = COORD {
                X: cols as i16,
                Y: rows as i16,
            };
            let mut hpc: HPCON = 0;
            let hr = CreatePseudoConsole(size, input_read, output_write, 0, &mut hpc);
            // ConPTY duplicates the child-side handles; release our copies.
            CloseHandle(input_read);
            CloseHandle(output_write);
            if hr != 0 {
                CloseHandle(input_write);
                CloseHandle(output_read);
                return Err(std::io::Error::from_raw_os_error(hr));
            }

            // Build STARTUPINFOEXW carrying the pseudoconsole attribute.
            let mut si: STARTUPINFOEXW = std::mem::zeroed();
            si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            let mut attr_size: usize = 0;
            // First call "fails" by design, only to report the required size.
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attr_size);
            let mut attr_buf = vec![0u8; attr_size];
            si.lpAttributeList = attr_buf.as_mut_ptr() as *mut _;
            if InitializeProcThreadAttributeList(si.lpAttributeList, 1, 0, &mut attr_size) == 0 {
                let e = std::io::Error::last_os_error();
                ClosePseudoConsole(hpc);
                CloseHandle(input_write);
                CloseHandle(output_read);
                return Err(e);
            }
            if UpdateProcThreadAttribute(
                si.lpAttributeList,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                hpc as *const core::ffi::c_void,
                std::mem::size_of::<HPCON>(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ) == 0
            {
                let e = std::io::Error::last_os_error();
                DeleteProcThreadAttributeList(si.lpAttributeList);
                ClosePseudoConsole(hpc);
                CloseHandle(input_write);
                CloseHandle(output_read);
                return Err(e);
            }

            // The configured shell wins; else honor %COMSPEC%, default cmd.exe.
            // Bare names (`powershell`, `pwsh`, `wsl`) resolve through the
            // standard CreateProcessW search path, and arguments pass through
            // (`wsl -d Ubuntu`, `cmd /K ...`). An unquoted path containing
            // spaces is ambiguous to CreateProcessW (it would try
            // `C:\Program.exe` first), so when the whole string names an
            // existing file we quote it.
            let shell = shell
                .map(str::to_owned)
                .or_else(|| std::env::var("COMSPEC").ok())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "cmd.exe".to_string());
            let shell = if shell.contains(' ')
                && !shell.starts_with('"')
                && std::path::Path::new(&shell).is_file()
            {
                format!("\"{shell}\"")
            } else {
                shell
            };
            // CreateProcessW needs a mutable, null-terminated wide command line.
            // `shell` may already carry its own args (CreateProcessW parses the
            // whole string itself); explicit `args` — the caller's pre-split
            // argv, e.g. a trailing `-- prog arg...` — are appended, each
            // quoted so the child's argv parser splits them back out unchanged.
            let mut cmdline: Vec<u16> = std::ffi::OsStr::new(&shell).encode_wide().collect();
            for arg in args {
                cmdline.push(' ' as u16);
                push_quoted_arg(arg, &mut cmdline);
            }
            cmdline.push(0);

            let cwd_wide: Option<Vec<u16>> = cwd.map(|p| {
                p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
            });
            let cwd_ptr = cwd_wide.as_ref().map_or(std::ptr::null(), |w| w.as_ptr());

            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            let ok = CreateProcessW(
                std::ptr::null(),
                cmdline.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0, // bInheritHandles = FALSE: handles flow via the pseudoconsole
                EXTENDED_STARTUPINFO_PRESENT,
                std::ptr::null(),
                cwd_ptr,
                &si.StartupInfo,
                &mut pi,
            );
            // The attribute list is only needed through CreateProcessW.
            DeleteProcThreadAttributeList(si.lpAttributeList);
            drop(attr_buf);
            if ok == 0 {
                let e = std::io::Error::last_os_error();
                ClosePseudoConsole(hpc);
                CloseHandle(input_write);
                CloseHandle(output_read);
                return Err(e);
            }

            Ok(Box::new(WindowsHandle {
                input_write,
                output_read,
                hpc,
                process: pi.hProcess,
                thread: pi.hThread,
                owns: true,
            }))
        }
    }

    fn set_raw_mode(&self, enabled: bool) -> Result<(), std::io::Error> {
        unsafe {
            let hin = GetStdHandle(STD_INPUT_HANDLE);
            let hout = GetStdHandle(STD_OUTPUT_HANDLE);
            if enabled {
                let mut in_mode: u32 = 0;
                let mut out_mode: u32 = 0;
                if GetConsoleMode(hin, &mut in_mode) == 0
                    || GetConsoleMode(hout, &mut out_mode) == 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                *ORIGINAL_MODES.lock() = Some((in_mode, out_mode));
                // Raw input: no line buffering, echo, or Ctrl-C handling, but do
                // decode VT input sequences. Output: enable VT processing.
                let new_in = (in_mode
                    & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
                    | ENABLE_VIRTUAL_TERMINAL_INPUT;
                let new_out =
                    out_mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
                if SetConsoleMode(hin, new_in) == 0 || SetConsoleMode(hout, new_out) == 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if let Some((in_mode, out_mode)) = ORIGINAL_MODES.lock().take() {
                SetConsoleMode(hin, in_mode);
                SetConsoleMode(hout, out_mode);
            }
        }
        Ok(())
    }

    fn terminal_size(&self) -> Option<(u16, u16)> {
        unsafe {
            let hout = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(hout, &mut info) == 0 {
                return None;
            }
            let w = info.srWindow;
            let cols = (w.Right - w.Left + 1) as u16;
            let rows = (w.Bottom - w.Top + 1) as u16;
            if cols > 0 && rows > 0 {
                Some((cols, rows))
            } else {
                None
            }
        }
    }
}

/// Appends `arg` to `out` (a wide command-line buffer), quoted per the
/// MSVCRT/`CreateProcessW` argument rules — backslashes only escape when
/// immediately followed by a quote — so the child's argv parser splits it
/// back out unchanged. Mirrors the algorithm Windows itself documents.
fn push_quoted_arg(arg: &str, out: &mut Vec<u16>) {
    use std::os::windows::ffi::OsStrExt;
    let needs_quotes = arg.is_empty() || arg.chars().any(|c| c == ' ' || c == '\t' || c == '"');
    if !needs_quotes {
        out.extend(std::ffi::OsStr::new(arg).encode_wide());
        return;
    }
    out.push(b'"' as u16);
    let mut chars = arg.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let mut backslashes = 1;
            while chars.peek() == Some(&'\\') {
                chars.next();
                backslashes += 1;
            }
            let doubled = matches!(chars.peek(), Some('"') | None);
            for _ in 0..(if doubled { backslashes * 2 } else { backslashes }) {
                out.push(b'\\' as u16);
            }
        } else if c == '"' {
            out.push(b'\\' as u16);
            out.push(b'"' as u16);
        } else {
            let mut buf = [0u16; 2];
            out.extend(c.encode_utf16(&mut buf).iter());
        }
    }
    out.push(b'"' as u16);
}

/// Owns the master side of a ConPTY: the pipe ends we read/write and the
/// `HPCON` used for resizing. The handle returned by `spawn_shell` also owns the
/// child process/thread handles and the pseudoconsole, closing them on drop;
/// cloned handles own only their duplicated pipe ends.
struct WindowsHandle {
    input_write: HANDLE,
    output_read: HANDLE,
    hpc: HPCON,
    process: HANDLE,
    thread: HANDLE,
    owns: bool,
}

// The raw handles are only ever touched by one thread at a time (each clone is
// handed to a single thread), so moving a handle across threads is sound.
unsafe impl Send for WindowsHandle {}

impl BackendHandle for WindowsHandle {
    fn read(&mut self) -> Result<Vec<u8>, std::io::Error> {
        let mut buf = vec![0u8; 4096];
        let mut read: u32 = 0;
        let ok = unsafe {
            ReadFile(
                self.output_read,
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = std::io::Error::last_os_error();
            // The child closing its end surfaces as a broken pipe; treat it as a
            // clean EOF (the contract's empty-slice signal).
            if err.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                return Ok(Vec::new());
            }
            return Err(err);
        }
        buf.truncate(read as usize);
        Ok(buf)
    }

    fn write(&mut self, data: &[u8]) -> Result<(), std::io::Error> {
        // Pipes accept short writes; loop until everything is taken.
        let mut written_total = 0;
        while written_total < data.len() {
            let mut written: u32 = 0;
            let ok = unsafe {
                WriteFile(
                    self.input_write,
                    data[written_total..].as_ptr() as *const _,
                    (data.len() - written_total) as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            if written == 0 {
                return Err(std::io::Error::other("ConPTY write returned 0"));
            }
            written_total += written as usize;
        }
        Ok(())
    }

    fn try_clone(&self) -> Result<Box<dyn BackendHandle>, std::io::Error> {
        unsafe {
            let proc = GetCurrentProcess();
            let mut dup_in: HANDLE = std::ptr::null_mut();
            let mut dup_out: HANDLE = std::ptr::null_mut();
            if DuplicateHandle(
                proc,
                self.input_write,
                proc,
                &mut dup_in,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            ) == 0
                || DuplicateHandle(
                    proc,
                    self.output_read,
                    proc,
                    &mut dup_out,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS,
                ) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            // The clone shares the owner's `hpc` (an opaque value, not a kernel
            // handle to duplicate) but does not own it; the owner must outlive
            // its clones. It closes only its own duplicated pipe ends.
            Ok(Box::new(WindowsHandle {
                input_write: dup_in,
                output_read: dup_out,
                hpc: self.hpc,
                process: std::ptr::null_mut(),
                thread: std::ptr::null_mut(),
                owns: false,
            }))
        }
    }

    fn set_winsize(&mut self, cols: u16, rows: u16) -> Result<(), std::io::Error> {
        let size = COORD {
            X: cols as i16,
            Y: rows as i16,
        };
        let hr = unsafe { ResizePseudoConsole(self.hpc, size) };
        if hr != 0 {
            return Err(std::io::Error::from_raw_os_error(hr));
        }
        Ok(())
    }

    fn exit_token(&self) -> Option<Box<dyn FnOnce() + Send>> {
        // Only the owning handle holds the child process handle.
        if self.process.is_null() {
            return None;
        }
        // Duplicate it so a watcher thread can wait without owning the child
        // (the owner still reaps/terminates it on drop).
        let dup = unsafe {
            let proc = GetCurrentProcess();
            let mut dup: HANDLE = std::ptr::null_mut();
            if DuplicateHandle(proc, self.process, proc, &mut dup, 0, 0, DUPLICATE_SAME_ACCESS) == 0
            {
                return None;
            }
            dup
        };
        // HANDLE isn't `Send`; wrap it so the watcher thread can own it. A
        // `self`-consuming method forces the closure to capture the whole
        // wrapper (not the bare field, which disjoint captures would pick).
        struct Waitable(HANDLE);
        unsafe impl Send for Waitable {}
        impl Waitable {
            fn wait(self) {
                unsafe {
                    WaitForSingleObject(self.0, u32::MAX); // u32::MAX == INFINITE
                    CloseHandle(self.0);
                }
            }
        }
        let waitable = Waitable(dup);
        Some(Box::new(move || waitable.wait()))
    }
}

impl Drop for WindowsHandle {
    fn drop(&mut self) {
        unsafe {
            if !self.input_write.is_null() {
                CloseHandle(self.input_write);
            }
            if !self.output_read.is_null() {
                CloseHandle(self.output_read);
            }
            if self.owns {
                // Closing the pseudoconsole asks the child to exit; give it a
                // brief grace period, then make sure it's gone.
                ClosePseudoConsole(self.hpc);
                if !self.process.is_null() {
                    WaitForSingleObject(self.process, 2000);
                    TerminateProcess(self.process, 0);
                    CloseHandle(self.process);
                }
                if !self.thread.is_null() {
                    CloseHandle(self.thread);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::push_quoted_arg;

    fn quote(arg: &str) -> String {
        let mut out = Vec::new();
        push_quoted_arg(arg, &mut out);
        String::from_utf16(&out).unwrap()
    }

    #[test]
    fn plain_arg_is_unquoted() {
        assert_eq!(quote("hi"), "hi");
        assert_eq!(quote("-lc"), "-lc");
    }

    #[test]
    fn spaces_and_empty_get_quoted() {
        assert_eq!(quote("echo hi"), "\"echo hi\"");
        assert_eq!(quote(""), "\"\"");
    }

    #[test]
    fn embedded_quote_is_escaped() {
        assert_eq!(quote(r#"say "hi""#), r#""say \"hi\"""#);
    }

    #[test]
    fn trailing_backslashes_before_closing_quote_are_doubled() {
        assert_eq!(quote(r"C:\some dir\"), r#""C:\some dir\\""#);
    }

    #[test]
    fn backslashes_not_before_a_quote_pass_through() {
        assert_eq!(quote(r"C:\some\path"), r"C:\some\path");
    }
}
