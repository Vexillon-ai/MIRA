// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/search/ddg.rs
//! DuckDuckGo HTML backend — no API key required.
//!
//! Endpoint: `https://html.duckduckgo.com/html/?q=<query>&kl=<region>`.
//! Parse the markup (CSS selectors) for result blocks. DDG is the
//! always-on fallback — `is_configured()` returns `true` unconditionally.
//!
//! Tradeoff: the parser is brittle, because DDG owns the markup. We keep
//! the selector set small and fall back to whatever we can extract. When
//! DDG changes layout, this file is where we patch it.

use async_trait::async_trait;
use scraper::{Html, Selector};
use url::Url;

use super::{PolicyHandle, SafeSearch, SearchBackend, SearchHit, SearchLimits};
use crate::MiraError;

pub struct DdgHtmlBackend {
    policy: PolicyHandle,
}

impl DdgHtmlBackend {
    pub fn new(policy: PolicyHandle) -> Self { Self { policy } }

    fn build_url(query: &str, limits: &SearchLimits) -> String {
        let mut url = Url::parse("https://html.duckduckgo.com/html/").unwrap();
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("q", query);
            if let Some(ref r) = limits.region {
                q.append_pair("kl", r);
            }
            // Safe-search: 1=moderate (DDG default), -1=off, 2=strict.
            let p = match limits.safe {
                SafeSearch::Off      => "-2",
                SafeSearch::Moderate => "-1",
                SafeSearch::Strict   => "1",
            };
            q.append_pair("kp", p);
        }
        url.to_string()
    }
}

#[async_trait]
impl SearchBackend for DdgHtmlBackend {
    fn id(&self) -> &'static str { "ddg" }
    fn requires_key(&self) -> bool { false }
    fn is_configured(&self) -> bool { true }

    async fn search(
        &self,
        query:   &str,
        limits:  &SearchLimits,
        user_id: &str,
    ) -> Result<Vec<SearchHit>, MiraError> {
        let url = Self::build_url(query, limits);
        let resp = self.policy.get_for_search(&url, user_id, &[])
            .await.map_err(|e| MiraError::ToolError(format!("ddg: {}", e)))?;

        if !(200..300).contains(&resp.status) {
            return Err(MiraError::ToolError(
                format!("ddg: http {} from {}", resp.status, resp.final_url),
            ));
        }

        let html = String::from_utf8_lossy(&resp.body);
        Ok(parse_ddg(&html, limits.top_k))
    }
}

// ── Parsing ──────────────────────────────────────────────────────────────────

fn parse_ddg(html: &str, top_k: usize) -> Vec<SearchHit> {
    let doc = Html::parse_document(html);

    // Each result lives in a `.result` block. Inside:
    //   .result__a                 → title + (possibly wrapped) href
    //   .result__snippet           → snippet text
    //   .result__url               → visible url (may be nicer than the href,
    //                                but href is authoritative after unwrap)
    let result_sel  = Selector::parse(".result").unwrap();
    let anchor_sel  = Selector::parse(".result__a").unwrap();
    let snippet_sel = Selector::parse(".result__snippet").unwrap();

    let mut out = Vec::new();
    for (i, block) in doc.select(&result_sel).enumerate() {
        if out.len() >= top_k { break; }

        let Some(anchor) = block.select(&anchor_sel).next() else { continue; };
        let title = anchor.text().collect::<String>().trim().to_owned();
        let href  = anchor.value().attr("href").unwrap_or("").to_owned();
        let url   = unwrap_ddg_redirect(&href).unwrap_or(href);
        let snippet = block.select(&snippet_sel).next()
            .map(|s| s.text().collect::<String>().trim().to_owned())
            .unwrap_or_default();

        if title.is_empty() || url.is_empty() { continue; }

        out.push(SearchHit {
            rank:    i + 1,
            title,
            url,
            snippet,
            source:  "ddg".into(),
        });
    }
    out
}

/// DDG wraps result URLs in a `//duckduckgo.com/l/?uddg=<encoded>&rut=...`
/// tracking redirect. Extract the `uddg` param to get the real target.
fn unwrap_ddg_redirect(href: &str) -> Option<String> {
    // Normalise protocol-relative to https so Url::parse accepts it.
    let normalised = if let Some(rest) = href.strip_prefix("//") {
        format!("https://{}", rest)
    } else {
        href.to_owned()
    };
    let parsed = Url::parse(&normalised).ok()?;
    // Only unwrap for duckduckgo.com redirect hosts; leave other URLs alone.
    let host = parsed.host_str()?.to_ascii_lowercase();
    if !host.ends_with("duckduckgo.com") { return Some(normalised); }
    let uddg = parsed.query_pairs()
        .find(|(k, _)| k == "uddg")
        .map(|(_, v)| v.into_owned())?;
    // uddg is already URL-decoded by query_pairs.
    if uddg.is_empty() { None } else { Some(uddg) }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_ddg_results() {
        let html = r##"
            <div class="result">
              <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Farticle&rut=abc">Example Article</a>
              <a class="result__snippet">This is an example snippet.</a>
            </div>
            <div class="result">
              <a class="result__a" href="https://rust-lang.org/">Rust Lang</a>
              <div class="result__snippet">Home of the Rust language.</div>
            </div>
        "##;
        let hits = parse_ddg(html, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].rank, 1);
        assert_eq!(hits[0].title, "Example Article");
        assert_eq!(hits[0].url, "https://example.com/article");
        assert_eq!(hits[0].snippet, "This is an example snippet.");
        assert_eq!(hits[1].url, "https://rust-lang.org/");
        assert_eq!(hits[1].source, "ddg");
    }

    #[test]
    fn respects_top_k() {
        let mut html = String::new();
        for i in 0..5 {
            html.push_str(&format!(
                r##"<div class="result"><a class="result__a" href="https://x/{i}">T{i}</a><div class="result__snippet">s</div></div>"##
            ));
        }
        assert_eq!(parse_ddg(&html, 3).len(), 3);
    }

    #[test]
    fn skips_malformed_blocks() {
        let html = r##"
            <div class="result"></div>
            <div class="result"><a class="result__a" href="">NoUrl</a></div>
            <div class="result"><a class="result__a" href="https://ok/">Ok</a></div>
        "##;
        let hits = parse_ddg(html, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Ok");
    }

    #[test]
    fn unwrap_ddg_redirect_handles_variants() {
        // protocol-relative DDG redirect
        assert_eq!(
            unwrap_ddg_redirect("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com&rut=x").as_deref(),
            Some("https://example.com"),
        );
        // absolute DDG redirect
        assert_eq!(
            unwrap_ddg_redirect("https://duckduckgo.com/l/?uddg=https%3A%2F%2Fa.b%2Fc").as_deref(),
            Some("https://a.b/c"),
        );
        // non-DDG urls pass through unchanged
        assert_eq!(
            unwrap_ddg_redirect("https://other.com/page").as_deref(),
            Some("https://other.com/page"),
        );
    }

    #[test]
    fn build_url_sets_query_and_region_and_safe() {
        let u = DdgHtmlBackend::build_url("hello world", &SearchLimits {
            top_k: 10, region: Some("us-en".into()), safe: SafeSearch::Strict,
        });
        assert!(u.contains("q=hello+world") || u.contains("q=hello%20world"));
        assert!(u.contains("kl=us-en"));
        assert!(u.contains("kp=1"));
    }
}
