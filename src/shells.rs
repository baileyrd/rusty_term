//! Shell discovery: which shells exist on this machine, and which one to
//! spawn when neither the config file nor the environment names one.
//!
//! Two consumers:
//! - `--list-shells` prints every detected shell with its resolved path and
//!   exits — a quick way to see what `shell = "..."` values will work.
//! - [`detect_default`] picks the best available shell when nothing is
//!   configured: on Windows that upgrades the historical `cmd.exe` default to
//!   PowerShell 7 / Windows PowerShell when present; on Unix `$SHELL` (the
//!   user's login shell) still wins, with a probe-based fallback past it.
//!
//! Detection is a filesystem/`PATH` probe of well-known candidates — nothing
//! is executed.

use std::path::PathBuf;

/// A shell found on this machine.
#[derive(Clone)]
pub struct DetectedShell {
    /// The friendly name a config would use (`pwsh`, `wsl`, `zsh`, ...).
    pub name: &'static str,
    /// The resolved on-disk path.
    pub path: PathBuf,
}

/// Candidate shells per platform, in preference order: `(name, absolute
/// candidate paths, also try PATH?)`. Absolute candidates are checked first
/// so the listing shows a concrete location even when PATH would also hit.
#[cfg(windows)]
const CANDIDATES: &[(&str, &[&str], bool)] = &[
    (
        "pwsh",
        &[r"C:\Program Files\PowerShell\7\pwsh.exe"],
        true, // winget/scoop installs land elsewhere; PATH covers them
    ),
    (
        "powershell",
        &[r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"],
        true,
    ),
    ("cmd", &[r"C:\Windows\System32\cmd.exe"], true),
    ("wsl", &[r"C:\Windows\System32\wsl.exe"], true),
    ("git-bash", &[r"C:\Program Files\Git\bin\bash.exe"], false),
    ("nu", &[], true),
];

#[cfg(not(windows))]
const CANDIDATES: &[(&str, &[&str], bool)] = &[
    ("zsh", &["/bin/zsh", "/usr/bin/zsh"], true),
    ("fish", &["/usr/bin/fish", "/usr/local/bin/fish"], true),
    ("bash", &["/bin/bash", "/usr/bin/bash"], true),
    ("nu", &[], true),
    ("dash", &["/bin/dash"], true),
    ("sh", &["/bin/sh"], true),
];

/// Every candidate shell present on this machine, in preference order.
pub fn detect_all() -> Vec<DetectedShell> {
    let mut found = Vec::new();
    for &(name, paths, try_path) in CANDIDATES {
        let hit = paths
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .or_else(|| if try_path { which(name) } else { None });
        if let Some(path) = hit {
            found.push(DetectedShell { name, path });
        }
    }
    found
}

/// The shell to spawn when neither the config nor the environment names one.
///
/// Windows: the first hit in preference order — `pwsh` > `powershell` > `cmd`
/// (wsl/git-bash/nu are never auto-picked: a Windows-native default is the
/// safe choice, and `%COMSPEC%`-honoring callers still beat this). `None`
/// lets the backend fall through to its own `cmd.exe` default.
///
/// Unix: `None` — the backend's `$SHELL`-then-`/bin/bash` resolution already
/// reflects the user's choice (their login shell) better than any probe.
pub fn detect_default() -> Option<String> {
    #[cfg(windows)]
    {
        detect_all()
            .into_iter()
            .find(|s| matches!(s.name, "pwsh" | "powershell" | "cmd"))
            .map(|s| s.path.to_string_lossy().into_owned())
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Print the detected shells (for `--list-shells`).
pub fn print_detected() {
    let shells = detect_all();
    if shells.is_empty() {
        println!("no known shells detected");
        return;
    }
    println!("detected shells (usable as `shell = \"...\"` in the config):");
    for s in &shells {
        println!("  {:<12} {}", s.name, s.path.display());
    }
    match detect_default() {
        Some(d) => println!("default when unconfigured: {d}"),
        None => {
            #[cfg(not(windows))]
            println!("default when unconfigured: $SHELL, else /bin/bash");
            #[cfg(windows)]
            println!("default when unconfigured: %COMSPEC%, else cmd.exe");
        }
    }
}

/// Resolve `name` through `PATH` (and `PATHEXT` on Windows), like the shell
/// would. First hit wins.
fn which(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    #[cfg(windows)]
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
        .split(';')
        .map(|e| e.to_ascii_lowercase())
        .collect();
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        #[cfg(windows)]
        {
            for ext in &exts {
                let p = dir.join(format!("{name}{ext}"));
                if p.is_file() {
                    return Some(p);
                }
            }
        }
        #[cfg(not(windows))]
        {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_all_finds_the_platform_staple() {
        // Every supported platform ships at least one candidate: cmd.exe on
        // Windows, /bin/sh on Unix. The probe must find it.
        let shells = detect_all();
        assert!(!shells.is_empty(), "no shells detected at all");
        #[cfg(windows)]
        assert!(shells.iter().any(|s| s.name == "cmd"), "cmd.exe not found");
        #[cfg(unix)]
        assert!(shells.iter().any(|s| s.name == "sh"), "/bin/sh not found");
        // Every reported path actually exists.
        for s in &shells {
            assert!(s.path.is_file(), "{} reported at non-file {:?}", s.name, s.path);
        }
    }

    #[test]
    fn default_pick_is_windows_native_or_none() {
        let d = detect_default();
        #[cfg(windows)]
        {
            // cmd.exe always exists, so Windows always has a pick, and it is
            // never wsl/git-bash (POSIX inside a Windows terminal would be a
            // surprising unconfigured default).
            let d = d.expect("windows always detects at least cmd");
            let lower = d.to_ascii_lowercase();
            assert!(
                lower.contains("pwsh") || lower.contains("powershell") || lower.contains("cmd"),
                "unexpected default: {d}"
            );
        }
        #[cfg(unix)]
        assert!(d.is_none(), "unix must defer to $SHELL");
    }

    #[test]
    fn which_resolves_a_known_binary() {
        // `cmd` (Windows) / `sh` (Unix) are PATH-resolvable everywhere.
        #[cfg(windows)]
        assert!(which("cmd").is_some());
        #[cfg(unix)]
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-shell-9z").is_none());
    }
}
