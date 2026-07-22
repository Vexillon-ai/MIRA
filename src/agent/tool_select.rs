// SPDX-License-Identifier: AGPL-3.0-or-later

// src/agent/tool_select.rs
//! Just-in-Time Tools — adaptive per-turn tool selection.
//!
//! Instead of sending every enabled tool (name + description + full JSON
//! schema) to the model on every request, pick a small per-turn subset:
//! a configurable **core** set, the **semantic top-K** for the user's
//! message, and (slice 2) conversation-**sticky** tools. A `find_tools`
//! meta-tool (slice 3) lets the model pull in anything else on demand, so
//! nothing is ever permanently hidden. See design-docs/just-in-time-tools.md.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;

use crate::config::ToolSelectionConfig;
use crate::memory::MemorySystem;
use crate::tools::ToolRegistry;
use crate::types::{ChatMessage, MessageRole, ToolSpec};

/// The synthetic meta-tool name used for progressive disclosure.
pub const FIND_TOOLS_NAME: &str = "find_tools";

/// Lets the tool loop pull additional tools into a turn on demand (progressive
/// disclosure). Implemented by `AgentCore` over the semantic tool index; the
/// loop calls it when the model invokes `find_tools`.
#[async_trait]
pub trait ToolExpander: Send + Sync {
    /// Return up to a few `(tool_name, one-line description)` matching `query`,
    /// drawn ONLY from `pool` (the user's security allow-list) and excluding
    /// tools already active this turn. Restricting to `pool` is what keeps
    /// `find_tools` from loading a tool outside the user's scope.
    async fn expand(&self, query: &str, pool: &[String], already_active: &[String]) -> Vec<(String, String)>;
}

/// The `find_tools` tool spec injected into the model's tool list when adaptive
/// selection + progressive disclosure are on.
pub fn find_tools_spec() -> ToolSpec {
    ToolSpec::function(
        FIND_TOOLS_NAME,
        "Search for and load additional tools by capability when your current tools don't \
         cover what you need. Call with a short description of the capability (e.g. \"send an \
         email\", \"browse a web page\", \"query a database\"); matching tools become available \
         on your next step. Only the most relevant tools are loaded up front, so use this \
         whenever a tool you need seems to be missing.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The capability you need, in a few words."
                }
            },
            "required": ["query"]
        }),
    )
}

/// One-line system-prompt hint telling the model how many tools are loaded this
/// turn, how many more exist behind `find_tools`, and the contract: search
/// before declaring a capability unavailable. Returns `None` when there is
/// nothing more to load (`total <= loaded`) — with the full set present there's
/// nothing to discover, so the hint would be false. Only injected on turns
/// where adaptive selection actually narrowed the toolset AND `find_tools` is
/// exposed. See `design-docs/just-in-time-tools.md` §3.
pub fn find_tools_hint(loaded: usize, total: usize) -> Option<String> {
    let more = total.checked_sub(loaded).filter(|m| *m > 0)?;
    Some(format!(
        "Tool availability: {loaded} of {total} tools are loaded for this turn. {more} more \
         can be loaded on demand — call {FIND_TOOLS_NAME}(\"<capability you need>\") to pull in \
         any that seem missing. ALWAYS call {FIND_TOOLS_NAME} before telling the user a \
         capability is unavailable; the tool you need is very likely among the {more} not yet loaded."
    ))
}

/// Rank the full catalog for an on-demand `find_tools` query: top-`k` by
/// cosine, excluding already-active tools and `find_tools` itself. No
/// min-similarity gate — the model explicitly asked, so return the best
/// matches regardless.
pub fn rank_for_expand(
    all_names: &[String],
    index:     &ToolIndex,
    query_emb: &[f32],
    exclude:   &[String],
    k:         usize,
) -> Vec<String> {
    let ex: HashSet<&str> = exclude.iter().map(|s| s.as_str()).collect();
    let mut scored: Vec<(&str, f32)> = all_names.iter()
        .filter(|n| !ex.contains(n.as_str()) && n.as_str() != FIND_TOOLS_NAME)
        .filter_map(|n| index.get(n).map(|v| (n.as_str(), cosine(query_emb, v))))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(k).map(|(n, _)| n.to_string()).collect()
}

/// Cosine similarity over slices (same formula as `memory::cosine_similarity`,
/// kept slice-based so the selector API works on `&[f32]`). Returns 0.0 for
/// empty / mismatched-length / zero-norm inputs.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() { return 0.0; }
    let mut dot = 0.0f32; let mut na = 0.0f32; let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na  += a[i] * a[i];
        nb  += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 { return 0.0; }
    dot / (na.sqrt() * nb.sqrt())
}

/// Name → embedding cache for the enabled tool surface. Lazily kept current:
/// only newly-appeared tools are embedded; vanished tools are dropped — a
/// cheap no-op when the tool set is unchanged (the common case).
#[derive(Default)]
pub struct ToolIndex {
    vectors: HashMap<String, Vec<f32>>,
}

impl ToolIndex {
    /// Embed any tool not already cached and drop any that disappeared.
    /// Returns the number of tools embedded this call (0 when unchanged).
    pub async fn ensure_current(
        &mut self,
        registry: &ToolRegistry,
        memory:   &MemorySystem,
    ) -> usize {
        let names = registry.list_tools();
        let present: HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
        self.vectors.retain(|k, _| present.contains(k.as_str()));

        let mut added = 0usize;
        for name in &names {
            if self.vectors.contains_key(name) { continue; }
            let Some(tool) = registry.get(name) else { continue };
            // Embed the model-facing identity of the tool.
            let text = format!("{}: {}", tool.name(), tool.description());
            if let Some(v) = memory.embed(&text).await {
                self.vectors.insert(name.clone(), v);
                added += 1;
            }
            // Embedding unavailable → leave it out; the selector treats an
            // empty/partial index as "fall back to all tools".
        }
        added
    }

    pub fn is_empty(&self) -> bool { self.vectors.is_empty() }
    pub fn len(&self) -> usize { self.vectors.len() }
    pub fn get(&self, name: &str) -> Option<&Vec<f32>> { self.vectors.get(name) }
}

/// Collect tool names invoked in the last `max_turns` assistant turns
/// (most-recent first) for conversation **stickiness** — a tool used earlier
/// in a workflow stays available without needing to be re-retrieved. Reads
/// structured `tool_calls` on assistant messages (native tool-calling
/// models); Hermes/Qwen XML-style calls aren't captured here, but `find_tools`
/// covers re-loading those.
pub fn sticky_from_messages(messages: &[ChatMessage], max_turns: usize) -> Vec<String> {
    if max_turns == 0 { return Vec::new(); }
    let mut names: Vec<String> = Vec::new();
    let mut turns = 0usize;
    for m in messages.iter().rev() {
        if m.role != MessageRole::Assistant { continue; }
        turns += 1;
        if let Some(tcs) = &m.tool_calls {
            for tc in tcs {
                if !names.contains(&tc.name) { names.push(tc.name.clone()); }
            }
        }
        if turns >= max_turns { break; }
    }
    names
}

/// Glob match supporting a single trailing `*` (else exact).
fn glob_match(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None         => pattern == name,
    }
}

fn add_unique(name: &str, out: &mut Vec<String>, seen: &mut HashSet<String>) {
    if seen.insert(name.to_string()) {
        out.push(name.to_string());
    }
}

/// Compute the active tool set for a turn: **core ∪ semantic-topK ∪ sticky**.
/// `query_emb` is the embedding of the user's message. Pure + deterministic
/// given its inputs. Returns deduped tool names (the caller passes them as
/// `allowed_tool_names`). Order: core, then best semantic matches, then sticky.
pub fn select_tools(
    all_names: &[String],
    index:     &ToolIndex,
    query_emb: &[f32],
    cfg:       &ToolSelectionConfig,
    sticky:    &[String],
) -> Vec<String> {
    let present: HashSet<&str> = all_names.iter().map(|s| s.as_str()).collect();
    let mut out:  Vec<String>  = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // 1. Core always-on set (glob-matched against the live tool surface).
    for name in all_names {
        if cfg.core_tools.iter().any(|p| glob_match(p, name)) {
            add_unique(name, &mut out, &mut seen);
        }
    }

    // 2. Semantic top-K above the similarity threshold.
    let mut scored: Vec<(&str, f32)> = all_names.iter()
        .filter_map(|n| index.get(n).map(|v| (n.as_str(), cosine(query_emb, v))))
        .filter(|(_, s)| *s >= cfg.min_similarity)
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (name, _) in scored.into_iter().take(cfg.top_k) {
        add_unique(name, &mut out, &mut seen);
    }

    // 3. Conversation-sticky tools that are still in the surface.
    for name in sticky {
        if present.contains(name.as_str()) {
            add_unique(name, &mut out, &mut seen);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(core: &[&str], top_k: usize, min: f32) -> ToolSelectionConfig {
        ToolSelectionConfig {
            mode: "adaptive".into(),
            core_tools: core.iter().map(|s| s.to_string()).collect(),
            top_k,
            min_similarity: min,
            stickiness_turns: 6,
            expose_find_tools: true,
        }
    }

    fn idx(pairs: &[(&str, Vec<f32>)]) -> ToolIndex {
        let mut t = ToolIndex::default();
        for (n, v) in pairs { t.vectors.insert(n.to_string(), v.clone()); }
        t
    }

    #[test]
    fn find_tools_hint_counts_and_gating() {
        // Narrowed set (8 of 120) → a hint naming both counts + the contract.
        let h = find_tools_hint(8, 120).expect("should hint when more exist");
        assert!(h.contains("8 of 120"));
        assert!(h.contains("112 more"));
        assert!(h.contains(FIND_TOOLS_NAME));
        assert!(h.to_lowercase().contains("before telling the user"));
        // Nothing more to load → no hint (avoids a false "N more available").
        assert!(find_tools_hint(120, 120).is_none());
        assert!(find_tools_hint(120, 8).is_none()); // loaded > total → saturates to None
    }

    #[test]
    fn glob_matches_prefix_and_exact() {
        assert!(glob_match("memory_*", "memory_read"));
        assert!(glob_match("now", "now"));
        assert!(!glob_match("now", "now_playing"));
        assert!(!glob_match("memory_*", "wiki_read"));
    }

    #[test]
    fn core_always_included_even_if_unmatched() {
        let names = vec!["now".to_string(), "puppeteer_nav".to_string()];
        let index = idx(&[("now", vec![0.0, 1.0]), ("puppeteer_nav", vec![1.0, 0.0])]);
        // Query orthogonal to everything; min_similarity high so semantic adds nothing.
        let q = vec![0.0, 0.0];
        let got = select_tools(&names, &index, &q, &cfg(&["now"], 5, 0.99), &[]);
        assert_eq!(got, vec!["now".to_string()]);
    }

    #[test]
    fn semantic_topk_picks_most_similar() {
        let names = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let index = idx(&[
            ("a", vec![1.0, 0.0]),
            ("b", vec![0.9, 0.1]),
            ("c", vec![0.0, 1.0]),
        ]);
        let q = vec![1.0, 0.0];
        let got = select_tools(&names, &index, &q, &cfg(&[], 2, 0.1), &[]);
        assert!(got.contains(&"a".to_string()));
        assert!(got.contains(&"b".to_string()));
        assert!(!got.contains(&"c".to_string())); // orthogonal, below cut
    }

    #[test]
    fn sticky_included_when_present() {
        let names = vec!["a".to_string(), "b".to_string()];
        let index = idx(&[("a", vec![1.0, 0.0]), ("b", vec![0.0, 1.0])]);
        let q = vec![0.0, 0.0];
        let got = select_tools(&names, &index, &q, &cfg(&[], 0, 0.99), &["b".to_string(), "gone".to_string()]);
        assert_eq!(got, vec!["b".to_string()]); // 'gone' not in surface → dropped
    }
}
