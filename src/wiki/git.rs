// SPDX-License-Identifier: AGPL-3.0-or-later

// src/wiki/git.rs
//! Git-backed durability for the wiki (Slice G).
//!
//! Each wiki gets its own git repo at its root. We shell out to the
//! `git` CLI rather than using a library so that:
//!
//! - The repos are interoperable with whatever `git` the user already
//!   has (their `.gitconfig`, their auth helpers, their `gh` setup).
//! - There's no extra Rust dependency that can drift with `libgit2`.
//! - Users can `cd ~/.mira/data/wikis/users/<id>` and inspect / fix
//!   anything from the command line.
//!
//! Auto-commit: after every successful apply, the wiki layer can
//! optionally call [`commit_changes`] with a short message derived from
//! the op. Push to remote is on demand (manual button in the UI, or a
//! cron via the existing automations layer).
//!
//! Errors that come back as non-zero git exit codes are surfaced as
//! [`GitError::CommandFailed`] with stderr captured so the UI can show
//! the user what actually went wrong (no auth, conflict, no remote).

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};
use tracing::debug;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git binary not on PATH (install git or set wiki.git.enabled=false)")]
    GitNotInstalled,
    #[error("git {0} failed: {1}")]
    CommandFailed(String, String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type GitResult<T> = Result<T, GitError>;

/// One-shot snapshot of the wiki's git state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitStatus {
    pub initialized: bool,
    pub branch: Option<String>,
    pub head_short: Option<String>,
    pub remote_url: Option<String>,
    /// Counts of paths with each status code from `git status --porcelain`.
    pub modified: usize,
    pub untracked: usize,
    pub deleted: usize,
    /// Commits ahead/behind the upstream branch, when one is configured.
    pub ahead: Option<usize>,
    pub behind: Option<usize>,
}

impl GitStatus {
    pub fn empty_uninitialized() -> Self {
        Self {
            initialized: false,
            branch: None,
            head_short: None,
            remote_url: None,
            modified: 0, untracked: 0, deleted: 0,
            ahead: None, behind: None,
        }
    }
}

/// Initialise `<root>/.git` if it doesn't already exist. Idempotent.
/// Configures `user.email` and `user.name` from the supplied identity
/// when the repo lacks them locally — without this, the first
/// `git commit` would fail on machines without a global identity set.
pub fn ensure_repo(root: &Path, identity_email: &str, identity_name: &str) -> GitResult<bool> {
    if root.join(".git").exists() {
        return Ok(false);
    }
    run(root, &["init", "-q"])?;
    // Per-repo identity so commits land cleanly even without ~/.gitconfig.
    run(root, &["config", "user.email", identity_email])?;
    run(root, &["config", "user.name",  identity_name])?;
    // Default branch name. Newer git defaults to main; older defaults to master.
    // Force main so the upstream-tracking helpers can rely on the name.
    let _ = run(root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    write_default_gitignore(root)?;
    // Stage the gitignore + initial wiki tree and make the seed commit so
    // subsequent commits have somewhere to point.
    run(root, &["add", "-A"])?;
    let _ = run(root, &["commit", "-q", "-m", "wiki: initial scaffold"]);
    Ok(true)
}

fn write_default_gitignore(root: &Path) -> GitResult<()> {
    // We DON'T ignore the audit DB by default — it lives in the parent
    // (`<data_dir>/wiki_<user>.db`), not in the wiki root. But the
    // `.pending/` scratch dir should never make it into git.
    let body = "\
# MIRA wiki .gitignore
.pending/
*.tmp
";
    let path = root.join(".gitignore");
    if !path.exists() {
        std::fs::write(&path, body)?;
    }
    Ok(())
}

/// Stage everything under `root` and commit with `message`. Returns
/// `Ok(true)` if a commit was created, `Ok(false)` if there was nothing
/// to commit (clean working tree).
pub fn commit_changes(root: &Path, message: &str) -> GitResult<bool> {
    if !root.join(".git").exists() {
        return Err(GitError::CommandFailed("commit".into(), ".git does not exist".into()));
    }
    run(root, &["add", "-A"])?;
    // Check if there's anything to commit before calling commit, so a
    // clean tree doesn't surface as an error.
    let out = run(root, &["status", "--porcelain"])?;
    if out.trim().is_empty() {
        return Ok(false);
    }
    run(root, &["commit", "-q", "-m", message])?;
    Ok(true)
}

/// Set or replace the `origin` remote.
pub fn set_remote(root: &Path, url: &str) -> GitResult<()> {
    let existing = run(root, &["remote"])?;
    let has_origin = existing.lines().any(|l| l.trim() == "origin");
    if has_origin {
        run(root, &["remote", "set-url", "origin", url])?;
    } else {
        run(root, &["remote", "add", "origin", url])?;
    }
    Ok(())
}

/// Push to `origin`. On first push, sets the upstream so subsequent
/// pulls work without re-specifying the branch.
pub fn push(root: &Path) -> GitResult<String> {
    let branch = current_branch(root)?.unwrap_or_else(|| "main".to_string());
    run(root, &["push", "-u", "origin", &branch])
}

/// Pull `origin/<current-branch>` with merge. Conflicts are surfaced
/// as `GitError::CommandFailed`; the caller is expected to tell the
/// user "conflict on path X — resolve via shell".
pub fn pull(root: &Path) -> GitResult<String> {
    let branch = current_branch(root)?.unwrap_or_else(|| "main".to_string());
    run(root, &["pull", "--no-edit", "origin", &branch])
}

/// Status snapshot for the wiki UI.
pub fn status(root: &Path) -> GitResult<GitStatus> {
    if !root.join(".git").exists() {
        return Ok(GitStatus::empty_uninitialized());
    }
    let branch     = current_branch(root)?;
    let head_short = run(root, &["rev-parse", "--short", "HEAD"]).ok()
        .map(|s| s.trim().to_string());
    let remote_url = run(root, &["remote", "get-url", "origin"]).ok()
        .map(|s| s.trim().to_string());

    let porcelain = run(root, &["status", "--porcelain"])?;
    let mut modified = 0;
    let mut untracked = 0;
    let mut deleted = 0;
    for line in porcelain.lines() {
        if line.starts_with("??") { untracked += 1; }
        else if line.starts_with(" D") || line.starts_with("D ") { deleted += 1; }
        else if !line.is_empty() { modified += 1; }
    }

    let (ahead, behind) = ahead_behind(root, branch.as_deref()).unwrap_or((None, None));

    Ok(GitStatus {
        initialized: true,
        branch, head_short, remote_url,
        modified, untracked, deleted,
        ahead, behind,
    })
}

/// Resolve the current branch name. Returns `None` for a detached HEAD.
pub fn current_branch(root: &Path) -> GitResult<Option<String>> {
    match run(root, &["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        Ok(s) => {
            let name = s.trim().to_string();
            if name.is_empty() { Ok(None) } else { Ok(Some(name)) }
        }
        Err(_) => Ok(None),
    }
}

/// Counts of commits the local branch is ahead/behind its upstream, if
/// one is configured. `(None, None)` when there's no upstream.
fn ahead_behind(root: &Path, branch: Option<&str>) -> GitResult<(Option<usize>, Option<usize>)> {
    let Some(branch) = branch else { return Ok((None, None)); };
    let upstream = format!("origin/{branch}");
    // Probe whether the upstream ref exists. If not, return (None, None).
    if run(root, &["rev-parse", "--verify", "--quiet", &upstream]).is_err() {
        return Ok((None, None));
    }
    let counts = run(root, &["rev-list", "--left-right", "--count", &format!("{upstream}...HEAD")])?;
    // Output: "<behind>\t<ahead>"
    let mut parts = counts.split_whitespace();
    let behind = parts.next().and_then(|s| s.parse().ok());
    let ahead  = parts.next().and_then(|s| s.parse().ok());
    Ok((ahead, behind))
}

/// Build the auto-commit message for a wiki op. Short, scannable, and
/// derived from the op kind + target so `git log --oneline` is usable.
pub fn auto_commit_message(op_kind: &str, target_path: &str, actor: &str) -> String {
    // Format: "wiki: <kind> <target> (by <actor>)"
    let mut msg = format!("wiki: {op_kind} {target_path}");
    if !actor.is_empty() {
        msg.push_str(&format!(" (by {actor})"));
    }
    // Cap so git log stays scannable.
    if msg.chars().count() > 160 {
        let cut: String = msg.chars().take(160).collect();
        msg = format!("{cut}…");
    }
    msg
}

// ── Internals ────────────────────────────────────────────────────────────────

fn run(cwd: &Path, args: &[&str]) -> GitResult<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                GitError::GitNotInstalled
            } else {
                GitError::Io(e)
            }
        })?;

    if !output.status.success() {
        let cmd = args.join(" ");
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let combined = if stderr.is_empty() { stdout } else { stderr };
        debug!("git {} failed: {}", cmd, combined);
        return Err(GitError::CommandFailed(cmd, combined));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// True if `git` is reachable on PATH. Cheap — runs `git --version` once.
/// Used by the wiki gateway to skip auto-init when git isn't installed,
/// rather than tripping every commit with `GitNotInstalled`.
pub fn is_available() -> bool {
    Command::new("git").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// Build a deterministic identity string for a per-user wiki. Used to
/// stamp `user.email` / `user.name` on the repo so commits work even
/// without a global gitconfig.
pub fn default_identity(user_id: &str) -> (String, String) {
    (
        format!("{user_id}@wiki.mira.local"),
        format!("MIRA wiki: {user_id}"),
    )
}

pub fn system_identity() -> (String, String) {
    ("system@wiki.mira.local".into(), "MIRA wiki: system".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn skip_if_no_git() -> bool {
        if !is_available() {
            eprintln!("(skip) git not installed");
            return true;
        }
        false
    }

    #[test]
    fn auto_commit_message_format() {
        let m = auto_commit_message("write_page", "pages/pong.md", "alice");
        assert_eq!(m, "wiki: write_page pages/pong.md (by alice)");
    }

    #[test]
    fn auto_commit_message_caps_long_input() {
        let long_path: String = "a".repeat(300);
        let m = auto_commit_message("append_section", &long_path, "actor");
        assert!(m.chars().count() <= 161, "got len={}: {m}", m.chars().count());
        assert!(m.ends_with('…'));
    }

    #[test]
    fn ensure_repo_is_idempotent() {
        if skip_if_no_git() { return; }
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("hello.md"), "# Hi\n").unwrap();

        let first = ensure_repo(dir.path(), "test@mira.local", "Test User").unwrap();
        assert!(first);
        assert!(dir.path().join(".git").exists());
        assert!(dir.path().join(".gitignore").exists());

        let second = ensure_repo(dir.path(), "test@mira.local", "Test User").unwrap();
        assert!(!second);
    }

    #[test]
    fn commit_changes_returns_false_on_clean_tree() {
        if skip_if_no_git() { return; }
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "x\n").unwrap();
        ensure_repo(dir.path(), "t@x", "t").unwrap();
        // Tree is clean right after the seed commit.
        assert!(!commit_changes(dir.path(), "noop").unwrap());
    }

    #[test]
    fn commit_changes_creates_commit_when_files_change() {
        if skip_if_no_git() { return; }
        let dir = tempdir().unwrap();
        ensure_repo(dir.path(), "t@x", "t").unwrap();
        std::fs::write(dir.path().join("new.md"), "hi\n").unwrap();
        let made = commit_changes(dir.path(), "wiki: add new.md").unwrap();
        assert!(made);
        let log = run(dir.path(), &["log", "--oneline"]).unwrap();
        assert!(log.contains("wiki: add new.md"));
    }

    #[test]
    fn status_reflects_uninitialized() {
        let dir = tempdir().unwrap();
        let s = status(dir.path()).unwrap();
        assert!(!s.initialized);
    }

    #[test]
    fn status_counts_dirty_paths() {
        if skip_if_no_git() { return; }
        let dir = tempdir().unwrap();
        ensure_repo(dir.path(), "t@x", "t").unwrap();
        std::fs::write(dir.path().join("u.md"), "u\n").unwrap();
        let s = status(dir.path()).unwrap();
        assert!(s.initialized);
        assert_eq!(s.untracked, 1);
        assert_eq!(s.modified + s.deleted, 0);
    }

    #[test]
    fn set_remote_creates_and_updates_origin() {
        if skip_if_no_git() { return; }
        let dir = tempdir().unwrap();
        ensure_repo(dir.path(), "t@x", "t").unwrap();
        set_remote(dir.path(), "https://example.com/a.git").unwrap();
        let url = run(dir.path(), &["remote", "get-url", "origin"]).unwrap();
        assert!(url.trim().ends_with("a.git"));
        set_remote(dir.path(), "https://example.com/b.git").unwrap();
        let url = run(dir.path(), &["remote", "get-url", "origin"]).unwrap();
        assert!(url.trim().ends_with("b.git"));
    }
}
