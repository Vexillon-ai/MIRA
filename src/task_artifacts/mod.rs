// SPDX-License-Identifier: AGPL-3.0-or-later

// src/task_artifacts/mod.rs
//! 0.111.0 — organised storage for subagent task deliverables.
//!
//! Each `spawn_background_task` invocation gets a dedicated directory
//! under the configured artifacts root. The subagent is spawned with
//! cwd = that directory and `MIRA_TASK_OUTPUT_DIR` env var set, plus
//! a brief addendum telling it to write a SLUG file as its first
//! action. On terminal completion, our handler reads SLUG and renames
//! the dir to `<slug>_<task_id_short>`.
//!
//! ## Layout
//!
//! ```text
//! ~/mira-artifacts/                 (configurable via artifacts.root_dir)
//!   claudecode/
//!     pong-game-modern_019e0e55/    (renamed after agent picks slug)
//!       MANIFEST.json               (status, brief, started/finished, …)
//!       SLUG                        (single line, kebab-case slug)
//!       output/                     (deliverables go here by convention)
//!       logs/                       (optional debug captures)
//!   migrated/                       (one-shot sweep of pre-0.111 dirs)
//! ```
//!
//! Distinct from `src/artifacts/` which is the content-addressed
//! image-blob store backing chat inline rendering. Different domain;
//! kept in a separate module to avoid the namespace overlap.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::MiraError;

/// Subdirectory under the artifacts root for each skill family. Keep
/// short — these become path segments. Add a row when a new skill
/// kind starts producing artifacts.
pub fn skill_short_name(skill_id: &str) -> &str {
    match skill_id {
        "com.mira.claudecode" => "claudecode",
        "com.mira.opencode"   => "opencode",
        "com.mira.research"   => "research",
        _ => "other",
    }
}

/// Persisted per-task MANIFEST.json. Written at allocation time,
/// updated on completion. Schema is deliberately small — anything
/// the user might want without opening the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub task_id:       String,
    pub skill_id:      String,
    pub user_id:       Option<String>,
    pub channel:       Option<String>,
    /// What the user asked for — first ~500 chars of the brief.
    pub brief_excerpt: String,
    pub created_at:    i64,
    /// Set to non-zero on terminal completion.
    pub finished_at:   Option<i64>,
    /// "running" | "completed" | "failed" | "abandoned".
    pub status:        String,
    /// Pulled from the SLUG file the agent writes. None until set.
    pub slug:          Option<String>,
}

impl Manifest {
    pub fn new(task_id: &str, skill_id: &str, user_id: Option<&str>,
               channel: Option<&str>, brief: &str) -> Self {
        let brief_excerpt: String = brief.chars().take(500).collect();
        Self {
            task_id: task_id.to_string(),
            skill_id: skill_id.to_string(),
            user_id: user_id.map(|s| s.to_string()),
            channel: channel.map(|s| s.to_string()),
            brief_excerpt,
            created_at: chrono::Utc::now().timestamp(),
            finished_at: None,
            status: "running".to_string(),
            slug: None,
        }
    }
}

/// Manages the artifacts root and per-task subdirs. Cheap to clone
/// (just an Arc<PathBuf>); pass through Extension or stash on stores
/// as needed.
#[derive(Clone)]
pub struct TaskArtifactsStore {
    root: std::sync::Arc<PathBuf>,
}

impl TaskArtifactsStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root: std::sync::Arc::new(root) }
    }

    pub fn root(&self) -> &Path { self.root.as_path() }

    /// Allocate a per-task directory + write the initial MANIFEST.
    /// The dir starts named just by task_id; rename happens on
    /// completion once the agent has chosen a slug.
    pub fn allocate(
        &self, skill_id: &str, task_id: &str, user_id: Option<&str>,
        channel: Option<&str>, brief: &str,
    ) -> Result<PathBuf, MiraError> {
        let skill = skill_short_name(skill_id);
        let dir = self.root.join(skill).join(task_id);
        std::fs::create_dir_all(dir.join("output"))
            .map_err(|e| MiraError::ConfigError(format!("create artifacts dir {}: {e}", dir.display())))?;
        std::fs::create_dir_all(dir.join("logs"))
            .map_err(|e| MiraError::ConfigError(format!("create logs dir: {e}")))?;
        let manifest = Manifest::new(task_id, skill_id, user_id, channel, brief);
        write_manifest(&dir, &manifest)?;
        debug!("artifacts.allocate: {} → {}", task_id, dir.display());
        Ok(dir)
    }

    /// Look up the directory for a task_id by scanning skill subdirs.
    /// Returns None if not found. Used by the completion handler to
    /// locate the dir for slug-rename.
    pub fn find_dir_by_task_id(&self, task_id: &str) -> Option<PathBuf> {
        let entries = std::fs::read_dir(self.root.as_path()).ok()?;
        for entry in entries.flatten() {
            let skill_dir = entry.path();
            if !skill_dir.is_dir() { continue }
            // Match either bare task_id (pre-rename) or *_<task_id_short>.
            let prefix = task_id_short(task_id);
            if let Ok(inner) = std::fs::read_dir(&skill_dir) {
                for e in inner.flatten() {
                    let name = e.file_name();
                    let s = name.to_str().unwrap_or("");
                    if s == task_id || s.ends_with(&format!("_{prefix}")) {
                        return Some(e.path());
                    }
                }
            }
        }
        None
    }

    /// On terminal completion: read SLUG (or MANIFEST.json's `slug`
    /// field if the agent updated it directly), rename the dir to
    /// `<slug>_<task_id_short>`, and update MANIFEST status.
    pub fn finalize(
        &self, task_id: &str, status: &str,
    ) -> Result<Option<PathBuf>, MiraError> {
        let Some(dir) = self.find_dir_by_task_id(task_id) else {
            return Ok(None);
        };
        // Update MANIFEST first so even a failed rename leaves the
        // metadata correct.
        let mut manifest = read_manifest(&dir).unwrap_or_else(|_| Manifest {
            task_id: task_id.to_string(), skill_id: String::new(),
            user_id: None, channel: None, brief_excerpt: String::new(),
            created_at: chrono::Utc::now().timestamp(),
            finished_at: None, status: status.to_string(), slug: None,
        });
        manifest.status = status.to_string();
        manifest.finished_at = Some(chrono::Utc::now().timestamp());
        // Try SLUG file first; fall back to whatever's already in the
        // manifest.
        let slug = std::fs::read_to_string(dir.join("SLUG"))
            .ok()
            .map(|s| sanitize_slug(s.lines().next().unwrap_or("")))
            .filter(|s| !s.is_empty())
            .or(manifest.slug.clone());
        manifest.slug = slug.clone();
        let _ = write_manifest(&dir, &manifest);

        // Rename if we have a slug AND the dir is still bare-task-id.
        let final_dir = if let Some(slug) = slug {
            let cur_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if cur_name == task_id {
                let parent = dir.parent().unwrap_or(self.root.as_path());
                let new_name = format!("{slug}_{}", task_id_short(task_id));
                let target = parent.join(&new_name);
                if let Err(e) = std::fs::rename(&dir, &target) {
                    warn!("artifacts.finalize: rename {} → {} failed: {e}", dir.display(), target.display());
                    dir
                } else {
                    debug!("artifacts.finalize: renamed → {}", target.display());
                    target
                }
            } else {
                dir
            }
        } else {
            dir
        };

        // A coding build that named its single page something other than
        // `index.html` (e.g. `game.html`, `snake.html`) would be invisible to
        // the web-app server — which only serves `output/index.html` — so
        // `list_web_apps`/`get_task_result` would surface no link and the
        // `/a/<id>/` route would 404. If the build completed and left exactly
        // one top-level `*.html` in `output/` with no `index.html`, promote it
        // so the one deliverable is reachable. See [`promote_sole_html`].
        if status == "completed" {
            promote_sole_html(&final_dir.join("output"));
        }

        Ok(Some(final_dir))
    }

    /// Browse: every task across every skill, newest first by created_at
    /// from MANIFEST. Skips directories without a manifest.
    pub fn list(&self) -> Vec<TaskListEntry> {
        let mut out: Vec<TaskListEntry> = Vec::new();
        let Ok(skills) = std::fs::read_dir(self.root.as_path()) else { return out; };
        for skill_entry in skills.flatten() {
            let skill_dir = skill_entry.path();
            if !skill_dir.is_dir() { continue }
            let skill_name = skill_entry.file_name().to_string_lossy().to_string();
            let Ok(tasks) = std::fs::read_dir(&skill_dir) else { continue };
            for task_entry in tasks.flatten() {
                let task_dir = task_entry.path();
                if !task_dir.is_dir() { continue }
                if let Ok(m) = read_manifest(&task_dir) {
                    let size_bytes = dir_size_bytes(&task_dir);
                    out.push(TaskListEntry {
                        path: task_dir.clone(),
                        skill: skill_name.clone(),
                        manifest: m,
                        size_bytes,
                    });
                }
            }
        }
        out.sort_by(|a, b| b.manifest.created_at.cmp(&a.manifest.created_at));
        out
    }

    /// List every file under a task's dir, recursively, as forward-slashed
    /// relative paths + size (Phase A4 — the artifact browser). `None` if the
    /// task dir isn't found.
    pub fn list_files(&self, task_id: &str) -> Option<Vec<FileEntry>> {
        let dir = self.find_dir_by_task_id(task_id)?;
        fn walk(base: &Path, cur: &Path, out: &mut Vec<FileEntry>) {
            let Ok(rd) = std::fs::read_dir(cur) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(base, &p, out);
                } else if let Ok(meta) = e.metadata() {
                    if let Ok(rel) = p.strip_prefix(base) {
                        out.push(FileEntry {
                            path: rel.to_string_lossy().replace('\\', "/"),
                            size_bytes: meta.len(),
                        });
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(&dir, &dir, &mut out);
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Some(out)
    }

    /// Resolve one file inside a task's dir with path-traversal safety: returns
    /// the absolute path only if it exists, is a regular file, and genuinely
    /// sits under the task dir (defeats `../` escapes). Phase A4.
    pub fn resolve_file(&self, task_id: &str, rel: &str) -> Option<PathBuf> {
        let dir = self.find_dir_by_task_id(task_id)?;
        let canon_base = dir.canonicalize().ok()?;
        let canon = dir.join(rel).canonicalize().ok()?;
        if canon.starts_with(&canon_base) && canon.is_file() {
            Some(canon)
        } else {
            None
        }
    }

    /// Wipe a task dir. Caller has already verified path is under root.
    pub fn delete(&self, path: &Path) -> Result<(), MiraError> {
        // Defensive: make sure the path is genuinely under our root
        // before any rm -rf-shaped operation.
        let canon_root = self.root.canonicalize().unwrap_or_else(|_| self.root.as_path().to_path_buf());
        let canon_path = path.canonicalize().map_err(|e|
            MiraError::ConfigError(format!("canonicalize {}: {e}", path.display())))?;
        if !canon_path.starts_with(&canon_root) {
            return Err(MiraError::ConfigError(
                format!("refusing to delete path outside artifacts root: {}", path.display())));
        }
        std::fs::remove_dir_all(&canon_path)
            .map_err(|e| MiraError::ConfigError(format!("rm {}: {e}", canon_path.display())))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskListEntry {
    pub path:       PathBuf,
    pub skill:      String,
    pub manifest:   Manifest,
    pub size_bytes: u64,
}

/// One file inside a task's artifact dir (Phase A4 — the browser/preview).
#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    /// Forward-slashed path relative to the task dir, e.g. `output/report.md`.
    pub path:       String,
    pub size_bytes: u64,
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Make a single-page build reachable by the web-app server, which serves only
/// `output/index.html`. If `output_dir` has no `index.html` but contains
/// **exactly one** top-level `*.html` file, rename that file to `index.html`.
///
/// Deliberately conservative — a no-op when:
///   * an `index.html` already exists (nothing to fix),
///   * there are zero `*.html` files (not a web app), or
///   * there are two or more (ambiguous — which is the entry point? don't
///     guess and risk hiding the real one).
/// Only top-level files are considered; a page nested under a subdir
/// (`output/dist/app.html`) is left alone. Any IO error is logged and ignored —
/// promotion is best-effort and must never fail finalize.
fn promote_sole_html(output_dir: &Path) {
    let index = output_dir.join("index.html");
    if index.exists() {
        return;
    }
    let Ok(rd) = std::fs::read_dir(output_dir) else { return };
    let mut htmls: Vec<PathBuf> = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        let is_html = p.is_file()
            && p.extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case("html"));
        if is_html {
            htmls.push(p);
        }
    }
    if htmls.len() == 1 {
        let sole = &htmls[0];
        match std::fs::rename(sole, &index) {
            Ok(())  => debug!("artifacts.finalize: promoted {} → index.html", sole.display()),
            Err(e)  => warn!(
                "artifacts.finalize: promote {} → index.html failed: {e}", sole.display()
            ),
        }
    }
}

pub fn write_manifest(dir: &Path, m: &Manifest) -> Result<(), MiraError> {
    let json = serde_json::to_vec_pretty(m)
        .map_err(|e| MiraError::ConfigError(format!("serialise manifest: {e}")))?;
    std::fs::write(dir.join("MANIFEST.json"), json)
        .map_err(|e| MiraError::ConfigError(format!("write manifest: {e}")))?;
    Ok(())
}

pub fn read_manifest(dir: &Path) -> Result<Manifest, MiraError> {
    let bytes = std::fs::read(dir.join("MANIFEST.json"))
        .map_err(|e| MiraError::ConfigError(format!("read manifest: {e}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| MiraError::ConfigError(format!("parse manifest: {e}")))
}

/// First 8 chars of a UUIDv7 task_id — distinct enough for dir naming.
pub fn task_id_short(task_id: &str) -> String {
    task_id.chars().take(8).collect()
}

/// Normalise an agent-supplied slug: lowercase, kebab-case, alphanumeric
/// + hyphens only, max 60 chars. Ensures the rename target is safe.
pub fn sanitize_slug(raw: &str) -> String {
    let lowered = raw.trim().to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut last_was_dash = true;  // suppress leading dashes
    for c in lowered.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    while out.ends_with('-') { out.pop(); }
    out.chars().take(60).collect()
}

fn dir_size_bytes(dir: &Path) -> u64 {
    fn walk(p: &Path, acc: &mut u64) {
        if let Ok(entries) = std::fs::read_dir(p) {
            for e in entries.flatten() {
                let path = e.path();
                if let Ok(meta) = e.metadata() {
                    if meta.is_dir() { walk(&path, acc); }
                    else            { *acc += meta.len(); }
                }
            }
        }
    }
    let mut total = 0u64;
    walk(dir, &mut total);
    total
}

// ── Migration ──────────────────────────────────────────────────────────────

/// Heuristic-driven sweep. Looks at $HOME for top-level dirs that
/// match patterns we know agents have created in the past, and
/// shuffles them under `<root>/migrated/<orig_name>/`. Best effort —
/// missed candidates just stay where they are. Returns the list of
/// moves performed.
pub fn migrate_existing(
    home: &Path, root: &Path,
) -> Result<Vec<(PathBuf, PathBuf)>, MiraError> {
    let migrated_root = root.join("migrated");
    std::fs::create_dir_all(&migrated_root)
        .map_err(|e| MiraError::ConfigError(format!("create migrated/: {e}")))?;
    let entries = std::fs::read_dir(home)
        .map_err(|e| MiraError::ConfigError(format!("read $HOME: {e}")))?;
    let mut moves = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() { continue; }
        let name = entry.file_name().to_string_lossy().to_string();
        if !looks_like_artifact_dir(&name) { continue; }
        let target = migrated_root.join(&name);
        if target.exists() {
            // Don't clobber. Append a timestamp.
            let ts = chrono::Utc::now().timestamp();
            let target = migrated_root.join(format!("{name}_{ts}"));
            if std::fs::rename(&path, &target).is_ok() {
                moves.push((path, target));
            }
        } else if std::fs::rename(&path, &target).is_ok() {
            moves.push((path, target));
        }
    }
    Ok(moves)
}

/// Regex-like patterns for "this looks like something an agent built".
/// Conservative — false negatives just leave dirs alone, false
/// positives could move user data unintentionally.
fn looks_like_artifact_dir(name: &str) -> bool {
    let lower = name.to_lowercase();
    // Known prefixes from past sessions.
    let prefixes = ["neon-pong", "pong-", "claude-task-", "opencode-task-"];
    if prefixes.iter().any(|p| lower.starts_with(p)) { return true; }
    // Known whole-name matches (the historical neon-pong-2024 etc).
    let exacts = ["neon-pong-2024"];
    exacts.contains(&lower.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_sanitises_ugly_input() {
        assert_eq!(sanitize_slug("Pong Game Modern!"), "pong-game-modern");
        assert_eq!(sanitize_slug("  --auth bug?? "), "auth-bug");
        assert_eq!(sanitize_slug(&"x".repeat(200)).len(), 60);
    }

    #[test]
    fn list_files_and_resolve_with_traversal_safety() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskArtifactsStore::new(dir.path().to_path_buf());
        let p = store.allocate("com.mira.claudecode", "task-xyz", None, None, "x").unwrap();
        std::fs::write(p.join("output/report.md"), b"# hello").unwrap();
        std::fs::write(p.join("logs/stdout.log"), b"log line").unwrap();

        let files = store.list_files("task-xyz").expect("task found");
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"output/report.md"));
        assert!(paths.contains(&"logs/stdout.log"));

        // Resolve a real file.
        assert!(store.resolve_file("task-xyz", "output/report.md").is_some());
        // Path-traversal escape is refused.
        assert!(store.resolve_file("task-xyz", "../../../etc/passwd").is_none());
        // A directory is not a servable file.
        assert!(store.resolve_file("task-xyz", "output").is_none());
        // Unknown task → None.
        assert!(store.list_files("nope").is_none());
    }

    #[test]
    fn allocate_creates_skeleton() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskArtifactsStore::new(dir.path().to_path_buf());
        let p = store.allocate(
            "com.mira.claudecode", "task-abc",
            Some("u1"), Some("web"), "build something",
        ).unwrap();
        assert!(p.join("MANIFEST.json").exists());
        assert!(p.join("output").exists());
        assert!(p.join("logs").exists());
        let m = read_manifest(&p).unwrap();
        assert_eq!(m.task_id, "task-abc");
        assert_eq!(m.status, "running");
    }

    #[test]
    fn finalize_renames_when_slug_present() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskArtifactsStore::new(dir.path().to_path_buf());
        let p = store.allocate(
            "com.mira.claudecode", "0123456789abcdef",
            None, None, "test",
        ).unwrap();
        std::fs::write(p.join("SLUG"), "Pong Game Modern\n").unwrap();
        let final_dir = store.finalize("0123456789abcdef", "completed").unwrap().unwrap();
        let name = final_dir.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(name, "pong-game-modern_01234567");
        assert!(read_manifest(&final_dir).unwrap().status == "completed");
    }

    #[test]
    fn finalize_promotes_sole_html_to_index() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskArtifactsStore::new(dir.path().to_path_buf());
        let p = store.allocate("com.mira.claudecode", "abcdef0123456789", None, None, "snake").unwrap();
        // The build wrote its one page as `snake.html`, not `index.html`.
        std::fs::write(p.join("output/snake.html"), b"<canvas></canvas>").unwrap();

        let final_dir = store.finalize("abcdef0123456789", "completed").unwrap().unwrap();
        // Promoted: served path now exists, the odd name is gone.
        assert!(final_dir.join("output/index.html").exists(), "sole html promoted to index.html");
        assert!(!final_dir.join("output/snake.html").exists(), "original renamed, not copied");
        assert_eq!(
            std::fs::read(final_dir.join("output/index.html")).unwrap(),
            b"<canvas></canvas>",
            "content preserved across the rename",
        );
    }

    #[test]
    fn finalize_leaves_existing_index_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskArtifactsStore::new(dir.path().to_path_buf());
        let p = store.allocate("com.mira.claudecode", "1111222233334444", None, None, "app").unwrap();
        std::fs::write(p.join("output/index.html"), b"REAL").unwrap();
        std::fs::write(p.join("output/other.html"), b"OTHER").unwrap();

        let final_dir = store.finalize("1111222233334444", "completed").unwrap().unwrap();
        // index.html present → no promotion, no clobber.
        assert_eq!(std::fs::read(final_dir.join("output/index.html")).unwrap(), b"REAL");
        assert!(final_dir.join("output/other.html").exists());
    }

    #[test]
    fn finalize_does_not_guess_among_multiple_html() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskArtifactsStore::new(dir.path().to_path_buf());
        let p = store.allocate("com.mira.claudecode", "aaaabbbbccccdddd", None, None, "multi").unwrap();
        std::fs::write(p.join("output/a.html"), b"A").unwrap();
        std::fs::write(p.join("output/b.html"), b"B").unwrap();

        let final_dir = store.finalize("aaaabbbbccccdddd", "completed").unwrap().unwrap();
        // Ambiguous → don't invent an index.html; leave both in place.
        assert!(!final_dir.join("output/index.html").exists());
        assert!(final_dir.join("output/a.html").exists());
        assert!(final_dir.join("output/b.html").exists());
    }

    #[test]
    fn migrate_moves_neon_pong() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("neon-pong-2024/css")).unwrap();
        std::fs::create_dir_all(home.path().join("Documents")).unwrap();
        let moves = migrate_existing(home.path(), root.path()).unwrap();
        assert_eq!(moves.len(), 1);
        assert!(root.path().join("migrated/neon-pong-2024/css").exists());
        assert!(home.path().join("Documents").exists()); // untouched
    }
}
