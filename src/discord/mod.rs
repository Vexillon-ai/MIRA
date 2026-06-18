// SPDX-License-Identifier: AGPL-3.0-or-later

// src/discord/mod.rs
//
//! Discord channel (Tier-1 #3 continuation, slices D1+D2).
//!
//! Per-user Discord bot handled as a first-class MIRA channel:
//!   * D1 — `ChannelKind::Discord` + `DiscordAccountConfig` + storage +
//!     Settings UI to add bots (mirrors the Telegram per-user-bot model).
//!   * D2 — Inbound: persistent WebSocket gateway connection per enabled
//!     account, MESSAGE_CREATE → AgentCore dispatch, minimal text reply
//!     posted back via REST.
//!
//! D3+ (deferred): companion / automations / notification-bus dispatch,
//! slash commands, ephemeral interactions, attachments, voice notes.
//!
//! Multi-user model:
//!   Each MIRA user registers their own Discord application + bot in the
//!   Discord Developer Portal and configures it through Settings →
//!   Channels. There is NO shared MIRA-operator bot — every user owns
//!   their own identity on the channel, matching how Signal/Telegram
//!   accounts already work. Per-bot rate limits, per-bot DM scope, and
//!   per-bot token rotation all follow naturally from this. Documented
//!   in `design-docs/discord-channel.md`.

pub mod api;
pub mod dispatch;
pub mod gateway;
pub mod types;

pub use dispatch::DiscordDispatcherDeps;
pub use gateway::{spawn_gateway, DiscordAccountCtx};
