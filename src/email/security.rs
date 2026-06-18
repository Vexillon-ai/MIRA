// SPDX-License-Identifier: AGPL-3.0-or-later

// src/email/security.rs
//! Inbound-email security gates (slice E1+E3, chunk 3).
//!
//! Every message that passes the IMAP fetch hits [`evaluate`], which
//! emits a [`Verdict`] used by the dispatch path (chunk 4) to decide
//! what to do:
//!
//!   - `Accept`     → run the agent turn (chunk 4)
//!   - `Quarantine` → write to the queue (chunk 5), no agent
//!   - `Drop`       → discard with an audit row (chunk 5)
//!
//! Each gate maps to a defence row in `design-docs/email-channel.md` §6.2.
//! Per-sender + per-account *rate* limits are accepted as input but
//! the storage that drives them lands in chunk 6 — until then the
//! caller passes `None` for the rate-tracker.

use crate::email::parser::ParsedEmail;
use crate::email::store::EmailSecurity;

/// What to do with the message after the security pipeline.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// Pass through to the agent (chunk 4). The reason string is
    /// empty for the normal allowlist path or carries a marker like
    /// "allowed-domain" when something other than an exact match
    /// granted entry — useful for the audit log.
    Accept { reason: String },
    /// Hold in the quarantine queue (chunk 5) for the user to
    /// approve / reject. `reason` is the short tag we surface in the
    /// UI: `unknown_sender`, `header_fail`, `oversized`, ...
    Quarantine { reason: String },
    /// Discard without further processing. Used for hard-block
    /// matches (denylist, auto-responder headers) where review
    /// would just give the operator decision fatigue.
    Drop { reason: String },
}

/// Inbound rate-limit state. Implemented by
/// [`crate::email::rate::InMemoryRateLimiter`] in production; the
/// `NoopRateLimiter` below is the test/fallback. Both methods take
/// the effective limit as an argument so the caller can resolve
/// per-account overrides vs the system default in one place and the
/// limiter stays a dumb counter store. `limit == 0` means "no
/// limit"; the implementation returns `true` without recording.
pub trait InboundRateLimiter {
    fn check_and_record_sender(
        &self,
        account_id:     &str,
        sender:         &str,
        limit_per_hour: u32,
    ) -> bool;
    fn check_and_record_account(
        &self,
        account_id:    &str,
        limit_per_day: u32,
    ) -> bool;
}

/// Pass-through rate limiter — used in unit tests + as the safety
/// fallback when no real limiter is wired. Always accepts.
pub struct NoopRateLimiter;
impl InboundRateLimiter for NoopRateLimiter {
    fn check_and_record_sender(&self, _: &str, _: &str, _: u32) -> bool { true }
    fn check_and_record_account(&self, _: &str, _: u32) -> bool { true }
}

/// Evaluate one parsed message against the account's security
/// settings and (optionally) a rate-limit tracker. Returns the
/// `Verdict` the caller should act on.
///
/// `raw_size` is the byte length of the original RFC822 — we use
/// this for the size gate rather than the parsed body length so a
/// message stuffed with attachments still trips the limit even when
/// the attachments were dropped before extraction.
pub fn evaluate(
    parsed:     &ParsedEmail,
    raw_size:   usize,
    headers:    &InboundHeaders,
    settings:   &EmailSecurity,
    effective_max_size_kb:      u32,
    effective_rate_per_sender:  u32,
    effective_rate_per_account: u32,
    account_id: &str,
    rate:       &dyn InboundRateLimiter,
) -> Verdict {
    // ── Hard drops: cheapest checks first ──────────────────────────

    // 1. Denylist match — operator's explicit "never engage" list.
    //    Checked before allowlist so a blanket-allow can still be
    //    punched through.
    if sender_matches_any(&parsed.sender_address, &settings.denied_senders) {
        return Verdict::Drop { reason: "denylist".into() };
    }

    // 2. Auto-submitted / list traffic. The MCP spec for outbound
    //    auto-reply (`Auto-Submitted: auto-replied`) and the
    //    venerable `Precedence: bulk/list/junk` cover the common
    //    cases. Engaging here is what creates auto-responder
    //    loops; refuse to engage at all.
    if headers.is_auto_submitted || headers.is_bulk_or_list {
        return Verdict::Drop {
            reason: if headers.is_auto_submitted { "auto_submitted".into() }
                    else                          { "bulk_or_list".into() }
        };
    }

    // 3. SPF / DKIM hard-fail — when present in
    //    Authentication-Results and any verifier returned `fail`,
    //    treat the sender as forged. Quarantine rather than drop
    //    so the operator can recover from a misconfigured upstream
    //    relay.
    if headers.auth_fail {
        return Verdict::Quarantine { reason: "header_fail".into() };
    }

    // ── Size gate ──────────────────────────────────────────────────
    let max_bytes = (effective_max_size_kb as usize).saturating_mul(1024);
    if max_bytes > 0 && raw_size > max_bytes {
        return Verdict::Drop {
            reason: format!("oversized:{}KB", raw_size / 1024)
        };
    }

    // ── Allowlist gate ─────────────────────────────────────────────
    let on_allowlist = sender_matches_any(&parsed.sender_address, &settings.allowed_senders);
    if !on_allowlist && !settings.accept_from_unknown_senders {
        return Verdict::Quarantine { reason: "unknown_sender".into() };
    }

    // ── Rate limits ───────────────────────────────────────────────
    // Per-sender first (cheaper, scoped). Both gates use Drop
    // rather than Quarantine — quarantining a flood would just
    // move the burn from the agent path to the queue table.
    if !rate.check_and_record_sender(
        account_id, &parsed.sender_address, effective_rate_per_sender,
    ) {
        return Verdict::Drop { reason: "rate:sender".into() };
    }
    if !rate.check_and_record_account(account_id, effective_rate_per_account) {
        return Verdict::Drop { reason: "rate:account".into() };
    }

    Verdict::Accept {
        reason: if on_allowlist { "allowlist".into() } else { "unknown_accepted".into() }
    }
}

/// Headers the security pipeline cares about. Extracted from the
/// raw message before parsing the body — kept separate so the
/// audit row can capture them even when body extraction fails.
#[derive(Debug, Clone, Default)]
pub struct InboundHeaders {
    pub is_auto_submitted: bool,
    pub is_bulk_or_list:   bool,
    /// `true` when `Authentication-Results:` contained any
    /// `spf=fail`, `dkim=fail`, or `dmarc=fail`. Conservative — any
    /// fail is treated as the whole result failing.
    pub auth_fail:         bool,
}

impl InboundHeaders {
    /// Extract from a `mail_parser::Message` — the parser already
    /// has the headers; we just need them in our simpler shape.
    pub fn from_message(msg: &mail_parser::Message<'_>) -> Self {
        let is_auto_submitted = msg.header("Auto-Submitted")
            .and_then(|h| h.as_text())
            .map(|v| v.trim().to_ascii_lowercase() != "no" && !v.trim().is_empty())
            .unwrap_or(false);

        let is_bulk_or_list = msg.header("Precedence")
            .and_then(|h| h.as_text())
            .map(|v| {
                let lc = v.trim().to_ascii_lowercase();
                lc == "bulk" || lc == "list" || lc == "junk"
            })
            .unwrap_or(false)
            // List-Id / List-Unsubscribe presence is a strong "this
            // is mailing-list mail" signal even without Precedence.
            || msg.header("List-Id").is_some()
            || msg.header("List-Unsubscribe").is_some();

        let auth_fail = msg.header("Authentication-Results")
            .and_then(|h| h.as_text())
            .map(|v| {
                let lc = v.to_ascii_lowercase();
                lc.contains("spf=fail")
                    || lc.contains("dkim=fail")
                    || lc.contains("dmarc=fail")
            })
            .unwrap_or(false);

        Self { is_auto_submitted, is_bulk_or_list, auth_fail }
    }
}

/// Check whether `addr` matches any entry in `patterns`. Each entry
/// is either an exact email or a `*@domain` wildcard. Case-
/// insensitive on both sides because email addresses are.
pub fn sender_matches_any(addr: &str, patterns: &[String]) -> bool {
    if addr.is_empty() || patterns.is_empty() { return false; }
    let addr_lc = addr.to_ascii_lowercase();
    let domain  = addr_lc.split('@').nth(1).unwrap_or("");
    for p in patterns {
        let p_lc = p.trim().to_ascii_lowercase();
        if p_lc.is_empty() { continue; }
        if let Some(suffix) = p_lc.strip_prefix("*@") {
            if domain == suffix { return true; }
        } else if p_lc == addr_lc {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rl() -> NoopRateLimiter { NoopRateLimiter }

    fn parsed(sender: &str) -> ParsedEmail {
        ParsedEmail {
            sender_display: sender.into(),
            sender_address: sender.into(),
            subject: "x".into(),
            message_id: "1@x".into(),
            in_reply_to: "".into(),
            references: vec![],
            text_body: "hi".into(),
            html_present: false,
            dropped_attachments: vec![],
        }
    }

    fn security_with(allowed: &[&str], denied: &[&str], unknown: bool) -> EmailSecurity {
        EmailSecurity {
            allowed_senders: allowed.iter().map(|s| s.to_string()).collect(),
            denied_senders:  denied.iter().map(|s| s.to_string()).collect(),
            accept_from_unknown_senders: unknown,
            ..EmailSecurity::default()
        }
    }

    #[test]
    fn allowlisted_sender_accepted() {
        let v = evaluate(
            &parsed("alice@example.com"),
            500, &InboundHeaders::default(),
            &security_with(&["alice@example.com"], &[], false),
            1024, 0, 0, "acct", &rl(),
        );
        assert!(matches!(v, Verdict::Accept { .. }));
    }

    #[test]
    fn domain_wildcard_works() {
        let v = evaluate(
            &parsed("anyone@example.com"),
            500, &InboundHeaders::default(),
            &security_with(&["*@example.com"], &[], false),
            1024, 0, 0, "acct", &rl(),
        );
        assert!(matches!(v, Verdict::Accept { .. }));
    }

    #[test]
    fn unknown_sender_quarantined_when_strict() {
        let v = evaluate(
            &parsed("rando@example.com"),
            500, &InboundHeaders::default(),
            &security_with(&[], &[], false),
            1024, 0, 0, "acct", &rl(),
        );
        assert!(matches!(v, Verdict::Quarantine { reason } if reason == "unknown_sender"));
    }

    #[test]
    fn unknown_sender_accepted_when_open() {
        let v = evaluate(
            &parsed("rando@example.com"),
            500, &InboundHeaders::default(),
            &security_with(&[], &[], true),
            1024, 0, 0, "acct", &rl(),
        );
        assert!(matches!(v, Verdict::Accept { .. }));
    }

    #[test]
    fn denylist_beats_allowlist() {
        let v = evaluate(
            &parsed("spam@example.com"),
            500, &InboundHeaders::default(),
            &security_with(&["*@example.com"], &["spam@example.com"], true),
            1024, 0, 0, "acct", &rl(),
        );
        assert!(matches!(v, Verdict::Drop { reason } if reason == "denylist"));
    }

    #[test]
    fn auto_submitted_dropped() {
        let mut h = InboundHeaders::default();
        h.is_auto_submitted = true;
        let v = evaluate(
            &parsed("alice@example.com"),
            500, &h,
            &security_with(&["alice@example.com"], &[], true),
            1024, 0, 0, "acct", &rl(),
        );
        assert!(matches!(v, Verdict::Drop { reason } if reason == "auto_submitted"));
    }

    #[test]
    fn oversized_dropped() {
        let v = evaluate(
            &parsed("alice@example.com"),
            2_000_000, &InboundHeaders::default(),
            &security_with(&["alice@example.com"], &[], true),
            1024, 0, 0, "acct", &rl(),
        );
        assert!(matches!(v, Verdict::Drop { reason } if reason.starts_with("oversized")));
    }
}
