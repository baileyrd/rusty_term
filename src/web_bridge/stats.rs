//! Session stats for the web frontend's status ribbon and side dock: system
//! load and memory pressure (Linux `/proc`; `null` elsewhere), and the git
//! branch + working-tree counts for the shell's reported cwd.
//!
//! Serialized by hand into one small JSON object per push — the bridge only
//! ever *writes* JSON, so this stays dependency-free like the rest of the
//! tree. Git branch comes from walking to `.git/HEAD` (pure file reads,
//! mirroring the native status ribbon); the added/modified/deleted counts
//! are the one place a `git status --porcelain` subprocess is used — there
//! is no sane hand-rolled way to diff the index, and this is an opt-in dev
//! bridge, not the terminal core. A missing `git` binary degrades to
//! branch-only.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long one gathered `GitInfo` is trusted before the cwd is re-examined.
const GIT_TTL: Duration = Duration::from_secs(3);

/// Git facts for one directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GitInfo {
    pub(crate) branch: Option<String>,
    pub(crate) added: u32,
    pub(crate) modified: u32,
    pub(crate) deleted: u32,
}

/// Per-session cache: `git status` every stats tick would be rude.
pub(crate) struct GitCache {
    cached: Option<(PathBuf, GitInfo, Instant)>,
}

impl GitCache {
    pub(crate) fn new() -> Self {
        GitCache { cached: None }
    }

    pub(crate) fn info(&mut self, cwd: &Path) -> GitInfo {
        if let Some((dir, info, at)) = &self.cached
            && dir == cwd
            && at.elapsed() < GIT_TTL
        {
            return info.clone();
        }
        let info = gather_git(cwd);
        self.cached = Some((cwd.to_path_buf(), info.clone(), Instant::now()));
        info
    }
}

/// Branch from `.git/HEAD` (worktree `gitdir:` files followed) plus
/// working-tree counts from `git status --porcelain`.
fn gather_git(cwd: &Path) -> GitInfo {
    let branch = read_git_branch(cwd);
    if branch.is_none() {
        return GitInfo::default(); // not a repository: no point running git
    }
    let (added, modified, deleted) = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_porcelain(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or((0, 0, 0));
    GitInfo { branch, added, modified, deleted }
}

/// Count added / modified / deleted entries in `git status --porcelain`
/// output: untracked and index-added files are "added", deletions in either
/// tree are "deleted", everything else changed is "modified". Rename lines
/// (`R`) count as modified.
pub(crate) fn parse_porcelain(out: &str) -> (u32, u32, u32) {
    let (mut added, mut modified, mut deleted) = (0, 0, 0);
    for line in out.lines() {
        let mut chars = line.chars();
        let (x, y) = (chars.next().unwrap_or(' '), chars.next().unwrap_or(' '));
        if x == '?' || x == 'A' {
            added += 1;
        } else if x == 'D' || y == 'D' {
            deleted += 1;
        } else if x != ' ' || y != ' ' {
            modified += 1;
        }
    }
    (added, modified, deleted)
}

/// The current git branch for `dir`: walk up to the nearest `.git`, follow a
/// worktree/submodule `gitdir:` file, parse `HEAD` — the same pure-file-read
/// resolution the native status ribbon uses.
fn read_git_branch(dir: &Path) -> Option<String> {
    let mut cur = Some(dir);
    let git = loop {
        let d = cur?;
        let candidate = d.join(".git");
        if candidate.exists() {
            break candidate;
        }
        cur = d.parent();
    };
    let head_path = if git.is_file() {
        let text = std::fs::read_to_string(&git).ok()?;
        let target = text.strip_prefix("gitdir:")?.trim();
        let target = Path::new(target);
        let base =
            if target.is_absolute() { target.to_path_buf() } else { git.parent()?.join(target) };
        base.join("HEAD")
    } else {
        git.join("HEAD")
    };
    let head = std::fs::read_to_string(head_path).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: ") {
        Some(r) => Some(r.strip_prefix("refs/heads/").unwrap_or(r).to_string()),
        None if head.len() >= 8 => Some(head[..8].to_string()),
        None => None,
    }
}

/// 1-minute load average normalized by core count, `0.0..` (may exceed 1 on
/// an overloaded box; the sparkline clamps). Linux only.
pub(crate) fn system_load() -> Option<f32> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/loadavg").ok()?;
        let one: f32 = text.split_ascii_whitespace().next()?.parse().ok()?;
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f32;
        Some(one / cores)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Fraction of memory in use (`1 - MemAvailable/MemTotal`). Linux only.
pub(crate) fn memory_used() -> Option<f32> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        let field = |name: &str| -> Option<f32> {
            text.lines()
                .find(|l| l.starts_with(name))?
                .split_ascii_whitespace()
                .nth(1)?
                .parse()
                .ok()
        };
        let (total, avail) = (field("MemTotal:")?, field("MemAvailable:")?);
        (total > 0.0).then(|| 1.0 - avail / total)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Serialize one stats push. Numbers render plainly, absent values as
/// `null`; the only strings are the branch and cwd, escaped minimally
/// (backslash, quote, control bytes — the full set RFC 8259 requires).
pub(crate) fn stats_json(
    load: Option<f32>,
    mem: Option<f32>,
    cwd: Option<&Path>,
    git: &GitInfo,
) -> String {
    let num = |v: Option<f32>| match v {
        Some(v) if v.is_finite() => format!("{v:.3}"),
        _ => "null".to_string(),
    };
    let string = |v: Option<&str>| match v {
        Some(s) => format!("\"{}\"", json_escape(s)),
        None => "null".to_string(),
    };
    format!(
        "{{\"load\":{},\"mem\":{},\"cwd\":{},\"branch\":{},\"git\":{{\"added\":{},\"modified\":{},\"deleted\":{}}}}}",
        num(load),
        num(mem),
        string(cwd.and_then(|p| p.to_str())),
        string(git.branch.as_deref()),
        git.added,
        git.modified,
        git.deleted,
    )
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_counts_added_modified_deleted() {
        let out = "?? new.txt\nA  staged.rs\n M dirty.rs\nMM both.rs\n D gone.rs\nD  staged-gone.rs\nR  old -> new\n";
        assert_eq!(parse_porcelain(out), (2, 3, 2));
        assert_eq!(parse_porcelain(""), (0, 0, 0));
    }

    #[test]
    fn stats_json_shape_and_escaping() {
        let git = GitInfo {
            branch: Some("feat/\"quo\\ted\"".to_string()),
            added: 1,
            modified: 2,
            deleted: 3,
        };
        let json = stats_json(Some(0.25), None, Some(Path::new("/tmp/x")), &git);
        assert_eq!(
            json,
            "{\"load\":0.250,\"mem\":null,\"cwd\":\"/tmp/x\",\"branch\":\"feat/\\\"quo\\\\ted\\\"\",\"git\":{\"added\":1,\"modified\":2,\"deleted\":3}}"
        );
        // And it must be machine-parseable: a control char escapes.
        let git = GitInfo { branch: Some("a\nb".into()), ..GitInfo::default() };
        assert!(stats_json(None, None, None, &git).contains("a\\u000ab"));
    }

    #[test]
    fn git_branch_resolves_in_this_repo() {
        // The test runs inside a checkout, so the walk finds a branch (or a
        // detached short hash) somewhere above the test cwd.
        let here = std::env::current_dir().unwrap();
        assert!(read_git_branch(&here).is_some());
    }
}
