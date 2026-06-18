// SPDX-License-Identifier: AGPL-3.0-or-later

//! LLM-call policy helper — phase 1.3.
//!
//! Tiny seam over [`PolicyEngine`] specifically for the LLM client
//! integration point. Callers (currently the agent's tool loop, future
//! call sites in chat handler / adapters) build an [`LlmCallContext`]
//! and call [`check_llm_call`] before issuing a `provider.generate*()`
//! call. On `Allow` the function returns Ok and the caller proceeds;
//! on `Deny` it returns [`MiraError::PolicyDenied`] which the caller
//! propagates with the standard `?` operator.
//!
//! Why a helper instead of a wrapper trait impl:
//! - The provider trait stays unchanged — every provider impl
//!   keeps working without code changes.
//! - Each call site explicitly opts in by constructing the context.
//!   No surprise gating in code paths that don't have agent context
//!   (memory rollup, summarizer, onboarding extractor) — those
//!   remain pure MIRA-internal calls.
//! - The `running_cost_usd` / `session_cost_usd` fields are
//!   optional from the caller's perspective — pass `0.0` when the
//!   caller doesn't track running cost. Cost-cap rules (D2) still
//!   fire correctly because they compare against a configured cap
//!   rather than against the running tally, and the supervisor
//!   enforces hard budgets independently.

use std::sync::Arc;

use crate::agent::instance::AgentId;
use crate::policy::engine::{PolicyDecision, PolicyEngine};
use crate::policy::event::PolicyEvent;
use crate::MiraError;

// What the engine needs to know about an upcoming LLM call. Built
// per-call by the integration point.
#[derive(Debug, Clone)]
pub struct LlmCallContext {
    pub agent_id:         AgentId,
    pub skill_id:         Option<String>,
    // Provider id (`"openrouter"`, `"lmstudio"`, etc.). Used by
    // admin rules that match on `provider_equals`.
    pub provider:         String,
    // Specific model. Empty string when not known at request time
    // (most providers don't surface the resolved model until the
    // response). Admin `model_equals` rules will simply not match
    // in that case — better than fabricating a placeholder.
    pub model:             String,
    pub running_cost_usd:  f64,
    pub session_cost_usd:  f64,
    // the calling agent's per-agent USD cap, when
    // known. Lets `PerAgentBudgetRule` enforce per-agent limits
    // without an AgentRegistry lookup. None = caller doesn't know
    // the cap; the rule falls back to its configured default.
    pub agent_budget_usd:  Option<f64>,
}

// Consult the engine for a `LlmCall` event. Returns `Ok(())` on
// `Allow`; `Err(MiraError::PolicyDenied { rule, reason })` on `Deny`.
pub async fn check_llm_call(
    engine: &Arc<dyn PolicyEngine>,
    ctx:    &LlmCallContext,
) -> Result<(), MiraError> {
    let event = PolicyEvent::LlmCall {
        agent_id:         ctx.agent_id,
        skill_id:         ctx.skill_id.clone(),
        provider:         ctx.provider.clone(),
        model:            ctx.model.clone(),
        running_cost_usd: ctx.running_cost_usd,
        session_cost_usd: ctx.session_cost_usd,
        agent_budget_usd: ctx.agent_budget_usd,
    };
    match engine.evaluate(&event).await {
        PolicyDecision::Allow => Ok(()),
        PolicyDecision::Deny { rule, reason } => Err(MiraError::PolicyDenied { rule, reason }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::engine::{AllowAllEngine, DenyAllEngine};
    use std::sync::Mutex as StdMutex;
    use async_trait::async_trait;

    fn ctx() -> LlmCallContext {
        LlmCallContext {
            agent_id:         AgentId::new(),
            skill_id:         Some("com.test.x".into()),
            provider:         "openrouter".into(),
            model:            "anthropic/claude-sonnet-4.6".into(),
            running_cost_usd: 0.0,
            session_cost_usd: 0.0,
            agent_budget_usd: None,
        }
    }

    // Engine that records each event + replies with a closure.
    struct RecordingEngine {
        seen:   StdMutex<Vec<PolicyEvent>>,
        decide: Box<dyn Fn(&PolicyEvent) -> PolicyDecision + Send + Sync>,
    }
    #[async_trait]
    impl PolicyEngine for RecordingEngine {
        async fn evaluate(&self, event: &PolicyEvent) -> PolicyDecision {
            self.seen.lock().unwrap().push(event.clone());
            (self.decide)(event)
        }
    }

    #[tokio::test]
    async fn allow_engine_returns_ok() {
        let eng: Arc<dyn PolicyEngine> = Arc::new(AllowAllEngine);
        check_llm_call(&eng, &ctx()).await.expect("allow → ok");
    }

    #[tokio::test]
    async fn deny_engine_returns_policy_denied_error() {
        let eng: Arc<dyn PolicyEngine> = Arc::new(DenyAllEngine::new("blocked"));
        let err = check_llm_call(&eng, &ctx()).await.unwrap_err();
        match err {
            MiraError::PolicyDenied { rule, reason } => {
                assert_eq!(rule, "test/deny-all");
                assert_eq!(reason, "blocked");
            }
            other => panic!("expected PolicyDenied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn engine_receives_llm_call_event_with_full_payload() {
        let eng = Arc::new(RecordingEngine {
            seen:   StdMutex::new(Vec::new()),
            decide: Box::new(|_| PolicyDecision::Allow),
        });
        let dyn_eng: Arc<dyn PolicyEngine> = eng.clone();
        let mut c = ctx();
        c.running_cost_usd = 0.42;
        c.session_cost_usd = 1.18;
        check_llm_call(&dyn_eng, &c).await.unwrap();

        let seen = eng.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        match &seen[0] {
            PolicyEvent::LlmCall { skill_id, provider, model, running_cost_usd, session_cost_usd, .. } => {
                assert_eq!(skill_id.as_deref(), Some("com.test.x"));
                assert_eq!(provider, "openrouter");
                assert_eq!(model,    "anthropic/claude-sonnet-4.6");
                assert!((running_cost_usd - 0.42).abs() < 1e-9);
                assert!((session_cost_usd - 1.18).abs() < 1e-9);
            }
            other => panic!("expected LlmCall, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_model_string_is_passed_through_verbatim() {
        // Caller can't always know the model at request time — empty
        // string must serialise + match cleanly so admin rules that
        // don't care about model still work.
        let eng = Arc::new(RecordingEngine {
            seen:   StdMutex::new(Vec::new()),
            decide: Box::new(|_| PolicyDecision::Allow),
        });
        let dyn_eng: Arc<dyn PolicyEngine> = eng.clone();
        let mut c = ctx();
        c.model = String::new();
        check_llm_call(&dyn_eng, &c).await.unwrap();

        let seen = eng.seen.lock().unwrap();
        match &seen[0] {
            PolicyEvent::LlmCall { model, .. } => assert!(model.is_empty()),
            other => panic!("expected LlmCall, got {other:?}"),
        }
    }
}
