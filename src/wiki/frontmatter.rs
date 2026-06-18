// SPDX-License-Identifier: AGPL-3.0-or-later

// src/wiki/frontmatter.rs
//! YAML frontmatter at the top of every wiki page.
//!
//! A page on disk looks like:
//!
//! ```text
//! ---
//! title: Pong game project
//! writer: agent
//! tags: [project, game]
//! ---
//!
//! # Body markdown follows
//! ```
//!
//! [`PageFrontmatter`] is the typed view of the YAML block. Unknown
//! keys are preserved in [`PageFrontmatter::extra`] so a human can add
//! their own fields without the parser dropping them on rewrite.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::wiki::{Result, WikiError};

const FRONTMATTER_DELIM: &str = "---";

/// Who is allowed to mutate this page. Enforced by the applier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Writer {
    /// Only user-driven writes (UI edits) are allowed.
    User,
    /// Only agent-driven writes (extractor, tool calls) are allowed.
    Agent,
    /// Both sources may write.
    Both,
}

impl Writer {
    /// Default writer policy when frontmatter is missing or absent.
    pub const fn default_value() -> Self { Writer::Both }

    pub fn as_str(&self) -> &'static str {
        match self {
            Writer::User => "user",
            Writer::Agent => "agent",
            Writer::Both => "both",
        }
    }
}

impl Default for Writer {
    fn default() -> Self { Self::default_value() }
}

/// Where the content originated. Each applied op appends an entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenanceEntry {
    /// `turn`, `user_ui`, `tool`, `import`, etc. — free-form string.
    pub source: String,
    /// Optional turn id when source = "turn".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Optional conversation id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    pub extracted_at: DateTime<Utc>,
}

/// The typed view of a page's YAML frontmatter.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PageFrontmatter {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub writer: Writer,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provenance: Vec<ProvenanceEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<NaiveDate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<NaiveDate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// Anything the typed schema doesn't know about — preserved on rewrite
    /// so a user can extend the frontmatter without losing fields.
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

/// Split a file's text into (frontmatter, body). When no frontmatter is
/// present, returns `(default, full_text)`.
pub fn parse(text: &str) -> Result<(PageFrontmatter, String)> {
    let Some(rest) = strip_opening_delim(text) else {
        return Ok((PageFrontmatter::default(), text.to_string()));
    };
    let Some((yaml, body)) = split_at_closing_delim(rest) else {
        return Err(WikiError::InvalidFrontmatter("missing closing '---'".into()));
    };
    let fm: PageFrontmatter = serde_yaml::from_str(yaml).map_err(WikiError::from)?;
    // Conventionally the closing `---\n` is followed by a single blank line
    // before the body. Strip up to one leading `\n` so a round-trip
    // (parse → serialize → parse) yields the same body the caller wrote.
    let body = body.strip_prefix('\n').unwrap_or(body);
    Ok((fm, body.to_string()))
}

/// Serialize frontmatter + body back into the on-disk format. When the
/// frontmatter is empty (all defaults), the delimiters are still
/// emitted so a future parse round-trips cleanly.
pub fn serialize(fm: &PageFrontmatter, body: &str) -> Result<String> {
    let yaml = serde_yaml::to_string(fm).map_err(WikiError::from)?;
    let mut out = String::new();
    out.push_str(FRONTMATTER_DELIM);
    out.push('\n');
    out.push_str(yaml.trim_end());
    out.push('\n');
    out.push_str(FRONTMATTER_DELIM);
    out.push_str("\n\n");
    // Normalize: strip leading blank lines from body so repeated round-trips
    // don't accrete whitespace.
    out.push_str(body.trim_start_matches('\n'));
    if !out.ends_with('\n') { out.push('\n'); }
    Ok(out)
}

// ── Internals ────────────────────────────────────────────────────────────────

fn strip_opening_delim(text: &str) -> Option<&str> {
    // The opening delimiter must be at byte offset 0, optionally followed by
    // a BOM. The file then has `---\n` (or `---\r\n`).
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n"))
}

fn split_at_closing_delim(rest: &str) -> Option<(&str, &str)> {
    // Look for `\n---\n` (or `\n---\r\n`, or trailing `---` at EOF).
    let mut idx = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == FRONTMATTER_DELIM {
            let yaml = &rest[..idx];
            let body = &rest[idx + line.len()..];
            return Some((yaml, body));
        }
        idx += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_returns_defaults() {
        let (fm, body) = parse("just a body").unwrap();
        assert_eq!(fm.writer, Writer::Both);
        assert!(fm.tags.is_empty());
        assert_eq!(body, "just a body");
    }

    #[test]
    fn parse_typed_fields() {
        let src = "\
---
title: Pong game project
writer: agent
tags: [project, in-progress]
valid_from: 2026-05-13
confidence: 0.87
---

# Hello
body text
";
        let (fm, body) = parse(src).unwrap();
        assert_eq!(fm.title.as_deref(), Some("Pong game project"));
        assert_eq!(fm.writer, Writer::Agent);
        assert_eq!(fm.tags, vec!["project".to_string(), "in-progress".to_string()]);
        assert_eq!(fm.valid_from.unwrap().to_string(), "2026-05-13");
        assert!((fm.confidence.unwrap() - 0.87).abs() < 1e-6);
        assert!(body.starts_with("# Hello"));
    }

    #[test]
    fn parse_preserves_unknown_extra_keys() {
        let src = "---\nwriter: user\ncustom_key: 42\n---\n\nbody\n";
        let (fm, _) = parse(src).unwrap();
        assert!(fm.extra.contains_key("custom_key"));
    }

    #[test]
    fn round_trip_preserves_fields() {
        let mut fm = PageFrontmatter::default();
        fm.title = Some("Test".into());
        fm.writer = Writer::Agent;
        fm.tags = vec!["a".into(), "b".into()];
        let body = "# Body\nhello\n";

        let on_disk = serialize(&fm, body).unwrap();
        let (fm2, body2) = parse(&on_disk).unwrap();
        assert_eq!(fm2.title, fm.title);
        assert_eq!(fm2.writer, fm.writer);
        assert_eq!(fm2.tags, fm.tags);
        assert_eq!(body2.trim_end(), body.trim_end());
    }

    #[test]
    fn parse_rejects_unclosed_frontmatter() {
        let src = "---\nwriter: user\nno closing delim";
        assert!(parse(src).is_err());
    }
}
