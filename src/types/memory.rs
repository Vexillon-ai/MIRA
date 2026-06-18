// SPDX-License-Identifier: AGPL-3.0-or-later

// src/types/memory.rs
//
// NOTE: Category, MemoryItem, and MemorySource are defined in `memory::storage`
// and re-exported via `memory::mod` → `lib.rs`.  This file only contains types
// that are unique to the types layer.

/// Query parameters for memory retrieval
#[derive(Debug, Clone)]
pub struct MemoryQuery {
    pub query: String,
    pub category_filter: Option<crate::memory::storage::Category>,
    pub limit: usize,
    pub min_relevance: f32,
}

impl Default for MemoryQuery {
    fn default() -> Self {
        Self {
            query: String::new(),
            category_filter: None,
            limit: 10,
            min_relevance: 0.0,
        }
    }
}
