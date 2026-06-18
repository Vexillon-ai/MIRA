// SPDX-License-Identifier: AGPL-3.0-or-later

// src/email/mod.rs
//! Email channel (Q2 #8, slice E1+E3).
//!
//! Per-user IMAP/SMTP mailbox handled as a first-class MIRA channel:
//! the poller reads inbound, the security pipeline runs every message
//! through allowlist + sanitisation + header checks + rate limits, and
//! survivors are routed into the conversation system the same way
//! Signal/Telegram inbound is. Outbound (SMTP) lands in slice E2.
//!
//! See `design-docs/email-channel.md` for the full design, threat model, and
//! slicing plan — that's the source of truth; this module implements it.
//!
//! Chunks landing across multiple commits:
//!   * Chunk 1 (this file + `store.rs`): account row, CRUD plumbing.
//!     The channel is not yet runnable end-to-end after this commit.
//!   * Chunk 2: `imap_poll.rs` — async-imap daemon per account, wired
//!     into `channel_manager`.
//!   * Chunk 3: `parser.rs` + `security.rs` — MIME walk, HTML
//!     sanitisation, header inspection, allowlist + size limits.
//!   * Chunk 4: `dispatch.rs` — thread matching, prompt-injection
//!     wrapping, narrow tool allowlist, agent invocation.
//!   * Chunk 5: `quarantine.rs` + audit table + UI surface.
//!   * Chunk 6: rate limits + final wiring; bump version.

pub mod audit;
pub mod dispatch;
pub mod imap_poll;
pub mod oauth;
pub mod parser;
pub mod quarantine;
pub mod rate;
pub mod runtime;
pub mod security;
pub mod smtp;
pub mod store;
pub mod system;
pub mod webhook;

pub use audit::{AuditEntry, EmailAuditStore, NewAuditEntry};
pub use dispatch::{dispatch_inbound, DispatchError};
pub use imap_poll::EmailPollerStatus;
pub use parser::{parse_email, ParseSettings, ParsedEmail};
pub use quarantine::{EmailQuarantineStore, NewQuarantineEntry, QuarantineEntry};
pub use oauth::{OAuthProvider, OAuthStateStore};
pub use rate::InMemoryRateLimiter;
pub use runtime::EmailPollerRegistry;
pub use security::{evaluate, InboundHeaders, InboundRateLimiter, NoopRateLimiter, Verdict};
pub use smtp::{reply_subject, send as smtp_send, send_for_account as smtp_send_for_account, OutboundMessage, ReplyLoopCache};
pub use system::SystemMailer;
pub use store::{
    EmailAccountRow, EmailAccountStore, EmailSecurity,
    NewEmailAccount, UpdateEmailAccount,
};
