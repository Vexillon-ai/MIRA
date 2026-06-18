// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/url_preview.rs
//! `url_preview` — Tier 2 network tool.
//!
//! Cheaper cousin of `web_fetch`: pulls `<title>`, `<meta description>` and
//! OpenGraph tags from a URL without committing to a full-body fetch. The
//! body cap is tight (128 KB by default) because OG tags live near the top
//! of the HTML. See `design-docs/phase7-tier2-web-tools.md` §4.

use std::sync::Arc;

use async_trait::async_trait;
use scraper::{Html, Selector};
use serde_json::{json, Value};

use super::{Tier, Tool, ToolArgs, ToolResult};
use super::http_policy::{HttpPolicy, PolicyError};
use crate::MiraError;

pub struct UrlPreviewTool {
    policy: Arc<HttpPolicy>,
}

impl UrlPreviewTool {
    pub fn new(policy: Arc<HttpPolicy>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for UrlPreviewTool {
    fn name(&self) -> &str { "url_preview" }

    fn description(&self) -> &str {
        "Inspect the <title>, description and OpenGraph metadata of a URL \
         without downloading the full page. Ideal when the user pastes a \
         link and you want a one-line card about it before (or instead of) \
         a full web_fetch. Cheaper and faster than web_fetch; returns only \
         head-level metadata."
    }

    fn tier(&self) -> Tier { Tier::Network }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["url"],
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Absolute http(s) URL."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let url = match args.get("url").and_then(|v| v.as_str()) {
            Some(u) if !u.trim().is_empty() => u.trim().to_owned(),
            _ => return Ok(ToolResult::failure(
                "url_preview: `url` is required".to_string(),
            )),
        };
        let user_id = args.get("_user_id").and_then(|v| v.as_str()).unwrap_or("anonymous");

        let resp = match self.policy.get(&url, user_id).await {
            Ok(r)  => r,
            Err(e) => return Ok(ToolResult::failure(format_policy_err(&e))),
        };

        if !(200..300).contains(&resp.status) {
            return Ok(ToolResult::failure(
                format!("url_preview: http {} for {}", resp.status, resp.final_url),
            ));
        }

        let ct = resp.content_type.as_deref().unwrap_or("").to_ascii_lowercase();
        if !ct.contains("html") && !ct.is_empty() {
            return Ok(ToolResult::failure(
                format!("url_preview: content-type '{}' is not HTML", ct),
            ));
        }

        let body = String::from_utf8_lossy(&resp.body);
        let meta = extract_meta(&body);

        let out = json!({
            "url":         url,
            "final_url":   resp.final_url,
            "title":       meta.title,
            "description": meta.description,
            "site_name":   meta.site_name,
            "image":       meta.image,
            "published":   meta.published,
        });
        Ok(ToolResult::success(out.to_string()))
    }
}

// ── Parsing ──────────────────────────────────────────────────────────────────

#[derive(Debug, Default, PartialEq)]
struct Preview {
    title:       Option<String>,
    description: Option<String>,
    site_name:   Option<String>,
    image:       Option<String>,
    published:   Option<String>,
}

fn extract_meta(html: &str) -> Preview {
    let doc = Html::parse_document(html);
    let mut p = Preview::default();

    // OpenGraph takes precedence; fall back to standard tags.
    p.title     = meta_prop(&doc, "og:title").or_else(|| title_tag(&doc));
    p.description = meta_prop(&doc, "og:description")
        .or_else(|| meta_name(&doc, "description"));
    p.site_name = meta_prop(&doc, "og:site_name");
    p.image     = meta_prop(&doc, "og:image");
    p.published = meta_prop(&doc, "article:published_time")
        .or_else(|| meta_name(&doc, "date"));
    p
}

fn meta_prop(doc: &Html, prop: &str) -> Option<String> {
    let sel_str = format!("meta[property=\"{}\"]", prop);
    let sel = Selector::parse(&sel_str).ok()?;
    doc.select(&sel)
        .find_map(|el| el.value().attr("content").map(|s| s.trim().to_owned()))
        .filter(|s| !s.is_empty())
}

fn meta_name(doc: &Html, name: &str) -> Option<String> {
    let sel_str = format!("meta[name=\"{}\"]", name);
    let sel = Selector::parse(&sel_str).ok()?;
    doc.select(&sel)
        .find_map(|el| el.value().attr("content").map(|s| s.trim().to_owned()))
        .filter(|s| !s.is_empty())
}

fn title_tag(doc: &Html) -> Option<String> {
    let sel = Selector::parse("title").ok()?;
    doc.select(&sel)
        .next()
        .map(|el| el.text().collect::<String>().trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn format_policy_err(e: &PolicyError) -> String {
    match e {
        PolicyError::BlockedHost { reason, host } =>
            format!("url_preview: blocked host '{}' ({})", host, reason),
        PolicyError::BlockedScheme { scheme } =>
            format!("url_preview: blocked scheme '{}' — only http/https", scheme),
        PolicyError::DenylistedDomain { host } =>
            format!("url_preview: domain '{}' is on the admin denylist", host),
        PolicyError::AllowlistOnly { host } =>
            format!("url_preview: domain '{}' is not on the admin allowlist", host),
        PolicyError::RateLimited { retry_after_ms, scope } =>
            format!("url_preview: rate_limited ({}) — retry after {}ms", scope, retry_after_ms),
        PolicyError::TooLarge { limit, observed } =>
            format!("url_preview: response too large ({} > {} bytes)", observed, limit),
        PolicyError::Timeout                 => "url_preview: request timed out".into(),
        PolicyError::TooManyRedirects        => "url_preview: too many redirects".into(),
        PolicyError::Http { status, url }    => format!("url_preview: http {} for {}", status, url),
        PolicyError::DnsResolution { host, detail } =>
            format!("url_preview: dns resolution failed for '{}': {}", host, detail),
        PolicyError::InvalidUrl(s)           => format!("url_preview: invalid url '{}'", s),
        PolicyError::Transport(s)            => format!("url_preview: transport error: {}", s),
        PolicyError::PolicyDenied { rule, reason } =>
            format!("url_preview: policy/{} denied: {}", rule, reason),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_og_tags_over_plain_head() {
        let html = r#"
            <html><head>
              <title>Fallback title</title>
              <meta name="description" content="Fallback desc">
              <meta property="og:title" content="OG Title">
              <meta property="og:description" content="OG Desc">
              <meta property="og:site_name" content="Example">
              <meta property="og:image" content="https://example.com/og.png">
              <meta property="article:published_time" content="2024-07-12T00:00:00Z">
            </head><body></body></html>
        "#;
        let p = extract_meta(html);
        assert_eq!(p.title.as_deref(), Some("OG Title"));
        assert_eq!(p.description.as_deref(), Some("OG Desc"));
        assert_eq!(p.site_name.as_deref(), Some("Example"));
        assert_eq!(p.image.as_deref(), Some("https://example.com/og.png"));
        assert_eq!(p.published.as_deref(), Some("2024-07-12T00:00:00Z"));
    }

    #[test]
    fn falls_back_to_title_and_description_meta() {
        let html = r#"
            <html><head>
              <title>Just a page</title>
              <meta name="description" content="A description">
            </head><body></body></html>
        "#;
        let p = extract_meta(html);
        assert_eq!(p.title.as_deref(), Some("Just a page"));
        assert_eq!(p.description.as_deref(), Some("A description"));
        assert!(p.image.is_none());
    }

    #[test]
    fn returns_all_none_for_empty_doc() {
        let p = extract_meta("<html><head></head><body>hi</body></html>");
        assert_eq!(p, Preview::default());
    }
}
