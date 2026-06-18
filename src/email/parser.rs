// SPDX-License-Identifier: AGPL-3.0-or-later

// src/email/parser.rs
//! MIME parsing + body sanitisation (slice E1+E3, chunk 3).
//!
//! Walks the multipart tree via `mail-parser`, extracts a single
//! plain-text body the agent can read, and tallies any attachments
//! that were dropped per the security policy. HTML bodies pass
//! through `ammonia` (whitelist tags, strip scripts/styles/event-
//! handlers/iframes/external image src) then get reduced to plain
//! text — the agent never sees markup.
//!
//! Output is [`ParsedEmail`]. Chunk 4 wraps that into a prompt for
//! the agent; chunk 3 stops at extraction so the pipeline can be
//! tested in isolation.

use mail_parser::{Address, MessageParser, MimeHeaders, PartType};

/// Parsed inbound email — everything downstream needs in one place.
/// Fields the security pipeline depends on are guaranteed present;
/// optional fields (`html_present`, `dropped_attachments`) carry the
/// information needed for the audit trail without surfacing the
/// content itself.
#[derive(Debug, Clone)]
pub struct ParsedEmail {
    /// Header `From:` rendered as `name <addr>` or just `addr`.
    pub sender_display: String,
    /// Just the address part of `From:` — used for allowlist matching.
    pub sender_address: String,
    pub subject:        String,
    /// First non-empty `Message-Id` header in canonical form
    /// (no surrounding `<>`). Empty when absent.
    pub message_id:     String,
    /// `In-Reply-To` header, stripped. Empty when absent.
    pub in_reply_to:    String,
    /// Every `References:` entry, stripped. Empty when absent.
    pub references:     Vec<String>,
    /// Plain-text body the agent reads. Already sanitised — HTML
    /// has been ammonia'd and stripped; never contains markup.
    pub text_body:      String,
    /// `true` when the message had an HTML part we extracted from
    /// (after sanitisation). Surfaced for the audit log.
    pub html_present:   bool,
    /// Count + collected MIME types of attachments we declined to
    /// surface to the agent. The text_body gets a one-line summary
    /// appended so the user knows something was attached.
    pub dropped_attachments: Vec<String>,
}

/// Per-account knob the parser consults. Surface chosen to match
/// the system-wide settings + per-account overrides from the design
/// doc — chunk 6 resolves overrides → effective values; chunk 3
/// accepts the effective values as a single struct.
#[derive(Debug, Clone, Copy)]
pub struct ParseSettings {
    pub accept_html:          bool,
    pub accept_inline_images: bool,
    pub accept_attachments:   bool,
}

impl Default for ParseSettings {
    fn default() -> Self {
        // Mirrors design-docs/email-channel.md §5 — conservative defaults.
        Self {
            accept_html:          true,   // strip to text after sanitisation
            accept_inline_images: false,
            accept_attachments:   false,
        }
    }
}

impl ParsedEmail {
    /// E6 — construct a `ParsedEmail` from already-extracted fields
    /// rather than RFC822 bytes. Used by the webhook ingest path
    /// (Postmark / Resend / Mailgun) since the providers POST
    /// parsed JSON and we'd otherwise re-serialise to MIME just to
    /// re-parse on the other side. Skips ammonia sanitisation when
    /// `text_body` is supplied directly; HTML coming via webhook
    /// has already been pulled out of `multipart/alternative` by
    /// the provider — we just sanitise it here.
    pub fn from_fields(
        sender_address: String,
        sender_display: Option<String>,
        subject:        String,
        message_id:     String,
        in_reply_to:    String,
        references:     Vec<String>,
        text_body:      Option<String>,
        html_body:      Option<String>,
        accept_html:    bool,
    ) -> Self {
        // Prefer plain text when given. Fall back to sanitised + stripped
        // HTML when the provider only delivered HTML and the account opts
        // in. Match the IMAP path's behaviour for consistency.
        let mut html_present = false;
        let body = if let Some(t) = text_body.filter(|s| !s.trim().is_empty()) {
            t
        } else if let Some(h) = html_body.filter(|s| !s.trim().is_empty()) {
            html_present = true;
            if accept_html {
                strip_html_to_text(&ammonia::clean(&h))
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        let display = sender_display
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                if sender_address.is_empty() { "(unknown sender)".to_string() }
                else { sender_address.clone() }
            });

        Self {
            sender_display: display,
            sender_address,
            subject,
            message_id,
            in_reply_to,
            references,
            text_body: body,
            html_present,
            dropped_attachments: Vec::new(),
        }
    }
}

/// Parse one inbound message. Returns `None` only when the raw bytes
/// aren't a parseable email — every well-formed message produces a
/// `ParsedEmail` even when it has no body (text_body becomes empty
/// and the security pipeline catches that downstream).
pub fn parse_email(raw: &[u8], settings: ParseSettings) -> Option<ParsedEmail> {
    let msg = MessageParser::default().parse(raw)?;

    // ── Headers ────────────────────────────────────────────────────
    let (sender_display, sender_address) = parse_from(&msg);
    let subject = msg.subject().unwrap_or("(no subject)").to_string();
    let message_id = msg.message_id().unwrap_or("").trim_matches(['<','>']).to_string();
    let in_reply_to = msg.in_reply_to().as_text_list()
        .and_then(|v| v.into_iter().next().map(|s| s.to_string()))
        .unwrap_or_default();
    let references = msg.references().as_text_list()
        .map(|v| v.into_iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .unwrap_or_default();

    // ── Body extraction ────────────────────────────────────────────
    // Prefer text/plain — never have to sanitise, never lose
    // structure to a stripper. Fall back to text/html only when no
    // plain part exists AND the account opted in.
    let mut text_body = String::new();
    let mut html_present = false;
    let mut dropped_attachments: Vec<String> = Vec::new();

    // Prefer the first text/plain part across the tree.
    for part in msg.parts.iter() {
        if let PartType::Text(t) = &part.body {
            if !t.trim().is_empty() {
                text_body = t.to_string();
                break;
            }
        }
    }

    // If no plain part, try HTML (when allowed).
    if text_body.is_empty() && settings.accept_html {
        for part in msg.parts.iter() {
            if let PartType::Html(h) = &part.body {
                html_present = true;
                let sanitised = ammonia::clean(h);
                let stripped  = strip_html_to_text(&sanitised);
                if !stripped.trim().is_empty() {
                    text_body = stripped;
                    break;
                }
            }
        }
    } else if text_body.is_empty() {
        // We have HTML but the account refused it — note for audit.
        for part in msg.parts.iter() {
            if let PartType::Html(_) = &part.body {
                html_present = true;
                break;
            }
        }
    }

    // ── Attachment tally ──────────────────────────────────────────
    // mail-parser flags non-body parts via `is_attachment` /
    // `content_disposition`. We *don't* extract their bytes in chunk
    // 3 — bodies stay in memory only long enough to surface in the
    // text body; binary content gets dropped + summarised. The
    // accept_attachments / accept_inline_images flags will gate
    // future opt-in extraction.
    if !settings.accept_attachments || !settings.accept_inline_images {
        for part in msg.parts.iter() {
            let is_body_part = matches!(part.body, PartType::Text(_) | PartType::Html(_));
            if is_body_part { continue; }
            let mime = part.content_type()
                .and_then(|ct| Some(format!("{}/{}", ct.ctype(), ct.subtype()?)))
                .unwrap_or_else(|| "application/octet-stream".to_string());
            // Inline image vs explicit attachment — both dropped by
            // default; one toggle each is what the per-account
            // settings expose.
            let is_inline_image = mime.starts_with("image/")
                && part.content_disposition()
                    .map(|d| d.ctype().eq_ignore_ascii_case("inline"))
                    .unwrap_or(true);
            let allowed = if is_inline_image {
                settings.accept_inline_images
            } else {
                settings.accept_attachments
            };
            if !allowed {
                dropped_attachments.push(mime);
            }
        }
    }

    // Append a one-line summary so the agent + the user-facing log
    // know something was attached even if we dropped it.
    if !dropped_attachments.is_empty() {
        let mut summary = format!(
            "\n\n[security: {} attachment{} dropped",
            dropped_attachments.len(),
            if dropped_attachments.len() == 1 { "" } else { "s" },
        );
        // Show up to 3 types so a phishing campaign blasting
        // "invoice.pdf" doesn't reveal that fact via a verbose log.
        let preview: Vec<_> = dropped_attachments.iter().take(3).cloned().collect();
        if !preview.is_empty() {
            summary.push_str(&format!(" ({})", preview.join(", ")));
            if dropped_attachments.len() > preview.len() {
                summary.push_str(" + more");
            }
        }
        summary.push(']');
        text_body.push_str(&summary);
    }

    Some(ParsedEmail {
        sender_display,
        sender_address,
        subject,
        message_id,
        in_reply_to,
        references,
        text_body,
        html_present,
        dropped_attachments,
    })
}

/// Pull the From header into a (display, address) pair. Picks the
/// first address when multiple are present (the spec allows it but
/// most clients don't).
fn parse_from(msg: &mail_parser::Message<'_>) -> (String, String) {
    let from = match msg.from() {
        Some(a) => a,
        None    => return ("(unknown sender)".into(), "".into()),
    };
    let addr = first_address(from);
    let addr_str = addr.as_ref()
        .and_then(|a| a.address.as_deref())
        .unwrap_or("")
        .to_string();
    let name = addr.as_ref()
        .and_then(|a| a.name.as_deref())
        .filter(|n| !n.is_empty())
        .map(|n| n.to_string());
    let display = match (name, addr_str.is_empty()) {
        (Some(n), false) => format!("{n} <{addr_str}>"),
        (None,    false) => addr_str.clone(),
        _                => "(unknown sender)".to_string(),
    };
    (display, addr_str)
}

fn first_address<'x>(addr: &'x Address<'_>) -> Option<&'x mail_parser::Addr<'x>> {
    addr.iter().next()
}

/// Walk the sanitised HTML and emit a plain-text rendering. Block
/// elements become newlines, links become `text (url)`, everything
/// else is just text content. Conservative — we'd rather lose
/// formatting than ship a tracking URL the agent might follow.
fn strip_html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut current_tag = String::new();
    let mut href: Option<String> = None;
    let mut pending_link_text: Option<String> = None;

    for ch in html.chars() {
        if ch == '<' {
            in_tag = true;
            current_tag.clear();
            continue;
        }
        if ch == '>' {
            in_tag = false;
            let tag = current_tag.to_ascii_lowercase();
            let trimmed = tag.trim_start_matches('/');
            // Block elements that should newline-separate.
            if matches!(
                trimmed.split_whitespace().next().unwrap_or(""),
                "p" | "br" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6",
            ) {
                if !out.ends_with('\n') { out.push('\n'); }
            }
            // Link extraction. Pull href when entering an <a>; emit
            // `text (url)` when leaving the closing </a>. Order
            // matters — the closing branch must run before the
            // opening branch since `trimmed` has the leading `/`
            // stripped and would otherwise look identical.
            let is_closing = tag.starts_with('/');
            if is_closing && (trimmed == "a" || trimmed.starts_with("a ")) {
                if let (Some(text), Some(url)) = (pending_link_text.take(), href.take()) {
                    out.push_str(text.trim());
                    out.push_str(" (");
                    out.push_str(&url);
                    out.push(')');
                }
            } else if !is_closing && (trimmed == "a" || trimmed.starts_with("a ")) {
                href = extract_href(&current_tag);
                pending_link_text = Some(String::new());
            }
            continue;
        }
        if in_tag {
            current_tag.push(ch);
        } else if let Some(buf) = pending_link_text.as_mut() {
            buf.push(ch);
        } else {
            out.push(ch);
        }
    }

    // Collapse 3+ blank lines that the naive newline insertion can
    // produce around table cells / lists.
    let mut prev_blank = false;
    let mut compacted = String::with_capacity(out.len());
    for line in out.lines() {
        let blank = line.trim().is_empty();
        if blank && prev_blank { continue; }
        compacted.push_str(line);
        compacted.push('\n');
        prev_blank = blank;
    }
    compacted.trim().to_string()
}

fn extract_href(attrs: &str) -> Option<String> {
    let lower = attrs.to_ascii_lowercase();
    let key = lower.find("href")?;
    let after = &attrs[key..];
    let eq = after.find('=')?;
    let rest = after[eq + 1..].trim_start();
    if rest.starts_with('"') {
        let end = rest[1..].find('"')?;
        Some(rest[1..1 + end].to_string())
    } else if rest.starts_with('\'') {
        let end = rest[1..].find('\'')?;
        Some(rest[1..1 + end].to_string())
    } else {
        let end = rest.find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_text_email() {
        let raw = b"From: Alice <alice@example.com>\r\n\
                    To: bot@mira.local\r\n\
                    Subject: hi\r\n\
                    Message-ID: <abc@example.com>\r\n\
                    \r\n\
                    hello there\r\n";
        let p = parse_email(raw, ParseSettings::default()).unwrap();
        assert_eq!(p.sender_address, "alice@example.com");
        assert_eq!(p.subject, "hi");
        assert_eq!(p.message_id, "abc@example.com");
        assert!(p.text_body.contains("hello there"));
        assert!(!p.html_present);
        assert!(p.dropped_attachments.is_empty());
    }

    #[test]
    fn html_email_gets_stripped_to_text() {
        let raw = b"From: Bob <bob@example.com>\r\n\
                    Subject: pretty\r\n\
                    Content-Type: text/html\r\n\
                    \r\n\
                    <p>Hello <b>world</b></p><p>line two</p>";
        let p = parse_email(raw, ParseSettings::default()).unwrap();
        assert!(p.html_present);
        assert!(p.text_body.contains("Hello"));
        assert!(p.text_body.contains("world"));
        assert!(!p.text_body.contains('<'), "markup must not leak through: {:?}", p.text_body);
    }

    #[test]
    fn link_renders_as_text_then_url() {
        let raw = b"From: Eve <eve@example.com>\r\n\
                    Subject: check this\r\n\
                    Content-Type: text/html\r\n\
                    \r\n\
                    Click <a href=\"https://example.com/abc\">here</a> please";
        let p = parse_email(raw, ParseSettings::default()).unwrap();
        assert!(p.text_body.contains("here (https://example.com/abc)"),
                "got: {:?}", p.text_body);
    }

    #[test]
    fn html_refused_when_setting_off() {
        let raw = b"From: Bob <bob@example.com>\r\n\
                    Subject: pretty\r\n\
                    Content-Type: text/html\r\n\
                    \r\n\
                    <p>hello</p>";
        let mut s = ParseSettings::default();
        s.accept_html = false;
        let p = parse_email(raw, s).unwrap();
        assert!(p.html_present);
        assert!(p.text_body.is_empty(),
                "HTML-only message with accept_html=false should leave body empty: {:?}",
                p.text_body);
    }
}
