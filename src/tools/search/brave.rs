// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/search/brave.rs
//! Brave Search API backend.
//!
//! Endpoint: `https://api.search.brave.com/res/v1/web/search`.
//! Auth: `X-Subscription-Token: <key>` header. Free tier = 2000 req/mo.
//! Docs: <https://api.search.brave.com/app/documentation/web-search/get-started>

use async_trait::async_trait;
use serde::Deserialize;
use url::Url;

use super::{PolicyHandle, SafeSearch, SearchBackend, SearchHit, SearchLimits};
use crate::MiraError;

pub struct BraveApiBackend {
    policy:  PolicyHandle,
    api_key: Option<String>,
}

impl BraveApiBackend {
    /// Construct from an explicit key. If `None`, also reads
    /// `BRAVE_SEARCH_API_KEY` from the environment.
    pub fn new(policy: PolicyHandle, api_key: Option<String>) -> Self {
        let api_key = api_key
            .filter(|s| !s.trim().is_empty())
            .or_else(|| std::env::var("BRAVE_SEARCH_API_KEY").ok())
            .filter(|s| !s.trim().is_empty());
        Self { policy, api_key }
    }

    fn build_url(query: &str, limits: &SearchLimits) -> String {
        let mut url = Url::parse("https://api.search.brave.com/res/v1/web/search").unwrap();
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("q", query);
            q.append_pair("count", &limits.top_k.to_string());
            if let Some(ref r) = limits.region {
                q.append_pair("country", r);
            }
            // `safesearch=off|moderate|strict`
            let s = match limits.safe {
                SafeSearch::Off      => "off",
                SafeSearch::Moderate => "moderate",
                SafeSearch::Strict   => "strict",
            };
            q.append_pair("safesearch", s);
        }
        url.to_string()
    }
}

#[async_trait]
impl SearchBackend for BraveApiBackend {
    fn id(&self) -> &'static str { "brave" }
    fn requires_key(&self) -> bool { true }
    fn is_configured(&self) -> bool { self.api_key.is_some() }

    async fn search(
        &self,
        query:   &str,
        limits:  &SearchLimits,
        user_id: &str,
    ) -> Result<Vec<SearchHit>, MiraError> {
        let Some(ref key) = self.api_key else {
            return Err(MiraError::ToolError("brave: api_key not configured".into()));
        };
        let url = Self::build_url(query, limits);

        let resp = self.policy.get_for_search(&url, user_id, &[
            ("X-Subscription-Token", key.as_str()),
            ("Accept",               "application/json"),
        ]).await.map_err(|e| MiraError::ToolError(format!("brave: {}", e)))?;

        if !(200..300).contains(&resp.status) {
            return Err(MiraError::ToolError(
                format!("brave: http {} from {}", resp.status, resp.final_url),
            ));
        }

        let text = String::from_utf8_lossy(&resp.body);
        parse_brave(&text, limits.top_k)
    }
}

// ── Parsing ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct BraveResponse {
    web: Option<BraveWeb>,
}

#[derive(Deserialize)]
struct BraveWeb {
    results: Option<Vec<BraveResult>>,
}

#[derive(Deserialize)]
struct BraveResult {
    title:       Option<String>,
    url:         Option<String>,
    description: Option<String>,
}

fn parse_brave(body: &str, top_k: usize) -> Result<Vec<SearchHit>, MiraError> {
    let parsed: BraveResponse = serde_json::from_str(body)
        .map_err(|e| MiraError::ToolError(format!("brave: invalid json: {}", e)))?;
    let Some(web) = parsed.web else { return Ok(vec![]); };
    let Some(results) = web.results else { return Ok(vec![]); };

    let mut hits = Vec::new();
    for (i, r) in results.into_iter().enumerate() {
        if hits.len() >= top_k { break; }
        let (title, url) = match (r.title, r.url) {
            (Some(t), Some(u)) if !t.trim().is_empty() && !u.trim().is_empty() => (t, u),
            _ => continue,
        };
        hits.push(SearchHit {
            rank:    i + 1,
            title,
            url,
            snippet: strip_html(&r.description.unwrap_or_default()),
            source:  "brave".into(),
        });
    }
    Ok(hits)
}

/// Brave embeds `<strong>` tags in snippets to highlight matches — strip
/// them so the model gets plain text.
fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_brave_json() {
        let body = r#"{
            "web": {
                "results": [
                    {
                        "title": "Rust",
                        "url":   "https://rust-lang.org",
                        "description": "A language empowering <strong>everyone</strong>."
                    },
                    {
                        "title": "Crates.io",
                        "url":   "https://crates.io",
                        "description": "The Rust package registry."
                    }
                ]
            }
        }"#;
        let hits = parse_brave(body, 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].rank, 1);
        assert_eq!(hits[0].title, "Rust");
        assert_eq!(hits[0].url, "https://rust-lang.org");
        assert_eq!(hits[0].snippet, "A language empowering everyone.");
        assert_eq!(hits[1].source, "brave");
    }

    #[test]
    fn handles_missing_fields_gracefully() {
        let body = r#"{"web": {"results": [
            {"title": "ok", "url": "https://x"},
            {"title": "nope"},
            {"url":   "https://no-title"}
        ]}}"#;
        let hits = parse_brave(body, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "ok");
    }

    #[test]
    fn empty_response_returns_empty_vec() {
        assert!(parse_brave(r#"{}"#, 10).unwrap().is_empty());
        assert!(parse_brave(r#"{"web":{}}"#, 10).unwrap().is_empty());
    }

    #[test]
    fn is_configured_reflects_key_presence() {
        use std::sync::Arc;
        use crate::tools::http_policy::{HttpPolicy, HttpPolicyConfig};
        let policy = Arc::new(HttpPolicy::new(HttpPolicyConfig::default()));
        let no_key = BraveApiBackend::new(Arc::clone(&policy), None);
        // Note: picks up env if set. In CI/dev this is typically unset.
        if std::env::var("BRAVE_SEARCH_API_KEY").is_err() {
            assert!(!no_key.is_configured());
        }
        let with_key = BraveApiBackend::new(policy, Some("k".into()));
        assert!(with_key.is_configured());
    }

    #[test]
    fn build_url_includes_count_and_safesearch() {
        let u = BraveApiBackend::build_url("foo bar", &SearchLimits {
            top_k: 5, region: Some("US".into()), safe: SafeSearch::Off,
        });
        assert!(u.contains("count=5"));
        assert!(u.contains("country=US"));
        assert!(u.contains("safesearch=off"));
    }
}
