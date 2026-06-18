// SPDX-License-Identifier: AGPL-3.0-or-later

// src/slack/mod.rs
//
//! Slack channel via the **Events API** (Tier-1 #3 — bridge matrix).
//!
//! Webhook-based (like WhatsApp + Telegram-webhook): Slack POSTs inbound
//! events to `/webhook/slack/{account_id}`; MIRA replies via the Web API
//! `chat.postMessage`. No new dependency — `reqwest` for outbound,
//! `hmac`/`sha2` (already deps) for the request-signature verification.
//!
//! Per-account config: a bot OAuth token (`xoxb-…`) + a signing secret.
//! Set up a Slack app, add the bot scopes (`chat:write`, `app_mentions:read`
//! and/or `im:history`/`message.channels`), subscribe to message events,
//! and point the Event Subscriptions Request URL at the webhook.
//!
//! Module shape mirrors the WhatsApp channel:
//!   * `types`    — Events API envelope + url_verification models.
//!   * `api`      — chat.postMessage + signature verification (v0 scheme
//!                  with the 5-minute replay window) + chunker.
//!   * `dispatch` — one inbound message → AgentCore → reply, full R1+R2
//!                  routing + link-code flow.
//!   * `handler`  — the shared POST webhook (url_verification + events),
//!                  account-resolved via a lookup map.

pub mod api;
pub mod dispatch;
pub mod handler;
pub mod types;

pub use dispatch::{SlackAccountCtx, SlackDispatcherDeps};
pub use handler::{slack_inbound, SlackState};
