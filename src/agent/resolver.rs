// SPDX-License-Identifier: AGPL-3.0-or-later

//! Production [`SkillExecutorResolver`] — maps known skill IDs to the
//! adapters that run them.
//!
//! The supervisor consults this when a worker requests a child Skill
//! and when the user-facing `spawn_background_task` tool kicks off a
//! new top-level worker. Without a real resolver wired, every spawn
//! request is denied (`NullExecutorResolver` is the default).
//!
//! Owners of new adapters register them via [`MiraSkillResolver::with_skill`]
//! at gateway-build time. Lookup is `O(1)` over a `HashMap`.

use std::collections::HashMap;
use std::sync::Arc;

use super::supervisor::{SkillExecutorResolver, WorkerTask};

pub struct MiraSkillResolver {
    by_skill: HashMap<String, Arc<dyn WorkerTask>>,
}

impl MiraSkillResolver {
    pub fn new() -> Self {
        Self { by_skill: HashMap::new() }
    }

    pub fn with_skill(
        mut self,
        skill_id: impl Into<String>,
        executor: Arc<dyn WorkerTask>,
    ) -> Self {
        self.by_skill.insert(skill_id.into(), executor);
        self
    }

    pub fn known_skills(&self) -> Vec<String> {
        let mut v: Vec<String> = self.by_skill.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn is_empty(&self) -> bool { self.by_skill.is_empty() }
}

impl Default for MiraSkillResolver {
    fn default() -> Self { Self::new() }
}

impl SkillExecutorResolver for MiraSkillResolver {
    fn executor_for(&self, skill_id: &str) -> Option<Arc<dyn WorkerTask>> {
        self.by_skill.get(skill_id).cloned()
    }
}

/// Tries each inner resolver in order, returning the first executor found.
/// Used to compose the built-in skill resolver with the dynamic
/// [`NamedAgentResolver`](super::named_agent::NamedAgentResolver) so a spawn
/// can target either a packaged skill (`com.mira.*`) or a user-defined named
/// agent (`named:<handle>`) through the same `executor_for` lookup.
pub struct ChainedResolver {
    inner: Vec<Arc<dyn SkillExecutorResolver>>,
}

impl ChainedResolver {
    pub fn new(inner: Vec<Arc<dyn SkillExecutorResolver>>) -> Self {
        Self { inner }
    }
}

impl SkillExecutorResolver for ChainedResolver {
    fn executor_for(&self, skill_id: &str) -> Option<Arc<dyn WorkerTask>> {
        self.inner.iter().find_map(|r| r.executor_for(skill_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use crate::agent::supervisor::{
        WorkerAssignment, WorkerComplete, WorkerContext, WorkerFailure,
    };

    struct DummyExec(&'static str);
    #[async_trait]
    impl WorkerTask for DummyExec {
        async fn run(
            &self,
            _: WorkerAssignment,
            _: WorkerContext,
        ) -> Result<WorkerComplete, WorkerFailure> {
            Ok(WorkerComplete {
                result_summary: self.0.into(),
                artifacts: vec![],
            })
        }
    }

    #[test]
    fn maps_skill_to_executor() {
        let r = MiraSkillResolver::new()
            .with_skill("com.mira.research", Arc::new(DummyExec("r")))
            .with_skill("com.mira.claudecode",   Arc::new(DummyExec("c")));
        assert!(r.executor_for("com.mira.research").is_some());
        assert!(r.executor_for("com.mira.claudecode").is_some());
        assert!(r.executor_for("com.mira.unknown").is_none());
        assert_eq!(r.known_skills(), vec!["com.mira.claudecode", "com.mira.research"]);
    }

    #[test]
    fn empty_resolver_denies_all() {
        let r = MiraSkillResolver::new();
        assert!(r.is_empty());
        assert!(r.executor_for("com.mira.research").is_none());
    }
}
