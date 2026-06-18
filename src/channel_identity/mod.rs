// SPDX-License-Identifier: AGPL-3.0-or-later

// src/channel_identity/mod.rs
//
//! Per-user channel identity store (R1+R2).
//!
//! When a `ChannelAccount` is in `Shared` or `GuestOk` routing mode, the
//! dispatcher needs to translate the inbound sender's external id
//! (Discord snowflake / Telegram chat id / Signal phone) into a MIRA
//! user id so the agent runs as the right person.
//!
//! This module owns two small tables, both in `auth.db`:
//!
//! - `user_channel_links` — confirmed `(user_id, channel, external_id)`
//!   mappings. One row per identity-on-a-channel; `UNIQUE(channel,
//!   external_id)` prevents two MIRA users from claiming the same
//!   external account on the same channel.
//!
//! - `channel_link_codes` — short-lived one-time codes generated when a
//!   user clicks "Link Discord" in the web UI. The user DMs the code to
//!   the admin's bot; the dispatcher recognises the `LINK-XXXX` pattern,
//!   calls `consume()`, and on success creates the link row + replies.
//!
//! The store is intentionally minimal — no soft-deletes, no audit log
//! (that lives in `tool_audit.db`), no per-account scoping (a link works
//! for every bot on a given channel; a user only has one Discord id).

pub mod link_codes;
pub mod store;

pub use link_codes::{ChannelLinkCode, LinkCodeStore};
pub use store::{ChannelLink, IdentityStore};
