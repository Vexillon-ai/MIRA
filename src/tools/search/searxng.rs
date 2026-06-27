// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/search/searxng.rs
//! SearXNG backend — hits a user-operated SearXNG instance.
//!
//! Endpoint: `<base_url>/search?q=<query>&format=json`.
//! Typical deployment is a private LAN URL; the HTTP policy's
//! `searxng_exception` (auto-derived from `searxng.url` by the gateway
//! builder) relaxes the private-IP block for that single host+port.

use async_trait::async_trait;
use serde::Deserialize;
use url::Url;

use super::{PolicyHandle, SafeSearch, SearchBackend, SearchHit, SearchLimits};
use crate::MiraError;

pub struct SearxngBackend {
    policy:   PolicyHandle,
    base_url: Option<String>,
}

impl SearxngBackend {
    pub fn new(policy: PolicyHandle, base_url: Option<String>) -> Self {
        let base_url = base_url
            .map(|s| s.trim().trim_end_matches('/').to_owned())
            .filter(|s| !s.is_empty());
        Self { policy, base_url }
    }

    fn build_url(base: &str, query: &str, limits: &SearchLimits) -> Result<String, MiraError> {
        let mut url = Url::parse(&format!("{}/search", base))
            .map_err(|e| MiraError::ToolError(format!("searxng: invalid base url: {}", e)))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("q", query);
            q.append_pair("format", "json");
            // SearXNG honours `safesearch=0|1|2`.
            let s = match limits.safe {
                SafeSearch::Off      => "0",
                SafeSearch::Moderate => "1",
                SafeSearch::Strict   => "2",
            };
            q.append_pair("safesearch", s);
            if let Some(ref r) = limits.region {
                q.append_pair("language", r);
            }
        }
        Ok(url.to_string())
    }
}

#[async_trait]
impl SearchBackend for SearxngBackend {
    fn id(&self) -> &'static str { "searxng" }
    fn requires_key(&self) -> bool { false }
    fn is_configured(&self) -> bool { self.base_url.is_some() }

    async fn search(
        &self,
        query:   &str,
        limits:  &SearchLimits,
        user_id: &str,
    ) -> Result<Vec<SearchHit>, MiraError> {
        let Some(ref base) = self.base_url else {
            return Err(MiraError::ToolError("searxng: base url not configured".into()));
        };
        let url = Self::build_url(base, query, limits)?;

        let resp = self.policy.get_for_search(&url, user_id, &[
            ("Accept", "application/json"),
        ]).await.map_err(|e| MiraError::ToolError(format!("searxng: {}", e)))?;

        if !(200..300).contains(&resp.status) {
            return Err(MiraError::ToolError(
                format!("searxng: http {} from {}", resp.status, resp.final_url),
            ));
        }

        let body = String::from_utf8_lossy(&resp.body);
        parse_searxng(&body, limits.top_k)
    }
}

// ── Parsing ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SearxngResponse {
    results: Option<Vec<SearxngResult>>,
}

#[derive(Deserialize)]
struct SearxngResult {
    title:   Option<String>,
    url:     Option<String>,
    content: Option<String>,
}

fn parse_searxng(body: &str, top_k: usize) -> Result<Vec<SearchHit>, MiraError> {
    let parsed: SearxngResponse = serde_json::from_str(body)
        .map_err(|e| MiraError::ToolError(format!("searxng: invalid json: {}", e)))?;
    let Some(results) = parsed.results else { return Ok(vec![]); };

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
            snippet: r.content.unwrap_or_default(),
            source:  "searxng".into(),
        });
    }
    Ok(hits)
}

/// Extract the `host:port` tuple from a base URL. Used by the gateway
/// builder to auto-whitelist the SearXNG LAN address in the HTTP policy.
///
/// Returns `None` for URLs without a host or with an unknown-default port.
pub fn extract_host_port(base_url: &str) -> Option<(String, u16)> {
    let url = Url::parse(base_url).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let port = url.port_or_known_default()?;
    Some((host, port))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_searxng_json() {
        let body = r#"{"results": [
            {"title": "Rust", "url": "https://rust-lang.org", "content": "Systems lang"},
            {"title": "Cargo", "url": "https://doc.rust-lang.org/cargo/"}
        ]}"#;
        let hits = parse_searxng(body, 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Rust");
        assert_eq!(hits[0].snippet, "Systems lang");
        assert_eq!(hits[1].snippet, "");
        assert_eq!(hits[1].source, "searxng");
    }

    #[test]
    fn skips_rows_missing_title_or_url() {
        let body = r#"{"results": [
            {"title": "ok", "url": "https://x"},
            {"url":   "https://no-title"},
            {"title": "no-url"}
        ]}"#;
        let hits = parse_searxng(body, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "ok");
    }

    #[test]
    fn extract_host_port_covers_common_cases() {
        assert_eq!(extract_host_port("http://searxng.example.com:8080").as_ref().map(|(h,p)| (h.as_str(), *p)),
                   Some(("searxng.example.com", 8080)));
        assert_eq!(extract_host_port("http://192.168.1.5:8888").as_ref().map(|(h,p)| (h.as_str(), *p)),
                   Some(("192.168.1.5", 8888)));
        // Default ports are inferred from scheme.
        assert_eq!(extract_host_port("https://sx.example.com").as_ref().map(|(h,p)| (h.as_str(), *p)),
                   Some(("sx.example.com", 443)));
        assert_eq!(extract_host_port("not a url"), None);
    }

    #[test]
    fn build_url_appends_json_format() {
        let u = SearxngBackend::build_url(
            "http://x/", "hello",
            &SearchLimits { top_k: 5, region: Some("en".into()), safe: SafeSearch::Strict },
        ).unwrap();
        assert!(u.contains("/search?"));
        assert!(u.contains("format=json"));
        assert!(u.contains("safesearch=2"));
        assert!(u.contains("language=en"));
    }

    #[test]
    fn is_configured_reflects_url_presence() {
        use std::sync::Arc;
        use crate::tools::http_policy::{HttpPolicy, HttpPolicyConfig};
        let policy = Arc::new(HttpPolicy::new(HttpPolicyConfig::default()));
        assert!(!SearxngBackend::new(Arc::clone(&policy), None).is_configured());
        assert!(!SearxngBackend::new(Arc::clone(&policy), Some("   ".into())).is_configured());
        assert!( SearxngBackend::new(policy, Some("http://x".into())).is_configured());
    }
}
