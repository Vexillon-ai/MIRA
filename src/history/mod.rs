// SPDX-License-Identifier: AGPL-3.0-or-later

// src/history/mod.rs

pub mod indexer;
pub mod models;
pub mod storage;

pub use indexer::{IndexerConfig, MessageIndexer};
pub use models::{
    ChannelStats, Conversation, HistoryStats, Message, MessageRole,
    NewConversation, NewMessage,
};
pub use storage::{
    is_conversation_reset_command,
    HistoryStore, MessageVectorHit, MessageVectorRow, UnindexedMessage,
};
