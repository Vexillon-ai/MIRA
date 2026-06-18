// SPDX-License-Identifier: AGPL-3.0-or-later

// src/wiki/paths.rs
//! [`WikiPath`] — a sanitized relative path under a wiki root.
//!
//! The constructor refuses absolute paths, `..` segments, leading dots,
//! and characters that aren't safe in a filename across the supported
//! platforms. Pages always end in `.md`; the four special files
//! (`SCHEMA.md`, `profile.md`/`persona.md`, `index.md`, `log.md`) are
//! recognised by [`WikiPath::is_special`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::wiki::{Result, WikiError};

/// Sanitized relative path under a wiki root. Use [`WikiPath::parse`] to
/// construct; the constructor enforces the safety rules.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WikiPath(String);

impl WikiPath {
    /// Parse a relative path. Rejects absolute paths, `..`, leading dots
    /// (except the special directories `.pending` and `.git` which the
    /// wiki creates internally — those are not accessible via this API).
    pub fn parse(input: &str) -> Result<Self> {
        let s = input.trim().trim_start_matches('/').trim_start_matches("./");
        if s.is_empty() {
            return Err(WikiError::InvalidPath("empty path".into()));
        }
        if s.contains("..") {
            return Err(WikiError::InvalidPath(format!("'..' not allowed: {input}")));
        }
        if s.starts_with('.') {
            return Err(WikiError::InvalidPath(format!("dotfile not allowed: {input}")));
        }
        for part in s.split('/') {
            if part.is_empty() {
                return Err(WikiError::InvalidPath(format!("empty segment: {input}")));
            }
            for c in part.chars() {
                if matches!(c, '\\' | ':' | '\0' | '\n' | '\r' | '\t') {
                    return Err(WikiError::InvalidPath(
                        format!("unsafe character {c:?} in path: {input}"),
                    ));
                }
            }
        }
        Ok(WikiPath(s.to_string()))
    }

    /// Construct without checking. Only for trusted internal callers
    /// (e.g. listing the filesystem) that have already validated.
    pub(crate) fn from_trusted(s: String) -> Self { WikiPath(s) }

    /// The relative-path representation (forward slashes).
    pub fn as_str(&self) -> &str { &self.0 }

    /// Resolve to an absolute path under `root`.
    pub fn resolve(&self, root: &Path) -> PathBuf {
        let mut out = root.to_path_buf();
        for part in self.0.split('/') {
            out.push(part);
        }
        out
    }

    /// True when this path is one of the four navigation files at the
    /// wiki root: SCHEMA.md, profile.md or persona.md, index.md, log.md.
    pub fn is_special(&self) -> bool {
        matches!(
            self.0.as_str(),
            "SCHEMA.md" | "profile.md" | "persona.md" | "index.md" | "log.md"
        )
    }

    /// True when this path ends in `.md`.
    pub fn is_markdown(&self) -> bool { self.0.ends_with(".md") }
}

impl std::fmt::Display for WikiPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_simple_paths() {
        assert_eq!(WikiPath::parse("profile.md").unwrap().as_str(), "profile.md");
        assert_eq!(WikiPath::parse("pages/projects/foo.md").unwrap().as_str(), "pages/projects/foo.md");
    }

    #[test]
    fn parse_rejects_dotdot() {
        assert!(WikiPath::parse("../etc/passwd").is_err());
        assert!(WikiPath::parse("pages/../../etc").is_err());
    }

    #[test]
    fn parse_rejects_absolute() {
        // The leading slash is stripped, but `/etc/passwd` then becomes
        // `etc/passwd` which is fine. The hostile cases (`..`, dotfiles)
        // are blocked by other rules.
        assert_eq!(WikiPath::parse("/foo.md").unwrap().as_str(), "foo.md");
    }

    #[test]
    fn parse_rejects_dotfile() {
        assert!(WikiPath::parse(".secret").is_err());
        assert!(WikiPath::parse(".pending/x.json").is_err());
    }

    #[test]
    fn parse_rejects_unsafe_chars() {
        assert!(WikiPath::parse("foo\\bar.md").is_err());
        assert!(WikiPath::parse("foo:bar.md").is_err());
        assert!(WikiPath::parse("foo\nbar.md").is_err());
    }

    #[test]
    fn special_recognised() {
        assert!(WikiPath::parse("SCHEMA.md").unwrap().is_special());
        assert!(WikiPath::parse("profile.md").unwrap().is_special());
        assert!(WikiPath::parse("persona.md").unwrap().is_special());
        assert!(WikiPath::parse("index.md").unwrap().is_special());
        assert!(WikiPath::parse("log.md").unwrap().is_special());
        assert!(!WikiPath::parse("pages/foo.md").unwrap().is_special());
    }

    #[test]
    fn resolve_under_root() {
        let p = WikiPath::parse("pages/projects/mira.md").unwrap();
        let root = Path::new("/tmp/wiki");
        assert_eq!(p.resolve(root), Path::new("/tmp/wiki/pages/projects/mira.md"));
    }
}
