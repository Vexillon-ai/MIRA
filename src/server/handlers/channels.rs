// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/channels.rs
//! `GET /api/channels` — list every channel the server knows about.
//!
//! The list is built from [`ChannelRegistry`], which seeds the four built-in
//! channels and accepts plugin-contributed descriptors at runtime. The web UI
//! drives its per-channel voice grids off this endpoint so adding a channel
//! never requires a frontend change.

use std::sync::Arc;

use axum::{response::IntoResponse, Extension, Json};

use crate::auth::AuthUser;
use crate::voice::ChannelRegistry;

pub async fn list_channels(
    AuthUser(_):       AuthUser,
    Extension(reg):    Extension<Arc<ChannelRegistry>>,
) -> impl IntoResponse {
    Json(reg.list())
}
