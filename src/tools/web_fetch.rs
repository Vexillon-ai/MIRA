// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/web_fetch.rs
//! `web_fetch` — Tier 2 network tool.
//!
//! Retrieves a URL and returns either readability-extracted body text
//! (default) or the raw body, capped by the configured character budget.
//! Every outbound call goes through [`HttpPolicy`], so the SSRF guard,
//! denylist, rate limits and redirect re-validation all apply here. See
//! `design-docs/phase7-tier2-web-tools.md` §3.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tier, Tool, ToolArgs, ToolResult};
use super::http_policy::{HttpPolicy, HttpResponse, PolicyError};
use crate::MiraError;

// Configuration captured from `MiraConfig` at build time.
#[derive(Debug, Clone)]
pub struct WebFetchSettings {
    pub max_text_chars: usize,
}

pub struct WebFetchTool {
    policy:   Arc<HttpPolicy>,
    settings: WebFetchSettings,
}

impl WebFetchTool {
    pub fn new(policy: Arc<HttpPolicy>, settings: WebFetchSettings) -> Self {
        Self { policy, settings }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str { "web_fetch" }

    fn description(&self) -> &str {
        "Fetch a web page by URL and return its readable body text. Use this \
         whenever the user asks you to read, quote, or summarise a specific \
         page. `mode: \"readable\"` (default) strips nav/ads/scripts and \
         returns the article body; `mode: \"raw\"` returns the response body \
         verbatim (useful for JSON endpoints). Only http/https URLs are \
         allowed; localhost, private IPs and cloud metadata are blocked. \
         GET only — never mutates remote state."
    }

    fn tier(&self) -> Tier { Tier::Network }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["url"],
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Absolute http(s) URL to fetch."
                },
                "mode": {
                    "type": "string",
                    "enum": ["readable", "raw"],
                    "description":
                        "'readable' (default) — run readability over HTML and \
                         return just the article body. 'raw' — return the \
                         response body verbatim."
                },
                "max_chars": {
                    "type": "integer",
                    "description":
                        "Truncate output to at most this many characters. \
                         Capped by the server's configured limit."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let url = match args.get("url").and_then(|v| v.as_str()) {
            Some(u) if !u.trim().is_empty() => u.trim().to_owned(),
            _ => return Ok(ToolResult::failure(
                "web_fetch: `url` is required".to_string(),
            )),
        };
        let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("readable");
        let user_id = args.get("_user_id").and_then(|v| v.as_str()).unwrap_or("anonymous");

        let max_chars = args.get("max_chars")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).min(self.settings.max_text_chars))
            .unwrap_or(self.settings.max_text_chars);

        // when an `_agent_id` is in the injected args we
        // route through the policy engine via `get_with_context` so
        // network-allowlist + admin rules can deny. Skill id flows
        // through too when the call is made from a Skill-routed tool.
        let ctx = build_request_context(user_id, &args);
        let resp = match self.policy.get_with_context(&url, &ctx).await {
            Ok(r)  => r,
            Err(e) => return Ok(ToolResult::failure(format_policy_err(&e))),
        };

        if !(200..300).contains(&resp.status) {
            return Ok(ToolResult::failure(
                format!("web_fetch: http {} for {}", resp.status, resp.final_url),
            ));
        }

        let body = render_body(&resp, mode, max_chars)?;
        let plaintext_warning = url.starts_with("http://");

        let mut out = json!({
            "url":          url,
            "final_url":    resp.final_url,
            "status":       resp.status,
            "content_type": resp.content_type,
            "title":        body.title,
            "text":         body.text,
            "truncated":    body.truncated,
        });
        if plaintext_warning {
            out["warning"] = Value::String("plaintext HTTP — contents are not confidential".into());
        } else if body.truncated {
            out["warning"] = Value::String("content truncated".into());
        } else {
            out["warning"] = Value::Null;
        }
        Ok(ToolResult::success(out.to_string()))
    }
}

// ── Rendering ────────────────────────────────────────────────────────────────

struct RenderedBody {
    title:     Option<String>,
    text:      String,
    truncated: bool,
}

fn render_body(
    resp:      &HttpResponse,
    mode:      &str,
    max_chars: usize,
) -> Result<RenderedBody, MiraError> {
    let ct = resp.content_type.as_deref().unwrap_or("").to_ascii_lowercase();

    // Non-text content types: refuse to stringify binary.
    if !is_text_like(&ct) {
        return Ok(RenderedBody {
            title: None,
            text:  format!("[binary content, not text: content-type={}]", ct),
            truncated: false,
        });
    }

    match mode {
        "readable" if ct.contains("html") => {
            let (title, text) = extract_readable(&resp.body, &resp.final_url);
            let (text, truncated) = cap_chars(&text, max_chars, resp.truncated);
            Ok(RenderedBody { title, text, truncated })
        }
        _ => {
            // raw mode, or non-HTML (plain text, json, xml): decode as UTF-8.
            let raw = String::from_utf8_lossy(&resp.body).into_owned();
            let (text, truncated) = cap_chars(&raw, max_chars, resp.truncated);
            Ok(RenderedBody { title: None, text, truncated })
        }
    }
}

fn is_text_like(ct: &str) -> bool {
    ct.starts_with("text/")
        || ct.contains("json")
        || ct.contains("xml")
        || ct.contains("javascript")
        || ct.is_empty() // missing content-type, assume text
}

// Truncate to at most `max_chars` graphemes-agnostic characters (chars).
// Returns the truncated string plus the combined truncated flag.
fn cap_chars(s: &str, max_chars: usize, upstream_truncated: bool) -> (String, bool) {
    if s.chars().count() <= max_chars {
        return (s.to_owned(), upstream_truncated);
    }
    let mut out = String::with_capacity(max_chars);
    for (i, c) in s.chars().enumerate() {
        if i >= max_chars { break; }
        out.push(c);
    }
    (out, true)
}

// Run readability-rs over the body. Falls back to raw text on any failure.
fn extract_readable(body: &[u8], url: &str) -> (Option<String>, String) {
    let url_parsed = url::Url::parse(url);
    let mut cursor = std::io::Cursor::new(body);
    match url_parsed.as_ref() {
        Ok(u) => match readability::extractor::extract(&mut cursor, u) {
            Ok(p) => {
                let title = if p.title.trim().is_empty() { None } else { Some(p.title) };
                // Prefer the `text` field (plain text); fall back to `content`
                // (HTML) stripped minimally if `text` came back empty.
                let text = if !p.text.trim().is_empty() { p.text }
                           else { strip_html(&p.content) };
                (title, text)
            }
            Err(_) => (None, String::from_utf8_lossy(body).into_owned()),
        },
        Err(_) => (None, String::from_utf8_lossy(body).into_owned()),
    }
}

// Minimal HTML tag stripper for the fallback path. Readability itself
// already returns plain text in `Product.text`; this is only used when it
// comes back empty.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

// Build the policy-engine context from the standard injected-args
// keys. `_agent_id` is set by the agent's tool loop when known; when
// absent the context's `agent_id` is `None` and the engine consult
// is skipped (the existing SSRF / denylist / rate-limit guards still
// apply). `_skill_id` is set when the tool was called via a Skill
// router (slice A3 onwards).
fn build_request_context(user_id: &str, args: &ToolArgs) -> super::http_policy::RequestContext {
    use crate::agent::instance::AgentId;
    let agent_id = args.get("_agent_id")
        .and_then(|v| v.as_str())
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(AgentId);
    let skill_id = args.get("_skill_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    super::http_policy::RequestContext {
        user_id: user_id.to_owned(),
        agent_id,
        skill_id,
    }
}

fn format_policy_err(e: &PolicyError) -> String {
    match e {
        PolicyError::BlockedHost { reason, host } =>
            format!("web_fetch: blocked host '{}' ({}) — the URL resolves to a restricted address range", host, reason),
        PolicyError::BlockedScheme { scheme } =>
            format!("web_fetch: blocked scheme '{}' — only http/https is allowed", scheme),
        PolicyError::DenylistedDomain { host } =>
            format!("web_fetch: domain '{}' is on the admin denylist", host),
        PolicyError::AllowlistOnly { host } =>
            format!("web_fetch: domain '{}' is not on the admin allowlist", host),
        PolicyError::RateLimited { retry_after_ms, scope } =>
            format!("web_fetch: rate_limited ({}) — retry after {}ms", scope, retry_after_ms),
        PolicyError::TooLarge { limit, observed } =>
            format!("web_fetch: response too large ({} > {} bytes)", observed, limit),
        PolicyError::Timeout                 => "web_fetch: request timed out".into(),
        PolicyError::TooManyRedirects        => "web_fetch: too many redirects".into(),
        PolicyError::Http { status, url }    => format!("web_fetch: http {} for {}", status, url),
        PolicyError::DnsResolution { host, detail } =>
            format!("web_fetch: dns resolution failed for '{}': {}", host, detail),
        PolicyError::InvalidUrl(s)           => format!("web_fetch: invalid url '{}'", s),
        PolicyError::Transport(s)            => format!("web_fetch: transport error: {}", s),
        PolicyError::PolicyDenied { rule, reason } =>
            format!("web_fetch: policy/{} denied: {}", rule, reason),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::http_policy::HttpPolicyConfig;

    #[test]
    fn is_text_like_covers_common_web_types() {
        assert!(is_text_like("text/html"));
        assert!(is_text_like("text/plain"));
        assert!(is_text_like("application/json"));
        assert!(is_text_like("application/xml"));
        assert!(is_text_like("application/ld+json; charset=utf-8"));
        assert!(is_text_like("")); // missing -> treat as text
        assert!(!is_text_like("image/png"));
        assert!(!is_text_like("application/octet-stream"));
    }

    #[test]
    fn cap_chars_truncates_and_flags() {
        let (out, t) = cap_chars("0123456789", 5, false);
        assert_eq!(out, "01234");
        assert!(t);
        let (out, t) = cap_chars("abc", 100, false);
        assert_eq!(out, "abc");
        assert!(!t);
        let (_, t) = cap_chars("abc", 100, true);
        assert!(t, "upstream truncation must propagate");
    }

    #[test]
    fn strip_html_removes_tags_keeps_text() {
        let s = strip_html("<p>hello <b>world</b></p>");
        assert_eq!(s, "hello world");
    }

    // ── End-to-end: tool → policy → reqwest ────────────────────────────────

    async fn spawn_html_server(body: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr     = listener.local_addr().unwrap();
        let url      = format!("http://127.0.0.1:{}/", addr.port());
        let h = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body,
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        (url, h)
    }

    #[tokio::test]
    async fn web_fetch_refuses_loopback_by_default() {
        let (url, _h) = spawn_html_server("<html><body>hi</body></html>").await;
        let policy = Arc::new(HttpPolicy::new(HttpPolicyConfig::default()));
        let tool = WebFetchTool::new(policy, WebFetchSettings { max_text_chars: 10_000 });
        let out = tool.execute(json!({"url": url})).await.unwrap();
        assert!(!out.success, "loopback fetch must fail by default");
        let err = out.error.unwrap();
        assert!(err.contains("blocked host") || err.contains("restricted"),
            "expected SSRF-style failure, got: {}", err);
    }

    #[tokio::test]
    async fn web_fetch_rejects_missing_url() {
        let policy = Arc::new(HttpPolicy::new(HttpPolicyConfig::default()));
        let tool = WebFetchTool::new(policy, WebFetchSettings { max_text_chars: 10_000 });
        let out = tool.execute(json!({})).await.unwrap();
        assert!(!out.success);
        assert!(out.error.unwrap().contains("url"));
    }
}
