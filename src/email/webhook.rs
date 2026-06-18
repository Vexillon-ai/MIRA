// SPDX-License-Identifier: AGPL-3.0-or-later

// src/email/webhook.rs
//! Webhook ingest for inbound email (slice E6).
//!
//! Hosted-mail providers (Postmark, Resend, Mailgun) parse messages
//! at their end and POST a normalised JSON / form payload at MIRA.
//! This module owns the per-provider parsers that normalise those
//! shapes into a single `ParsedEmail`, which then flows through the
//! exact same security pipeline (`evaluate`) and dispatch path
//! (`dispatch_inbound`) the IMAP poller uses.
//!
//! Authentication: each account row carries a 32-char random
//! `webhook_secret` generated at creation time. The provider POSTs
//! to `/webhook/email/{account_id}/{webhook_secret}`; the secret
//! path segment is the bearer, validated in constant time. No per-
//! provider HMAC signing in v1 — the URL secret is enough for
//! hosted-mail providers' threat model, and adding signature
//! verification per provider is a noisy follow-up.

use serde::Deserialize;
use serde_json::Value;

use crate::MiraError;
use crate::email::parser::ParsedEmail;
use crate::email::security::InboundHeaders;

/// Per-provider parser entry point. `body` is the raw POST body —
/// JSON for Postmark/Resend, form-urlencoded for Mailgun.
/// `content_type` is the request's Content-Type header so the
/// Mailgun branch can decide whether to expect form-data.
pub fn parse(
    provider:     &str,
    body:         &[u8],
    content_type: Option<&str>,
    accept_html:  bool,
) -> Result<(ParsedEmail, InboundHeaders, usize), MiraError> {
    let raw_size = body.len();
    let (parsed, headers) = match provider {
        "postmark" => parse_postmark(body, accept_html)?,
        "resend"   => parse_resend(body, accept_html)?,
        "mailgun"  => parse_mailgun(body, content_type, accept_html)?,
        other => return Err(MiraError::ConfigError(format!(
            "webhook: unknown provider {other:?} (expected postmark|resend|mailgun)"
        ))),
    };
    Ok((parsed, headers, raw_size))
}

// ── Postmark ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PostmarkPayload {
    #[serde(default)] from:        String,
    #[serde(default)] from_name:   Option<String>,
    #[serde(default)] subject:     String,
    #[serde(default)] message_id:  Option<String>,
    #[serde(default)] text_body:   Option<String>,
    #[serde(default)] html_body:   Option<String>,
    #[serde(default)] headers:     Vec<PostmarkHeader>,
    // Postmark uses a single `To` field for the rcpt; we don't
    // currently need it (the account row's address is the rcpt
    // by construction) so we just accept-and-ignore.
    #[serde(default)] _to:         Option<String>,
}

#[derive(Debug, Deserialize)]
struct PostmarkHeader {
    #[serde(rename = "Name")]   name:  String,
    #[serde(rename = "Value")]  value: String,
}

fn parse_postmark(body: &[u8], accept_html: bool)
    -> Result<(ParsedEmail, InboundHeaders), MiraError>
{
    let p: PostmarkPayload = serde_json::from_slice(body)
        .map_err(|e| MiraError::ConfigError(format!("postmark webhook json: {e}")))?;

    // From Postmark may be `"Alice <alice@example.com>"` or just an
    // address. Extract the bare address for allowlist matching;
    // keep the display form when present.
    let (sender_address, sender_display) = split_addr(&p.from, p.from_name.as_deref());

    let mut in_reply_to = String::new();
    let mut references  = Vec::new();
    let mut auto_submitted = false;
    let mut bulk_or_list   = false;
    let mut auth_fail      = false;
    for h in &p.headers {
        let name_lc = h.name.to_ascii_lowercase();
        match name_lc.as_str() {
            "in-reply-to"  => in_reply_to = strip_angle(&h.value).to_string(),
            "references"   => references.extend(h.value.split_whitespace().map(|s| strip_angle(s).to_string())),
            "auto-submitted" => {
                let v = h.value.trim().to_ascii_lowercase();
                if v != "no" && !v.is_empty() { auto_submitted = true; }
            }
            "precedence" => {
                let v = h.value.trim().to_ascii_lowercase();
                if matches!(v.as_str(), "bulk" | "list" | "junk") { bulk_or_list = true; }
            }
            "list-id" | "list-unsubscribe" => { bulk_or_list = true; }
            "authentication-results" => {
                let lc = h.value.to_ascii_lowercase();
                if lc.contains("spf=fail") || lc.contains("dkim=fail") || lc.contains("dmarc=fail") {
                    auth_fail = true;
                }
            }
            _ => {}
        }
    }

    let parsed = ParsedEmail::from_fields(
        sender_address, sender_display,
        p.subject, p.message_id.map(|s| strip_angle(&s).to_string()).unwrap_or_default(),
        in_reply_to, references,
        p.text_body, p.html_body, accept_html,
    );
    Ok((parsed, InboundHeaders { is_auto_submitted: auto_submitted, is_bulk_or_list: bulk_or_list, auth_fail }))
}

// ── Resend ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ResendPayload {
    // Resend Inbound posts an event envelope: { type: "email.received",
    // data: { from, to, subject, text, html, headers: {...} } }
    #[serde(default)] data: Option<ResendData>,
}
#[derive(Debug, Deserialize)]
struct ResendData {
    #[serde(default)] from:    String,
    #[serde(default)] subject: String,
    #[serde(default)] text:    Option<String>,
    #[serde(default)] html:    Option<String>,
    #[serde(default)] headers: Value, // {"In-Reply-To": "...", ...}
}

fn parse_resend(body: &[u8], accept_html: bool)
    -> Result<(ParsedEmail, InboundHeaders), MiraError>
{
    let env: ResendPayload = serde_json::from_slice(body)
        .map_err(|e| MiraError::ConfigError(format!("resend webhook json: {e}")))?;
    let d = env.data.ok_or_else(|| MiraError::ConfigError(
        "resend webhook: missing data envelope".into()
    ))?;

    let (sender_address, sender_display) = split_addr(&d.from, None);

    let header = |k: &str| -> Option<String> {
        d.headers.get(k).and_then(|v| v.as_str()).map(|s| s.to_string())
    };
    let in_reply_to = header("In-Reply-To").or(header("in-reply-to"))
        .map(|s| strip_angle(&s).to_string()).unwrap_or_default();
    let references = header("References").or(header("references"))
        .map(|s| s.split_whitespace().map(|t| strip_angle(t).to_string()).collect())
        .unwrap_or_default();
    let message_id = header("Message-ID").or(header("message-id"))
        .map(|s| strip_angle(&s).to_string()).unwrap_or_default();

    let is_auto_submitted = header("Auto-Submitted").or(header("auto-submitted"))
        .map(|v| { let lc = v.trim().to_ascii_lowercase(); lc != "no" && !lc.is_empty() })
        .unwrap_or(false);
    let is_bulk_or_list = header("Precedence").or(header("precedence"))
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "bulk"|"list"|"junk"))
        .unwrap_or(false)
        || header("List-Id").is_some() || header("List-Unsubscribe").is_some();
    let auth_fail = header("Authentication-Results")
        .or(header("authentication-results"))
        .map(|v| {
            let lc = v.to_ascii_lowercase();
            lc.contains("spf=fail") || lc.contains("dkim=fail") || lc.contains("dmarc=fail")
        })
        .unwrap_or(false);

    let parsed = ParsedEmail::from_fields(
        sender_address, sender_display,
        d.subject, message_id, in_reply_to, references,
        d.text, d.html, accept_html,
    );
    Ok((parsed, InboundHeaders { is_auto_submitted, is_bulk_or_list, auth_fail }))
}

// ── Mailgun ─────────────────────────────────────────────────────────────────

fn parse_mailgun(body: &[u8], content_type: Option<&str>, accept_html: bool)
    -> Result<(ParsedEmail, InboundHeaders), MiraError>
{
    // Mailgun's "Routes → Forward as Store" posts multipart/form-data
    // for inbound. The "Store and Notify" + "Forward to URL"
    // notification webhook posts application/x-www-form-urlencoded.
    // We only handle the URL-encoded form here; multipart will hit
    // "unsupported content_type" and surface a clear error.
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    if !ct.contains("x-www-form-urlencoded") {
        return Err(MiraError::ConfigError(format!(
            "mailgun webhook: expected application/x-www-form-urlencoded, got {ct:?}"
        )));
    }
    let body_str = std::str::from_utf8(body)
        .map_err(|e| MiraError::ConfigError(format!("mailgun webhook: utf-8: {e}")))?;
    let form: std::collections::HashMap<String, String> = url::form_urlencoded::parse(body_str.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    let from        = form.get("sender").or(form.get("From")).cloned().unwrap_or_default();
    let (sender_address, sender_display) = split_addr(&from, None);
    let subject     = form.get("subject").or(form.get("Subject")).cloned().unwrap_or_default();
    let text_body   = form.get("body-plain").or(form.get("stripped-text")).cloned();
    let html_body   = form.get("body-html").or(form.get("stripped-html")).cloned();
    let message_id  = form.get("Message-Id").cloned()
        .map(|s| strip_angle(&s).to_string()).unwrap_or_default();
    let in_reply_to = form.get("In-Reply-To").cloned()
        .map(|s| strip_angle(&s).to_string()).unwrap_or_default();
    let references  = form.get("References")
        .map(|s| s.split_whitespace().map(|t| strip_angle(t).to_string()).collect())
        .unwrap_or_default();

    let is_auto_submitted = form.get("Auto-Submitted")
        .map(|v| { let lc = v.trim().to_ascii_lowercase(); lc != "no" && !lc.is_empty() })
        .unwrap_or(false);
    let is_bulk_or_list = form.get("Precedence")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "bulk"|"list"|"junk"))
        .unwrap_or(false)
        || form.contains_key("List-Id") || form.contains_key("List-Unsubscribe");
    let auth_fail = form.get("Authentication-Results")
        .map(|v| {
            let lc = v.to_ascii_lowercase();
            lc.contains("spf=fail") || lc.contains("dkim=fail") || lc.contains("dmarc=fail")
        })
        .unwrap_or(false);

    let parsed = ParsedEmail::from_fields(
        sender_address, sender_display,
        subject, message_id, in_reply_to, references,
        text_body, html_body, accept_html,
    );
    Ok((parsed, InboundHeaders { is_auto_submitted, is_bulk_or_list, auth_fail }))
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// `"Alice <alice@example.com>"` → `("alice@example.com", Some("Alice <alice@example.com>"))`.
/// Bare address → `(addr, None)`.
fn split_addr(raw: &str, name_hint: Option<&str>) -> (String, Option<String>) {
    let raw = raw.trim();
    if let (Some(lt), Some(gt)) = (raw.find('<'), raw.rfind('>')) {
        if gt > lt {
            let addr = raw[lt + 1..gt].trim().to_ascii_lowercase();
            return (addr, Some(raw.to_string()));
        }
    }
    // Bare address; optionally prefix with name_hint as display.
    let addr_lc = raw.to_ascii_lowercase();
    let display = name_hint
        .filter(|n| !n.is_empty())
        .map(|n| format!("{n} <{raw}>"));
    (addr_lc, display)
}

/// Strip leading `<` + trailing `>` from a Message-Id value.
fn strip_angle(s: &str) -> &str {
    s.trim().trim_start_matches('<').trim_end_matches('>')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postmark_minimal() {
        let body = br#"{
            "From": "Alice <alice@example.com>",
            "Subject": "hi",
            "MessageID": "abc@example.com",
            "TextBody": "hello there",
            "Headers": []
        }"#;
        let (p, h, _) = parse("postmark", body, None, true).unwrap();
        assert_eq!(p.sender_address, "alice@example.com");
        assert_eq!(p.subject, "hi");
        assert!(p.text_body.contains("hello there"));
        assert!(!h.is_auto_submitted);
    }

    #[test]
    fn postmark_auto_submitted_flagged() {
        let body = br#"{
            "From": "bot@example.com",
            "Subject": "out of office",
            "TextBody": "",
            "Headers": [{"Name":"Auto-Submitted","Value":"auto-replied"}]
        }"#;
        let (_, h, _) = parse("postmark", body, None, true).unwrap();
        assert!(h.is_auto_submitted);
    }

    #[test]
    fn resend_envelope() {
        let body = br#"{
            "type": "email.received",
            "data": {
                "from": "Bob <bob@example.com>",
                "subject": "test",
                "text": "body text",
                "headers": {"In-Reply-To": "<old@example.com>"}
            }
        }"#;
        let (p, _, _) = parse("resend", body, None, true).unwrap();
        assert_eq!(p.sender_address, "bob@example.com");
        assert_eq!(p.in_reply_to, "old@example.com");
    }

    #[test]
    fn mailgun_form_encoded() {
        let body = b"sender=carol%40example.com&subject=hi&body-plain=hi+there";
        let (p, _, _) = parse("mailgun", body, Some("application/x-www-form-urlencoded"), true).unwrap();
        assert_eq!(p.sender_address, "carol@example.com");
        assert_eq!(p.subject, "hi");
        assert!(p.text_body.contains("hi there"));
    }

    #[test]
    fn unknown_provider_errors() {
        let r = parse("yahoo", b"{}", None, true);
        assert!(r.is_err());
    }
}
