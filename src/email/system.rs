// SPDX-License-Identifier: AGPL-3.0-or-later

// src/email/system.rs
//! System email account (slice E5).
//!
//! Application-initiated outbound mail — MIRA-the-software sending
//! as itself ("your password reset link", "MIRA noticed an incident
//! overnight", "thanks for joining the waitlist"). Distinct from
//! per-user email_accounts: there's no inbound, no auth_mode
//! variations, no per-user attribution.
//!
//! Wraps the existing `smtp::send` rather than re-implementing SMTP
//! — synthesises an `EmailAccountRow` from the global
//! `system_email` config on every send, hands it to the same path
//! the per-user email channel uses. That keeps reply-loop guard,
//! header construction, and TLS handling in exactly one place.
//!
//! v1 is password auth only — transactional SMTP providers
//! (Postmark / SendGrid / SES) all expose SMTP-relay credentials,
//! and the OAuth dance only makes sense for user-attributed
//! mailboxes. Add OAuth here when a use case appears.

use std::sync::Arc;

use tracing::info;

use crate::MiraError;
use crate::email::smtp::{self, OutboundMessage, ReplyLoopCache};
use crate::email::store::EmailAccountRow;
use crate::web::LiveConfig;

/// Shared service. Built once at gateway startup; cloneable
/// `Arc<SystemMailer>` for any handler / background task that needs
/// to send application mail.
pub struct SystemMailer {
    cfg:        Arc<LiveConfig>,
    loop_cache: Arc<ReplyLoopCache>,
}

impl SystemMailer {
    pub fn new(cfg: Arc<LiveConfig>, loop_cache: Arc<ReplyLoopCache>) -> Self {
        Self { cfg, loop_cache }
    }

    /// Send one message. Reads the live config snapshot every call
    /// so config edits via the Settings UI take effect on the next
    /// send without a restart.
    pub async fn send(&self, to: &str, subject: &str, body: &str) -> Result<(), MiraError> {
        let live = self.cfg.get().await;
        let s    = &live.system_email;
        if !s.enabled {
            return Err(MiraError::ConfigError(
                "system_email is not enabled — set system_email.enabled = true in config".into()
            ));
        }
        let missing = [
            ("from_address",  s.from_address.is_empty()),
            ("smtp_host",     s.smtp_host.is_empty()),
            ("smtp_username", s.smtp_username.is_empty()),
            ("smtp_password", s.smtp_password.is_empty()),
        ].iter().filter_map(|(n, m)| if *m { Some(*n) } else { None }).collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(MiraError::ConfigError(format!(
                "system_email missing required fields: {}", missing.join(", ")
            )));
        }

        // Synthesise a transient EmailAccountRow so we can reuse the
        // existing smtp::send. The fields it doesn't read (IMAP
        // creds, security_json, last_uid_seen, timestamps) are left
        // at sensible defaults — `smtp::send` only touches the SMTP
        // ones.
        let row = EmailAccountRow {
            id:        "system".into(),
            user_id:   "system".into(),
            label:     "System".into(),
            address:   format_from(&s.from_address, &s.from_name),
            auth_mode: "password".into(),
            imap_host:     None,
            imap_port:     None,
            imap_use_tls:  false,
            imap_username: None,
            imap_password: None,
            smtp_host:     Some(s.smtp_host.clone()),
            smtp_port:     Some(s.smtp_port),
            smtp_use_tls:  s.smtp_use_tls,
            smtp_username: Some(s.smtp_username.clone()),
            smtp_password: Some(s.smtp_password.clone()),
            oauth_access_token:  None,
            oauth_refresh_token: None,
            oauth_expires_at:    None,
            webhook_provider:    None,
            webhook_secret:      None,
            security_json: "{}".into(),
            enabled:       true,
            last_uid_seen: 0,
            created_at:    0,
            updated_at:    0,
        };

        let msg = OutboundMessage {
            to, subject, body,
            in_reply_to: None,
            references:  &[],
        };
        smtp::send(&row, msg, &self.loop_cache, None).await?;
        info!("system_email: sent to={to} subject={subject:?}");
        Ok(())
    }
}

/// Build the value used as the `address` on the synthetic row. When
/// `from_name` is non-empty, render as `"Name" <addr>`; lettre's
/// `Mailbox::parse` accepts that form. Otherwise just the bare
/// address.
fn format_from(addr: &str, name: &str) -> String {
    let n = if name.is_empty() { "MIRA" } else { name };
    format!("\"{n}\" <{addr}>")
}
