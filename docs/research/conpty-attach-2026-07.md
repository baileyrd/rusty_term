# ConPTY child-attach failure on Windows 11 Insider 26200 (July 2026)

**Status: open — blocks all ConPTY e2e verification on this machine.**

While verifying G10 (win32-input-mode) against a real ConPTY child, both new
backend e2e tests failed the same way and initially wedged the whole test
suite. Investigation result: **this is not a rusty_term bug.**

## Symptom

On this machine (`beast`, Windows 11 Insider **10.0.26200.8737**),
`CreateProcessW` with `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` +
`EXTENDED_STARTUPINFO_PRESENT` **silently ignores the pseudoconsole
attribute**. Every API in the chain returns success, but:

- the child attaches to the *parent's* console — its output (`cmd.exe`
  banner, `echo` output) leaks to the parent console instead of the ConPTY
  output pipe;
- the ConPTY output pipe carries only conhost's own init sequences
  (`ESC[?9001h ESC[?1004h`), then goes silent forever;
- anything blocking on child output then hangs indefinitely (this is what
  originally wedged `cargo test` and required killing orphaned
  `conhost`/`cmd` processes).

## What was ruled out

- **rusty_term's spawn code** — a minimal standalone repro of Microsoft's
  canonical ConPTY sample (below) fails identically, all APIs returning
  success. `windows-sys 0.59` (real crate, correct constants).
- **Claude Code's command sandbox** — token is identical with the sandbox
  disabled (medium integrity, no AppContainer, no restricted SIDs); failure
  reproduces both ways, and detached via `Start-Process`.
- **Default-terminal delegation** — `HKCU\Console\%%Startup` delegation
  GUIDs are all-zero.
- **Session weirdness** — normal user, interactive session 1.

Suspiciously similar neighbor: GitHub Desktop is broken on 26200/26300
Insider builds via its process-creation sandbox attributes
(<https://github.com/desktop/desktop/issues/22306>) — same
`UpdateProcThreadAttribute` API family.

## Not yet discriminated

Whether the attach failure is (a) machine/build-wide, or (b) specific to
running nested under Claude Code's own ConPTY/process tree. Discriminator:
run the repro below from a plain interactive Windows Terminal window. If it
prints `ATTACHED` there, only harness-spawned processes are affected; if
`NOT ATTACHED`, the Insider build has broken ConPTY attach machine-wide
(rusty_term's GUI cannot spawn shells at all on this build) and this
deserves an upstream report to microsoft/terminal.

## Consequences for the test suite

- The two backend e2e tests (`conpty_child_output_reaches_the_reader`,
  `win32_input_records_round_trip_through_a_real_conpty_child`) are
  **expected to fail** in this environment until the OS issue is resolved;
  they are believed correct.
- Both must remain hang-proof: blocking `ReadFile` on a detached reader
  thread, deadlines enforced by polling a shared buffer, never an unbounded
  `join()`/teardown on the assertion path. The smoke test has been converted;
  keep the pattern for any future ConPTY test.

## Minimal repro

`cargo new conpty-repro`, `windows-sys 0.59` with features
`Win32_Foundation, Win32_Security, Win32_Storage_FileSystem,
Win32_System_Console, Win32_System_IO, Win32_System_Pipes,
Win32_System_Threading`:

```rust
// Success = "boot_ok" arrives via the ConPTY output pipe.
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::System::Console::{ClosePseudoConsole, CreatePseudoConsole, COORD, HPCON};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, UpdateProcThreadAttribute, WaitForSingleObject,
    EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
    STARTUPINFOEXW,
};

fn main() {
    unsafe {
        let (mut in_r, mut in_w, mut out_r, mut out_w): (HANDLE, HANDLE, HANDLE, HANDLE) =
            (std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
        assert!(CreatePipe(&mut in_r, &mut in_w, std::ptr::null(), 0) != 0);
        assert!(CreatePipe(&mut out_r, &mut out_w, std::ptr::null(), 0) != 0);

        let mut hpc: HPCON = std::mem::zeroed();
        let hr = CreatePseudoConsole(COORD { X: 80, Y: 24 }, in_r, out_w, 0, &mut hpc);
        eprintln!("CreatePseudoConsole hr={hr:#x} hpc={hpc:?}");
        assert_eq!(hr, 0);
        CloseHandle(in_r);
        CloseHandle(out_w);

        let mut si: STARTUPINFOEXW = std::mem::zeroed();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        let mut size: usize = 0;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
        let mut buf = vec![0u8; size];
        si.lpAttributeList = buf.as_mut_ptr() as *mut _;
        assert!(InitializeProcThreadAttributeList(si.lpAttributeList, 1, 0, &mut size) != 0);
        let ok = UpdateProcThreadAttribute(
            si.lpAttributeList,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            hpc as *const core::ffi::c_void,
            std::mem::size_of::<HPCON>(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        eprintln!("UpdateProcThreadAttribute ok={ok}");
        assert!(ok != 0);

        let mut cmdline: Vec<u16> = std::ffi::OsStr::new("cmd.exe /c echo boot_ok")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessW(
            std::ptr::null(),
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT,
            std::ptr::null(),
            std::ptr::null(),
            &si.StartupInfo,
            &mut pi,
        );
        eprintln!("CreateProcessW ok={ok} err={:?}", std::io::Error::last_os_error());
        assert!(ok != 0);
        DeleteProcThreadAttributeList(si.lpAttributeList);

        WaitForSingleObject(pi.hProcess, 5000);
        let mut code: u32 = u32::MAX;
        GetExitCodeProcess(pi.hProcess, &mut code);
        eprintln!("child exit code (259=still running): {code}");

        let out_r_val = out_r as usize;
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let h = out_r_val as HANDLE;
            let mut chunk = [0u8; 4096];
            loop {
                let mut n: u32 = 0;
                if ReadFile(h, chunk.as_mut_ptr() as *mut _, 4096, &mut n, std::ptr::null_mut()) == 0
                    || n == 0
                {
                    break;
                }
                if tx.send(chunk[..n as usize].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut all = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if let Ok(b) = rx.recv_timeout(std::time::Duration::from_millis(200)) {
                all.extend_from_slice(&b);
            }
        }
        eprintln!("pipe output ({} bytes): {:?}", all.len(), String::from_utf8_lossy(&all));
        let verdict = if all.windows(7).any(|w| w == b"boot_ok") { "ATTACHED" } else { "NOT ATTACHED" };
        eprintln!("verdict: {verdict}");
        ClosePseudoConsole(hpc);
        std::process::exit(if verdict == "ATTACHED" { 0 } else { 1 });
    }
}
```

Observed output in the failing environment:

```
CreatePseudoConsole hr=0x0 hpc=1973928567120
UpdateProcThreadAttribute ok=1
CreateProcessW ok=1 err=Os { code: 0, ... }
boot_ok                      <-- leaked to the PARENT console
child exit code (259=still running): 0
pipe output (16 bytes): "\u{1b}[?9001h\u{1b}[?1004h"
verdict: NOT ATTACHED
```

---

## Follow-up option (tracked, not committed): vendor OpenConsole

**Status: option only — do not implement until a *stable*-Windows conhost
regression actually bites. There is no evidence of one yet; the failure
above is on a pre-release Insider build.**

### The idea

`conhost.exe` — the console host that ConPTY spins up to translate the
legacy Win32 Console API into a VT byte stream — is open source. Microsoft
develops it in the open as **OpenConsole** (`microsoft/terminal`, the
`OpenConsole.exe` build target) and ships a copy inside Windows Terminal.
ConPTY supports pointing at a *bundled* `OpenConsole.exe` instead of the
system `conhost.exe`, so an app can pin a known-good host rather than
depend on whatever the OS ships. Windows Terminal itself does exactly this.

This is **not** "roll our own ConPTY." We are not reimplementing the
private `\Device\ConDrv` client/server protocol, console attachment, or
the legacy Console API surface — the undocumented, per-build-unstable
machinery that stopped winpty and forced it into screen-scraping. We would
be shipping *Microsoft's own* host, just a version we control instead of
the system's.

### How it wires in

The pseudoconsole is created in `WindowsBackend::spawn_shell`
(`src/backend/windows.rs`) via `CreatePseudoConsole`. Two mechanisms exist
to redirect it to a bundled host:

1. **Environment steering.** ConPTY honors the location of the current
   process's `conhost`; launching the child-host chain against a bundled
   binary is done by placing our `OpenConsole.exe` where the loader finds
   it first (alongside the executable) and letting `CreatePseudoConsole`'s
   internal host launch resolve to it. This is the low-touch path but the
   least explicitly documented.
2. **Explicit host handoff (the sanctioned path).** The
   `CreatePseudoConsole` flow that Windows Terminal uses hands the ConPTY
   an already-spawned `OpenConsole.exe --headless ...` over a
   signal/reference-handle pair (the `PseudoConsole`/`ConptyConnection`
   plumbing in the Terminal source). We would port the minimal slice of
   that: spawn our bundled `OpenConsole.exe` with the pseudoconsole
   in/out pipe handles and the `--headless --width --height --signal
   --server` arguments, instead of calling `CreatePseudoConsole` and
   letting it launch the system host. The rest of the backend
   (`ReadFile`/`WriteFile` on the pipes, `ResizePseudoConsole` →
   the signal pipe, teardown) is unchanged; only the host-spawn step moves.

Either way, `BackendHandle` and everything above it stay identical — this
is a backend-internal swap, invisible to the parser, grid, and renderers.

### Why it's an option and not a plan

The costs cut squarely against this project's minimal-footprint,
everything-from-source ethos:

- **A ~2 MB prebuilt binary in the tree / installer.** `OpenConsole.exe`
  is a Microsoft C++ build we would redistribute, not build from source
  (building it needs the full Windows Terminal toolchain — MSVC, the
  Windows SDK, its vcpkg deps — which is not something this repo will ever
  carry). That is a genuine "vendored blob" — the one thing the codebase
  otherwise never does.
- **A maintenance and update burden.** We would own tracking OpenConsole
  releases, security fixes, and version-compatibility with our ConPTY
  calls — the escape from "at the mercy of the system conhost" is really a
  trade for "on the hook to update a bundled conhost."
- **Licensing/redistribution diligence.** OpenConsole is MIT, so
  redistribution is fine, but bundling a Microsoft binary wants an
  explicit NOTICE/attribution entry and a provenance note (which exact
  release, which hash).
- **It fixes a problem we don't have.** The only observed failure is on an
  Insider build, and it's in `CreateProcessW`'s pseudoconsole-attribute
  handling — *upstream* of the host, so a bundled host would very likely
  still be attached through the same broken mechanism. Vendoring insures
  against a *system-conhost* regression specifically, which has not
  occurred on any shipping build.

### Decision criteria — when to revisit

Pull this option off the shelf only if **all** of these hold:

1. A reproducible ConPTY/console-host defect appears on a **stable**
   (non-Insider) Windows release, not just a pre-release build.
2. It is demonstrably *inside the host's* behavior (VT translation, mode
   handling, resize) rather than upstream in `CreateProcessW`/attach — so
   a newer host would actually fix it.
3. No system update resolves it in a reasonable window, and an upstream
   `microsoft/terminal` issue confirms it's host-side.

Absent all three, the recommendation stands: use the system ConPTY, treat
the Insider failure as an OS regression to wait out and report, and keep
the tree blob-free.

### Effort estimate if it is ever built

- Port the explicit host-handoff spawn from the Windows Terminal source
  into `spawn_shell` (path 2 above): **M** — mostly careful FFI mirroring
  of an existing, working sequence, plus the signal-pipe resize plumbing.
- Build/CI: fetch-and-verify (hash-pinned) the OpenConsole release in the
  Windows packaging step, or check the blob in under `vendor/` with a
  provenance record: **S**.
- The interesting risk is not code volume but the two undocumented-ish
  edges: exactly which `OpenConsole.exe` arguments a given release accepts,
  and whether the headless-host handoff stays stable across OpenConsole
  versions (the same "Microsoft may change it" caveat, just scoped to a
  binary we pin rather than the system).
