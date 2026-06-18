// SPDX-License-Identifier: AGPL-3.0-or-later

//! Policy engine — slice D1.
//!
//! The policy engine is a deterministic, non-LLM module that gates
//! every interesting action a Skill, agent, or tool takes against
//! declared rules. Per `design-docs/skills-and-agents.md` Part 3:
//!
//! > The policy engine is what makes "unverified Skills allowed with
//! > a toggle" tolerable in practice. It's a deterministic, non-LLM
//! > module that gates every action a Skill or agent takes against
//! > declared rules.
//!
//! # What ships in D1
//!
//!   - The [`PolicyEvent`] enum covering the action categories
//!     described in the design doc: tool invocation, LLM call, spawn
//!     worker, network egress, filesystem access, secret read.
//!   - The [`PolicyDecision`] enum — `Allow` or `Deny { rule, reason }`.
//!   - The [`PolicyEngine`] trait async callers consult before acting.
//!   - [`AllowAllEngine`] — the default no-op engine. D1 is plumbing,
//!     not policy. Real rules ship in D2.
//!   - [`DenyAllEngine`] — useful for tests that want to verify the
//!     supervisor honours the engine's decisions.
//!   - First plumbing point: `Supervisor::handle_spawn_child` consults
//!     the engine before approving a worker-requested spawn (slice D1
//!     extends the existing B6 spawn-approval flow).
//!
//! # Integration points (rolled out across D-phase slices)
//!
//!   - **Spawn approval** (D1 — this slice): supervisor checks before
//!     creating a child agent.
//!   - **Tool invocation** (D2): tool runtime checks before each
//!     `Tool::execute`.
//!   - **LLM call** (D2): the LLM client checks before calling
//!     `provider.generate()`.
//!   - **Network egress** (D2): `HttpPolicy::get` calls into the
//!     engine alongside its existing SSRF / denylist checks.
//!   - **Filesystem access** (D3): sandboxed file-read / file-write
//!     tools query the engine in addition to the existing per-Skill
//!     `permissions.filesystem` allowlist.
//!
//! # Design notes
//!
//!   - The engine is **async** even though v1 rules are sync. This is
//!     a one-time API choice; rules that need to query state (audit
//!     log lookups, rate-limit counters) become possible without
//!     changing the call sites later.
//!   - The default decision is **Allow**. A missing engine = no
//!     policy. The supervisor's optional `Arc<dyn PolicyEngine>`
//!     mirrors the optional `Arc<AuditStore>` from B9 — easy to wire
//!     in production, easy to omit in tests.
//!   - Policy decisions are also **audit events** (`AuditEvent::PolicyDecision`,
//!     already declared in slice B9). The plumbing in this slice
//!     records every deny; allow-records can land later if the audit
//!     log proves too quiet without them.

pub mod admin;
pub mod engine;
pub mod event;
pub mod llm;
pub mod rules;

pub use admin::{
    AdminRule, AdminRulesEngine, AdminRulesStore, ChainedEngine, Predicate,
};
pub use engine::{
    AllowAllEngine, DenyAllEngine, PolicyDecision, PolicyEngine,
};
pub use event::{NetworkEgressDirection, PolicyEvent};
pub use llm::{check_llm_call, LlmCallContext};
pub use rules::{
    BuiltinRulesEngine, FilesystemAllowlistRule, MaxRecursionDepthRule,
    NetworkAllowlistRule, PerAgentBudgetRule, Rule, RuleContext,
    SecretsAllowlistRule, SessionBudgetRule,
};
