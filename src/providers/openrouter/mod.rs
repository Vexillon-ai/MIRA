// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/openrouter/mod.rs

//! OpenRouter gateway provider implementation

pub mod client;
pub mod catalog;
pub mod pricing;

use super::ModelProvider;
pub use client::OpenRouterProvider;
pub use catalog::{Catalog, CatalogEntry, Pricing};
pub use pricing::{cost_for, format_usd};

/// Create a new OpenRouter provider instance
pub fn create_openrouter(api_key: &str, model: &str) -> Box<dyn ModelProvider> {
    Box::new(OpenRouterProvider::new(api_key.to_string(), model.to_string()))
}

/// Create an OpenRouter provider from environment variable
pub fn create_from_env(model: &str) -> Option<Box<dyn ModelProvider>> {
    std::env::var("OPENROUTER_API_KEY")
        .ok()
        .map(|api_key| Box::new(OpenRouterProvider::new(api_key, model.to_string())) as Box<dyn ModelProvider>)
}
