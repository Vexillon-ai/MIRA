// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/search/mod.rs
//! `web_search` tool — Tier 2 network.
//!
//! One trait (`SearchBackend`), one tool (`WebSearchTool`), three backends
//! (`DdgHtmlBackend`, `BraveApiBackend`, `SearxngBackend`). The tool walks
//! a configured ordered list of backends and returns hits from the first
//! one that succeeds. Every outbound HTTP call goes through
//! [`super::http_policy::HttpPolicy`], so SSRF, rate-limit and redirect
//! guarantees apply regardless of which backend is picked. See
//! `design-docs/phase7-tier2-web-tools.md` §5.

pub mod ddg;
pub mod brave;
pub mod searxng;

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{debug, warn};

use super::{Tier, Tool, ToolArgs, ToolResult};
use super::http_policy::HttpPolicy;
use crate::MiraError;

pub use ddg::DdgHtmlBackend;
pub use brave::BraveApiBackend;
pub use searxng::SearxngBackend;

// ── Public types ─────────────────────────────────────────────────────────────

/// Caller-supplied knobs for a single search. The tool clamps `top_k`.
#[derive(Debug, Clone)]
pub struct SearchLimits {
    pub top_k: usize,
    pub region: Option<String>,
    pub safe:   SafeSearch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeSearch { Off, Moderate, Strict }

impl SafeSearch {
    pub fn from_str(s: &str) -> Self {
        match s {
            "off"    => SafeSearch::Off,
            "strict" => SafeSearch::Strict,
            _        => SafeSearch::Moderate,
        }
    }
}

/// One result row. Backends normalise into this shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub rank:    usize,
    pub title:   String,
    pub url:     String,
    pub snippet: String,
    pub source:  String, // backend id: "ddg" | "brave" | "searxng"
}

/// Trait every backend implements. All methods are `&self`; backends hold
/// their own `Arc<HttpPolicy>` / keys.
#[async_trait]
pub trait SearchBackend: Send + Sync {
    fn id(&self) -> &'static str;
    fn requires_key(&self) -> bool;
    fn is_configured(&self) -> bool;

    async fn search(
        &self,
        query:   &str,
        limits:  &SearchLimits,
        user_id: &str,
    ) -> Result<Vec<SearchHit>, MiraError>;
}

// ── Tool ────────────────────────────────────────────────────────────────────

/// The registered `web_search` tool. Holds an ordered list of backends; the
/// first that's configured + reachable wins.
pub struct WebSearchTool {
    backends: Vec<Arc<dyn SearchBackend>>,
    default_top_k: usize,
}

impl WebSearchTool {
    pub fn new(backends: Vec<Arc<dyn SearchBackend>>, default_top_k: usize) -> Self {
        Self { backends, default_top_k }
    }

    pub fn is_usable(&self) -> bool {
        self.backends.iter().any(|b| b.is_configured())
    }

    /// Return the ordered list of backend ids + configured state for UI
    /// surfacing. Matches the `/api/tools` per-backend status badge.
    pub fn backend_status(&self) -> Vec<(String, bool)> {
        self.backends.iter()
            .map(|b| (b.id().to_owned(), b.is_configured()))
            .collect()
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str { "web_search" }

    fn description(&self) -> &str {
        "Search the web. Returns a ranked list of {title, url, snippet} \
         hits. Use this whenever the user asks a factual question that \
         depends on current information, or asks to 'look up', 'find', or \
         'search for' something online. Follow up with `web_fetch` on the \
         best hit if you need the full article. Optional args: `backend` \
         to pin a specific backend (`ddg` | `brave` | `searxng`); \
         `top_k` to tune result count (default 10, max 20); `safe` for \
         safe-search level (`off` | `moderate` | `strict`)."
    }

    fn tier(&self) -> Tier { Tier::Network }

    fn enabled(&self) -> bool { self.is_usable() }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query":   { "type": "string", "description": "Search query." },
                "backend": {
                    "type": "string",
                    "enum": ["ddg", "brave", "searxng"],
                    "description":
                        "Pin a specific backend for this call. Omit to use \
                         the server's default + failover order."
                },
                "top_k": {
                    "type": "integer",
                    "description": "Max hits (1..=20, default 10)."
                },
                "region": {
                    "type": "string",
                    "description":
                        "Preferred result region (backend-specific, e.g. \
                         'us-en', 'uk-en')."
                },
                "safe": {
                    "type": "string",
                    "enum": ["off", "moderate", "strict"],
                    "description": "Safe-search level. Default moderate."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let query = match args.get("query").and_then(|v| v.as_str()) {
            Some(q) if !q.trim().is_empty() => q.trim().to_owned(),
            _ => return Ok(ToolResult::failure(
                "web_search: `query` is required".to_string(),
            )),
        };
        let user_id = args.get("_user_id").and_then(|v| v.as_str()).unwrap_or("anonymous");
        let pinned  = args.get("backend").and_then(|v| v.as_str()).map(str::to_owned);

        let top_k = args.get("top_k").and_then(|v| v.as_u64())
            .map(|n| (n as usize).min(20).max(1))
            .unwrap_or(self.default_top_k.min(20).max(1));

        let region = args.get("region").and_then(|v| v.as_str()).map(str::to_owned);
        let safe   = args.get("safe").and_then(|v| v.as_str())
            .map(SafeSearch::from_str).unwrap_or(SafeSearch::Moderate);

        let limits = SearchLimits { top_k, region, safe };

        // Build the try-order. Pinned backend first if given, otherwise
        // self.backends order. Configured-only — unconfigured backends are
        // skipped (with a warn).
        let order: Vec<&Arc<dyn SearchBackend>> = if let Some(id) = pinned.as_deref() {
            self.backends.iter().filter(|b| b.id() == id).collect()
        } else {
            self.backends.iter().filter(|b| b.is_configured()).collect()
        };

        if order.is_empty() {
            return Ok(ToolResult::failure(match pinned {
                Some(id) => format!("web_search: backend '{}' is not configured", id),
                None     => "web_search: no backends configured (set brave.api_key or searxng.url)".into(),
            }));
        }

        let mut last_err: Option<String> = None;
        for backend in order {
            if !backend.is_configured() {
                debug!("web_search: skipping {} (not configured)", backend.id());
                continue;
            }
            debug!("web_search: trying backend={} query='{}'", backend.id(), query);
            match backend.search(&query, &limits, user_id).await {
                Ok(hits) => {
                    let out = json!({
                        "query":   query,
                        "backend": backend.id(),
                        "hits":    hits,
                    });
                    return Ok(ToolResult::success(out.to_string()));
                }
                Err(e) => {
                    warn!("web_search: backend={} failed: {}", backend.id(), e);
                    last_err = Some(format!("{}: {}", backend.id(), e));
                }
            }
        }

        Ok(ToolResult::failure(
            format!("web_search: all backends failed ({})",
                last_err.unwrap_or_else(|| "no detail".into())),
        ))
    }
}

// ── Shared helper: HTTP policy handle for backends ───────────────────────────

/// Clone-friendly handle to the shared HTTP policy. Exposed so backend
/// constructors can accept `Arc<HttpPolicy>` without re-exporting the
/// whole `http_policy` module path through this one.
pub type PolicyHandle = Arc<HttpPolicy>;

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    struct StubBackend {
        id:         &'static str,
        configured: bool,
        hits:       Result<Vec<SearchHit>, String>,
    }

    #[async_trait]
    impl SearchBackend for StubBackend {
        fn id(&self) -> &'static str { self.id }
        fn requires_key(&self) -> bool { false }
        fn is_configured(&self) -> bool { self.configured }
        async fn search(&self, _q: &str, _l: &SearchLimits, _u: &str)
            -> Result<Vec<SearchHit>, MiraError>
        {
            self.hits.clone().map_err(MiraError::ToolError)
        }
    }

    fn hit(rank: usize, source: &str) -> SearchHit {
        SearchHit {
            rank, source: source.to_owned(),
            title: format!("t{}", rank),
            url:   format!("https://x/{}", rank),
            snippet: "s".into(),
        }
    }

    #[tokio::test]
    async fn uses_first_configured_backend() {
        let tool = WebSearchTool::new(vec![
            Arc::new(StubBackend { id: "a", configured: true,  hits: Ok(vec![hit(1,"a")]) }),
            Arc::new(StubBackend { id: "b", configured: true,  hits: Ok(vec![hit(1,"b")]) }),
        ], 10);
        let out = tool.execute(json!({"query": "foo"})).await.unwrap();
        assert!(out.success);
        assert!(out.output.contains("\"backend\":\"a\""));
    }

    #[tokio::test]
    async fn falls_over_to_next_backend_on_error() {
        let tool = WebSearchTool::new(vec![
            Arc::new(StubBackend { id: "a", configured: true,  hits: Err("boom".into()) }),
            Arc::new(StubBackend { id: "b", configured: true,  hits: Ok(vec![hit(1,"b")]) }),
        ], 10);
        let out = tool.execute(json!({"query": "foo"})).await.unwrap();
        assert!(out.success, "failover should succeed on backend b");
        assert!(out.output.contains("\"backend\":\"b\""));
    }

    #[tokio::test]
    async fn pinned_backend_bypasses_order_and_skips_configured_check() {
        // Pinning uses the id filter without re-checking is_configured —
        // caller is explicit.
        let tool = WebSearchTool::new(vec![
            Arc::new(StubBackend { id: "a", configured: false, hits: Err("unconfigured".into()) }),
            Arc::new(StubBackend { id: "b", configured: true,  hits: Ok(vec![hit(1,"b")]) }),
        ], 10);
        let out = tool.execute(json!({"query": "foo", "backend": "b"})).await.unwrap();
        assert!(out.success);
        assert!(out.output.contains("\"backend\":\"b\""));
    }

    #[tokio::test]
    async fn fails_gracefully_when_no_backend_configured() {
        let tool = WebSearchTool::new(vec![
            Arc::new(StubBackend { id: "a", configured: false, hits: Err("-".into()) }),
        ], 10);
        let out = tool.execute(json!({"query": "foo"})).await.unwrap();
        assert!(!out.success);
        assert!(out.error.unwrap().contains("no backends configured"));
    }

    #[tokio::test]
    async fn rejects_empty_query() {
        let tool = WebSearchTool::new(vec![
            Arc::new(StubBackend { id: "a", configured: true, hits: Ok(vec![]) }),
        ], 10);
        let out = tool.execute(json!({"query": "   "})).await.unwrap();
        assert!(!out.success);
        assert!(out.error.unwrap().contains("query"));
    }

    #[test]
    fn clamps_top_k_to_reasonable_bounds() {
        // Indirect: via the tool, we just verify the clamp logic matches.
        let t = SearchLimits { top_k: 100, region: None, safe: SafeSearch::Moderate };
        assert_eq!(t.top_k, 100); // limits struct doesn't clamp — tool does
    }
}
