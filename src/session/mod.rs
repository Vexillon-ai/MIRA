// SPDX-License-Identifier: AGPL-3.0-or-later

// src/session/mod.rs

//! Session management for multi-channel conversations
//! 
//! Provides:
//! - Per-user session tracking across channels
//! - Conversation history persistence
//! - Context window management

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

/// Session identifier (channel-specific)
pub type SessionId = String;

/// User identifier across channels
pub type UserId = String;

/// Session data stored in memory
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionData {
    pub session_id: SessionId,
    pub user_id: UserId,
    pub channel: String,  // "cli", "telegram", "signal"
    pub created_at: u64,  // Unix timestamp
    pub last_active: u64,
    /// Conversation history (user + assistant messages)
    pub conversation_history: Vec<ConversationTurn>,
    /// Session metadata
    pub metadata: HashMap<String, serde_json::Value>,
}

/// A single turn in a conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationTurn {
    pub role: String,  // "user" or "assistant"
    pub content: String,
    pub timestamp: u64,
}

impl SessionData {
    pub fn new(session_id: SessionId, user_id: UserId, channel: String) -> Self {
        let now = chrono::Utc::now().timestamp() as u64;
        Self {
            session_id,
            user_id,
            channel,
            created_at: now,
            last_active: now,
            conversation_history: Vec::new(),
            metadata: HashMap::new(),
        }
    }
    
    /// Add a turn to the conversation
    pub fn add_turn(&mut self, role: &str, content: String) {
        let now = chrono::Utc::now().timestamp() as u64;
        self.conversation_history.push(ConversationTurn {
            role: role.to_string(),
            content,
            timestamp: now,
        });
        self.last_active = now;
    }
    
    /// Get conversation history as messages
    pub fn to_messages(&self) -> Vec<crate::ChatMessage> {
        self.conversation_history
            .iter()
            .map(|turn| match turn.role.as_str() {
                "user" => crate::ChatMessage::user(turn.content.clone()),
                "assistant" => crate::ChatMessage::assistant(turn.content.clone()),
                _ => crate::ChatMessage::user(turn.content.clone()),
            })
            .collect()
    }
    
    /// Truncate history to keep only last N turns
    pub fn truncate_history(&mut self, max_turns: usize) {
        if self.conversation_history.len() > max_turns {
            let excess = self.conversation_history.len() - max_turns;
            self.conversation_history.drain(0..excess);
            debug!("Truncated session {} history from {} to {} turns",
                  self.session_id, self.conversation_history.len() + excess, max_turns);
        }
    }
}

/// In-memory session store (production: replace with Redis)
pub struct SessionStore {
    sessions: Arc<RwLock<HashMap<SessionId, SessionData>>>,
    /// Session timeout in seconds (default: 1 hour)
    timeout_secs: u64,
}

impl SessionStore {
    pub fn new() -> Self {
        info!("Initializing in-memory session store");
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            timeout_secs: 3600,  // 1 hour
        }
    }

    /// Create a session store with explicit configuration.
    pub fn new_with_config(_max_turns: usize, timeout_secs: u64) -> Self {
        info!("Initializing session store (timeout={}s)", timeout_secs);
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            timeout_secs,
        }
    }

    /// Spawn a background tokio task that calls `cleanup_expired()` every
    /// `interval_secs` seconds. Returns the task handle (drop to cancel).
    pub fn start_cleanup_task(
        store: std::sync::Arc<Self>,
        interval_secs: u64,
    ) -> tokio::task::JoinHandle<()> {
        info!("Starting session cleanup task (interval={}s)", interval_secs);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(
                tokio::time::Duration::from_secs(interval_secs)
            );
            ticker.tick().await; // skip first immediate tick
            loop {
                ticker.tick().await;
                let removed = store.cleanup_expired().await;
                if removed > 0 {
                    info!("Session cleanup: removed {} expired sessions", removed);
                }
            }
        })
    }
    
    /// Get or create a session
    pub async fn get_or_create(&self, session_id: SessionId, user_id: UserId, channel: String) -> SessionData {
        let mut sessions = self.sessions.write().await;
        
        // Check for existing session
        if let Some(session) = sessions.get(&session_id).cloned() {
            debug!("Retrieved existing session {}", session_id);
            return session;
        }
        
        // Create new session
        let session = SessionData::new(session_id.clone(), user_id.clone(), channel.clone());
        sessions.insert(session_id.clone(), session.clone());
        info!("Created new session {} for user {} on channel {}", 
              session_id, user_id, channel);
        
        session
    }
    
    /// Like [`Self::get_or_create`], but when no session exists yet the new
    /// one is pre-populated with `seed` instead of starting empty.
    ///
    /// The session store is an **in-memory cache** — a process restart or the
    /// 1-hour idle eviction ([`Self::cleanup_expired`]) wipes it. The
    /// persisted history DB, not this cache, is the source of truth for a
    /// conversation. Seeding on a cache miss is what lets the agent pick up an
    /// existing conversation's context after a restart instead of acting as
    /// though the thread were brand new (even though the UI still shows it).
    ///
    /// An already-live session is returned **untouched** (`seed` ignored), so
    /// an in-flight conversation is never clobbered by a stale replay.
    pub async fn get_or_create_seeded(
        &self,
        session_id: SessionId,
        user_id:    UserId,
        channel:    String,
        seed:       Vec<ConversationTurn>,
    ) -> SessionData {
        let mut sessions = self.sessions.write().await;

        if let Some(session) = sessions.get(&session_id).cloned() {
            debug!("Retrieved existing session {}", session_id);
            return session;
        }

        let mut session = SessionData::new(session_id.clone(), user_id.clone(), channel.clone());
        let seeded = seed.len();
        session.conversation_history = seed;
        sessions.insert(session_id.clone(), session.clone());
        info!("Created session {} for user {} on channel {} (rehydrated {} turns from history)",
              session_id, user_id, channel, seeded);

        session
    }

    /// Update a session (merge changes)
    pub async fn update(&self, session: SessionData) {
        let mut sessions = self.sessions.write().await;
        sessions.insert(session.session_id.clone(), session);
    }
    
    /// Remove expired sessions
    pub async fn cleanup_expired(&self) -> usize {
        let now = chrono::Utc::now().timestamp() as u64;
        let mut sessions = self.sessions.write().await;
        
        let before_len = sessions.len();
        sessions.retain(|_, session| {
            now - session.last_active < self.timeout_secs
        });
        let removed = before_len - sessions.len();
        
        if removed > 0 {
            info!("Cleaned up {} expired sessions", removed);
        }
        
        removed
    }
    
    /// Get session count
    pub async fn len(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// List all active sessions.
    pub async fn list_all(&self) -> Vec<SessionData> {
        let sessions = self.sessions.read().await;
        sessions.values().cloned().collect()
    }

    /// Evict (forcefully remove) a session by ID. Returns true if it existed.
    pub async fn evict(&self, session_id: &str) -> bool {
        let mut sessions = self.sessions.write().await;
        sessions.remove(session_id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_session_creation() {
        let store = SessionStore::new();
        
        let session = store.get_or_create(
            "session-123".to_string(),
            "user-456".to_string(),
            "telegram".to_string()
        ).await;
        
        assert_eq!(session.session_id, "session-123");
        assert_eq!(session.user_id, "user-456");
        assert_eq!(session.channel, "telegram");
    }
    
    #[tokio::test]
    async fn test_conversation_history() {
        let store = SessionStore::new();
        
        let mut session = store.get_or_create(
            "session-789".to_string(),
            "user-abc".to_string(),
            "cli".to_string()
        ).await;
        
        // Add some turns
        session.add_turn("user", "Hello!".to_string());
        session.add_turn("assistant", "Hi there! How can I help?".to_string());
        session.add_turn("user", "What's the weather like?".to_string());
        
        assert_eq!(session.conversation_history.len(), 3);
        
        // Convert to messages
        let messages = session.to_messages();
        assert_eq!(messages.len(), 3);
    }
    
    #[tokio::test]
    async fn test_session_truncation() {
        let store = SessionStore::new();
        
        let mut session = store.get_or_create(
            "session-truncate".to_string(),
            "user-test".to_string(),
            "signal".to_string()
        ).await;
        
        // Add many turns
        for i in 0..100 {
            session.add_turn("user", format!("Message {}", i));
            session.add_turn("assistant", format!("Response {}", i));
        }
        
        assert_eq!(session.conversation_history.len(), 200);
        
        // Truncate to last 50 turns
        session.truncate_history(50);
        assert_eq!(session.conversation_history.len(), 50);
    }

    #[tokio::test]
    async fn test_session_update_persists() {
        let store = SessionStore::new();
        let mut session = store.get_or_create("s1".to_string(), "u1".to_string(), "cli".to_string()).await;
        session.add_turn("user", "Hello".to_string());
        store.update(session).await;

        // Retrieve again; history should still be there
        let retrieved = store.get_or_create("s1".to_string(), "u1".to_string(), "cli".to_string()).await;
        assert_eq!(retrieved.conversation_history.len(), 1);
    }

    #[tokio::test]
    async fn test_seeded_session_rehydrates_on_miss() {
        // Simulates a restart: the cache is empty, but history has prior turns.
        let store = SessionStore::new();
        let seed = vec![
            ConversationTurn { role: "user".into(),      content: "first".into(),  timestamp: 1 },
            ConversationTurn { role: "assistant".into(), content: "reply".into(), timestamp: 2 },
        ];
        let s = store
            .get_or_create_seeded("web-c1".into(), "u1".into(), "web".into(), seed)
            .await;
        assert_eq!(s.conversation_history.len(), 2, "fresh session must carry the seed");
        assert_eq!(s.conversation_history[0].content, "first");
    }

    #[tokio::test]
    async fn test_seeded_session_does_not_clobber_live_session() {
        // A live, in-flight session must NOT be overwritten by a stale replay.
        let store = SessionStore::new();
        let mut live = store.get_or_create("web-c1".into(), "u1".into(), "web".into()).await;
        live.add_turn("user", "live turn".into());
        store.update(live).await;

        let stale_seed = vec![
            ConversationTurn { role: "user".into(), content: "stale".into(), timestamp: 1 },
        ];
        let s = store
            .get_or_create_seeded("web-c1".into(), "u1".into(), "web".into(), stale_seed)
            .await;
        assert_eq!(s.conversation_history.len(), 1);
        assert_eq!(s.conversation_history[0].content, "live turn", "seed must be ignored for a live session");
    }

    #[tokio::test]
    async fn test_store_len() {
        let store = SessionStore::new();
        assert_eq!(store.len().await, 0);
        store.get_or_create("s1".to_string(), "u1".to_string(), "cli".to_string()).await;
        store.get_or_create("s2".to_string(), "u2".to_string(), "telegram".to_string()).await;
        assert_eq!(store.len().await, 2);
    }

    #[tokio::test]
    async fn test_to_messages_roles() {
        let mut session = SessionData::new("s".to_string(), "u".to_string(), "cli".to_string());
        session.add_turn("user", "question".to_string());
        session.add_turn("assistant", "answer".to_string());
        let msgs = session.to_messages();
        assert_eq!(msgs[0].role, crate::types::MessageRole::User);
        assert_eq!(msgs[1].role, crate::types::MessageRole::Assistant);
    }

    #[tokio::test]
    async fn test_truncation_keeps_most_recent() {
        let mut session = SessionData::new("s".to_string(), "u".to_string(), "cli".to_string());
        for i in 0..10 {
            session.add_turn("user", format!("msg-{}", i));
        }
        session.truncate_history(3);
        assert_eq!(session.conversation_history.len(), 3);
        assert_eq!(session.conversation_history[0].content, "msg-7");
        assert_eq!(session.conversation_history[2].content, "msg-9");
    }

    #[tokio::test]
    async fn test_cleanup_expired_removes_old_sessions() {
        use std::sync::Arc;
        let store = Arc::new(SessionStore::new_with_config(5, 1)); // 1-second timeout
        store.get_or_create("s1".into(), "u1".into(), "cli".into()).await;
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        let removed = store.cleanup_expired().await;
        assert_eq!(removed, 1);
        assert_eq!(store.len().await, 0);
    }

    #[test]
    fn test_new_with_config() {
        let store = SessionStore::new_with_config(30, 7200);
        drop(store);
    }
}