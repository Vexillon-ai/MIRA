// SPDX-License-Identifier: AGPL-3.0-or-later

//! Provider conformance suite (Part B3).
//!
//! The contract every [`MemoryProvider`] must satisfy — the built-in store, the
//! files baseline, an MCP-backed store, and any third-party provider (a remote HTTP service
//! included) are all judged by the same checks. Written as `async fn … ->
//! Result<(), String>` (not `#[test]`) so the same suite can back a unit test, a
//! future `mira memory conformance <id>` command, or an external provider's CI.
//!
//! Split so a read-only provider (e.g. a read-only HTTP service) runs only what applies:
//! - [`run_read_contract`] — read-side invariants for any provider.
//! - [`run_write_contract`] — the write+forget round-trip for a writable one;
//!   the caller supplies content the provider will actually store plus a keyword
//!   to find it, since providers differ (verbatim store vs. extraction).

use super::{ForgetSelector, MemoryProvider, MemoryQuery, MemoryRetrieval, TurnRecord};

/// Read-side contract — safe to run against any provider, including read-only.
pub async fn run_read_contract(p: &dyn MemoryProvider) -> Result<(), String> {
    let caps = p.capabilities();
    if !caps.read {
        return Err("a memory provider must advertise `read`".into());
    }

    // health() must return a value without panicking.
    let _ = p.health().await;

    // A never-seen user must get `Absent` (or empty `Facts`) — never `Err`,
    // never `Unavailable` from a healthy provider, and never another user's data.
    let fresh = MemoryQuery::new("conf_fresh_user_7Q", "a distinctive nonexistent subject", 5);
    match p.retrieve(&fresh).await {
        Ok(MemoryRetrieval::Absent { .. }) => {}
        // Withheld (a near/ambiguous match) is a healthy non-serve — acceptable.
        Ok(MemoryRetrieval::Withheld { .. }) => {}
        Ok(MemoryRetrieval::Facts(f)) if f.is_empty() => {}
        Ok(MemoryRetrieval::Facts(_)) => return Err("fresh user returned facts".into()),
        Ok(MemoryRetrieval::Unavailable { reason }) =>
            return Err(format!("healthy provider returned Unavailable for a fresh user: {reason}")),
        Err(e) => return Err(format!("fresh-user retrieve errored: {e}")),
    }

    // A call with an empty user_id must never return facts (the isolation key is
    // required; a provider may Err or report Absent, but must not serve data).
    if let Ok(MemoryRetrieval::Facts(f)) = p.retrieve(&MemoryQuery::new("", "x", 5)).await {
        if !f.is_empty() {
            return Err("empty user_id returned facts (isolation key not enforced)".into());
        }
    }

    // forget of a bogus id must be graceful (Ok or Err, never a panic/hang).
    if caps.forget {
        let _ = p.forget(&ForgetSelector::Id {
            caller_user_id: "conf_fresh_user_7Q".into(),
            item: "conf-nonexistent-id".into(),
            caller_username: None,
            caller_email: None,
        }).await;
    }
    Ok(())
}

/// Write+forget round-trip for a writable provider. `content` is ingested for
/// `user`; `keyword` must appear in the stored fact so retrieval can find it.
pub async fn run_write_contract(
    p: &dyn MemoryProvider,
    user: &str,
    content: &str,
    keyword: &str,
) -> Result<(), String> {
    let caps = p.capabilities();
    if !caps.write {
        return Err("run_write_contract on a non-write provider".into());
    }
    let turn = TurnRecord {
        user_id: user.into(), agent_id: None, thread_id: None, channel: "conf".into(),
        user_msg: content.into(), assistant_msg: String::new(), conversation_id: None,
    };
    p.ingest(&turn).await.map_err(|e| format!("ingest: {e}"))?;

    let kw = keyword.to_lowercase();
    let facts = match p.retrieve(&MemoryQuery::new(user, keyword, 10)).await {
        Ok(MemoryRetrieval::Facts(f)) => f,
        Ok(other) => return Err(format!("after ingest, expected Facts, got {other:?}")),
        Err(e) => return Err(format!("post-ingest retrieve errored: {e}")),
    };
    let fact = facts.iter().find(|f| f.content.to_lowercase().contains(&kw))
        .ok_or_else(|| "ingested content not found on retrieve".to_string())?;
    // Every fact must carry the provider's id as source_id.
    if fact.source_id != p.id() {
        return Err(format!("fact source_id '{}' != provider id '{}'", fact.source_id, p.id()));
    }

    // Isolation: a different user must not see the ingested fact.
    if let Ok(MemoryRetrieval::Facts(other)) =
        p.retrieve(&MemoryQuery::new("conf_other_user_9Z", keyword, 10)).await
    {
        if other.iter().any(|f| f.content.to_lowercase().contains(&kw)) {
            return Err("cross-user leak: another user saw the ingested fact".into());
        }
    }

    // Forget removes it.
    if caps.forget {
        let id = fact.id.clone().ok_or_else(|| "writable fact has no id to forget".to_string())?;
        // The caller owns this fact; for an id of the form "<user>/<item>" the
        // caller must equal that user segment (the files cross-user guard).
        let caller = id.split_once('/').map(|(u, _)| u.to_string()).unwrap_or_else(|| id.clone());
        p.forget(&ForgetSelector::Id { caller_user_id: caller, item: id, caller_username: None, caller_email: None }).await.map_err(|e| format!("forget: {e}"))?;
        if let Ok(MemoryRetrieval::Facts(after)) =
            p.retrieve(&MemoryQuery::new(user, keyword, 10)).await
        {
            if after.iter().any(|f| f.content.to_lowercase().contains(&kw)) {
                return Err("fact still present after forget".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemorySystem;
    use crate::memory::provider::{builtin::BuiltinMemoryProvider, files::FilesMemoryProvider};
    use std::sync::Arc;
    use tempfile::tempdir;

    #[tokio::test]
    async fn builtin_passes_read_contract() {
        let dir = tempdir().unwrap();
        let m = Arc::new(MemorySystem::new(dir.path().join("m.db")).unwrap());
        let p = BuiltinMemoryProvider::new(m);
        run_read_contract(&p).await.expect("builtin read contract");
    }

    #[tokio::test]
    async fn files_passes_read_and_write_contract() {
        let dir = tempdir().unwrap();
        let p = FilesMemoryProvider::new("files", dir.path().to_path_buf());
        run_read_contract(&p).await.expect("files read contract");
        // Files stores the turn verbatim, so any keyword in the content is found.
        run_write_contract(&p, "conf_user_a", "the user owns a vintage typewriter", "typewriter")
            .await.expect("files write contract");
    }
}
