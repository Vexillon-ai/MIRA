// SPDX-License-Identifier: AGPL-3.0-or-later

// src/wiki/page.rs
//! In-memory representation of a wiki page and atomic read/write.
//!
//! Every write goes through [`write_atomic`] which writes to a temp file
//! in the same directory then renames — so readers see either the old
//! content or the new, never a truncated in-progress state. This mirrors
//! the pattern used by `src/onboarding/profile_file.rs`.

use std::fs;
use std::io;
use std::path::Path;

use crate::wiki::frontmatter::{self, PageFrontmatter};
use crate::wiki::paths::WikiPath;
use crate::wiki::{Result, WikiError};

/// A loaded page: its path within the wiki, its parsed frontmatter,
/// and the body text after the frontmatter delimiters.
#[derive(Debug, Clone)]
pub struct WikiPage {
    pub path: WikiPath,
    pub frontmatter: PageFrontmatter,
    pub body: String,
}

impl WikiPage {
    /// Read a page from disk, parsing frontmatter. Returns `WikiError::PageNotFound`
    /// for ENOENT.
    pub fn read(root: &Path, path: &WikiPath) -> Result<Self> {
        let abs = path.resolve(root);
        let text = match fs::read_to_string(&abs) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(WikiError::PageNotFound(path.to_string()));
            }
            Err(e) => return Err(e.into()),
        };
        let (frontmatter, body) = frontmatter::parse(&text)?;
        Ok(Self {
            path: path.clone(),
            frontmatter,
            body,
        })
    }

    /// Try to read; return `Ok(None)` if the file doesn't exist yet.
    pub fn try_read(root: &Path, path: &WikiPath) -> Result<Option<Self>> {
        match Self::read(root, path) {
            Ok(p) => Ok(Some(p)),
            Err(WikiError::PageNotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Serialize frontmatter + body and write atomically.
    pub fn write(&self, root: &Path) -> Result<()> {
        let serialized = frontmatter::serialize(&self.frontmatter, &self.body)?;
        write_atomic(&self.path.resolve(root), serialized.as_bytes())?;
        Ok(())
    }
}

/// Write the contents to `target` atomically. Creates parent directories.
pub fn write_atomic(target: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = target.with_extension(format!(
        "{}.tmp",
        target.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, target)?;
    Ok(())
}

/// Write a string to `target` atomically. Convenience for the default
/// content the wiki creates on first boot.
pub fn write_raw(target: &Path, contents: &str) -> Result<()> {
    write_atomic(target, contents.as_bytes())
}

/// Replace the body of one `## <heading>` section. If the heading is
/// missing, append it at the end of `body`.
/// Return the body of a `## <heading>` section, with leading and
/// trailing blank lines trimmed. Returns `None` when the heading
/// isn't present in `body`, or `Some("")` when the section exists
/// but is empty (so callers can distinguish "missing" from "empty").
pub fn read_section(body: &str, heading: &str) -> Option<String> {
    let heading_line = format!("## {heading}");
    let lines: Vec<&str> = body.lines().collect();
    let start = lines.iter().position(|l| l.trim_end() == heading_line)?;
    let body_start = start + 1;
    let body_end = lines[body_start..]
        .iter()
        .position(|l| l.starts_with("## "))
        .map(|i| body_start + i)
        .unwrap_or(lines.len());
    let section = lines[body_start..body_end].join("\n");
    Some(section.trim_matches('\n').to_string())
}

pub fn replace_section(body: &str, heading: &str, new_body: &str) -> String {
    let heading_line = format!("## {heading}");
    let lines: Vec<&str> = body.lines().collect();

    let Some(start) = lines.iter().position(|l| l.trim_end() == heading_line) else {
        let mut out = body.trim_end().to_owned();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&heading_line);
        out.push('\n');
        out.push('\n');
        out.push_str(new_body.trim_end());
        out.push('\n');
        return out;
    };

    let body_start = start + 1;
    let body_end = lines[body_start..]
        .iter()
        .position(|l| l.starts_with("## "))
        .map(|i| body_start + i)
        .unwrap_or(lines.len());

    let mut out = String::new();
    for l in &lines[..=start] {
        out.push_str(l);
        out.push('\n');
    }
    let trimmed_new = new_body.trim_end();
    if !trimmed_new.is_empty() {
        out.push('\n');
        out.push_str(trimmed_new);
        out.push('\n');
    }
    if body_end < lines.len() {
        out.push('\n');
        for l in &lines[body_end..] {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

/// Append text under a `## <heading>` section, preserving prior content.
/// If the heading doesn't exist, the section is created at the end.
pub fn append_section(body: &str, heading: &str, addition: &str) -> String {
    let heading_line = format!("## {heading}");
    let lines: Vec<&str> = body.lines().collect();

    let Some(start) = lines.iter().position(|l| l.trim_end() == heading_line) else {
        let mut out = body.trim_end().to_owned();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&heading_line);
        out.push('\n');
        out.push('\n');
        out.push_str(addition.trim_end());
        out.push('\n');
        return out;
    };

    let body_start = start + 1;
    let body_end = lines[body_start..]
        .iter()
        .position(|l| l.starts_with("## "))
        .map(|i| body_start + i)
        .unwrap_or(lines.len());

    let mut out = String::new();
    for l in &lines[..body_end] {
        out.push_str(l);
        out.push('\n');
    }
    // Trim trailing blank lines from the existing section before appending.
    while out.ends_with("\n\n") { out.pop(); }
    out.push_str("\n\n");
    out.push_str(addition.trim_end());
    out.push('\n');
    if body_end < lines.len() {
        out.push('\n');
        for l in &lines[body_end..] {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempdir().unwrap();
        let p = WikiPath::parse("pages/foo.md").unwrap();
        let mut fm = PageFrontmatter::default();
        fm.title = Some("Foo".into());
        fm.tags = vec!["t1".into()];

        let page = WikiPage { path: p.clone(), frontmatter: fm, body: "# Hello\n".into() };
        page.write(dir.path()).unwrap();

        let back = WikiPage::read(dir.path(), &p).unwrap();
        assert_eq!(back.frontmatter.title.as_deref(), Some("Foo"));
        assert_eq!(back.frontmatter.tags, vec!["t1".to_string()]);
        assert!(back.body.contains("# Hello"));
    }

    #[test]
    fn try_read_returns_none_for_missing() {
        let dir = tempdir().unwrap();
        let p = WikiPath::parse("missing.md").unwrap();
        assert!(WikiPage::try_read(dir.path(), &p).unwrap().is_none());
    }

    #[test]
    fn replace_section_swaps_body() {
        let body = "## A\nfirst\n\n## B\nsecond\n";
        let out = replace_section(body, "A", "FIRST-NEW");
        assert!(out.contains("FIRST-NEW"));
        assert!(out.contains("## B"));
        assert!(out.contains("second"));
        assert!(!out.contains("first\n"));
    }

    #[test]
    fn replace_section_creates_when_missing() {
        let body = "## A\nfirst\n";
        let out = replace_section(body, "B", "new-b");
        assert!(out.contains("## A"));
        assert!(out.contains("## B"));
        assert!(out.contains("new-b"));
    }

    #[test]
    fn append_section_keeps_old_and_adds_new() {
        let body = "## A\nfirst\n\n## B\nsecond\n";
        let out = append_section(body, "A", "added");
        assert!(out.contains("first"));
        assert!(out.contains("added"));
        // "second" must still appear under ## B
        assert!(out.contains("## B\nsecond"));
    }

    #[test]
    fn append_section_creates_when_missing() {
        let body = "## A\nfirst\n";
        let out = append_section(body, "Z", "zzz");
        assert!(out.contains("## A"));
        assert!(out.contains("## Z"));
        assert!(out.contains("zzz"));
    }

    #[test]
    fn read_section_extracts_body_between_headings() {
        let body = "## A\nfirst\nstill-first\n\n## B\nsecond\n";
        assert_eq!(read_section(body, "A").as_deref(), Some("first\nstill-first"));
        assert_eq!(read_section(body, "B").as_deref(), Some("second"));
    }

    #[test]
    fn read_section_returns_none_for_missing() {
        let body = "## A\nfirst\n";
        assert!(read_section(body, "Z").is_none());
    }

    #[test]
    fn read_section_returns_empty_string_for_empty_section() {
        let body = "## A\n\n## B\nsecond\n";
        assert_eq!(read_section(body, "A").as_deref(), Some(""));
    }
}
