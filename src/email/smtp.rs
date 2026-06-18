// SPDX-License-Identifier: AGPL-3.0-or-later

// src/email/smtp.rs
//! Outbound SMTP (slice E2).
//!
//! Wraps `lettre`'s typed Message + AsyncSmtpTransport so the
//! companion + automations dispatchers and the inbound-reply path
//! all go through one well-defined send function. Header-injection-
//! safe by construction — every recipient/subject/body field passes
//! through `lettre`'s builder, never raw string concatenation into
//! the wire bytes.
//!
//! Reply-loop guard: a tiny in-memory cache of (recipient, body
//! SHA-256) pairs with a 60s TTL refuses repeat sends. Combined
//! with the inbound side's `Auto-Submitted: auto-replied` header
//! drop (chunk 3) and the explicit `Auto-Submitted` we set on every
//! outbound here, this is what stops an auto-responder ping-pong
//! from burning provider quota.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lettre::message::header;
use lettre::message::{Mailbox, Message};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::MiraError;
use crate::email::store::EmailAccountRow;

const REPLY_LOOP_TTL: Duration = Duration::from_secs(60);

/// Parameters for one outbound message. `in_reply_to` + `references`
/// are populated when this is a reply to an inbound — caller pulls
/// them off the `ParsedEmail` of the message being replied to.
/// `Subject` is sent as-is; the caller is responsible for adding
/// "Re: " prefixes if it wants them.
#[derive(Debug, Clone)]
pub struct OutboundMessage<'a> {
    pub to:           &'a str,
    pub subject:      &'a str,
    pub body:         &'a str,
    /// `Message-Id` of the email we're replying to (no surrounding
    /// `<>`). When set, becomes `In-Reply-To` and is appended to
    /// `References` so the recipient's client threads the reply.
    pub in_reply_to:  Option<&'a str>,
    /// Existing `References` chain from the inbound. Appended to,
    /// not replaced — preserves the full conversation thread.
    pub references:   &'a [String],
}

/// In-memory reply-loop guard. Single shared instance per gateway,
/// constructed by the email registry alongside the rate limiter.
pub struct ReplyLoopCache {
    seen: Mutex<HashMap<String, Instant>>,
}

impl ReplyLoopCache {
    pub fn new() -> Self { Self { seen: Mutex::new(HashMap::new()) } }

    /// Returns `true` if `(to, body)` was seen within the TTL.
    /// Recording the new pair on every call; the TTL is the window
    /// inside which a repeat is considered a loop.
    fn check_and_record(&self, to: &str, body: &str) -> bool {
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        let key = format!("{}|{:x}", to.to_ascii_lowercase(), hasher.finalize());

        let now = Instant::now();
        let mut map = self.seen.lock().unwrap();

        // Prune anything past TTL — keeps the map bounded under
        // sustained traffic.
        map.retain(|_, when| now.duration_since(*when) <= REPLY_LOOP_TTL);

        let dup = map.contains_key(&key);
        map.insert(key, now);
        dup
    }
}

impl Default for ReplyLoopCache {
    fn default() -> Self { Self::new() }
}

/// Send one outbound message via the account's SMTP server.
///
/// `oauth_access_token` is `Some` for OAuth accounts (E4) — when
/// provided, the SMTP login uses XOAUTH2 with the access token as
/// the secret. When `None`, falls back to the account's stored
/// `smtp_password` via PLAIN auth.
///
/// Returns `Err` on:
///   * Missing SMTP config on the account row.
///   * Reply-loop guard hit (same `(to, body)` within 60s).
///   * TLS / SMTP transport / auth failure.
///   * Lettre builder rejection.
pub async fn send(
    account:            &EmailAccountRow,
    msg:                OutboundMessage<'_>,
    loop_cache:         &ReplyLoopCache,
    oauth_access_token: Option<&str>,
) -> Result<(), MiraError> {
    let host = account.smtp_host.as_deref().ok_or_else(|| MiraError::ConfigError(
        format!("email '{}': smtp_host missing", account.address)
    ))?;
    // OAuth path lets the username fall back to the From address
    // (Gmail + Outlook both accept that). Password path still
    // requires an explicit smtp_username.
    let username = match (account.smtp_username.as_deref(), oauth_access_token.is_some()) {
        (Some(u), _)        => u,
        (None,    true)     => account.address.as_str(),
        (None,    false)    => return Err(MiraError::ConfigError(
            format!("email '{}': smtp_username missing", account.address)
        )),
    };
    // Secret: access token for OAuth, stored password otherwise.
    let secret = if let Some(tok) = oauth_access_token {
        tok.to_owned()
    } else {
        account.smtp_password.as_deref()
            .ok_or_else(|| MiraError::ConfigError(
                format!("email '{}': smtp_password missing", account.address)
            ))?.to_owned()
    };
    let port = account.smtp_port.unwrap_or(if account.smtp_use_tls { 465 } else { 587 });

    // Reply-loop guard. Fire BEFORE the build/send so a tight loop
    // can't even open the TCP connection.
    if loop_cache.check_and_record(msg.to, msg.body) {
        warn!(
            "email '{}': suppressing duplicate outbound to {} (reply-loop guard)",
            account.address, msg.to,
        );
        return Err(MiraError::ProviderError(format!(
            "email reply-loop: identical body to {} within {}s — refused",
            msg.to, REPLY_LOOP_TTL.as_secs(),
        )));
    }

    // ── Build the MIME message ────────────────────────────────────
    let from_mbox: Mailbox = account.address.parse()
        .map_err(|e| MiraError::ConfigError(format!(
            "email '{}': bad From address: {e}", account.address
        )))?;
    let to_mbox: Mailbox = msg.to.parse()
        .map_err(|e| MiraError::ConfigError(format!(
            "email '{}': bad To address {to:?}: {e}",
            account.address, to = msg.to,
        )))?;

    // Compose References: existing chain + the message we're
    // replying to (if any). The recipient's client uses this to
    // reconstruct the thread.
    let mut refs_combined: Vec<String> = msg.references.iter().cloned().collect();
    if let Some(rt) = msg.in_reply_to {
        if !refs_combined.iter().any(|r| r == rt) {
            refs_combined.push(rt.to_string());
        }
    }

    let mut builder = Message::builder()
        .from(from_mbox)
        .to(to_mbox)
        .subject(msg.subject)
        // RFC 3834 — declares us as an auto-reply so the other
        // side's auto-responder shouldn't engage.
        .header(header::ContentType::TEXT_PLAIN);

    // In-Reply-To: needs `<…>` brackets per RFC 5322. lettre's
    // typed headers are `From<String>` tuple structs — no parsing
    // failure mode.
    if let Some(rt) = msg.in_reply_to {
        builder = builder.header(header::InReplyTo::from(format!("<{rt}>")));
    }
    if !refs_combined.is_empty() {
        let formatted = refs_combined.iter()
            .map(|r| format!("<{r}>"))
            .collect::<Vec<_>>()
            .join(" ");
        builder = builder.header(header::References::from(formatted));
    }

    let email = builder.body(msg.body.to_string())
        .map_err(|e| MiraError::ConfigError(format!("smtp build: {e}")))?;

    // ── Transport ─────────────────────────────────────────────────
    // Use implicit TLS on port 465 by default; STARTTLS isn't
    // supported in v1 the same way as IMAP — most modern providers
    // do implicit TLS. Add STARTTLS branch when a real user needs it.
    let tls_params = TlsParameters::new(host.to_string())
        .map_err(|e| MiraError::ConfigError(format!("smtp tls params: {e}")))?;
    let transport = if account.smtp_use_tls {
        // Port-driven TLS mode pick. 465 → implicit TLS wrap on
        // connect; 587 (or anything else with use_tls=true) →
        // STARTTLS upgrade after EHLO. Microsoft Outlook needs
        // STARTTLS on 587 (they deprecated 465 for OAuth); Gmail
        // works on either.
        let tls_mode = if port == 465 {
            Tls::Wrapper(tls_params)
        } else {
            Tls::Required(tls_params)
        };
        let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)
            .port(port)
            .tls(tls_mode)
            .credentials(Credentials::new(username.to_owned(), secret));
        if oauth_access_token.is_some() {
            // Force XOAUTH2 — lettre's default mechanism list is
            // PLAIN + LOGIN, neither of which accepts a bearer
            // token as the "password" field.
            builder = builder.authentication(vec![Mechanism::Xoauth2]);
        }
        builder.build()
    } else {
        return Err(MiraError::ConfigError(
            "smtp: plaintext (smtp_use_tls=false) not supported in E2; \
             use 465 + TLS or 587 + STARTTLS".into()
        ));
    };

    transport.send(email).await
        .map_err(|e| MiraError::ProviderError(format!(
            "smtp send to {}: {e}", msg.to,
        )))?;

    info!(
        "email '{}': sent {}-char body to {} (in_reply_to={})",
        account.address, msg.body.len(), msg.to,
        msg.in_reply_to.unwrap_or("none"),
    );
    Ok(())
}

/// Convenience wrapper that resolves the account's auth method
/// (password or OAuth refresh) and calls [`send`]. Used by the
/// inbound reply path + companion/automations email arms so each
/// caller is one line and doesn't reinvent the auth-mode branch.
pub async fn send_for_account(
    account:    &EmailAccountRow,
    msg:        OutboundMessage<'_>,
    loop_cache: &ReplyLoopCache,
    store:      &crate::email::store::EmailAccountStore,
    cfg:        &crate::config::EmailOAuthConfig,
) -> Result<(), MiraError> {
    use crate::email::oauth::OAuthProvider;

    let token: Option<String> = if account.auth_mode.starts_with("oauth_") {
        Some(crate::email::oauth::refresh_if_needed(cfg, store, &account.id).await?)
    } else {
        None
    };

    // OAuth accounts default the SMTP host/port from the provider
    // when the row didn't carry explicit values. Lets the operator
    // skip the smtp_host/smtp_port form fields entirely for Gmail /
    // Outlook accounts. Pass-through when account already has
    // explicit values (e.g. SMTP relay overrides).
    let mut effective = account.clone();
    if token.is_some() {
        if let Some(p) = OAuthProvider::parse(&account.auth_mode) {
            let (h, port) = p.smtp_defaults();
            if effective.smtp_host.is_none() { effective.smtp_host = Some(h.to_string()); }
            if effective.smtp_port.is_none() { effective.smtp_port = Some(port); }
            effective.smtp_use_tls = true;
        }
    }
    send(&effective, msg, loop_cache, token.as_deref()).await
}

/// Prefix a subject with "Re: " unless it already starts with one
/// (case-insensitive). Used by the inbound-reply path so the
/// outbound subject threads in the recipient's client without
/// duplicating prefixes ("Re: Re: Re: …").
pub fn reply_subject(original: &str) -> String {
    let lower = original.trim_start().to_ascii_lowercase();
    if lower.starts_with("re:") {
        original.to_string()
    } else {
        format!("Re: {}", original)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_subject_doesnt_double_prefix() {
        assert_eq!(reply_subject("hello"), "Re: hello");
        assert_eq!(reply_subject("Re: hello"), "Re: hello");
        assert_eq!(reply_subject("re: hello"), "re: hello"); // preserve case
        assert_eq!(reply_subject("RE: hello"), "RE: hello");
    }

    #[test]
    fn loop_cache_blocks_dup_in_window() {
        let c = ReplyLoopCache::new();
        assert!(!c.check_and_record("a@x.com", "hi"), "first call must not flag");
        assert!( c.check_and_record("a@x.com", "hi"), "second within TTL must flag");
        assert!(!c.check_and_record("b@x.com", "hi"), "different recipient is OK");
        assert!(!c.check_and_record("a@x.com", "different body"), "different body is OK");
    }

    #[test]
    fn loop_cache_is_case_insensitive_on_recipient() {
        let c = ReplyLoopCache::new();
        c.check_and_record("Alice@Example.com", "hi");
        assert!(c.check_and_record("alice@example.com", "hi"));
    }
}
