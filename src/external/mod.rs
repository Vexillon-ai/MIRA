// SPDX-License-Identifier: AGPL-3.0-or-later

// src/external/mod.rs
//
//! Channel Provider Protocol (CPP) — "MCP for channels".
//!
//! The generic plugin channel: an external **provider** process owns the
//! transport to some messaging system (Nextcloud Talk, IRC, …) and relays
//! messages to/from MIRA over two signed HTTP calls. New channels ship as
//! separate programs in any language — no MIRA rebuild. See
//! `design-docs/channel-provider-protocol.md` for the full spec.
//!
//! This is the symmetric counterpart of the MCP host: MCP plugs in *tools*
//! the agent calls; CPP plugs in *channels* that reach the user.
//!
//! Module shape mirrors the built-in webhook channels:
//!   * `types`    — CPP inbound/outbound bodies.
//!   * `api`      — signed outbound send + dual-direction signature
//!                  verification + the `sign` reference helper.
//!   * `dispatch` — one inbound message → AgentCore → reply, full R1+R2
//!                  routing + link-code flow, channel `external:<kind>`.
//!   * `handler`  — the shared `/webhook/external/{id}` endpoint.

pub mod api;
pub mod dispatch;
pub mod handler;
pub mod types;

pub use dispatch::{ExternalAccountCtx, ExternalDispatcherDeps};
pub use handler::{external_inbound, ExternalState};
