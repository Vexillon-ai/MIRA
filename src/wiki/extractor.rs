// SPDX-License-Identifier: AGPL-3.0-or-later

// src/wiki/extractor.rs
//! LLM-based wiki extractor — post-turn pass that derives [`WikiOp`]s
//! from a completed conversation turn.
//!
//! Sister to `src/memory/auto_extract.rs::LlmMemoryExtractor`. The memory
//! extractor pulls **atomic facts** ("user lives in Melbourne"); the wiki
//! extractor pulls **narrative observations** worth filing on a page
//! ("user is working on a Pong game; created `pages/projects/pong.md`
//! during this turn").
//!
//! Failure-tolerant by design: any parse or provider error returns an
//! empty list so an extraction hiccup never affects the user's chat.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tracing::{debug, warn};

use crate::providers::ModelProvider;
use crate::types::{ChatMessage, GenerationOptions};
use crate::wiki::frontmatter::{PageFrontmatter, Writer};
use crate::wiki::ops::{LogKind, WikiOp};
use crate::wiki::paths::WikiPath;

/// Upper bound on the extractor call. Mirrors the memory extractor.
const WIKI_EXTRACTOR_TIMEOUT: Duration = Duration::from_secs(180);

/// One raw candidate from the model. We do typed parsing only for the
/// fields we care about; an extractor that emits extra fields won't be
/// rejected.
#[derive(Debug, Deserialize)]
struct RawCandidate {
    kind: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    section: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    confidence: f32,
}

#[derive(Debug, Deserialize)]
struct RawExtraction {
    #[serde(default)]
    wiki_ops: Vec<RawCandidate>,
}

/// Public entry point.
///
/// `existing_pages` is the list of `WikiPath`s currently in the wiki —
/// the model uses this to decide whether to append to an existing page
/// or create a new one.
pub async fn extract_wiki_ops(
    provider: &Arc<dyn ModelProvider>,
    user_msg: &str,
    assistant_msg: &str,
    existing_pages: &[WikiPath],
    min_confidence: f32,
    max_ops: usize,
) -> Vec<(WikiOp, f32)> {
    let messages = vec![
        ChatMessage::system(build_system_prompt(existing_pages)),
        ChatMessage::user(build_user_prompt(user_msg, assistant_msg)),
    ];
    let opts = GenerationOptions {
        temperature: 0.0,
        // Verbose models can blow past a tight cap mid-array; 4096 fits the
        // typical handful of ops, and `parse_json` salvages a truncated tail.
        max_tokens: Some(4096),
        ..Default::default()
    };

    let response = match tokio::time::timeout(
        WIKI_EXTRACTOR_TIMEOUT,
        provider.generate(&messages, &opts),
    ).await {
        Ok(Ok(r))  => r.content,
        Ok(Err(e)) => {
            warn!("wiki extractor: provider call failed, skipping: {}", e);
            return vec![];
        }
        Err(_) => {
            warn!("wiki extractor: timed out after {:?}, skipping", WIKI_EXTRACTOR_TIMEOUT);
            return vec![];
        }
    };

    let raw = match parse_json(&response) {
        Some(r) => r,
        None => {
            // After the salvage attempt, an unparseable response is either
            // truncated beyond any complete op or genuinely not JSON.
            let looks_json = response.trim_start().starts_with('{') || response.contains("wiki_ops");
            if looks_json {
                debug!("wiki extractor: response truncated with no complete op (raise max_tokens or model too verbose). Head: {:?}",
                       truncate_for_log(&response, 200));
            } else {
                warn!("wiki extractor: no JSON in response. Raw: {:?}",
                      truncate_for_log(&response, 200));
            }
            return vec![];
        }
    };

    let mut ops: Vec<(WikiOp, f32)> = Vec::new();
    for cand in raw.wiki_ops {
        if cand.confidence < min_confidence {
            debug!("wiki extractor: dropping kind='{}' (confidence {} < {})",
                   cand.kind, cand.confidence, min_confidence);
            continue;
        }
        let confidence = cand.confidence;
        match candidate_to_op(cand) {
            Ok(op) => ops.push((op, confidence)),
            Err(e) => debug!("wiki extractor: dropping candidate: {}", e),
        }
        if ops.len() >= max_ops {
            break;
        }
    }
    debug!("wiki extractor: emitted {} ops (cap {})", ops.len(), max_ops);
    ops
}

// ── Prompt building ──────────────────────────────────────────────────────────

fn build_system_prompt(existing_pages: &[WikiPath]) -> String {
    let pages_str: String = if existing_pages.is_empty() {
        "(none yet)".to_string()
    } else {
        existing_pages.iter()
            .filter(|p| !p.is_special())
            .map(|p| format!("- {}", p.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(r#"You file narrative observations into a user's personal wiki.

Output ONLY a JSON object, no prose, no reasoning, no code fences:
{{"wiki_ops":[{{"kind":"<append_section|write_page|log_entry>","path":"<wiki path>","section":"<heading>","title":"<page title>","body":"<markdown body>","summary":"<short>","tags":["<tag>"],"confidence":<0.0-1.0>}}]}}

What to extract:
- Multi-turn observations or projects the user is working on
- Synthesized insights or plans worth referencing later
- New entities (people, projects, tools) worth a dedicated page

What NOT to extract:
- Atomic facts ("user lives in Melbourne") — those go to memory, not the wiki
- Things the assistant said — only user-facing observations
- Trivia the user wouldn't want filed

Existing pages (prefer appending to one of these over creating a new page):
{pages_str}

Rules:
- `kind = "append_section"` requires `path` (existing page) + `section` + `body`
- `kind = "write_page"` requires `path` (new page, e.g. "pages/projects/foo.md") + `title` + `body`
- `kind = "log_entry"` requires `summary` only (records that this conversation happened)
- `confidence` must be a float in [0.0, 1.0] — only emit if you're at least somewhat confident
- Maximum 3 ops per extraction; fewer is better
- If nothing wiki-worthy was discussed, return {{"wiki_ops":[]}}
- All paths use forward slashes; never use `..`

Return ONLY the JSON, no explanation.
"#)
}

fn build_user_prompt(user_msg: &str, assistant_msg: &str) -> String {
    format!(
        "User said:\n{user}\n\nAssistant replied:\n{assistant}\n\nExtract wiki ops as JSON.",
        user = truncate_for_log(user_msg, 4000),
        assistant = truncate_for_log(assistant_msg, 4000),
    )
}

// ── Parsing ──────────────────────────────────────────────────────────────────

fn parse_json(response: &str) -> Option<RawExtraction> {
    // Find the outermost JSON object; tolerate code fences and prose wrappers.
    let stripped = strip_code_fences(response);
    let start = stripped.find('{')?;
    let body  = &stripped[start..];

    // Fast path: well-formed response.
    let end = stripped.rfind('}').map(|p| p + 1);
    if let Some(end) = end {
        if let Ok(r) = serde_json::from_str::<RawExtraction>(&stripped[start..end]) {
            return Some(r);
        }
    }
    // Salvage path: the model almost always truncates mid-array (a verbose
    // body blows past max_tokens). Recover the `wiki_ops` elements that *did*
    // complete by closing the array + root after the last finished object.
    salvage_truncated_ops(body)
}

/// Reconstruct a valid `{"wiki_ops":[...]}` from a response truncated mid-array
/// by keeping only the objects that closed cleanly. `body` must start at `{`.
fn salvage_truncated_ops(body: &str) -> Option<RawExtraction> {
    let bytes = body.as_bytes();
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    let mut last_elem_end: Option<usize> = None; // byte index just past a completed array element
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if esc { esc = false; } else if b == b'\\' { esc = true; } else if b == b'"' { in_str = false; }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => depth += 1,
            b'}' => { depth -= 1; if depth == 2 { last_elem_end = Some(i + 1); } } // closed an element of the top-level array
            b']' => depth -= 1,
            _ => {}
        }
    }
    let end = last_elem_end?;
    let rebuilt = format!("{}]}}", &body[..end]); // close the array + the root object
    serde_json::from_str(&rebuilt).ok()
}

fn strip_code_fences(s: &str) -> String {
    s.lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_for_log(s: &str, max: usize) -> String {
    if s.chars().count() <= max { return s.to_string(); }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("…");
    out
}

// ── Candidate → WikiOp ───────────────────────────────────────────────────────

fn candidate_to_op(c: RawCandidate) -> Result<WikiOp, String> {
    match c.kind.as_str() {
        "append_section" => {
            let path    = c.path.ok_or("append_section missing path")?;
            let section = c.section.ok_or("append_section missing section")?;
            let body    = c.body.ok_or("append_section missing body")?;
            if body.trim().is_empty() { return Err("empty body".into()); }
            let path = WikiPath::parse(&path).map_err(|e| e.to_string())?;
            Ok(WikiOp::AppendSection { path, section, body })
        }
        "write_page" => {
            let path  = c.path.ok_or("write_page missing path")?;
            let title = c.title.ok_or("write_page missing title")?;
            let body  = c.body.ok_or("write_page missing body")?;
            if body.trim().is_empty() { return Err("empty body".into()); }
            let path = WikiPath::parse(&path).map_err(|e| e.to_string())?;
            let mut fm = PageFrontmatter::default();
            fm.title = Some(title);
            fm.writer = Writer::Agent;
            fm.tags = c.tags;
            fm.confidence = Some(c.confidence);
            Ok(WikiOp::WritePage { path, frontmatter: fm, body })
        }
        "log_entry" => {
            let summary = c.summary.or(c.body).ok_or("log_entry missing summary")?;
            if summary.trim().is_empty() { return Err("empty summary".into()); }
            Ok(WikiOp::LogEntry { kind: LogKind::Note, summary, page_refs: vec![] })
        }
        other => Err(format!("unknown kind '{}'", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_response() {
        let resp = r#"{"wiki_ops":[
            {"kind":"append_section","path":"pages/proj.md","section":"Notes","body":"hello","confidence":0.9},
            {"kind":"log_entry","summary":"Discussed X","confidence":0.7}
        ]}"#;
        let parsed = parse_json(resp).unwrap();
        assert_eq!(parsed.wiki_ops.len(), 2);
    }

    #[test]
    fn salvages_truncated_response() {
        // One complete op, then a second op cut off mid-string (the real
        // failure mode: a verbose body blows past max_tokens). We should
        // recover the completed op rather than dropping everything.
        let resp = r#"{"wiki_ops":[
            {"kind":"append_section","path":"pages/guitar.md","section":"Resources","body":"Guitar Tricks course","confidence":0.9},
            {"kind":"append_section","path":"pages/guitar.md","section":"More","body":"TrueFire library, $29.95/month or $299.95/yr (free tri"#;
        let parsed = parse_json(resp).expect("should salvage the complete op");
        assert_eq!(parsed.wiki_ops.len(), 1);
    }

    #[test]
    fn parses_with_code_fences_and_prose() {
        let resp = "Here are the ops:\n```json\n{\"wiki_ops\":[]}\n```\nDone.";
        let parsed = parse_json(resp).unwrap();
        assert!(parsed.wiki_ops.is_empty());
    }

    #[test]
    fn candidate_to_op_append_section() {
        let c = RawCandidate {
            kind: "append_section".into(),
            path: Some("pages/foo.md".into()),
            section: Some("Notes".into()),
            title: None,
            body: Some("hello".into()),
            summary: None,
            tags: vec![],
            confidence: 0.9,
        };
        let op = candidate_to_op(c).unwrap();
        assert!(matches!(op, WikiOp::AppendSection { .. }));
    }

    #[test]
    fn candidate_to_op_rejects_missing_path() {
        let c = RawCandidate {
            kind: "append_section".into(),
            path: None,
            section: Some("Notes".into()),
            title: None,
            body: Some("hello".into()),
            summary: None,
            tags: vec![],
            confidence: 0.9,
        };
        assert!(candidate_to_op(c).is_err());
    }

    #[test]
    fn candidate_to_op_rejects_dotdot_path() {
        let c = RawCandidate {
            kind: "append_section".into(),
            path: Some("../etc/passwd".into()),
            section: Some("Notes".into()),
            title: None,
            body: Some("hello".into()),
            summary: None,
            tags: vec![],
            confidence: 0.9,
        };
        assert!(candidate_to_op(c).is_err());
    }

    #[test]
    fn candidate_to_op_write_page_sets_agent_writer() {
        let c = RawCandidate {
            kind: "write_page".into(),
            path: Some("pages/new.md".into()),
            section: None,
            title: Some("New".into()),
            body: Some("body".into()),
            summary: None,
            tags: vec!["t1".into()],
            confidence: 0.8,
        };
        let op = candidate_to_op(c).unwrap();
        if let WikiOp::WritePage { frontmatter, .. } = op {
            assert_eq!(frontmatter.writer, Writer::Agent);
            assert_eq!(frontmatter.tags, vec!["t1".to_string()]);
        } else {
            panic!("expected WritePage");
        }
    }

    #[test]
    fn candidate_to_op_log_entry() {
        let c = RawCandidate {
            kind: "log_entry".into(),
            path: None,
            section: None,
            title: None,
            body: None,
            summary: Some("convo".into()),
            tags: vec![],
            confidence: 0.5,
        };
        let op = candidate_to_op(c).unwrap();
        assert!(matches!(op, WikiOp::LogEntry { .. }));
    }
}
