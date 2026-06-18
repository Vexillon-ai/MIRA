// SPDX-License-Identifier: AGPL-3.0-or-later

//! Engine trait + ship-in-D1 implementations.

use async_trait::async_trait;
use serde::Serialize;

use super::event::PolicyEvent;

/// What the engine returns. `Allow` means the action proceeds; `Deny`
/// includes a `rule` identifier (so the audit log can group by rule)
/// and a human-readable `reason` (surfaced to the agents UI / user).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    Deny {
        /// Stable rule identifier — `"max-recursion-depth"`,
        /// `"session-budget"`, `"network-allowlist"`, etc. Used for
        /// audit-log grouping and the admin UI's "rules that
        /// triggered this week" view.
        rule:   String,
        /// One-sentence explanation of why this specific event was
        /// denied. Includes the offending value when relevant
        /// (e.g. "depth 6 exceeds cap of 5").
        reason: String,
    },
}

impl PolicyDecision {
    /// Builder for `Deny` — rule and reason both as `Into<String>`.
    pub fn deny(rule: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Deny { rule: rule.into(), reason: reason.into() }
    }

    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }

    pub fn is_deny(&self) -> bool {
        matches!(self, Self::Deny { .. })
    }
}

/// Every policy engine implements this. Async even though v1 rules
/// are sync — see module doc for the rationale.
#[async_trait]
pub trait PolicyEngine: Send + Sync {
    /// Inspect `event` and return a [`PolicyDecision`]. Implementors
    /// should be fast and side-effect-free; recording the decision
    /// to an audit log is the *caller's* job, not the engine's.
    async fn evaluate(&self, event: &PolicyEvent) -> PolicyDecision;
}

/// The default engine in D1: every event is allowed. Lets the
/// supervisor wire the engine seam in production without changing
/// runtime behaviour. Real rules ship in D2 by replacing this with
/// `BuiltinRulesEngine`.
pub struct AllowAllEngine;

#[async_trait]
impl PolicyEngine for AllowAllEngine {
    async fn evaluate(&self, _event: &PolicyEvent) -> PolicyDecision {
        PolicyDecision::Allow
    }
}

/// Useful in tests that want to verify a caller honours the engine's
/// `Deny` decision. Returns `Deny { rule: "test/deny-all", reason }`
/// for everything.
pub struct DenyAllEngine {
    pub reason: String,
}

impl DenyAllEngine {
    pub fn new(reason: impl Into<String>) -> Self {
        Self { reason: reason.into() }
    }
}

#[async_trait]
impl PolicyEngine for DenyAllEngine {
    async fn evaluate(&self, _event: &PolicyEvent) -> PolicyDecision {
        PolicyDecision::Deny {
            rule:   "test/deny-all".into(),
            reason: self.reason.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::instance::AgentId;
    use crate::policy::event::NetworkEgressDirection;

    fn id() -> AgentId { AgentId::new() }

    fn sample_event() -> PolicyEvent {
        PolicyEvent::SpawnWorker {
            parent_id: id(),
            skill_id: "com.test.x".into(),
            child_depth: 2,
            budget_usd: 1.0,
            session_spent_usd: 0.10,
        }
    }

    #[tokio::test]
    async fn allow_all_engine_allows_every_event_kind() {
        let eng = AllowAllEngine;

        for event in [
            sample_event(),
            PolicyEvent::ToolInvocation {
                agent_id: id(), skill_id: None,
                tool: "web_search".into(), args_summary: "x".into(),
                running_cost_usd: 0.0, session_cost_usd: 0.0,
            },
            PolicyEvent::LlmCall {
                agent_id: id(), skill_id: None,
                provider: "p".into(), model: "m".into(),
                running_cost_usd: 0.0, session_cost_usd: 0.0,
                agent_budget_usd: None,
            },
            PolicyEvent::NetworkEgress {
                agent_id: id(), skill_id: None,
                url: "https://x".into(), host: "x".into(), scheme: "https".into(),
                direction: NetworkEgressDirection::Outbound,
            },
        ] {
            assert_eq!(eng.evaluate(&event).await, PolicyDecision::Allow);
        }
    }

    #[tokio::test]
    async fn deny_all_engine_denies_with_configured_reason_and_stable_rule_id() {
        let eng = DenyAllEngine::new("everything denied for tests");
        let d = eng.evaluate(&sample_event()).await;
        match d {
            PolicyDecision::Deny { rule, reason } => {
                assert_eq!(rule, "test/deny-all");
                assert_eq!(reason, "everything denied for tests");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn decision_helpers_classify_allow_and_deny() {
        let a = PolicyDecision::Allow;
        let d = PolicyDecision::deny("rule-x", "because");
        assert!( a.is_allow());
        assert!(!a.is_deny());
        assert!(!d.is_allow());
        assert!( d.is_deny());
    }

    #[test]
    fn deny_builder_round_trips_strings() {
        let d = PolicyDecision::deny("max-depth", "depth 6 exceeds cap of 5");
        match d {
            PolicyDecision::Deny { rule, reason } => {
                assert_eq!(rule,   "max-depth");
                assert_eq!(reason, "depth 6 exceeds cap of 5");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }
}
