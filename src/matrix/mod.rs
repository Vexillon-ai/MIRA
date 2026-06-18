// SPDX-License-Identifier: AGPL-3.0-or-later

// src/matrix/mod.rs
//
//! Matrix channel (Tier-1 #3 — bridge matrix continuation).
//!
//! Per-user (or shared admin) Matrix bot. Unlike Discord's WebSocket
//! gateway, Matrix uses HTTP long-polling against the Client-Server API
//! `/sync` endpoint — so this channel reuses `reqwest` with no new
//! dependency. A user provides a homeserver URL + an access token (an
//! Element "access token" from Settings → Help & About → Advanced, or one
//! minted via /login).
//!
//! Shape mirrors the Discord channel exactly:
//!   * `types`     — partial /sync + /whoami response models.
//!   * `api`       — send_message / whoami / join_room / sync_once + the
//!                   chunker.
//!   * `dispatch`  — one inbound event → AgentCore → reply, with the full
//!                   R1+R2 routing + link-code flow.
//!   * `sync_loop` — the long-lived long-poll task (the inbound daemon),
//!                   analogous to `discord::gateway`.
//!
//! Outbound proactive delivery (companion check-ins + automations) is
//! wired through `ChannelAccountStore::outbound_matrix_token` +
//! `matrix::api::send_message`, the same personal-first / shared-bot
//! fallback the Telegram + Discord paths use.

pub mod api;
pub mod dispatch;
pub mod sync_loop;
pub mod types;

pub use dispatch::{MatrixDispatcherDeps};
pub use sync_loop::{spawn_sync_loop, MatrixLoopConfig};
