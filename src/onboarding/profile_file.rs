// SPDX-License-Identifier: AGPL-3.0-or-later

// src/onboarding/profile_file.rs
//! Section-aware read/write for the per-user `profile.md`.
//!
//! Stored under `{data_dir}/profiles/{user_id}/profile.md`. The file is a
//! plain markdown document with a fixed set of `## Section` headings (see
//! [`PROFILE_SECTIONS`]). A section-scoped writer replaces just one
//! section's body without disturbing the others, so onboarding tool calls
//! that touch one topic don't clobber unrelated topics.
//!
//! Writes are atomic via temp-file + rename: readers see either the old
//! content or the new, never a truncated in-progress state.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Canonical section headings, in the order they should appear in a fresh
/// file. Must stay in sync with the `writes_to: profile_md.<section>` values
/// referenced in `prompts/onboarding.yaml` — unknown section names are
/// rejected by [`write_profile_section`].
pub const PROFILE_SECTIONS: &[(&str, &str)] = &[
    ("communication_style", "Communication style"),
    ("autonomy",            "Autonomy"),
    ("how_to_address_me",   "How to address me"),
    ("what_to_call_mira",   "What to call me (the assistant)"),
    ("goals",               "Goals"),
    ("off_limits",          "Off-limits"),
];

/// Map a legacy section key to the canonical `## Heading` shown on the
/// page. Returns `None` for unknown keys (so callers can skip rather
/// than create odd headings on the wiki mirror).
pub fn profile_md_heading(section_key: &str) -> Option<&'static str> {
    PROFILE_SECTIONS.iter().find(|(k, _)| *k == section_key).map(|(_, h)| *h)
}

#[derive(Debug)]
pub enum ProfileMdError {
    Io(io::Error),
    UnknownSection(String),
}

impl std::fmt::Display for ProfileMdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProfileMdError::Io(e)              => write!(f, "profile.md IO error: {}", e),
            ProfileMdError::UnknownSection(s)  => write!(f, "unknown profile section: {}", s),
        }
    }
}

impl std::error::Error for ProfileMdError {}

impl From<io::Error> for ProfileMdError {
    fn from(e: io::Error) -> Self { ProfileMdError::Io(e) }
}

/// Absolute path to a user's `profile.md`. Does not create the file.
pub fn profile_md_path(data_dir: &Path, user_id: &str) -> PathBuf {
    data_dir.join("profiles").join(user_id).join("profile.md")
}

/// Read the raw markdown. Returns `Ok(None)` if the file doesn't exist yet.
pub fn read_profile_md(data_dir: &Path, user_id: &str) -> Result<Option<String>, ProfileMdError> {
    let path = profile_md_path(data_dir, user_id);
    match fs::read_to_string(&path) {
        Ok(s)                                                    => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e)                                                   => Err(e.into()),
    }
}

/// Replace the body of one section with `body_markdown`. Creates the file
/// (with all sections present but empty) if it doesn't exist. `body_markdown`
/// is written verbatim under the section heading — callers are responsible
/// for formatting (bullet lists, prose, etc.). Trailing whitespace is
/// trimmed to avoid accreting blank lines across writes.
pub fn write_profile_section(
    data_dir:      &Path,
    user_id:       &str,
    section_key:   &str,
    body_markdown: &str,
) -> Result<(), ProfileMdError> {
    let heading = PROFILE_SECTIONS
        .iter()
        .find(|(k, _)| *k == section_key)
        .map(|(_, h)| *h)
        .ok_or_else(|| ProfileMdError::UnknownSection(section_key.to_owned()))?;

    let existing = read_profile_md(data_dir, user_id)?
        .unwrap_or_else(|| initial_skeleton(user_id));

    let updated = replace_section(&existing, heading, body_markdown.trim_end());
    atomic_write(&profile_md_path(data_dir, user_id), &updated)?;
    Ok(())
}

// ── Internals ─────────────────────────────────────────────────────────────────

fn initial_skeleton(user_id: &str) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("user_id: {}\n", user_id));
    out.push_str("version: 1\n");
    out.push_str("---\n\n");
    out.push_str("# Profile\n\n");
    for (_, heading) in PROFILE_SECTIONS {
        out.push_str(&format!("## {}\n\n", heading));
    }
    out
}

/// Replace the body between `## <heading>` and the next `## ` (or EOF). If
/// the heading is missing (e.g. a manually edited file), the section is
/// appended.
fn replace_section(existing: &str, heading: &str, new_body: &str) -> String {
    let heading_line = format!("## {}", heading);
    let lines: Vec<&str> = existing.lines().collect();

    let start = lines.iter().position(|l| l.trim_end() == heading_line);
    let Some(start) = start else {
        // Append a new section at the end.
        let mut out = existing.trim_end().to_owned();
        if !out.is_empty() { out.push_str("\n\n"); }
        out.push_str(&heading_line);
        out.push('\n');
        out.push('\n');
        out.push_str(new_body);
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
    // Up to and including the heading line.
    for l in &lines[..=start] {
        out.push_str(l);
        out.push('\n');
    }
    // Blank line + body (if non-empty).
    if !new_body.is_empty() {
        out.push('\n');
        out.push_str(new_body);
        out.push('\n');
    }
    // Tail: remaining sections, separated by a blank line.
    if body_end < lines.len() {
        out.push('\n');
        for l in &lines[body_end..] {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

fn atomic_write(path: &Path, content: &str) -> Result<(), io::Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("md.tmp");
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn round_trip_creates_file_with_all_sections() {
        let dir = tempdir().unwrap();
        write_profile_section(dir.path(), "u1", "goals", "- Ship onboarding\n- Write tests").unwrap();
        let body = read_profile_md(dir.path(), "u1").unwrap().unwrap();
        assert!(body.contains("## Goals"));
        assert!(body.contains("## Communication style"));
        assert!(body.contains("- Ship onboarding"));
    }

    #[test]
    fn writing_one_section_preserves_others() {
        let dir = tempdir().unwrap();
        write_profile_section(dir.path(), "u1", "goals", "First").unwrap();
        write_profile_section(dir.path(), "u1", "autonomy", "Ask before acting").unwrap();
        write_profile_section(dir.path(), "u1", "goals", "Second").unwrap();

        let body = read_profile_md(dir.path(), "u1").unwrap().unwrap();
        assert!(body.contains("Second"));
        assert!(!body.contains("First"));
        assert!(body.contains("Ask before acting"));
    }

    #[test]
    fn unknown_section_errors() {
        let dir = tempdir().unwrap();
        let err = write_profile_section(dir.path(), "u1", "nope", "x").unwrap_err();
        matches!(err, ProfileMdError::UnknownSection(_));
    }

    #[test]
    fn reading_nonexistent_returns_none() {
        let dir = tempdir().unwrap();
        assert!(read_profile_md(dir.path(), "ghost").unwrap().is_none());
    }
}
