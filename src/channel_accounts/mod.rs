// SPDX-License-Identifier: AGPL-3.0-or-later

// src/channel_accounts/mod.rs
//! Per-user channel accounts.
//!
//! Each user owns zero or more `ChannelAccount` rows — one per Signal number,
//! one per Telegram bot. Inbound messages from any channel are stamped with
//! the owning user's id so the conversation surfaces only in that user's
//! sidebar (admins see everything).
//!
//! Storage lives next to the user table in `auth.db` so the foreign key on
//! `user_id` enforces referential integrity.

pub mod legacy_migrate;
pub mod models;
pub mod store;

pub use legacy_migrate::migrate_if_empty;
pub use models::{
    ChannelAccount, ChannelKind, DiscordAccountConfig, ExternalAccountConfig,
    MatrixAccountConfig, NewChannelAccount, RoutingMode, SignalAccountConfig,
    SlackAccountConfig, TelegramAccountConfig, UpdateChannelAccount, WhatsAppAccountConfig,
};
pub use store::ChannelAccountStore;
