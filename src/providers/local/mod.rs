// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/local/mod.rs

//! Local model providers (Ollama, llama.cpp)

pub mod ollama;

use super::ModelProvider;
pub use ollama::OllamaProvider;

/// Create a new Ollama provider instance
pub fn create_ollama(url: &str, model: &str) -> Box<dyn ModelProvider> {
    Box::new(OllamaProvider::new(url.to_string(), model.to_string()))
}
