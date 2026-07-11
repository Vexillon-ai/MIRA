// SPDX-License-Identifier: AGPL-3.0-or-later

// src/artifacts/mod.rs
//! Content-addressed artifact store for tool-produced binary outputs.
//!
//! Image files (and other small binary blobs) produced by sandboxed tools
//! land here, keyed by SHA-256 of their bytes. The HTTP layer serves them
//! at `/api/artifacts/<sha>.<ext>`; because the URL is the digest, it's a
//! ~256-bit unguessable capability — no bearer auth needed for reads, which
//! is what lets `<img src>` render artifacts inline in chat without an
//! extra fetch+blob-URL dance.
//!
//! ## Storage layout
//!
//! ```text
//! <data_dir>/artifacts/
//!   ab12cd34...ef.png
//!   ...
//! ```
//!
//! Files are written once and never mutated; identical inputs collapse to
//! the same path. We don't garbage-collect this iteration — image artifacts
//! are small and disk pressure is a future problem.
//!
//! ## Allowed extensions
//!
//! Only the small set in [`ALLOWED_EXTENSIONS`] is accepted. Anything else
//! returns an error from [`ArtifactStore::save_bytes`] before it touches
//! disk, and the HTTP handler validates the same set on the read side.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// File extensions the store will accept. Lowercase, no leading dot.
/// Mirrored on the HTTP read path so a stale or hand-crafted URL with a
/// disallowed extension is rejected before we open the file.
pub const ALLOWED_EXTENSIONS: &[&str] = &[
    // images
    "png", "jpg", "jpeg", "gif", "svg", "webp",
    // audio (MCP audio content blocks, generated speech, recordings)
    "mp3", "wav", "ogg", "opus", "m4a", "flac",
    // video
    "mp4", "webm", "mov",
];

/// Identifier for one stored artifact: the hex-encoded SHA-256 digest plus
/// the original extension, in `<sha>.<ext>` form. This is exactly what the
/// HTTP path segment carries and what the markdown image ref points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactId {
    pub sha256_hex: String,
    pub extension:  String,
}

impl ArtifactId {
    /// `<sha>.<ext>` — the on-disk filename and the URL path segment.
    pub fn filename(&self) -> String { format!("{}.{}", self.sha256_hex, self.extension) }

    /// Markdown image reference suitable for splicing into a tool output
    /// string. The web UI's `urlTransform` whitelists this prefix.
    pub fn markdown_image(&self, alt: &str) -> String {
        format!("![{alt}](/api/artifacts/{})", self.filename())
    }
}

/// Repair artifact URLs that a weak model mangled when it re-typed one into its
/// prose. Our artifact layer always emits root-relative `/api/artifacts/<sha>`,
/// but a model summarising a tool result sometimes "helpfully" rewrites that
/// leading path into a bogus host — `/api/artifacts/<sha>.png` becomes
/// `https://api.artifacts/<sha>.png`, which no client can load. `api.artifacts`
/// is never a real host, so folding it back to the root-relative form is safe
/// and lossless. Applied to assistant content before it's persisted so reloads
/// render the image on every client.
pub fn normalize_artifact_urls(content: &str) -> String {
    content
        .replace("https://api.artifacts/", "/api/artifacts/")
        .replace("http://api.artifacts/",  "/api/artifacts/")
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("artifact extension `{0}` is not in the allowlist")]
    DisallowedExtension(String),
    #[error("artifact io: {0}")]
    Io(#[from] std::io::Error),
}

/// Filesystem-backed artifact store rooted at `<data_dir>/artifacts/`.
/// Cheap to clone — wraps a `PathBuf`.
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    /// Construct, ensuring the root directory exists. The caller passes the
    /// resolved data dir path; we own the `artifacts/` subdir.
    pub fn new(data_dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let root = data_dir.as_ref().join("artifacts");
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// Hex-encoded SHA-256 of `bytes`. Lowercase, 64 chars.
    fn digest_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        hex::encode(h.finalize())
    }

    /// Lowercase the extension and confirm it's allowed.
    fn normalize_extension(extension: &str) -> Result<String, ArtifactError> {
        let ext = extension.trim_start_matches('.').to_ascii_lowercase();
        if !ALLOWED_EXTENSIONS.iter().any(|e| *e == ext) {
            return Err(ArtifactError::DisallowedExtension(ext));
        }
        Ok(ext)
    }

    /// Store `bytes` under `<sha>.<ext>`. Idempotent: re-saving identical
    /// bytes is a no-op (we skip the write if the path already exists with
    /// matching length, which avoids a needless fsync on the hot path).
    pub fn save_bytes(&self, bytes: &[u8], extension: &str) -> Result<ArtifactId, ArtifactError> {
        let ext = Self::normalize_extension(extension)?;
        let sha = Self::digest_hex(bytes);
        let id  = ArtifactId { sha256_hex: sha, extension: ext };

        let path = self.root.join(id.filename());
        let needs_write = match std::fs::metadata(&path) {
            Ok(m)  => m.len() as usize != bytes.len(),
            Err(_) => true,
        };
        if needs_write {
            std::fs::write(&path, bytes)?;
        }
        Ok(id)
    }

    /// Resolve an on-disk path for an already-validated filename. Returns
    /// `None` if the file doesn't exist; the caller decides whether that's a
    /// 404 or a 500.
    ///
    /// **Caller MUST validate the filename first** — see
    /// [`is_valid_filename`]. We don't re-validate here so the HTTP layer
    /// can reject malformed input with a 400 before this is called.
    pub fn path_for(&self, filename: &str) -> Option<PathBuf> {
        let p = self.root.join(filename);
        if p.is_file() { Some(p) } else { None }
    }
}

/// Strict shape check for a `<sha>.<ext>` URL path segment: 64 lowercase
/// hex chars, a literal `.`, then one of [`ALLOWED_EXTENSIONS`]. Anything
/// else (`..`, slashes, uppercase, query strings) is rejected.
pub fn is_valid_filename(filename: &str) -> bool {
    let Some((sha, ext)) = filename.split_once('.') else { return false };
    if sha.len() != 64 || !sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return false;
    }
    ALLOWED_EXTENSIONS.iter().any(|e| *e == ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_resolve_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(dir.path()).unwrap();
        let id = store.save_bytes(b"hello-world", "png").unwrap();
        assert_eq!(id.sha256_hex.len(), 64);
        assert_eq!(id.extension, "png");
        assert!(store.path_for(&id.filename()).is_some());
    }

    #[test]
    fn save_is_idempotent_on_identical_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(dir.path()).unwrap();
        let a = store.save_bytes(b"x", "png").unwrap();
        let b = store.save_bytes(b"x", "png").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn rejects_disallowed_extension() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(dir.path()).unwrap();
        let r = store.save_bytes(b"data", "exe");
        assert!(matches!(r, Err(ArtifactError::DisallowedExtension(_))));
    }

    #[test]
    fn extension_is_lowercased() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(dir.path()).unwrap();
        let id = store.save_bytes(b"x", "PNG").unwrap();
        assert_eq!(id.extension, "png");
    }

    #[test]
    fn markdown_ref_format() {
        let id = ArtifactId {
            sha256_hex: "a".repeat(64),
            extension:  "png".to_string(),
        };
        let md = id.markdown_image("chart");
        assert_eq!(md, format!("![chart](/api/artifacts/{}.png)", "a".repeat(64)));
    }

    #[test]
    fn normalizes_mangled_artifact_host() {
        let sha = "a".repeat(64);
        let mangled = format!("Here it is: ![img](https://api.artifacts/{sha}.png) enjoy");
        let fixed   = normalize_artifact_urls(&mangled);
        assert_eq!(fixed, format!("Here it is: ![img](/api/artifacts/{sha}.png) enjoy"));
        // http variant too.
        assert_eq!(
            normalize_artifact_urls("http://api.artifacts/x.wav"),
            "/api/artifacts/x.wav",
        );
        // Already-correct + unrelated content is untouched.
        let ok = format!("![img](/api/artifacts/{sha}.png) and https://example.com/photo.png");
        assert_eq!(normalize_artifact_urls(&ok), ok);
    }

    #[test]
    fn filename_validator_accepts_valid() {
        let good = format!("{}.png", "a".repeat(64));
        assert!(is_valid_filename(&good));
    }

    #[test]
    fn filename_validator_rejects_traversal_uppercase_and_bad_ext() {
        assert!(!is_valid_filename("../etc/passwd"));
        assert!(!is_valid_filename(&format!("{}.exe", "a".repeat(64))));
        assert!(!is_valid_filename(&format!("{}.png", "A".repeat(64))));
        assert!(!is_valid_filename(&format!("{}.png", "a".repeat(63))));
        assert!(!is_valid_filename("noextension"));
        assert!(!is_valid_filename(&format!("{}/{}.png", "a".repeat(32), "b".repeat(64))));
    }
}
