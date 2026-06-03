//! Windows ConPTY backend.
//!
//! Allocates a [pseudoconsole](https://learn.microsoft.com/windows/console/pseudoconsoles),
//! wires it to a pair of pipes, and spawns the shell (`%COMSPEC%`, default
//! `cmd.exe`) attached to it. The master side (our pipe ends plus the `HPCON`)
//! is exposed as a [`BackendHandle`], mirroring the Unix PTY backend.
//!
//! This module is only compiled on Windows (gated in `backend/mod.rs`).
//!
//! NOTE: this path has been type-checked (via a cross-target `cargo check`) but
//! not run — it needs a real Windows host to exercise.

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
    fn spawn_shell(&self, cols: u16, rows: u16) -> Result<Box<dyn BackendHandle>, std::io::Error> {
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

            // Honor %COMSPEC%, default cmd.exe. CreateProcessW needs a mutable,
            // null-terminated wide command line.
            let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
            let mut cmdline: Vec<u16> = std::ffi::OsStr::new(&shell)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();

            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            let ok = CreateProcessW(
                std::ptr::null(),
                cmdline.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0, // bInheritHandles = FALSE: handles flow via the pseudoconsole
                EXTENDED_STARTUPINFO_PRESENT,
                std::ptr::null(),
                std::ptr::null(),
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
