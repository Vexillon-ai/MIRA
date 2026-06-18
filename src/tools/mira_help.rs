// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/mira_help.rs
//! `mira_help` — MIRA's self-knowledge tool.
//!
//! Lets the agent answer questions about MIRA *the product*: its features,
//! limitations, settings, and how to do things. The docs live in
//! `mira-docs/` at the repo root and are compiled into the binary via
//! `include_dir!`, so they're always version-matched to the running build
//! (no drift, no per-user wiki duplication). `settings-reference.md` is
//! generated from the config schema.
//!
//! This is **read-only product knowledge** available to every user. The
//! live *value* of a setting (and changing it) is access-gated and handled
//! by the settings tools (/3), not here.

use async_trait::async_trait;
use include_dir::{include_dir, Dir};
use serde_json::{json, Value};

use crate::tools::{Tool, ToolArgs, ToolResult};
use crate::MiraError;

static DOCS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/mira-docs");

const MAX_SECTIONS: usize = 4;
const MAX_SECTION_CHARS: usize = 1800;

pub struct MiraHelpTool;

#[async_trait]
impl Tool for MiraHelpTool {
    fn name(&self) -> &str { "mira_help" }

    fn description(&self) -> &str {
        "Look up how MIRA itself works — its features, limitations, settings, and how to do \
         things in MIRA. Use this for any question about MIRA the product (e.g. \"what can you \
         do?\", \"how do I enable companion check-ins?\", \"what does the agent.max_tool_rounds \
         setting do?\", \"what are MIRA's limitations?\"). Pass `query` describing what you want \
         to know; returns the most relevant documentation sections. This is product knowledge — \
         for the live value of a setting, use the settings tools instead."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What you want to know about MIRA." },
                "topic": { "type": "string", "description": "Optional: open a specific doc — one of: overview, features, settings, how-to, limitations, settings-reference." }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let topic = args.get("topic").and_then(|v| v.as_str()).map(|s| s.trim().to_lowercase());

        let sections = all_sections();

        // Explicit topic → return that doc's sections verbatim.
        if let Some(t) = topic.as_deref().filter(|s| !s.is_empty()) {
            let hits: Vec<&Section> = sections.iter().filter(|s| s.source.contains(t)).collect();
            if !hits.is_empty() {
                return Ok(ToolResult::success(render(&hits)));
            }
        }

        let scored = score(&sections, &query);
        if scored.is_empty() {
            return Ok(ToolResult::success(format!(
                "No MIRA documentation section matched \"{query}\". Available topics: {}.\n\
                 Try a broader query, or pass `topic` to open one.",
                topic_list().join(", ")
            )));
        }
        Ok(ToolResult::success(render(&scored)))
    }
}

// ─────────────────────────────────────────────────────────────────────────────

struct Section {
    source:  String, // doc filename stem, e.g. "settings"
    heading: String,
    body:    String,
}

// Split every bundled doc into `## `/`# ` sections.
fn all_sections() -> Vec<Section> {
    let mut out = Vec::new();
    for file in DOCS.files() {
        let Some(name) = file.path().file_stem().and_then(|s| s.to_str()) else { continue };
        let Some(text) = file.contents_utf8() else { continue };
        let mut heading = name.to_string();
        let mut body = String::new();
        let flush = |out: &mut Vec<Section>, heading: &str, body: &str| {
            if !body.trim().is_empty() || !heading.is_empty() {
                out.push(Section { source: name.to_string(), heading: heading.to_string(), body: body.trim().to_string() });
            }
        };
        for line in text.lines() {
            if let Some(h) = line.strip_prefix("# ").or_else(|| line.strip_prefix("## ")) {
                flush(&mut out, &heading, &body);
                heading = h.trim().to_string();
                body.clear();
            } else {
                body.push_str(line);
                body.push('\n');
            }
        }
        flush(&mut out, &heading, &body);
    }
    out
}

fn topic_list() -> Vec<String> {
    let mut t: Vec<String> = DOCS.files()
        .filter_map(|f| f.path().file_stem().and_then(|s| s.to_str()).map(String::from))
        .collect();
    t.sort();
    t.dedup();
    t
}

// Rank sections by query-term overlap (heading weighted 3×).
fn score<'a>(sections: &'a [Section], query: &str) -> Vec<&'a Section> {
    let terms: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(String::from)
        .collect();
    if terms.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(usize, &Section)> = Vec::new();
    for s in sections {
        let h = s.heading.to_lowercase();
        let b = s.body.to_lowercase();
        let mut sc = 0usize;
        for t in &terms {
            sc += h.matches(t.as_str()).count() * 3;
            sc += b.matches(t.as_str()).count();
        }
        if sc > 0 {
            scored.push((sc, s));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().take(MAX_SECTIONS).map(|(_, s)| s).collect()
}

fn render(sections: &[&Section]) -> String {
    let mut out = String::new();
    for s in sections {
        out.push_str(&format!("## {} (from {})\n", s.heading, s.source));
        let body = if s.body.chars().count() > MAX_SECTION_CHARS {
            let t: String = s.body.chars().take(MAX_SECTION_CHARS).collect();
            format!("{t}…")
        } else {
            s.body.clone()
        };
        out.push_str(&body);
        out.push_str("\n\n");
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docs_bundled_and_parsed() {
        let secs = all_sections();
        assert!(secs.len() > 10, "expected many sections, got {}", secs.len());
        assert!(topic_list().iter().any(|t| t == "features"));
        assert!(topic_list().iter().any(|t| t == "settings-reference"));
    }

    #[tokio::test]
    async fn query_finds_relevant_section() {
        let r = MiraHelpTool.execute(json!({"query": "companion check-in proactive"})).await.unwrap();
        assert!(r.success);
        assert!(r.output.to_lowercase().contains("companion"), "got: {}", r.output);
    }

    #[tokio::test]
    async fn query_finds_plugin_egress_blurb() {
        let r = MiraHelpTool
            .execute(json!({"query": "limit what a plugin connects to network egress allowlist"}))
            .await
            .unwrap();
        assert!(r.success);
        let out = r.output.to_lowercase();
        assert!(out.contains("egress allowlist"), "expected the egress section; got: {}", r.output);
        // both enforcement tiers should be described
        assert!(out.contains("privileged helper"), "native tier missing; got: {}", r.output);
        assert!(out.contains("sidecar"), "container tier missing; got: {}", r.output);
    }

    #[tokio::test]
    async fn topic_opens_specific_doc() {
        let r = MiraHelpTool.execute(json!({"query": "x", "topic": "limitations"})).await.unwrap();
        assert!(r.output.to_lowercase().contains("limitation"));
    }

    #[tokio::test]
    async fn unmatched_query_lists_topics() {
        let r = MiraHelpTool.execute(json!({"query": "zzzqqq nonsense xyzzy"})).await.unwrap();
        assert!(r.output.contains("Available topics"));
    }
}
