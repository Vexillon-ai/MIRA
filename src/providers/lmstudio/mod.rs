// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/lmstudio/mod.rs

//! LM Studio local LLM provider implementation
//! Uses OpenAI-compatible API format

pub mod client;

use super::ModelProvider;
pub use client::LmStudioProvider;

/// Create a new LM Studio provider instance
pub fn create_lmstudio(url: &str, model: &str) -> Box<dyn ModelProvider> {
    Box::new(LmStudioProvider::new(url.to_string(), model.to_string()))
}
