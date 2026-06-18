// SPDX-License-Identifier: AGPL-3.0-or-later

// src/agent/wiki_hook.rs
//! Wiki context injection for [`AgentCore`].
//!
//! Each turn this hook:
//!
//! 1. Always loads the user's `profile.md` (~400-token cap).
//! 2. Loads the navigation `index.md` (~200-token cap).
//! 3. Keyword-matches the user input against page titles + tags +
//!    file paths, picks up to [`MAX_PAGES_PER_TURN`] pages, and loads
//!    their bodies (~300 tokens each).
//!
//! The total injection is capped at [`TOTAL_BUDGET_CHARS`] regardless of
//! how big the individual sources are. The hook never returns more than
//! the budget — it truncates from the tail of the page bodies.
//!
//! Output format is plain text with `## wiki:<path>` markers so the
//! chat UI (Slice H) can parse out which pages fed each turn and show
//! them as context pills.

use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::config::WikiAutoExtractConfig;
use crate::providers::ModelProvider;
use crate::wiki::{
    extract_wiki_ops, frontmatter, Provenance, WikiPath, WikiRegistry, WikiSystem,
};

/// ~4 chars per token is the conventional cheap heuristic. Real-world
/// English is 3.5–4.5, so this is conservative.
const CHARS_PER_TOKEN: usize = 4;

const PROFILE_BUDGET_TOK:  usize = 400;
const INDEX_BUDGET_TOK:    usize = 200;
const PAGE_BUDGET_TOK:     usize = 300;
const MAX_PAGES_PER_TURN:  usize = 2;
const TOTAL_BUDGET_TOK:    usize = 1200;

const PROFILE_BUDGET_CHARS: usize = PROFILE_BUDGET_TOK * CHARS_PER_TOKEN;
const INDEX_BUDGET_CHARS:   usize = INDEX_BUDGET_TOK   * CHARS_PER_TOKEN;
const PAGE_BUDGET_CHARS:    usize = PAGE_BUDGET_TOK    * CHARS_PER_TOKEN;
const TOTAL_BUDGET_CHARS:   usize = TOTAL_BUDGET_TOK   * CHARS_PER_TOKEN;

/// Result of building wiki context for one turn — the system-prompt
/// block plus the list of page paths actually loaded. The chat UI uses
/// `loaded_pages` to render context pills under the assistant message
/// so users can see exactly what the model had visibility into.
#[derive(Debug, Clone, Default)]
pub struct WikiContextResult {
    pub block: String,
    pub loaded_pages: Vec<String>,
}

impl WikiContextResult {
    pub fn is_empty(&self) -> bool { self.block.is_empty() }
}

/// Build a wiki context block for the system prompt.
///
/// Returns an empty [`WikiContextResult`] when the wiki has nothing to
/// inject (fresh user, all files empty, all reads failed). Wiki
/// failures are always non-fatal — the turn proceeds without wiki
/// context. `loaded_pages` includes `profile.md` and the navigation
/// `index.md` when they're loaded, plus every drill-in page selected
/// by keyword match — so the UI pills reflect everything the model
/// saw, not just the drill-ins.
pub async fn pre_hook(wiki: &Arc<WikiSystem>, input: &str) -> WikiContextResult {
    // Wiki reads are sync and fast — no actual awaits — but the function
    // is async to mirror `memory_hook::pre_hook` and keep call sites
    // uniform.
    let store = wiki.store();

    let profile = read_body(store.read_core_raw(), PROFILE_BUDGET_CHARS);
    let index   = read_body(store.read_index_raw(), INDEX_BUDGET_CHARS);
    let selected = select_relevant_pages(&store.list_pages().unwrap_or_default(), input);
    let mut page_blocks: Vec<(WikiPath, String)> = Vec::new();
    for path in &selected {
        match store.read_page(path) {
            Ok(page) => {
                let body = truncate_chars(&page.body, PAGE_BUDGET_CHARS);
                page_blocks.push((path.clone(), body));
            }
            Err(e) => warn!("wiki_hook: failed reading {}: {}", path.as_str(), e),
        }
    }

    if profile.is_empty() && index.is_empty() && page_blocks.is_empty() {
        return WikiContextResult::default();
    }

    let drill_in_paths: Vec<&str> = page_blocks.iter().map(|(p, _)| p.as_str()).collect();
    let mut block = String::new();
    block.push_str("\n\n[Wiki context for this turn");
    if !drill_in_paths.is_empty() {
        block.push_str(" — pages loaded: ");
        block.push_str(&drill_in_paths.join(", "));
    }
    block.push_str("]\n");

    let mut loaded_pages: Vec<String> = Vec::new();
    if !profile.is_empty() {
        block.push_str("\n## wiki:profile.md\n");
        block.push_str(profile.trim());
        block.push('\n');
        loaded_pages.push("profile.md".to_string());
    }
    if !index.is_empty() {
        block.push_str("\n## wiki:index.md\n");
        block.push_str(index.trim());
        block.push('\n');
        loaded_pages.push("index.md".to_string());
    }
    for (path, body) in &page_blocks {
        block.push_str(&format!("\n## wiki:{}\n", path.as_str()));
        block.push_str(body.trim());
        block.push('\n');
        loaded_pages.push(path.as_str().to_string());
    }

    debug!(
        "wiki_hook: profile={} chars, index={} chars, pages={}, total={} chars",
        profile.len(), index.len(), page_blocks.len(), block.len()
    );

    WikiContextResult {
        block: truncate_chars(&block, TOTAL_BUDGET_CHARS).to_string(),
        loaded_pages,
    }
}

// ── Selection ────────────────────────────────────────────────────────────────

/// Pick up to [`MAX_PAGES_PER_TURN`] pages whose path matches the most
/// stem-tokens in the input. Special navigation files (profile/index/
/// SCHEMA/log) are excluded because they're loaded separately or aren't
/// useful as drill-in pages.
fn select_relevant_pages(pages: &[WikiPath], input: &str) -> Vec<WikiPath> {
    let tokens = stem_tokens(input);
    if tokens.is_empty() { return Vec::new(); }
    let mut scored: Vec<(usize, &WikiPath)> = pages.iter()
        .filter(|p| !p.is_special())
        .map(|p| {
            let hay = p.as_str().to_lowercase();
            let score = tokens.iter().filter(|t| hay.contains(t.as_str())).count();
            (score, p)
        })
        .filter(|(s, _)| *s > 0)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().take(MAX_PAGES_PER_TURN).map(|(_, p)| p.clone()).collect()
}

/// Lowercase, strip punctuation, keep tokens of length ≥4 (skips
/// stopwords like "the", "and").
fn stem_tokens(input: &str) -> Vec<String> {
    input.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .filter(|w| w.len() >= 4)
        .map(|w| w.to_string())
        .collect()
}

// ── Body helpers ─────────────────────────────────────────────────────────────

/// Parse a wiki file's raw text, strip frontmatter, and trim to `budget`.
/// On error, returns empty string and logs a warning.
fn read_body(raw: crate::wiki::Result<String>, budget: usize) -> String {
    let raw = match raw {
        Ok(s) if s.trim().is_empty() => return String::new(),
        Ok(s) => s,
        Err(e) => {
            warn!("wiki_hook: read failed (non-fatal): {e}");
            return String::new();
        }
    };
    let body = frontmatter::parse(&raw)
        .map(|(_, body)| body)
        .unwrap_or(raw);
    truncate_chars(body.trim(), budget)
}

/// Char-safe truncate (won't split a multi-byte UTF-8 sequence).
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push_str("\n…\n");
    out
}

// ── Post-turn hook (Slice C) ─────────────────────────────────────────────────

/// Fire-and-forget post-turn extraction. Spawns a background task that
/// calls the LLM extractor and submits the resulting [`WikiOp`]s to the
/// user's wiki.
///
/// Behaviour is gated by [`WikiAutoExtractConfig::mode`]:
/// - `"off"` → no-op (this function does nothing).
/// - `"review"` → ops land as `pending` in the audit DB; user reviews
///   them on the Wiki page (Slice E). **Default**, per the
///   ChatGPT-memory-lessons mitigation: never silently write.
/// - `"auto"` → ops are applied immediately. Use when you trust the
///   extractor and want zero friction.
pub fn post_hook(
    registry: Arc<WikiRegistry>,
    provider: Arc<dyn ModelProvider>,
    user_id: String,
    conversation_id: String,
    turn_id: String,
    user_msg: String,
    assistant_msg: String,
    cfg: WikiAutoExtractConfig,
) {
    if cfg.mode.eq_ignore_ascii_case("off") {
        debug!("wiki post_hook: mode=off, skipping");
        return;
    }
    tokio::spawn(run_wiki_extraction(
        registry, provider, user_id, conversation_id, turn_id, user_msg, assistant_msg, cfg,
    ));
}

/// Awaitable core of [`post_hook`]: extract wiki ops from one turn and
/// submit/apply them. Split out so callers that must run extraction
/// *synchronously* (e.g. the memory benchmark, which has to finish ingesting
/// before it asks the question) can `await` it instead of fire-and-forget.
pub async fn run_wiki_extraction(
    registry: Arc<WikiRegistry>,
    provider: Arc<dyn ModelProvider>,
    user_id: String,
    conversation_id: String,
    turn_id: String,
    user_msg: String,
    assistant_msg: String,
    cfg: WikiAutoExtractConfig,
) {
    if cfg.mode.eq_ignore_ascii_case("off") {
        return;
    }
    let wiki = match registry.for_user(&user_id) {
        Ok(w) => w,
        Err(e) => {
            warn!("wiki post_hook: open failed for {user_id}: {e}");
            return;
        }
    };
    let existing = wiki.store().list_pages().unwrap_or_default();
    let ops = extract_wiki_ops(
        &provider,
        &user_msg,
        &assistant_msg,
        &existing,
        cfg.min_confidence,
        cfg.max_ops_per_turn,
    ).await;

    if ops.is_empty() {
        debug!("wiki post_hook: no ops extracted for {user_id}");
        return;
    }

    // `mode="auto"` applies everything. In `mode="review"`, a per-op
    // confidence tier auto-applies ops at/above `auto_apply_above` (when set)
    // and queues the rest for review — so the review queue shrinks to the
    // genuinely uncertain ops instead of gating every write.
    let auto_all = cfg.mode.eq_ignore_ascii_case("auto");
    let mut applied = 0usize;
    let mut pending = 0usize;
    for (op, confidence) in ops {
        let prov = Provenance::from_turn("extractor", &turn_id, &conversation_id);
        let auto = auto_all
            || cfg.auto_apply_above.is_some_and(|t| confidence >= t);
        let result = if auto {
            wiki.submit_and_apply_conf(op, prov, Some(confidence))
        } else {
            wiki.submit_op_conf(op, prov, Some(confidence))
        };
        match result {
            Ok(_) if auto => applied += 1,
            Ok(_) => pending += 1,
            Err(e) => warn!("wiki post_hook: submit failed: {e}"),
        }
    }
    if applied + pending > 0 {
        info!(
            "wiki post_hook: user='{}' mode='{}' auto_apply_above={:?} applied={} pending={}",
            user_id, cfg.mode, cfg.auto_apply_above, applied, pending
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki::{PageFrontmatter, Provenance, WikiOp, WikiPath, WikiSystem};
    use tempfile::tempdir;

    async fn wiki_with(pages: &[(&str, &str, &str)]) -> (tempfile::TempDir, Arc<WikiSystem>) {
        let dir = tempdir().unwrap();
        let wiki = Arc::new(WikiSystem::for_user(dir.path(), "u1").unwrap());
        // Replace profile with a known body.
        wiki.submit_and_apply(
            WikiOp::WritePage {
                path: WikiPath::parse("profile.md").unwrap(),
                frontmatter: PageFrontmatter::default(),
                body: "# Profile\n- name: Alex\n- timezone: AU/Melbourne\n".into(),
            },
            Provenance::user_ui("u1"),
        ).unwrap();
        for (path, title, body) in pages {
            let mut fm = PageFrontmatter::default();
            fm.title = Some(title.to_string());
            wiki.submit_and_apply(
                WikiOp::WritePage {
                    path: WikiPath::parse(path).unwrap(),
                    frontmatter: fm,
                    body: body.to_string(),
                },
                Provenance::user_ui("u1"),
            ).unwrap();
        }
        (dir, wiki)
    }

    #[tokio::test]
    async fn pre_hook_always_loads_profile_when_input_unmatched() {
        let (_dir, wiki) = wiki_with(&[]).await;
        let out = pre_hook(&wiki, "hello world").await;
        assert!(out.block.contains("[Wiki context for this turn"));
        assert!(out.block.contains("## wiki:profile.md"));
        assert!(out.block.contains("Alex"));
        assert!(out.loaded_pages.iter().any(|p| p == "profile.md"));
    }

    #[tokio::test]
    async fn pre_hook_picks_relevant_page_by_keyword() {
        let (_dir, wiki) = wiki_with(&[
            ("pages/pong-game.md", "Pong game project", "# Pong\nThe Pong game project notes.\n"),
            ("pages/recipes.md",   "Cooking recipes",   "# Recipes\nFavourite recipes.\n"),
        ]).await;
        let out = pre_hook(&wiki, "let's continue working on the pong-game project").await;
        assert!(out.block.contains("pages/pong-game.md"));
        assert!(!out.block.contains("pages/recipes.md"));
        assert!(out.loaded_pages.iter().any(|p| p == "pages/pong-game.md"));
    }

    #[tokio::test]
    async fn pre_hook_respects_budget() {
        let (_dir, wiki) = wiki_with(&[
            ("pages/big.md", "Big page", &"x".repeat(20_000)),
        ]).await;
        let out = pre_hook(&wiki, "big").await;
        assert!(out.block.chars().count() <= TOTAL_BUDGET_CHARS + 4 /* tail marker */);
    }

    #[tokio::test]
    async fn pre_hook_excludes_special_files_from_selection() {
        let (_dir, wiki) = wiki_with(&[]).await;
        // Input that matches "index" by keyword.
        let out = pre_hook(&wiki, "show me the index of all topics here").await;
        // Profile is always loaded; SCHEMA/index/log are not drill-in targets.
        // The "pages loaded:" line should not list any of them.
        if let Some(line) = out.block.lines().find(|l| l.contains("pages loaded:")) {
            assert!(!line.contains("SCHEMA.md"));
            assert!(!line.contains("log.md"));
        }
        // SCHEMA and log are not in loaded_pages either.
        assert!(!out.loaded_pages.iter().any(|p| p == "SCHEMA.md" || p == "log.md"));
    }

    #[tokio::test]
    async fn pre_hook_returns_empty_when_wiki_has_no_content() {
        let dir = tempdir().unwrap();
        let wiki = Arc::new(WikiSystem::for_user(dir.path(), "empty").unwrap());
        // Overwrite profile.md with literally nothing so the hook has nothing to surface.
        let profile = wiki.root().join("profile.md");
        std::fs::write(&profile, "").unwrap();
        // Also wipe index.md so it's not loaded either.
        let index = wiki.root().join("index.md");
        std::fs::write(&index, "").unwrap();
        let out = pre_hook(&wiki, "anything").await;
        assert!(out.is_empty());
        assert!(out.loaded_pages.is_empty());
    }

    #[test]
    fn stem_tokens_drops_short_words() {
        let toks = stem_tokens("the pong-game IS fun!");
        assert!(toks.iter().any(|t| t == "pong-game"));
        // "the", "is" filtered out by length.
        assert!(!toks.iter().any(|t| t == "the"));
        assert!(!toks.iter().any(|t| t == "is"));
    }

    #[test]
    fn truncate_is_char_safe() {
        let s = "héllo wörld";  // multi-byte chars
        let out = truncate_chars(s, 5);
        // Truncated at char boundary, not byte boundary.
        assert!(out.starts_with("héllo"));
    }

    // ── Slice C: extraction + review-queue pipeline ──────────────────────────
    //
    // We don't go through `post_hook` (which tokio::spawns) — instead we
    // exercise the same pieces (`extract_wiki_ops` + `wiki.submit_op`) the
    // spawned task uses, so failures point at the actual logic.

    use crate::providers::ModelProvider;
    use crate::types::{GenerationResponse, ProviderId, TokenUsage};
    use crate::wiki::{extract_wiki_ops, OpStatus};
    use async_trait::async_trait;

    /// Provider that returns a fixed string for every call. The wiki
    /// extractor will parse this as if it were the LLM's JSON response.
    struct CannedProvider(String);

    #[async_trait]
    impl ModelProvider for CannedProvider {
        fn name(&self) -> &str { "canned" }
        async fn generate(
            &self,
            _m: &[crate::types::ChatMessage],
            _o: &crate::types::GenerationOptions,
        ) -> Result<GenerationResponse, crate::MiraError> {
            Ok(GenerationResponse {
                content: self.0.clone(),
                tool_calls: None,
                reasoning: None,
                usage: TokenUsage::default(),
                provider_id: ProviderId::Local("canned".into()),
                model_name: "canned".into(),
                fallback: None,
            })
        }
        async fn health_check(&self) -> bool { true }
    }

    #[tokio::test]
    async fn extract_lands_pending_ops_in_review_queue() {
        let (_dir, wiki) = wiki_with(&[]).await;
        let canned = r##"{"wiki_ops":[
            {"kind":"log_entry","summary":"User mentioned working on Pong","confidence":0.85},
            {"kind":"write_page","path":"pages/projects/pong.md","title":"Pong project","body":"# Pong\nNotes from convo.\n","confidence":0.8}
        ]}"##;
        let provider: Arc<dyn ModelProvider> = Arc::new(CannedProvider(canned.to_string()));

        let existing = wiki.store().list_pages().unwrap_or_default();
        let ops = extract_wiki_ops(
            &provider, "I'm working on Pong", "Cool, what's the design?",
            &existing, 0.6, 3,
        ).await;
        assert_eq!(ops.len(), 2);

        // Submit (don't apply) — simulates mode="review".
        for (op, _conf) in ops {
            let prov = Provenance::from_turn("extractor", "turn-1", "conv-1");
            wiki.submit_op(op, prov).unwrap();
        }

        // Both land as Pending; the file should NOT exist yet.
        let pending = wiki.list_pending_ops().unwrap();
        assert_eq!(pending.len(), 2);
        assert!(!wiki.root().join("pages/projects/pong.md").exists());

        // Approve one — the file appears.
        let write_op_id = pending.iter()
            .find(|e| e.op.kind() == "write_page")
            .map(|e| e.op_id.clone())
            .unwrap();
        wiki.approve_op(&write_op_id, "u1").unwrap();
        assert!(wiki.root().join("pages/projects/pong.md").exists());

        // The remaining pending op stays pending.
        let still_pending = wiki.list_pending_ops().unwrap();
        assert_eq!(still_pending.len(), 1);
        assert_eq!(still_pending[0].op.kind(), "log_entry");
    }

    #[tokio::test]
    async fn extract_filters_low_confidence() {
        let (_dir, wiki) = wiki_with(&[]).await;
        let canned = r#"{"wiki_ops":[
            {"kind":"log_entry","summary":"weak signal","confidence":0.3},
            {"kind":"log_entry","summary":"strong signal","confidence":0.95}
        ]}"#;
        let provider: Arc<dyn ModelProvider> = Arc::new(CannedProvider(canned.to_string()));
        let existing = wiki.store().list_pages().unwrap_or_default();
        let ops = extract_wiki_ops(&provider, "x", "y", &existing, 0.6, 5).await;
        // Only the strong one survives.
        assert_eq!(ops.len(), 1);
    }

    #[tokio::test]
    async fn extract_caps_max_ops_per_turn() {
        let (_dir, wiki) = wiki_with(&[]).await;
        let canned = r#"{"wiki_ops":[
            {"kind":"log_entry","summary":"one","confidence":0.9},
            {"kind":"log_entry","summary":"two","confidence":0.9},
            {"kind":"log_entry","summary":"three","confidence":0.9},
            {"kind":"log_entry","summary":"four","confidence":0.9}
        ]}"#;
        let provider: Arc<dyn ModelProvider> = Arc::new(CannedProvider(canned.to_string()));
        let existing = wiki.store().list_pages().unwrap_or_default();
        let ops = extract_wiki_ops(&provider, "x", "y", &existing, 0.5, 2).await;
        assert_eq!(ops.len(), 2);
    }

    #[tokio::test]
    async fn auto_apply_mode_writes_immediately() {
        let (_dir, wiki) = wiki_with(&[]).await;
        let canned = r##"{"wiki_ops":[
            {"kind":"write_page","path":"pages/auto.md","title":"Auto","body":"# Auto\n","confidence":0.9}
        ]}"##;
        let provider: Arc<dyn ModelProvider> = Arc::new(CannedProvider(canned.to_string()));
        let existing = wiki.store().list_pages().unwrap_or_default();
        let ops = extract_wiki_ops(&provider, "x", "y", &existing, 0.6, 5).await;

        // Simulate mode="auto": submit-and-apply for each op.
        for (op, _conf) in ops {
            let prov = Provenance::from_turn("extractor", "turn-1", "conv-1");
            wiki.submit_and_apply(op, prov).unwrap();
        }

        // File present and op status flipped to Applied.
        assert!(wiki.root().join("pages/auto.md").exists());
        let recent = wiki.list_recent_ops(
            chrono::Utc::now() - chrono::Duration::hours(1), 10,
        ).unwrap();
        assert!(recent.iter().any(|e| e.status == OpStatus::Applied));
    }
}
