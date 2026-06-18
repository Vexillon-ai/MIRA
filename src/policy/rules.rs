// SPDX-License-Identifier: AGPL-3.0-or-later

//! Built-in rule set — slice D2.
//!
//! Hand-coded rules implementing the categories from
//! `design-docs/skills-and-agents.md` §"Rule evaluation". Each rule is a
//! small struct implementing the [`Rule`] trait; the
//! [`BuiltinRulesEngine`] iterates them in order and returns
//! `PolicyDecision::Deny` on the first match (first-deny-wins).
//!
//! Rules shipping in D2:
//!
//!   - [`MaxRecursionDepthRule`] — caps `SpawnWorker.child_depth`.
//!     Defence-in-depth alongside the supervisor's hard-coded floor
//!     check; the engine version is overridable from D3 admin config.
//!   - [`SessionBudgetRule`] — caps `SpawnWorker.session_spent_usd`
//!     and `LlmCall.session_cost_usd`. Same defence-in-depth posture
//!     vs the supervisor's `SessionBudget` kill switch.
//!   - [`NetworkAllowlistRule`] — `NetworkEgress.url` must match the
//!     Skill's `permissions.network_egress` allowlist. Delegates to
//!     `skills::permissions::check_network_egress` so the matcher is
//!     identical to what the manifest layer already uses.
//!   - [`FilesystemAllowlistRule`] — `FilesystemAccess.{path, mode}`
//!     must match the Skill's `permissions.filesystem` allowlist.
//!     Delegates to `skills::permissions::check_filesystem`.
//!   - [`SecretsAllowlistRule`] — `SecretRead.secret_name` must be in
//!     the Skill's `permissions.secrets` allowlist. Delegates to
//!     `skills::permissions::check_secret_access`.
//!
//! Skill-specific rules need the manifest to evaluate. The engine
//! takes an optional `Arc<SkillRegistry>`; events with a `skill_id`
//! that the registry doesn't know about are denied with a
//! "skill not loaded" reason — it's safer than allowing an unknown
//! Skill through the engine just because the registry doesn't have
//! its manifest cached.
//!
//! What's deferred:
//!
//!   - Per-agent budget rule: needs the agent's `budget.max_usd`,
//!     which isn't in the event payload yet. Already enforced by the
//!     supervisor's manager loop; engine version is a follow-up that
//!     either threads the cap into the event or has the engine look
//!     it up via an `AgentRegistry` reference.
//!   - Subprocess gate: there's no `SubprocessExec` event variant
//!     yet. The existing `skills::permissions::check_subprocess` runs
//!     at tool-dispatch time. When tool integration lands the rule
//!     can be implemented against a `ToolInvocation` event whose
//!     `tool` matches the subprocess executor.

use std::sync::Arc;

use async_trait::async_trait;

use crate::policy::engine::{PolicyDecision, PolicyEngine};
use crate::policy::event::PolicyEvent;
use crate::skills::loader::SkillRegistry;
use crate::skills::permissions::{
    check_filesystem, check_network_egress, check_secret_access, AccessMode,
};

/// One rule in the built-in set. Returning `Some(reason)` means
/// "deny with this reason"; `None` means "no opinion — let other
/// rules decide." Rules are pure functions of `(event, ctx)` so
/// they're easy to reason about and to unit-test.
pub trait Rule: Send + Sync {
    /// Stable identifier for this rule. Surfaces into
    /// `PolicyDecision::Deny.rule` and into audit-log groupings.
    /// Use kebab-case (matches `policy/<rule>` in user-facing strings).
    fn id(&self) -> &'static str;

    /// Evaluate. `ctx` carries shared state (Skill registry, etc.) so
    /// rules don't each carry their own `Arc`.
    fn evaluate(&self, event: &PolicyEvent, ctx: &RuleContext) -> Option<String>;
}

/// State shared across rules. Cheap to clone — the registry is wrapped
/// in an `Arc`. Add fields here when new rules need new state.
#[derive(Clone, Default)]
pub struct RuleContext {
    /// Skill registry for manifest lookups. `None` = no Skill-specific
    /// rules can fire (events that need a manifest fall through Allow,
    /// since we have no rules to consult).
    pub skill_registry: Option<Arc<SkillRegistry>>,
}

/// The shipped built-in engine. Holds an ordered list of rules + the
/// `RuleContext` they share. Construct via `Default` for the no-op
/// case (zero rules), then `with_*` to add rules.
#[derive(Default)]
pub struct BuiltinRulesEngine {
    rules: Vec<Arc<dyn Rule>>,
    ctx:   RuleContext,
}

impl BuiltinRulesEngine {
    pub fn new() -> Self { Self::default() }

    pub fn with_skill_registry(mut self, registry: Arc<SkillRegistry>) -> Self {
        self.ctx.skill_registry = Some(registry);
        self
    }

    /// Append a rule. Order matters — earlier rules win. Convention:
    /// cheap rules (depth check) before expensive rules (Skill lookup
    /// + path canonicalisation).
    pub fn add_rule(mut self, rule: Arc<dyn Rule>) -> Self {
        self.rules.push(rule);
        self
    }

    /// One-shot helper that builds the standard set: depth + session
    /// budget + per-agent budget (1.5) + the three Skill-specific
    /// rules. Production callers use this; tests use `add_rule` to
    /// mix and match.
    ///
    /// `default_agent_budget_usd: None` means the per-agent rule
    /// no-ops unless a specific event carries `agent_budget_usd`.
    /// Pass `Some(cap)` to apply a fleet-wide default.
    pub fn standard(
        max_depth:                u8,
        session_budget_usd:       f64,
        default_agent_budget_usd: Option<f64>,
        skill_registry:           Option<Arc<SkillRegistry>>,
    ) -> Self {
        let mut e = Self::new();
        e = e.add_rule(Arc::new(MaxRecursionDepthRule { max_depth }));
        e = e.add_rule(Arc::new(SessionBudgetRule { max_usd: session_budget_usd }));
        e = e.add_rule(Arc::new(PerAgentBudgetRule {
            default_max_usd: default_agent_budget_usd,
        }));
        e = e.add_rule(Arc::new(NetworkAllowlistRule));
        e = e.add_rule(Arc::new(FilesystemAllowlistRule));
        e = e.add_rule(Arc::new(SecretsAllowlistRule));
        if let Some(reg) = skill_registry {
            e = e.with_skill_registry(reg);
        }
        e
    }
}

#[async_trait]
impl PolicyEngine for BuiltinRulesEngine {
    async fn evaluate(&self, event: &PolicyEvent) -> PolicyDecision {
        for rule in &self.rules {
            if let Some(reason) = rule.evaluate(event, &self.ctx) {
                return PolicyDecision::deny(rule.id(), reason);
            }
        }
        PolicyDecision::Allow
    }
}

// ─── Rules ─────────────────────────────────────────────────────────────

/// Cap the depth of the agent tree. The supervisor enforces a
/// hard-coded floor at `MAX_RECURSION_DEPTH = 5`; this rule lets D3
/// admin config tighten that further (e.g. "depth ≤ 3 in untrusted
/// mode") without rebuilding.
pub struct MaxRecursionDepthRule {
    pub max_depth: u8,
}
impl Rule for MaxRecursionDepthRule {
    fn id(&self) -> &'static str { "max-recursion-depth" }
    fn evaluate(&self, event: &PolicyEvent, _: &RuleContext) -> Option<String> {
        if let PolicyEvent::SpawnWorker { child_depth, .. } = event {
            if *child_depth > self.max_depth {
                return Some(format!(
                    "depth {child_depth} exceeds cap of {}", self.max_depth,
                ));
            }
        }
        None
    }
}

/// Cap the total LLM spend across an agent tree. Fires for both
/// `SpawnWorker` (refuse to spawn into an already-busted session) and
/// `LlmCall` (refuse a call that *would* push the session over).
pub struct SessionBudgetRule {
    pub max_usd: f64,
}
impl Rule for SessionBudgetRule {
    fn id(&self) -> &'static str { "session-budget" }
    fn evaluate(&self, event: &PolicyEvent, _: &RuleContext) -> Option<String> {
        match event {
            PolicyEvent::SpawnWorker { session_spent_usd, .. } => {
                if *session_spent_usd > self.max_usd {
                    return Some(format!(
                        "session spent ${:.4} exceeds cap ${:.4}",
                        session_spent_usd, self.max_usd,
                    ));
                }
            }
            PolicyEvent::LlmCall { session_cost_usd, .. } => {
                if *session_cost_usd > self.max_usd {
                    return Some(format!(
                        "session cost ${:.4} exceeds cap ${:.4}",
                        session_cost_usd, self.max_usd,
                    ));
                }
            }
            _ => {}
        }
        None
    }
}

/// Cap LLM spend on a per-agent basis. Fires only on `LlmCall`. The
/// effective cap is, in priority order:
///   1. `event.agent_budget_usd` when the caller threaded it through
///      (the supervisor populates this when the agent has a known
///      cap on `Agent.budget.max_usd`).
///   2. The rule's `default_max_usd` when the event didn't carry one.
///   3. None — neither set, the rule no-ops (consistent with
///      "no cap configured = no enforcement"; there's already a
///      `SessionBudgetRule` for the global limit).
///
/// Defence-in-depth alongside the supervisor's manager-loop kill
/// (slice B4): the manager loop is the canonical accountant + tear-
/// downer, this rule lets admins author tighter or laxer overrides
/// without recompiling.
pub struct PerAgentBudgetRule {
    pub default_max_usd: Option<f64>,
}
impl Rule for PerAgentBudgetRule {
    fn id(&self) -> &'static str { "per-agent-budget" }
    fn evaluate(&self, event: &PolicyEvent, _: &RuleContext) -> Option<String> {
        let PolicyEvent::LlmCall { running_cost_usd, agent_budget_usd, .. } = event
        else { return None; };
        let cap = agent_budget_usd.or(self.default_max_usd)?;
        if *running_cost_usd > cap {
            return Some(format!(
                "agent running cost ${:.4} exceeds cap ${:.4}",
                running_cost_usd, cap,
            ));
        }
        None
    }
}

/// `NetworkEgress.url`'s host must match the Skill's
/// `permissions.network_egress` allowlist. Stateless — delegates to
/// the same matcher the Skill loader uses (`check_network_egress`),
/// so the rule and the manifest enforcement can't drift apart.
pub struct NetworkAllowlistRule;
impl Rule for NetworkAllowlistRule {
    fn id(&self) -> &'static str { "network-allowlist" }
    fn evaluate(&self, event: &PolicyEvent, ctx: &RuleContext) -> Option<String> {
        let PolicyEvent::NetworkEgress { skill_id, url, .. } = event else { return None; };
        let Some(skill_id) = skill_id else { return None; }; // root agent: no manifest
        let Some(reg) = &ctx.skill_registry else {
            return Some(format!(
                "no skill registry configured to check network access for {skill_id}",
            ));
        };
        let Some(skill) = reg.get(skill_id) else {
            return Some(format!(
                "unknown skill {skill_id:?} — refusing network egress",
            ));
        };
        match check_network_egress(&skill.manifest.permissions, url) {
            Ok(())     => None,
            Err(denied) => Some(denied.0),
        }
    }
}

/// `FilesystemAccess.{path, mode}` must match the Skill's declared
/// `permissions.filesystem` allowlist. Modes are matched per-direction
/// (read / write / read+write) — the entire matcher lives in
/// `skills::permissions::check_filesystem`.
pub struct FilesystemAllowlistRule;
impl Rule for FilesystemAllowlistRule {
    fn id(&self) -> &'static str { "filesystem-allowlist" }
    fn evaluate(&self, event: &PolicyEvent, ctx: &RuleContext) -> Option<String> {
        let PolicyEvent::FilesystemAccess { skill_id, path, mode, .. } = event else { return None; };
        let Some(skill_id) = skill_id else { return None; };
        let Some(reg) = &ctx.skill_registry else {
            return Some(format!(
                "no skill registry configured to check filesystem access for {skill_id}",
            ));
        };
        let Some(skill) = reg.get(skill_id) else {
            return Some(format!(
                "unknown skill {skill_id:?} — refusing filesystem access",
            ));
        };
        let access = match mode.as_str() {
            "read"  | "list"            => AccessMode::Read,
            "write" | "create" | "delete" => AccessMode::Write,
            other => return Some(format!("unknown filesystem mode {other:?}")),
        };
        match check_filesystem(&skill.manifest.permissions, path, access) {
            Ok(())     => None,
            Err(denied) => Some(denied.0),
        }
    }
}

/// `SecretRead.secret_name` must be in the Skill's declared
/// `permissions.secrets` allowlist. Delegates to
/// `check_secret_access` so the rule and the loader use identical
/// matching logic.
pub struct SecretsAllowlistRule;
impl Rule for SecretsAllowlistRule {
    fn id(&self) -> &'static str { "secrets-allowlist" }
    fn evaluate(&self, event: &PolicyEvent, ctx: &RuleContext) -> Option<String> {
        let PolicyEvent::SecretRead { skill_id, secret_name, .. } = event else { return None; };
        let Some(skill_id) = skill_id else { return None; };
        let Some(reg) = &ctx.skill_registry else {
            return Some(format!(
                "no skill registry configured to check secret access for {skill_id}",
            ));
        };
        let Some(skill) = reg.get(skill_id) else {
            return Some(format!(
                "unknown skill {skill_id:?} — refusing secret read",
            ));
        };
        match check_secret_access(&skill.manifest.permissions, secret_name) {
            Ok(())     => None,
            Err(denied) => Some(denied.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::instance::AgentId;
    use crate::policy::event::NetworkEgressDirection;
    use crate::skills::loader::LoadedSkill;
    use crate::skills::manifest::{Permissions, SkillManifest, SkillMeta};
    use semver::Version;
    use std::path::PathBuf;

    fn id() -> AgentId { AgentId::new() }

    /// Build a SkillRegistry with one Skill whose `permissions` are
    /// supplied by the caller. Lets each test target a single rule.
    fn registry_with(skill_id: &str, perms: Permissions) -> Arc<SkillRegistry> {
        let manifest = SkillManifest {
            skill: SkillMeta {
                id: skill_id.into(),
                version: Version::new(0, 1, 0),
                display_name: "Test Skill".into(),
                description:  "for tests".into(),
                authors:      vec![],
                license:      None,
                mira_min:     None,
                system:       false,
            },
            permissions: perms,
            tools:        Default::default(),
            dependencies: Default::default(),
            verification: None,
        };
        let loaded = LoadedSkill {
            manifest,
            root_dir: PathBuf::from("/tmp/fake-skill"),
            signed:    false,
            verified:  false,
            publisher_label:    None,
            verification_error: None,
            system: false,
        };
        Arc::new(SkillRegistry { loaded: vec![loaded], errors: vec![] })
    }

    // ── MaxRecursionDepthRule ───────────────────────────────────────

    #[tokio::test]
    async fn max_depth_rule_denies_when_child_depth_over_cap() {
        let eng = BuiltinRulesEngine::new()
            .add_rule(Arc::new(MaxRecursionDepthRule { max_depth: 3 }));
        let event = PolicyEvent::SpawnWorker {
            parent_id: id(), skill_id: "x".into(),
            child_depth: 4, budget_usd: 1.0, session_spent_usd: 0.0,
        };
        match eng.evaluate(&event).await {
            PolicyDecision::Deny { rule, reason } => {
                assert_eq!(rule, "max-recursion-depth");
                assert!(reason.contains("depth 4"), "got: {reason}");
                assert!(reason.contains("cap of 3"), "got: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn max_depth_rule_allows_when_at_or_under_cap() {
        let eng = BuiltinRulesEngine::new()
            .add_rule(Arc::new(MaxRecursionDepthRule { max_depth: 5 }));
        for depth in [0, 3, 5] {
            let event = PolicyEvent::SpawnWorker {
                parent_id: id(), skill_id: "x".into(),
                child_depth: depth, budget_usd: 1.0, session_spent_usd: 0.0,
            };
            assert_eq!(eng.evaluate(&event).await, PolicyDecision::Allow,
                "depth {depth} should allow");
        }
    }

    #[tokio::test]
    async fn max_depth_rule_ignores_unrelated_events() {
        let eng = BuiltinRulesEngine::new()
            .add_rule(Arc::new(MaxRecursionDepthRule { max_depth: 1 }));
        let event = PolicyEvent::ToolInvocation {
            agent_id: id(), skill_id: None,
            tool: "x".into(), args_summary: "y".into(),
            running_cost_usd: 0.0, session_cost_usd: 0.0,
        };
        assert_eq!(eng.evaluate(&event).await, PolicyDecision::Allow);
    }

    // ── SessionBudgetRule ───────────────────────────────────────────

    #[tokio::test]
    async fn session_budget_rule_denies_spawn_when_session_over_cap() {
        let eng = BuiltinRulesEngine::new()
            .add_rule(Arc::new(SessionBudgetRule { max_usd: 1.00 }));
        let event = PolicyEvent::SpawnWorker {
            parent_id: id(), skill_id: "x".into(),
            child_depth: 1, budget_usd: 1.0, session_spent_usd: 1.50,
        };
        match eng.evaluate(&event).await {
            PolicyDecision::Deny { rule, reason } => {
                assert_eq!(rule, "session-budget");
                assert!(reason.contains("$1.5000"), "got: {reason}");
                assert!(reason.contains("cap $1.0000"), "got: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_budget_rule_denies_llm_call_when_session_over_cap() {
        let eng = BuiltinRulesEngine::new()
            .add_rule(Arc::new(SessionBudgetRule { max_usd: 1.00 }));
        let event = PolicyEvent::LlmCall {
            agent_id: id(), skill_id: None,
            provider: "p".into(), model: "m".into(),
            running_cost_usd: 0.50, session_cost_usd: 1.10,
            agent_budget_usd: None,
        };
        match eng.evaluate(&event).await {
            PolicyDecision::Deny { rule, reason } => {
                assert_eq!(rule, "session-budget");
                assert!(reason.contains("session cost"), "got: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_budget_rule_allows_when_under_cap() {
        let eng = BuiltinRulesEngine::new()
            .add_rule(Arc::new(SessionBudgetRule { max_usd: 1.00 }));
        let event = PolicyEvent::LlmCall {
            agent_id: id(), skill_id: None,
            provider: "p".into(), model: "m".into(),
            running_cost_usd: 0.50, session_cost_usd: 0.99,
            agent_budget_usd: None,
        };
        assert_eq!(eng.evaluate(&event).await, PolicyDecision::Allow);
    }

    // ── PerAgentBudgetRule ───────────────────────────────────────────

    fn llm_event(running: f64, agent_cap: Option<f64>) -> PolicyEvent {
        PolicyEvent::LlmCall {
            agent_id: id(), skill_id: None,
            provider: "p".into(), model: "m".into(),
            running_cost_usd: running,
            session_cost_usd: 0.0,
            agent_budget_usd: agent_cap,
        }
    }

    #[tokio::test]
    async fn per_agent_budget_uses_event_cap_when_present() {
        // Event carries agent_budget_usd = 0.50. Rule's default is
        // 100.00 — irrelevant here; event cap wins.
        let eng = BuiltinRulesEngine::new()
            .add_rule(Arc::new(PerAgentBudgetRule { default_max_usd: Some(100.00) }));

        // Under: allow.
        assert_eq!(eng.evaluate(&llm_event(0.49, Some(0.50))).await,
                   PolicyDecision::Allow);
        // At threshold: allow (strictly greater required).
        assert_eq!(eng.evaluate(&llm_event(0.50, Some(0.50))).await,
                   PolicyDecision::Allow);
        // Over: deny with rule id + cap value in reason.
        match eng.evaluate(&llm_event(0.51, Some(0.50))).await {
            PolicyDecision::Deny { rule, reason } => {
                assert_eq!(rule, "per-agent-budget");
                assert!(reason.contains("$0.5100"), "got: {reason}");
                assert!(reason.contains("$0.5000"), "got: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn per_agent_budget_falls_back_to_rule_default_when_event_cap_absent() {
        // Event has no per-agent cap; rule's default of 0.50 takes over.
        let eng = BuiltinRulesEngine::new()
            .add_rule(Arc::new(PerAgentBudgetRule { default_max_usd: Some(0.50) }));
        assert_eq!(eng.evaluate(&llm_event(0.49, None)).await,
                   PolicyDecision::Allow);
        match eng.evaluate(&llm_event(0.51, None)).await {
            PolicyDecision::Deny { rule, .. } => assert_eq!(rule, "per-agent-budget"),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn per_agent_budget_no_ops_when_neither_event_nor_default_set() {
        // No cap anywhere = no enforcement (consistent with "no cap
        // configured = no enforcement"). Session-budget covers global.
        let eng = BuiltinRulesEngine::new()
            .add_rule(Arc::new(PerAgentBudgetRule { default_max_usd: None }));
        assert_eq!(eng.evaluate(&llm_event(1_000_000.0, None)).await,
                   PolicyDecision::Allow);
    }

    #[tokio::test]
    async fn per_agent_budget_ignores_non_llm_call_events() {
        // SpawnWorker carries a budget but it's the *child's* proposed
        // budget, not the parent's running cost. Rule should NOT match.
        let eng = BuiltinRulesEngine::new()
            .add_rule(Arc::new(PerAgentBudgetRule { default_max_usd: Some(0.01) }));
        let spawn = PolicyEvent::SpawnWorker {
            parent_id: id(), skill_id: "x".into(),
            child_depth: 1, budget_usd: 5.0, session_spent_usd: 0.0,
        };
        assert_eq!(eng.evaluate(&spawn).await, PolicyDecision::Allow);
    }

    #[tokio::test]
    async fn per_agent_budget_event_cap_zero_or_negative_ignored() {
        // 0.0 / negative caps mean "no enforcement" (caller likely
        // sentineled an absent budget). Rule reads agent_budget_usd
        // verbatim, so an event with 0.0 would be a strict cap of
        // $0.00 — effectively denying every paid call. We avoid that
        // by having the tool loop already filter out non-positive
        // values before populating the event; this test documents
        // the design boundary.
        let eng = BuiltinRulesEngine::new()
            .add_rule(Arc::new(PerAgentBudgetRule { default_max_usd: Some(1.00) }));
        // Event explicitly carries 0.0 — rule treats it as a real cap
        // and denies anything > 0. This is correct behaviour for the
        // engine; the *tool loop* is responsible for not passing
        // sentinel 0.0 values (verified separately in tool_loop tests).
        match eng.evaluate(&llm_event(0.01, Some(0.0))).await {
            PolicyDecision::Deny { .. } => {}
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    // ── NetworkAllowlistRule ────────────────────────────────────────

    fn perms_net(allowlist: Vec<&str>) -> Permissions {
        Permissions {
            network_egress: allowlist.into_iter().map(String::from).collect(),
            ..Permissions::default()
        }
    }

    #[tokio::test]
    async fn network_rule_allows_url_matching_skill_allowlist() {
        let reg = registry_with("com.test.x", perms_net(vec!["https://*.wikipedia.org"]));
        let eng = BuiltinRulesEngine::new()
            .with_skill_registry(reg)
            .add_rule(Arc::new(NetworkAllowlistRule));
        let event = PolicyEvent::NetworkEgress {
            agent_id: id(), skill_id: Some("com.test.x".into()),
            url: "https://en.wikipedia.org/wiki/X".into(),
            host: "en.wikipedia.org".into(),
            scheme: "https".into(),
            direction: NetworkEgressDirection::Outbound,
        };
        assert_eq!(eng.evaluate(&event).await, PolicyDecision::Allow);
    }

    #[tokio::test]
    async fn network_rule_denies_url_outside_skill_allowlist() {
        let reg = registry_with("com.test.x", perms_net(vec!["https://*.wikipedia.org"]));
        let eng = BuiltinRulesEngine::new()
            .with_skill_registry(reg)
            .add_rule(Arc::new(NetworkAllowlistRule));
        let event = PolicyEvent::NetworkEgress {
            agent_id: id(), skill_id: Some("com.test.x".into()),
            url: "https://evil.example/".into(),
            host: "evil.example".into(),
            scheme: "https".into(),
            direction: NetworkEgressDirection::Outbound,
        };
        match eng.evaluate(&event).await {
            PolicyDecision::Deny { rule, .. } => assert_eq!(rule, "network-allowlist"),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn network_rule_denies_unknown_skill_id() {
        // Skill ID not in registry → deny rather than silently allow.
        let reg = registry_with("com.test.known", perms_net(vec!["https://*.wikipedia.org"]));
        let eng = BuiltinRulesEngine::new()
            .with_skill_registry(reg)
            .add_rule(Arc::new(NetworkAllowlistRule));
        let event = PolicyEvent::NetworkEgress {
            agent_id: id(), skill_id: Some("com.test.UNKNOWN".into()),
            url: "https://example.com/".into(),
            host: "example.com".into(),
            scheme: "https".into(),
            direction: NetworkEgressDirection::Outbound,
        };
        match eng.evaluate(&event).await {
            PolicyDecision::Deny { reason, .. } => {
                assert!(reason.contains("unknown skill"), "got: {reason}");
                assert!(reason.contains("com.test.UNKNOWN"), "got: {reason}");
            }
            other => panic!("expected Deny for unknown skill, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn network_rule_allows_event_without_skill_id() {
        // Root agent (no skill_id) — Skill-specific rule has no opinion,
        // engine reaches Allow.
        let reg = registry_with("com.test.x", perms_net(vec![]));
        let eng = BuiltinRulesEngine::new()
            .with_skill_registry(reg)
            .add_rule(Arc::new(NetworkAllowlistRule));
        let event = PolicyEvent::NetworkEgress {
            agent_id: id(), skill_id: None,
            url: "https://anywhere.example/".into(),
            host: "anywhere.example".into(),
            scheme: "https".into(),
            direction: NetworkEgressDirection::Outbound,
        };
        assert_eq!(eng.evaluate(&event).await, PolicyDecision::Allow);
    }

    #[tokio::test]
    async fn network_rule_denies_when_no_registry_is_configured() {
        // No Skill registry but the event has a skill_id — fail closed
        // rather than silently allowing.
        let eng = BuiltinRulesEngine::new()
            .add_rule(Arc::new(NetworkAllowlistRule));
        let event = PolicyEvent::NetworkEgress {
            agent_id: id(), skill_id: Some("com.test.x".into()),
            url: "https://anywhere.example/".into(),
            host: "anywhere.example".into(),
            scheme: "https".into(),
            direction: NetworkEgressDirection::Outbound,
        };
        match eng.evaluate(&event).await {
            PolicyDecision::Deny { reason, .. } => {
                assert!(reason.contains("no skill registry"), "got: {reason}");
            }
            other => panic!("expected fail-closed Deny, got {other:?}"),
        }
    }

    // ── FilesystemAllowlistRule ─────────────────────────────────────

    fn perms_fs(allowlist: Vec<&str>) -> Permissions {
        Permissions {
            filesystem: allowlist.into_iter().map(String::from).collect(),
            ..Permissions::default()
        }
    }

    #[tokio::test]
    async fn fs_rule_allows_read_within_allowlist() {
        let reg = registry_with("com.test.x", perms_fs(vec!["read:/tmp"]));
        let eng = BuiltinRulesEngine::new()
            .with_skill_registry(reg)
            .add_rule(Arc::new(FilesystemAllowlistRule));
        let event = PolicyEvent::FilesystemAccess {
            agent_id: id(), skill_id: Some("com.test.x".into()),
            path: "/tmp/foo.txt".into(), mode: "read".into(),
        };
        assert_eq!(eng.evaluate(&event).await, PolicyDecision::Allow);
    }

    #[tokio::test]
    async fn fs_rule_denies_write_when_only_read_granted() {
        let reg = registry_with("com.test.x", perms_fs(vec!["read:/tmp"]));
        let eng = BuiltinRulesEngine::new()
            .with_skill_registry(reg)
            .add_rule(Arc::new(FilesystemAllowlistRule));
        let event = PolicyEvent::FilesystemAccess {
            agent_id: id(), skill_id: Some("com.test.x".into()),
            path: "/tmp/foo.txt".into(), mode: "write".into(),
        };
        match eng.evaluate(&event).await {
            PolicyDecision::Deny { rule, .. } => {
                assert_eq!(rule, "filesystem-allowlist");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fs_rule_denies_path_outside_allowlist() {
        let reg = registry_with("com.test.x", perms_fs(vec!["read+write:/tmp"]));
        let eng = BuiltinRulesEngine::new()
            .with_skill_registry(reg)
            .add_rule(Arc::new(FilesystemAllowlistRule));
        let event = PolicyEvent::FilesystemAccess {
            agent_id: id(), skill_id: Some("com.test.x".into()),
            path: "/etc/shadow".into(), mode: "read".into(),
        };
        match eng.evaluate(&event).await {
            PolicyDecision::Deny { rule, reason } => {
                assert_eq!(rule, "filesystem-allowlist");
                assert!(reason.contains("/etc/shadow"), "got: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fs_rule_denies_unknown_mode_string() {
        let reg = registry_with("com.test.x", perms_fs(vec!["read+write:/tmp"]));
        let eng = BuiltinRulesEngine::new()
            .with_skill_registry(reg)
            .add_rule(Arc::new(FilesystemAllowlistRule));
        let event = PolicyEvent::FilesystemAccess {
            agent_id: id(), skill_id: Some("com.test.x".into()),
            path: "/tmp/x".into(), mode: "execute".into(),
        };
        match eng.evaluate(&event).await {
            PolicyDecision::Deny { reason, .. } => {
                assert!(reason.contains("unknown filesystem mode"), "got: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    // ── SecretsAllowlistRule ────────────────────────────────────────

    fn perms_secrets(allowlist: Vec<&str>) -> Permissions {
        Permissions {
            secrets: allowlist.into_iter().map(crate::skills::manifest::SecretSpec::from).collect(),
            ..Permissions::default()
        }
    }

    #[tokio::test]
    async fn secrets_rule_allows_listed_secret() {
        let reg = registry_with("com.test.x", perms_secrets(vec!["OPENAI_API_KEY"]));
        let eng = BuiltinRulesEngine::new()
            .with_skill_registry(reg)
            .add_rule(Arc::new(SecretsAllowlistRule));
        let event = PolicyEvent::SecretRead {
            agent_id: id(), skill_id: Some("com.test.x".into()),
            secret_name: "OPENAI_API_KEY".into(),
        };
        assert_eq!(eng.evaluate(&event).await, PolicyDecision::Allow);
    }

    #[tokio::test]
    async fn secrets_rule_denies_unlisted_secret() {
        let reg = registry_with("com.test.x", perms_secrets(vec!["OPENAI_API_KEY"]));
        let eng = BuiltinRulesEngine::new()
            .with_skill_registry(reg)
            .add_rule(Arc::new(SecretsAllowlistRule));
        let event = PolicyEvent::SecretRead {
            agent_id: id(), skill_id: Some("com.test.x".into()),
            secret_name: "AWS_SECRET_ACCESS_KEY".into(),
        };
        match eng.evaluate(&event).await {
            PolicyDecision::Deny { rule, .. } => assert_eq!(rule, "secrets-allowlist"),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    // ── First-deny-wins + standard() set ───────────────────────────

    #[tokio::test]
    async fn first_deny_wins_when_multiple_rules_would_fire() {
        // Both depth and budget would deny; depth is added first so its
        // rule id wins — proves the "first match" iteration order.
        let eng = BuiltinRulesEngine::new()
            .add_rule(Arc::new(MaxRecursionDepthRule { max_depth: 1 }))
            .add_rule(Arc::new(SessionBudgetRule { max_usd: 0.10 }));
        let event = PolicyEvent::SpawnWorker {
            parent_id: id(), skill_id: "x".into(),
            child_depth: 5, budget_usd: 1.0, session_spent_usd: 100.0,
        };
        match eng.evaluate(&event).await {
            PolicyDecision::Deny { rule, .. } => {
                assert_eq!(rule, "max-recursion-depth", "depth rule was added first");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_engine_allows_everything() {
        let eng = BuiltinRulesEngine::new();
        let event = PolicyEvent::SpawnWorker {
            parent_id: id(), skill_id: "x".into(),
            child_depth: 100, budget_usd: 1e9, session_spent_usd: 1e9,
        };
        assert_eq!(eng.evaluate(&event).await, PolicyDecision::Allow);
    }

    #[tokio::test]
    async fn standard_engine_loads_all_six_rules() {
        // Smoke test: build the standard engine and verify each kind
        // of denial fires through it. (Each of the six rules already
        // has its own focused test above; this confirms wiring.)
        let reg = registry_with("com.test.x", Permissions {
            filesystem:     vec!["read:/tmp".into()],
            network_egress: vec!["https://*.wikipedia.org".into()],
            secrets:        vec!["OK_KEY".into()],
            ..Permissions::default()
        });
        // 1.5 — pass a fleet-wide per-agent default of $0.50 so the
        // PerAgentBudgetRule has something to compare against in
        // events that don't carry their own cap.
        let eng = BuiltinRulesEngine::standard(3, 1.00, Some(0.50), Some(reg));

        // Depth.
        let depth_event = PolicyEvent::SpawnWorker {
            parent_id: id(), skill_id: "x".into(),
            child_depth: 4, budget_usd: 1.0, session_spent_usd: 0.0,
        };
        assert!(eng.evaluate(&depth_event).await.is_deny());

        // Session budget.
        let budget_event = PolicyEvent::SpawnWorker {
            parent_id: id(), skill_id: "x".into(),
            child_depth: 1, budget_usd: 1.0, session_spent_usd: 5.0,
        };
        assert!(eng.evaluate(&budget_event).await.is_deny());

        // Per-agent budget (1.5).
        let agent_budget_event = PolicyEvent::LlmCall {
            agent_id: id(), skill_id: None,
            provider: "p".into(), model: "m".into(),
            running_cost_usd: 0.99, session_cost_usd: 0.99,
            agent_budget_usd: None,  // falls back to the $0.50 default
        };
        match eng.evaluate(&agent_budget_event).await {
            PolicyDecision::Deny { rule, .. } => assert_eq!(rule, "per-agent-budget"),
            other => panic!("expected per-agent Deny, got {other:?}"),
        }

        // Network.
        let net_event = PolicyEvent::NetworkEgress {
            agent_id: id(), skill_id: Some("com.test.x".into()),
            url: "https://nope.example/".into(),
            host: "nope.example".into(),
            scheme: "https".into(),
            direction: NetworkEgressDirection::Outbound,
        };
        assert!(eng.evaluate(&net_event).await.is_deny());

        // Filesystem.
        let fs_event = PolicyEvent::FilesystemAccess {
            agent_id: id(), skill_id: Some("com.test.x".into()),
            path: "/etc/passwd".into(), mode: "read".into(),
        };
        assert!(eng.evaluate(&fs_event).await.is_deny());

        // Secrets.
        let secret_event = PolicyEvent::SecretRead {
            agent_id: id(), skill_id: Some("com.test.x".into()),
            secret_name: "BAD_KEY".into(),
        };
        assert!(eng.evaluate(&secret_event).await.is_deny());

        // And a happy path makes it through.
        let happy = PolicyEvent::SpawnWorker {
            parent_id: id(), skill_id: "x".into(),
            child_depth: 2, budget_usd: 1.0, session_spent_usd: 0.10,
        };
        assert!(eng.evaluate(&happy).await.is_allow());
    }
}
