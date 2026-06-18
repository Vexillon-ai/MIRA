// SPDX-License-Identifier: AGPL-3.0-or-later

// src/whatsapp/mod.rs
//
//! WhatsApp channel via the Meta **WhatsApp Business Cloud API**
//! (Tier-1 #3 — bridge matrix continuation).
//!
//! Architecturally this is the **webhook** model (like Telegram's webhook
//! mode), not a long-lived task: Meta POSTs inbound messages to a public
//! `/webhook/whatsapp/{account_id}` endpoint, and we reply via the Graph
//! API. No new dependency — `reqwest` for outbound, `hmac`/`sha2` (already
//! deps) for the X-Hub-Signature-256 verification.
//!
//! Per-account config: a Cloud API `phone_number_id`, a permanent
//! `access_token`, an `app_secret` (HMAC verification), and a
//! `verify_token` (GET subscription handshake).
//!
//! ## The 24-hour window
//!
//! Meta only permits free-form text replies within 24h of the user's last
//! inbound message. Inbound-triggered replies are always inside it.
//! Proactive sends (companion/automations) may fall outside it and require
//! a pre-approved **template** — not yet implemented; such sends surface
//! Meta's 131047 error in the logs. See `design-docs/whatsapp-channel.md`.
//!
//! Module shape mirrors the other channels:
//!   * `types`    — webhook envelope + GET-verify query models.
//!   * `api`      — send_text + signature verification + chunker.
//!   * `dispatch` — one inbound message → AgentCore → reply, full R1+R2
//!                  routing + link-code flow.
//!   * `handler`  — the shared GET (verify) + POST (inbound) webhook
//!                  endpoints, account-resolved via a lookup map.

pub mod api;
pub mod dispatch;
pub mod handler;
pub mod types;

pub use dispatch::{WhatsAppAccountCtx, WhatsAppDispatcherDeps};
pub use handler::{whatsapp_inbound, whatsapp_verify, WhatsAppState};
