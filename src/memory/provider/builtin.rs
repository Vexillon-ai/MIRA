// SPDX-License-Identifier: AGPL-3.0-or-later

//! `BuiltinMemoryProvider` — a thin [`MemoryProvider`] adapter over the
//! existing [`MemorySystem`]. It owns nothing new; it maps the trait verbs onto
//! the methods `MemorySystem` already exposes, so the built-in SQLite store is
//! just "provider `builtin`" to the rest of the seam.

use std::sync::Arc;
use async_trait::async_trait;

use super::{
    ForgetSelector, MemoryCapabilities, MemoryError, MemoryFact, MemoryHealth,
    MemoryLocality, MemoryProvider, MemoryQuery, MemoryRetrieval, Speaker, Trust,
};
use crate::memory::{MemorySystem, storage::MemoryItem};

/// Provider id for the built-in store.
pub const BUILTIN_ID: &str = "builtin";

pub struct BuiltinMemoryProvider {
    memory: Arc<MemorySystem>,
}

impl BuiltinMemoryProvider {
    pub fn new(memory: Arc<MemorySystem>) -> Self { Self { memory } }
}

/// Map a stored row to a provider fact. The built-in store is local + first-
/// party but does not *attest* facts, so trust stays `Unverified` and
/// `complete_set` stays false (it can't enumerate a scope — `can_assert_complete`
/// is false).
fn to_fact(item: MemoryItem) -> MemoryFact {
    MemoryFact {
        id:          Some(item.id.to_string()),
        content:     item.content,
        score:       Some(item.relevance_score),
        recorded_at: Some(item.created_at),
        valid_at:    None,
        source_id:   BUILTIN_ID.to_string(),
        provenance:  None,
        confidence:  None,
        speaker:     Speaker::parse(&item.source_speaker),
        trust:       Trust::Unverified,
        complete_set: false,
        provider_meta: serde_json::Value::Null,
    }
}

#[async_trait]
impl MemoryProvider for BuiltinMemoryProvider {
    fn id(&self) -> &str { BUILTIN_ID }

    fn capabilities(&self) -> MemoryCapabilities {
        MemoryCapabilities {
            read: true, write: true, forget: true, update: false, supersede: true,
            ingest_conversation: true, temporal: false, graph: false, citations: false,
            // The SQLite store retrieves by semantic top-k; it cannot enumerate a
            // scope, so it never claims completeness (this is the A.5 fix as a
            // capability rather than a hardcoded string).
            can_assert_complete: false,
            // No catalog surface: the store can't enumerate "what is held" by
            // subject the way a remote HTTP source can.
            catalog: false,
        }
    }

    fn locality(&self) -> MemoryLocality { MemoryLocality::Loopback }

    async fn health(&self) -> MemoryHealth { MemoryHealth::Ready }

    async fn retrieve(&self, q: &MemoryQuery) -> Result<MemoryRetrieval, MemoryError> {
        if q.user_id.trim().is_empty() {
            return Err(MemoryError::InvalidRequest("user_id is required".into()));
        }
        match self.memory
            .search_visible_for_context(&q.query, &q.user_id, &q.group_ids, q.k)
            .await
        {
            Ok(items) if items.is_empty() => Ok(MemoryRetrieval::Absent { reason: None }),
            Ok(items) => {
                // Reinforce on AGENT recall (this path serves the per-turn hook and
                // the memory_search tool). Browsing the Memory page must NOT change
                // ranking, so the HTTP list/get/search handlers no longer reinforce.
                for it in &items {
                    self.memory.reinforce(it.id, &q.user_id);
                }
                Ok(MemoryRetrieval::Facts(items.into_iter().map(to_fact).collect()))
            }
            // The correctness fix: a store error becomes an explicit `Unavailable`,
            // never an empty result the model reads as "the user has no such memory".
            Err(e) => Ok(MemoryRetrieval::Unavailable { reason: e.to_string() }),
        }
    }

    async fn ingest(&self, turn: &crate::memory::provider::TurnRecord) -> Result<usize, MemoryError> {
        // The built-in local (model-free) ingest: the heuristic extractor over
        // the user turn, user-scoped. The richer LLM extraction stays on the
        // turn path (routed via the memory_extract alias, A.8) until B2 unifies
        // the two under harness-driven ingest.
        // Harness ingest: allow all categories here (empty = allow all); the
        // capture-mode gate for harness ingest is applied by the caller.
        let n = self.memory
            .auto_extract_and_store(&turn.user_msg, &turn.user_id, &turn.channel, &[], false)
            .await?;
        Ok(n)
    }

    async fn forget(&self, sel: &ForgetSelector) -> Result<usize, MemoryError> {
        match sel {
            ForgetSelector::Id { caller_user_id, item, .. } => {
                let id: u64 = item.parse()
                    .map_err(|_| MemoryError::InvalidRequest(format!("bad memory id '{item}'")))?;
                // Authorize IN the provider (the SPI contract requires it): only a
                // live, user-scoped row owned by the caller is deleted. A cross-user
                // or system/group id affects nothing — so a crafted numeric id can
                // never reach another user's (or a system) memory even if a caller
                // skips its own scope check.
                Ok(self.memory.soft_delete_owned(id, caller_user_id).await? as usize)
            }
            ForgetSelector::AllForUser(user_id) => {
                self.memory.purge_user(user_id).await?;
                Ok(1) // purge_user doesn't report a count; signal success
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemorySystem;
    use tempfile::tempdir;

    async fn mem() -> Arc<MemorySystem> {
        let dir = tempdir().unwrap();
        // Keep the temp dir alive for the test process.
        let path = dir.path().join("m.db");
        std::mem::forget(dir);
        Arc::new(MemorySystem::new(path).unwrap())
    }

    #[tokio::test]
    async fn builtin_capabilities_and_locality() {
        let p = BuiltinMemoryProvider::new(mem().await);
        assert_eq!(p.id(), "builtin");
        let c = p.capabilities();
        assert!(c.read && c.write && c.forget);
        assert!(!c.can_assert_complete, "SQLite top-k can't claim completeness");
        assert_eq!(p.locality(), MemoryLocality::Loopback);
        assert_eq!(p.health().await, MemoryHealth::Ready);
    }

    #[tokio::test]
    async fn retrieve_empty_is_absent_not_error() {
        let p = BuiltinMemoryProvider::new(mem().await);
        let q = MemoryQuery::new("alice", "anything", 5);
        match p.retrieve(&q).await.unwrap() {
            MemoryRetrieval::Absent { .. } => {}
            other => panic!("expected Absent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retrieve_requires_user_id() {
        let p = BuiltinMemoryProvider::new(mem().await);
        let q = MemoryQuery::new("", "x", 5);
        assert!(matches!(p.retrieve(&q).await, Err(MemoryError::InvalidRequest(_))));
    }
}
