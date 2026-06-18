// SPDX-License-Identifier: AGPL-3.0-or-later

//! Events the engine evaluates.
//!
//! The shape mirrors the JSON schema in `design-docs/skills-and-agents.md`
//! §"Rule evaluation". Every variant carries the `agent_id` so rules
//! can scope by agent (per-agent budgets, depth caps), and the
//! `skill_id` when one is in scope so rules can scope by Skill
//! (per-Skill network allowlists, etc).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent::instance::AgentId;

// Direction of a network call. The HttpPolicy already has a
// SSRF guard; the engine can layer Skill-level allowlists on top by
// matching on the URL host + scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkEgressDirection {
    // Outbound from MIRA to a third-party (search backend, web fetch,
    // MCP server). The common case.
    Outbound,
    // Inbound webhook / push that the policy engine should validate
    // before dispatching (Phase E telemetry).
    Inbound,
}

// One event the engine evaluates. Every variant tags the action and
// carries the minimum context a rule needs to decide. New variants
// are non-breaking — the engine match-pattern with a catch-all `_`
// arm reaches Allow by default.
// // `kind()` returns the snake_case discriminator string that admin-
// defined rules (slice D3) match on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PolicyEvent {
    // A Skill (or built-in) tool is about to execute. Fired once per
    // `Tool::execute` call, before any work runs.
    ToolInvocation {
        agent_id:          AgentId,
        skill_id:          Option<String>,
        tool:              String,
        // Short, human-readable summary of the args. NEVER the raw
        // arg JSON — could leak secrets. Tools are responsible for
        // producing a one-liner like "query='rust async'".
        args_summary:      String,
        running_cost_usd:  f64,
        session_cost_usd:  f64,
    },

    // An LLM call is about to be issued. Fired by the LLM client
    // before `provider.generate()`. Lets per-provider allowlists +
    // session-cost caps gate calls before we spend money.
    LlmCall {
        agent_id:          AgentId,
        skill_id:          Option<String>,
        provider:          String,
        model:             String,
        running_cost_usd:  f64,
        session_cost_usd:  f64,
        // the calling agent's per-agent budget cap, when
        // known. `PerAgentBudgetRule` compares `running_cost_usd`
        // against this; if absent, the rule falls back to its own
        // configured default. `serde(default)` keeps existing audit
        // rows + admin-stored events deserialising cleanly.
        #[serde(default)]
        agent_budget_usd:  Option<f64>,
    },

    // A worker spawn has been requested — either the user kicking
    // off a fresh root, or a worker calling `WorkerContext::spawn_child`.
    // Fired by the supervisor before any child agent is registered.
    SpawnWorker {
        parent_id:         AgentId,
        skill_id:          String,
        // Depth the *child* would be at — already incremented from
        // the parent's depth, so a rule can compare directly against
        // `MAX_RECURSION_DEPTH`.
        child_depth:       u8,
        budget_usd:        f64,
        session_spent_usd: f64,
    },

    // An outbound HTTP call is about to fire. Fired by `HttpPolicy::get`
    // alongside its existing SSRF / denylist checks.
    NetworkEgress {
        agent_id:    AgentId,
        skill_id:    Option<String>,
        url:         String,
        host:        String,
        scheme:      String,
        direction:   NetworkEgressDirection,
    },

    // A filesystem read / write / list is about to happen. The
    // per-Skill `permissions.filesystem` allowlist already enforces
    // at the Skill layer; the engine can layer admin-defined rules
    // (e.g. "no skill may read ~/.ssh ever").
    FilesystemAccess {
        agent_id:    AgentId,
        skill_id:    Option<String>,
        path:        PathBuf,
        // One of "read", "write", "list".
        mode:        String,
    },

    // A Skill is asking to read a named secret. Fired by the secrets
    // store before any value is returned. The Skill's manifest
    // `permissions.secrets` allowlist gates this at the loader; the
    // engine adds runtime checks (e.g. "deny if outside business hours").
    SecretRead {
        agent_id:    AgentId,
        skill_id:    Option<String>,
        secret_name: String,
    },
}

impl PolicyEvent {
    // Snake-case wire form of the variant tag. Matches the serde
    // tag, so `event.kind() == "tool_invocation"` etc. Used by
    // admin-defined rules (slice D3) to dispatch on event type
    // without re-implementing the enum match in user-supplied code.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ToolInvocation    { .. } => "tool_invocation",
            Self::LlmCall           { .. } => "llm_call",
            Self::SpawnWorker       { .. } => "spawn_worker",
            Self::NetworkEgress     { .. } => "network_egress",
            Self::FilesystemAccess  { .. } => "filesystem_access",
            Self::SecretRead        { .. } => "secret_read",
        }
    }

    // The agent the event is attributed to. Useful when audit-logging
    // a deny — we want one row per agent so the agents UI can filter.
    pub fn agent_id(&self) -> AgentId {
        match self {
            Self::ToolInvocation    { agent_id, .. } => *agent_id,
            Self::LlmCall           { agent_id, .. } => *agent_id,
            Self::SpawnWorker       { parent_id, .. } => *parent_id,
            Self::NetworkEgress     { agent_id, .. } => *agent_id,
            Self::FilesystemAccess  { agent_id, .. } => *agent_id,
            Self::SecretRead        { agent_id, .. } => *agent_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> AgentId { AgentId::new() }

    #[test]
    fn kind_strings_match_serde_tag() {
        assert_eq!(PolicyEvent::SpawnWorker {
            parent_id: id(), skill_id: "x".into(),
            child_depth: 1, budget_usd: 1.0, session_spent_usd: 0.0,
        }.kind(), "spawn_worker");

        assert_eq!(PolicyEvent::ToolInvocation {
            agent_id: id(), skill_id: None, tool: "x".into(),
            args_summary: "".into(),
            running_cost_usd: 0.0, session_cost_usd: 0.0,
        }.kind(), "tool_invocation");

        assert_eq!(PolicyEvent::LlmCall {
            agent_id: id(), skill_id: None,
            provider: "p".into(), model: "m".into(),
            running_cost_usd: 0.0, session_cost_usd: 0.0,
            agent_budget_usd: None,
        }.kind(), "llm_call");

        assert_eq!(PolicyEvent::NetworkEgress {
            agent_id: id(), skill_id: None,
            url: "https://x".into(), host: "x".into(), scheme: "https".into(),
            direction: NetworkEgressDirection::Outbound,
        }.kind(), "network_egress");

        assert_eq!(PolicyEvent::FilesystemAccess {
            agent_id: id(), skill_id: None,
            path: "/tmp/x".into(), mode: "read".into(),
        }.kind(), "filesystem_access");

        assert_eq!(PolicyEvent::SecretRead {
            agent_id: id(), skill_id: None, secret_name: "X".into(),
        }.kind(), "secret_read");
    }

    #[test]
    fn agent_id_uses_parent_id_for_spawn_events() {
        // SpawnWorker is the only variant whose attribution field is
        // not literally `agent_id`. Make sure we get the parent (the
        // one *requesting* the spawn) rather than something synthetic.
        let parent = id();
        let e = PolicyEvent::SpawnWorker {
            parent_id: parent, skill_id: "x".into(),
            child_depth: 1, budget_usd: 1.0, session_spent_usd: 0.0,
        };
        assert_eq!(e.agent_id(), parent);
    }

    #[test]
    fn serde_roundtrip_preserves_tag_and_payload() {
        let e = PolicyEvent::ToolInvocation {
            agent_id: id(),
            skill_id: Some("com.x".into()),
            tool: "web_search".into(),
            args_summary: "query=rust".into(),
            running_cost_usd: 0.42,
            session_cost_usd: 1.18,
        };
        let json = serde_json::to_string(&e).unwrap();
        // Tag is on the outside as `"kind": "tool_invocation"`.
        assert!(json.contains(r#""kind":"tool_invocation""#));
        let back: PolicyEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind(), "tool_invocation");
    }
}
