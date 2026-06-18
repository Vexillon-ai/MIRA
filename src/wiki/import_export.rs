// SPDX-License-Identifier: AGPL-3.0-or-later

// src/wiki/import_export.rs
//! Tarball-based import / export for the wiki (Slice G).
//!
//! Export = gzipped tar of the wiki root, excluding `.git`, `.pending`,
//! and `*.tmp` scratch files. Import = inverse: extract a `.tar.gz`
//! into a target wiki root, with a containment check so a malicious
//! tarball can't write outside the root.
//!
//! Why tarball + not zip: matches the rest of MIRA's release pipeline
//! and skill-packaging format, and is friendlier to round-tripping
//! through `git` (which is what users will most often pair this with).

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

#[derive(Debug, thiserror::Error)]
pub enum IoError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("path traversal in archive: {0}")]
    PathTraversal(String),
    #[error("archive entry too large ({size} bytes; cap {cap})")]
    TooLarge { size: u64, cap: u64 },
}

pub type IoResult<T> = Result<T, IoError>;

/// Files/dirs we never include in an export. `.git` is the user's local
/// repo (they can re-init or sync separately); `.pending` is scratch.
/// Any file or directory starting with `.` is also excluded — the
/// importer rejects dotfile paths for safety, and the wiki layer
/// recreates necessary dotfiles (`.gitignore`, `.pending/`) on first
/// open of the imported root.
const EXCLUDED_DIRS: &[&str] = &[".git", ".pending"];

/// Hard cap on a single archive entry's size. Stops a malicious archive
/// from filling the disk via a single fake 8 GiB file. 64 MiB per page
/// is wildly more than anyone's wiki page should ever be.
const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;

/// Build a gzipped tar of `wiki_root` and write it to `out`. Returns the
/// number of files included.
pub fn export_tar_gz<W: Write>(wiki_root: &Path, out: W) -> IoResult<usize> {
    let enc = GzEncoder::new(out, Compression::default());
    let mut builder = tar::Builder::new(enc);
    let mut count = 0;

    walk_for_export(wiki_root, wiki_root, &mut builder, &mut count)?;
    builder.finish()?;
    Ok(count)
}

fn walk_for_export<W: Write>(
    root: &Path,
    cur: &Path,
    builder: &mut tar::Builder<GzEncoder<W>>,
    count: &mut usize,
) -> IoResult<()> {
    for entry in std::fs::read_dir(cur)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_s = name.to_string_lossy();

        if path.is_dir() {
            if EXCLUDED_DIRS.iter().any(|d| name_s == *d) { continue; }
            // Skip any dotdir (`.git` is already in EXCLUDED_DIRS, but
            // belt-and-braces in case a third tool dropped one).
            if name_s.starts_with('.') { continue; }
            walk_for_export(root, &path, builder, count)?;
            continue;
        }

        if name_s.ends_with(".tmp") { continue; }
        // Skip dotfiles at any depth — they're either internal scratch
        // (`.gitignore`, `.DS_Store`) or trip the importer's safety
        // filter. The wiki layer recreates the ones it needs after
        // import.
        if name_s.starts_with('.') { continue; }
        let rel = path.strip_prefix(root).unwrap();
        builder.append_path_with_name(&path, rel)?;
        *count += 1;
    }
    Ok(())
}

/// Extract a `.tar.gz` archive (read from `src`) into `wiki_root`.
///
/// Refuses any entry whose path is absolute or contains `..` so a
/// malicious archive can't escape the target dir. Returns the number
/// of entries extracted. The caller is responsible for the wiki being
/// in a sensible state after — typical flow is to call `enable_git`
/// after import so the change shows up as one fat commit.
pub fn import_tar_gz<R: Read>(wiki_root: &Path, src: R) -> IoResult<usize> {
    std::fs::create_dir_all(wiki_root)?;
    let dec = GzDecoder::new(src);
    let mut ar = tar::Archive::new(dec);
    let mut count = 0;

    for entry in ar.entries()? {
        let mut entry = entry?;
        let header = entry.header().clone();
        let size = header.size().unwrap_or(0);
        if size > MAX_ENTRY_BYTES {
            return Err(IoError::TooLarge { size, cap: MAX_ENTRY_BYTES });
        }
        let path = entry.path()?.into_owned();
        let safe_rel = sanitize_archive_path(&path)
            .ok_or_else(|| IoError::PathTraversal(path.display().to_string()))?;
        let target = wiki_root.join(&safe_rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        entry.unpack(&target)?;
        count += 1;
    }
    Ok(count)
}

/// Reject any path component that's absolute, `..`, or starts with a
/// dot-dir we don't allow.
fn sanitize_archive_path(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => {
                let s = p.to_string_lossy();
                if s.starts_with('.') {
                    // Permit dotfiles like `.gitignore` only inside an explicit
                    // allowlist. We don't currently need any; reject all.
                    return None;
                }
                out.push(p);
            }
            // No absolute paths, no parent traversal, no current-dir crumbs.
            _ => return None,
        }
    }
    if out.as_os_str().is_empty() { return None; }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(path: &Path, body: &str) {
        if let Some(p) = path.parent() { std::fs::create_dir_all(p).unwrap(); }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn export_then_import_round_trips() {
        let src = tempdir().unwrap();
        write(&src.path().join("profile.md"), "# Profile\n");
        write(&src.path().join("pages/projects/pong.md"), "# Pong\n");
        // Excluded content that should NOT appear in the archive.
        write(&src.path().join(".pending/scratch.json"), "{}");
        write(&src.path().join("pages/note.md.tmp"), "garbage");
        write(&src.path().join(".git/HEAD"), "ref: refs/heads/main\n");

        let mut buf = Vec::new();
        let n = export_tar_gz(src.path(), &mut buf).unwrap();
        // profile.md + pong.md = 2 files; everything else excluded.
        assert_eq!(n, 2, "expected 2 exported entries, got {n}");

        let dst = tempdir().unwrap();
        let extracted = import_tar_gz(dst.path(), &buf[..]).unwrap();
        assert_eq!(extracted, 2);

        assert!(dst.path().join("profile.md").exists());
        assert!(dst.path().join("pages/projects/pong.md").exists());
        assert!(!dst.path().join(".pending/scratch.json").exists());
        assert!(!dst.path().join(".git/HEAD").exists());
        assert!(!dst.path().join("pages/note.md.tmp").exists());
    }

    #[test]
    fn sanitize_archive_path_rejects_dotdot_and_absolute() {
        assert!(sanitize_archive_path(Path::new("../etc/passwd")).is_none());
        assert!(sanitize_archive_path(Path::new("/etc/passwd")).is_none());
        assert!(sanitize_archive_path(Path::new("./hidden")).is_none());
        assert!(sanitize_archive_path(Path::new(".git/HEAD")).is_none());
        // Empty path
        assert!(sanitize_archive_path(Path::new("")).is_none());
        // Valid
        assert_eq!(
            sanitize_archive_path(Path::new("pages/foo.md")).unwrap(),
            PathBuf::from("pages/foo.md"),
        );
    }

    #[test]
    fn export_excludes_root_level_dotfiles() {
        // Regression for an end-to-end issue: if the wiki had git
        // enabled, `.gitignore` landed in the tarball and then the
        // importer (correctly) rejected it as a dotfile, breaking the
        // round-trip. Export must filter dotfiles up front.
        let src = tempdir().unwrap();
        write(&src.path().join("profile.md"), "# Profile\n");
        write(&src.path().join(".gitignore"), ".pending/\n");
        write(&src.path().join(".hidden"), "secret");
        std::fs::create_dir_all(src.path().join(".git/objects")).unwrap();
        write(&src.path().join(".git/HEAD"), "ref: refs/heads/main\n");

        let mut buf = Vec::new();
        let n = export_tar_gz(src.path(), &mut buf).unwrap();
        assert_eq!(n, 1, "only profile.md should ship; got {n}");

        // Round-trip succeeds — no PathTraversal rejection.
        let dst = tempdir().unwrap();
        let extracted = import_tar_gz(dst.path(), &buf[..]).unwrap();
        assert_eq!(extracted, 1);
        assert!(dst.path().join("profile.md").exists());
        assert!(!dst.path().join(".gitignore").exists());
    }

    #[test]
    fn sanitize_archive_path_blocks_unsafe_components() {
        // The Rust `tar` crate refuses to *write* `..` paths in the
        // first place, so synthesising a malicious archive in-process
        // requires byte-level header forgery. The on-extract check
        // matters for archives produced by *other* tools (GNU tar, BSD
        // tar). We rely on `sanitize_archive_path` to be the gate, and
        // exercise it directly above.
        assert!(sanitize_archive_path(Path::new("subdir/../escape.md")).is_none());
        assert!(sanitize_archive_path(Path::new("..\\windows\\evil")).is_none()
            || sanitize_archive_path(Path::new("..\\windows\\evil"))
                .map(|p| !p.to_string_lossy().contains(".."))
                .unwrap_or(true));
    }
}
