//! Selecting the `TERM` identity advertised to the child shell.
//!
//! rusty_term ships a self-describing `rusty_term` terminfo entry (see
//! `extra/rusty_term.terminfo`), but that entry only helps once it's compiled
//! into a terminfo database with `tic`. To avoid ever handing the child a
//! `TERM` that ncurses can't resolve — which breaks every curses app — we probe
//! the standard terminfo locations and advertise `rusty_term` only when its
//! compiled entry is actually installed, falling back to the near-universal
//! `xterm-256color` (the repertoire we implement and inherit from) otherwise.

use std::path::PathBuf;

/// The `TERM` value to advertise to the child: our own entry when its compiled
/// terminfo is installed, else the portable fallback.
pub fn resolve_term() -> &'static str {
    if entry_in_dirs("rusty_term", &search_dirs()) {
        "rusty_term"
    } else {
        "xterm-256color"
    }
}

/// The ordered terminfo search path, mirroring ncurses: `$TERMINFO`, then
/// `$HOME/.terminfo`, then `$TERMINFO_DIRS` (an empty element meaning the system
/// default), then the conventional system directories.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("TERMINFO") {
        dirs.push(PathBuf::from(d));
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".terminfo"));
    }
    if let Some(list) = std::env::var_os("TERMINFO_DIRS") {
        for p in std::env::split_paths(&list) {
            dirs.push(if p.as_os_str().is_empty() {
                PathBuf::from("/usr/share/terminfo")
            } else {
                p
            });
        }
    }
    for d in [
        "/etc/terminfo",
        "/usr/share/terminfo",
        "/usr/lib/terminfo",
        "/lib/terminfo",
    ] {
        dirs.push(PathBuf::from(d));
    }
    dirs
}

/// Whether `name`'s compiled entry exists under any of `dirs`. ncurses stores it
/// as `<dir>/<first-letter>/<name>` and, on some systems (and case-insensitive
/// filesystems), `<dir>/<hex-first-byte>/<name>`; both leaf forms are checked.
fn entry_in_dirs(name: &str, dirs: &[PathBuf]) -> bool {
    let Some(first) = name.bytes().next() else {
        return false;
    };
    let leaves = [
        format!("{}/{}", first as char, name),
        format!("{:x}/{}", first, name),
    ];
    dirs.iter()
        .any(|dir| leaves.iter().any(|leaf| dir.join(leaf).exists()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A unique scratch directory under the system temp dir, removed on drop so
    /// the probe tests don't leak files or collide across parallel runs.
    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new(tag: &str) -> Self {
            let p =
                std::env::temp_dir().join(format!("rusty_term_tfi_{}_{}", std::process::id(), tag));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn detects_entry_under_first_letter_dir() {
        let tmp = TmpDir::new("letter");
        fs::create_dir_all(tmp.0.join("r")).unwrap();
        fs::write(tmp.0.join("r/rusty_term"), b"compiled").unwrap();
        assert!(entry_in_dirs("rusty_term", std::slice::from_ref(&tmp.0)));
    }

    #[test]
    fn detects_entry_under_hex_dir() {
        let tmp = TmpDir::new("hex");
        fs::create_dir_all(tmp.0.join("72")).unwrap(); // 0x72 == 'r'
        fs::write(tmp.0.join("72/rusty_term"), b"compiled").unwrap();
        assert!(entry_in_dirs("rusty_term", std::slice::from_ref(&tmp.0)));
    }

    #[test]
    fn missing_entry_is_not_detected() {
        let tmp = TmpDir::new("empty");
        assert!(!entry_in_dirs("rusty_term", std::slice::from_ref(&tmp.0)));
    }

    #[test]
    fn empty_name_is_not_detected() {
        assert!(!entry_in_dirs("", &[std::env::temp_dir()]));
    }
}
