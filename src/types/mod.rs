// SPDX-License-Identifier: AGPL-3.0-or-later

// src/types/mod.rs

mod message;
mod memory;
pub mod provider;
pub mod tool;

pub use message::*;
pub use memory::*;
pub use provider::*;
pub use tool::*;
use serde::{Deserialize, Serialize};

/// Unique identifier for memories (re-export)
pub type MemoryId = uuid::Uuid;

/// Token usage tracking
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl TokenUsage {
    /// Calculate cost at given price per 1K tokens
    pub fn cost_at_price_per_token(&self, price: f64) -> f64 {
        (self.total_tokens as f64) * price / 1000.0
    }
}
