// SPDX-License-Identifier: AGPL-3.0-or-later

// src/wiki/store.rs
//! Filesystem façade: list pages, read the navigation files, traverse
//! the wiki tree.
//!
//! `WikiStore` is read-oriented — all writes go through [`WikiApplier`]
//! and the audit log so they're observable. The store is what every
//! reader (UI, MCP server, context-injection hook) holds.

use std::path::{Path, PathBuf};

use crate::wiki::page::WikiPage;
use crate::wiki::paths::WikiPath;
use crate::wiki::{Result, WikiError};

/// Read-only access to the wiki tree rooted at `root`. Cheap to clone.
#[derive(Debug, Clone)]
pub struct WikiStore {
    root: PathBuf,
}

impl WikiStore {
    pub fn new(root: PathBuf) -> Self { Self { root } }

    pub fn root(&self) -> &Path { &self.root }

    /// Directory that holds embedded page images (`<root>/assets/`). Kept
    /// inside the wiki tree so git-sync / tarball export carry the images
    /// alongside the markdown that references them.
    pub fn assets_dir(&self) -> PathBuf { self.root.join("assets") }

    /// Save an image as a content-addressed asset and return its wiki-relative
    /// path (`assets/<sha256>.<ext>`). Content addressing dedupes identical
    /// uploads and keeps filenames safe (no user-controlled path component).
    /// `ext` is validated by the caller against an allow-list.
    pub fn save_asset(&self, bytes: &[u8], ext: &str) -> Result<String> {
        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(bytes);
        let name = format!("{:x}.{}", hash, ext);
        let dir = self.assets_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| WikiError::Other(format!("create assets dir: {e}")))?;
        let full = dir.join(&name);
        if !full.exists() {
            std::fs::write(&full, bytes)
                .map_err(|e| WikiError::Other(format!("write asset: {e}")))?;
        }
        Ok(format!("assets/{name}"))
    }

    /// Read an asset by its `assets/<name>` relative path. Rejects anything
    /// that isn't a plain filename directly under `assets/` (no traversal, no
    /// nested dirs) so a crafted path can't escape the wiki root.
    pub fn read_asset(&self, rel: &str) -> Result<Vec<u8>> {
        let name = rel.strip_prefix("assets/").unwrap_or(rel);
        // A single safe path segment: no separators, no `.` / `..`.
        let safe = !name.is_empty()
            && name != "."
            && name != ".."
            && !name.contains('/')
            && !name.contains('\\')
            && !name.contains("..");
        if !safe {
            return Err(WikiError::Other(format!("unsafe asset path: {rel}")));
        }
        let full = self.assets_dir().join(name);
        std::fs::read(&full).map_err(|e| WikiError::Other(format!("read asset: {e}")))
    }

    /// Read a single page.
    pub fn read_page(&self, path: &WikiPath) -> Result<WikiPage> {
        WikiPage::read(&self.root, path)
    }

    /// Try-read variant: `Ok(None)` if missing.
    pub fn try_read_page(&self, path: &WikiPath) -> Result<Option<WikiPage>> {
        WikiPage::try_read(&self.root, path)
    }

    /// Read the navigation file `index.md` as raw text. The wiki UI
    /// parses it as markdown; the context-injection hook treats it as
    /// an opaque blob to inject.
    pub fn read_index_raw(&self) -> Result<String> {
        read_text_or_default(&self.root.join("index.md"), "")
    }

    /// Read `SCHEMA.md` as raw text.
    pub fn read_schema_raw(&self) -> Result<String> {
        read_text_or_default(&self.root.join("SCHEMA.md"), "")
    }

    /// Read `log.md` as raw text.
    pub fn read_log_raw(&self) -> Result<String> {
        read_text_or_default(&self.root.join("log.md"), "")
    }

    /// Read `profile.md` (per-user wiki) or `persona.md` (system wiki),
    /// whichever exists. Returns an empty string if neither does.
    pub fn read_core_raw(&self) -> Result<String> {
        let profile = self.root.join("profile.md");
        if profile.exists() { return read_text_or_default(&profile, ""); }
        let persona = self.root.join("persona.md");
        if persona.exists() { return read_text_or_default(&persona, ""); }
        Ok(String::new())
    }

    /// Enumerate every `.md` page under the wiki root (excluding
    /// internal dirs `.pending` and `.git`). Returns `WikiPath`s
    /// relative to the root.
    pub fn list_pages(&self) -> Result<Vec<WikiPath>> {
        let mut out = Vec::new();
        walk_md(&self.root, &self.root, &mut out)?;
        out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(out)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn read_text_or_default(path: &Path, default: &str) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(default.to_string()),
        Err(e) => Err(WikiError::Io(e)),
    }
}

fn walk_md(root: &Path, current: &Path, out: &mut Vec<WikiPath>) -> Result<()> {
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_s = name.to_string_lossy();

        // Skip internal dirs.
        if path.is_dir() {
            if name_s.starts_with('.') { continue; }
            walk_md(root, &path, out)?;
            continue;
        }

        // Files: only `.md`, skip temp files and dotfiles.
        if name_s.starts_with('.') { continue; }
        if name_s.ends_with(".tmp") { continue; }
        if !name_s.ends_with(".md") { continue; }

        let rel = path
            .strip_prefix(root)
            .map_err(|e| WikiError::Other(format!("strip_prefix: {e}")))?;
        // Use forward slashes regardless of platform.
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        out.push(WikiPath::from_trusted(rel_str));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki::page::{write_atomic, write_raw};
    use tempfile::tempdir;

    fn fresh() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        write_raw(&dir.path().join("profile.md"), "---\nwriter: user\n---\n\n# Profile\n").unwrap();
        write_raw(&dir.path().join("index.md"), "# Index\n- profile.md\n").unwrap();
        write_raw(&dir.path().join("SCHEMA.md"), "# Schema\n").unwrap();
        write_raw(&dir.path().join("log.md"), "# Log\n").unwrap();
        std::fs::create_dir_all(dir.path().join("pages/projects")).unwrap();
        write_raw(&dir.path().join("pages/projects/foo.md"), "# Foo\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".pending")).unwrap();
        write_atomic(&dir.path().join(".pending/x.json"), b"{}").unwrap();
        dir
    }

    #[test]
    fn list_pages_excludes_internal_dirs() {
        let dir = fresh();
        let store = WikiStore::new(dir.path().to_path_buf());
        let pages = store.list_pages().unwrap();
        let names: Vec<&str> = pages.iter().map(|p| p.as_str()).collect();
        assert!(names.contains(&"profile.md"));
        assert!(names.contains(&"index.md"));
        assert!(names.contains(&"SCHEMA.md"));
        assert!(names.contains(&"log.md"));
        assert!(names.contains(&"pages/projects/foo.md"));
        // .pending content excluded.
        assert!(!names.iter().any(|n| n.starts_with(".pending")));
    }

    #[test]
    fn save_and_read_asset_round_trips_and_rejects_traversal() {
        let dir = tempdir().unwrap();
        let store = WikiStore::new(dir.path().to_path_buf());
        let png = b"\x89PNG\r\n\x1a\n fake image bytes";
        let rel = store.save_asset(png, "png").unwrap();
        assert!(rel.starts_with("assets/") && rel.ends_with(".png"));
        // Content-addressed: same bytes → same path (idempotent).
        assert_eq!(store.save_asset(png, "png").unwrap(), rel);
        // Round-trips by its returned relative path.
        assert_eq!(store.read_asset(&rel).unwrap(), png);
        // Assets are NOT enumerated as pages (only .md is).
        assert!(!store.list_pages().unwrap().iter().any(|p| p.as_str().starts_with("assets/")));
        // Traversal / nested paths are refused.
        assert!(store.read_asset("assets/../profile.md").is_err());
        assert!(store.read_asset("assets/sub/dir.png").is_err());
        assert!(store.read_asset("../secret").is_err());
    }

    #[test]
    fn read_core_returns_profile_or_persona() {
        let dir = fresh();
        let store = WikiStore::new(dir.path().to_path_buf());
        assert!(store.read_core_raw().unwrap().contains("# Profile"));
    }

    #[test]
    fn read_index_returns_text() {
        let dir = fresh();
        let store = WikiStore::new(dir.path().to_path_buf());
        assert!(store.read_index_raw().unwrap().contains("# Index"));
    }
}
