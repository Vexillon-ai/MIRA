// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/mod.rs

//! Tool registry and execution system
//!
//! Provides a trait-based architecture for extensible tools that agents can use
//! to accomplish tasks.

pub mod backup;
pub mod shell;
pub mod filesystem;
pub mod onboarding;
pub mod recall;
pub mod datetime;
pub mod math_eval;
pub mod weather;
pub mod pdf;
pub mod summarize;
pub mod memory_supersede;
pub mod http_policy;
pub mod web_fetch;
pub mod url_preview;
pub mod search;
pub mod audit;
pub mod calendar;
pub mod code_run;
pub mod image_generate;
pub mod video_generate;
pub mod automations;
pub mod agent_tasks;
pub mod workflow_tasks;
pub mod wiki;
pub mod companion;
pub mod mira_help;
pub mod guardian_inspect;
pub mod guardian_decide;
pub mod guardian_propose;
pub mod settings;

use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tracing::{debug, info, warn};

use crate::MiraError;
use audit::{args_digest, truncate_output, Outcome, ToolAuditStore};

// ── Visibility tier ───────────────────────────────────────────────────────────

// Who/what surfaces a tool in the registry.
// // The registry stores all tools in one place but `list_*` helpers filter by
// tier so user palettes never expose internal machinery. Matches the three
// roles described in `design/onboarding/ONBOARDING_DESIGN.md` §4.4.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tier", rename_all = "snake_case")]
pub enum ToolVisibility {
    // User-configurable tool. Shown in the user tool palette when it ships.
    User,
    // Admin-configurable capability. Same default flow availability as
    // `User` today; the distinction is about *who can toggle it*, not about
    // which flows see it.
    Admin,
    // MIRA-only tool auto-attached to specific flows. `flow` is the
    // `conversations.mode` value (e.g. `"onboarding"`) this tool serves.
    System { flow: String },
}

impl ToolVisibility {
    pub fn system(flow: impl Into<String>) -> Self {
        ToolVisibility::System { flow: flow.into() }
    }
}

// ── Capability tier ───────────────────────────────────────────────────────────

// What kinds of resources a tool may touch. Declared by each [`Tool`] impl
// so the registry — and the `/api/tools` surface — can group tools by risk.
// // See `design-docs/phase7-tools-and-sandbox.md` §1 for the full tier model. This
// enum is *orthogonal* to [`ToolVisibility`]: tier is *what the tool does*,
// visibility is *who is allowed to see it*. A tool can be `User`-visible +
// `Code`-tier (if sandboxed), or `System`-visible + `Pure`-tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    // No I/O beyond MIRA-owned state. Deterministic, safe to call freely.
    Pure,
    // Reaches the public internet. Rate-limited; allowlist applies.
    Network,
    // Reads/writes the local filesystem outside MIRA's own DBs.
    Filesystem,
    // Executes arbitrary code in a sandboxed subprocess.
    Code,
    // Internal machinery — never model-callable outside its declared flow.
    System,
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Tier::Pure       => write!(f, "pure"),
            Tier::Network    => write!(f, "network"),
            Tier::Filesystem => write!(f, "filesystem"),
            Tier::Code       => write!(f, "code"),
            Tier::System     => write!(f, "system"),
        }
    }
}

// Result of a tool execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub success: bool,
    pub output: String,
    #[serde(default)]
    pub error: Option<String>,
}

impl ToolResult {
    pub fn success(output: impl Into<String>) -> Self {
        Self {
            success: true,
            output: output.into(),
            error: None,
        }
    }
    
    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            success: false,
            output: String::new(),
            error: Some(error.into()),
        }
    }
}

// Arguments passed to a tool
pub type ToolArgs = serde_json::Value;

// Trait that all tools must implement
#[async_trait]
pub trait Tool: Send + Sync {
    // Get the tool name
    fn name(&self) -> &str;
    
    // Get a human-readable description
    fn description(&self) -> &str;
    
    // Get the JSON schema for arguments
    fn args_schema(&self) -> serde_json::Value;
    
    // Execute the tool with given arguments
    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError>;

    // Visibility tier. Defaults to [`ToolVisibility::User`] — override for
    // admin- or system-only tools.
    fn visibility(&self) -> ToolVisibility { ToolVisibility::User }

    // Capability tier — declares what resources the tool may touch. Defaults
    // to [`Tier::Pure`]; tools that hit the network, filesystem, or execute
    // code must override this so `/api/tools` can badge them accurately.
    fn tier(&self) -> Tier { Tier::Pure }

    // Whether the tool is currently operable. A tool may ship in the binary
    // but be effectively off (missing allowlist, missing backend, etc.); the
    // registry uses this to mark it in the UI without unregistering it.
    // Default `true`.
    fn enabled(&self) -> bool { true }
}

// Registry of available tools.
// // Tools are stored as `Arc<dyn Tool>` so a snapshot can be taken without
// copying the underlying tool state. The Skills system (slice A3.5) uses
// this to build a `BuiltinSnapshotDispatcher` that SkillTools call back
// into without an ownership cycle: the snapshot is taken *before*
// SkillTools are registered, so the dispatcher only references builtins.
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    // MCP tools, hot-swappable at runtime. Kept separate from the static
    // built-ins so the agent's shared `Arc<ToolRegistry>` can have its MCP
    // surface replaced via [`Self::set_mcp_tools`] — adding/removing an MCP
    // server takes effect without rebuilding the registry or restarting.
    // MCP tool names are `mcp__*`, so they never collide with built-ins;
    // lookups check `tools` first, then this map.
    mcp: RwLock<HashMap<String, Arc<dyn Tool>>>,
    audit: Option<Arc<ToolAuditStore>>,
    // when set, every `execute()` call emits a
    // `ToolInvocation` event to this engine before running the tool.
    // Deny short-circuits to a `ToolResult::failure("policy/<rule>
    // denied: <reason>")` so the model sees the gate fire and can
    // adjust its plan. Audit row is still written (with Failure
    // outcome) so denials are forensically visible.
    policy_engine: Option<Arc<dyn crate::policy::PolicyEngine>>,
}

impl ToolRegistry {
    // Create a new empty registry
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            mcp:   RwLock::new(HashMap::new()),
            audit: None,
            policy_engine: None,
        }
    }

    // Replace the entire MCP tool surface in one shot. Called at startup
    // and on every MCP-server CRUD change (hot-reload) — `&self`, so it
    // works through the shared `Arc<ToolRegistry>` the agent holds. Names
    // are `mcp__<server>__<tool>` (+ resource/prompt adapters).
    pub fn set_mcp_tools(&self, tools: Vec<Arc<dyn Tool>>) {
        let mut g = match self.mcp.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(), // poisoned: recover, we fully replace anyway
        };
        g.clear();
        for t in tools {
            g.insert(t.name().to_string(), t);
        }
        debug!("ToolRegistry: MCP surface reloaded ({} tools)", g.len());
    }

    // Snapshot of the current MCP tool `(name, Arc)` pairs. Used by the
    // list/visibility helpers to merge MCP tools with built-ins.
    fn mcp_snapshot(&self) -> Vec<Arc<dyn Tool>> {
        self.mcp.read()
            .map(|g| g.values().cloned().collect())
            .unwrap_or_default()
    }

    // Attach an audit store. Every subsequent `execute` call writes one
    // row; registries without a store skip auditing (useful for tests).
    pub fn with_audit(mut self, store: Arc<ToolAuditStore>) -> Self {
        self.audit = Some(store);
        self
    }

    // attach a [`crate::policy::PolicyEngine`]. Without
    // one, every tool call is allowed (legacy behaviour). With one,
    // tools whose call args carry an `_agent_id` get a
    // `ToolInvocation` policy consult before they run. Calls without
    // `_agent_id` (legacy / admin / internal) skip the consult — the
    // engine has no per-Skill rules to apply when there's no agent
    // in scope.
    pub fn with_policy_engine(
        mut self, engine: Arc<dyn crate::policy::PolicyEngine>,
    ) -> Self {
        self.policy_engine = Some(engine);
        self
    }

    // Whether a policy engine has been wired in. Useful for tests +
    // for tools that want to know whether to bother building a
    // context-rich args object.
    pub fn has_policy_engine(&self) -> bool {
        self.policy_engine.is_some()
    }

    // Borrow the wired-in policy engine. Used by other call sites
    // (e.g. the agent's tool loop in 1.3) that need to consult the
    // engine for events the registry itself doesn't generate
    // (`LlmCall`, `FilesystemAccess`, …) without standing up their
    // own engine handle.
    pub fn policy_engine(&self) -> Option<&Arc<dyn crate::policy::PolicyEngine>> {
        self.policy_engine.as_ref()
    }

    // Register a tool
    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        let name = tool.name().to_string();
        debug!("Registered tool: {}", name);
        self.tools.insert(name, Arc::new(tool));
    }

    pub fn is_empty(&self) -> bool { self.tools.is_empty() }

    // Get a tool by name. Returns an owned `Arc` (clone) so the lookup can
    // span both the static built-ins and the interior-mutable MCP map
    // without lending a reference out of the lock. Built-ins win on a name
    // clash (they never should — MCP names are `mcp__*`).
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if let Some(t) = self.tools.get(name) {
            return Some(Arc::clone(t));
        }
        self.mcp.read().ok().and_then(|g| g.get(name).cloned())
    }

    // Iterate over `(name, Arc<dyn Tool>)` pairs. Used by the Skills
    // system to snapshot builtins for SkillTool dispatch without
    // cloning tool state.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Arc<dyn Tool>)> {
        self.tools.iter()
    }

    // Execute a tool by name with arguments.
    //     // Writes one `tool_audit` row per call when the registry has an audit
    // store attached. The actor is read from the trusted `_user_id` key
    // that `chat` handlers inject into every tool call — if absent (tests,
    // legacy callers) the actor is recorded as `"unknown"`.
    pub async fn execute(
        &self,
        name: &str,
        args: ToolArgs,
    ) -> Result<ToolResult, MiraError> {
        let tool = self.get(name)
            .ok_or_else(|| MiraError::ToolError(format!("Unknown tool: {}", name)))?;

        debug!("Executing tool '{}' with args: {:?}", name, args);
        info!("[Tool: {}]", name);

        let actor = args.get("_user_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_owned();
        let digest        = args_digest(&args);
        let started_at_ms = Utc::now().timestamp_millis();
        let t0            = Instant::now();

        // consult the policy engine before running.
        // Skipped when (a) no engine is wired, OR (b) the args don't
        // carry an `_agent_id` (no agent context = nothing per-Skill
        // rules can match against). The shorter of the two early-outs
        // first to keep the legacy hot path branch-predictable.
        if let Some(engine) = &self.policy_engine {
            if let Some(agent_id) = parse_agent_id(&args) {
                let event = crate::policy::PolicyEvent::ToolInvocation {
                    agent_id,
                    skill_id:         parse_skill_id(&args),
                    tool:             name.to_owned(),
                    args_summary:     summarise_args(&args),
                    running_cost_usd: 0.0,
                    session_cost_usd: 0.0,
                };
                if let crate::policy::PolicyDecision::Deny { rule, reason } =
                    engine.evaluate(&event).await
                {
                    let err_msg = format!("policy/{rule} denied: {reason}");
                    warn!("tool '{}' denied by policy: {}", name, err_msg);
                    // Audit the denial so a per-rule view sees it
                    // even though the underlying tool never ran.
                    if let Some(store) = self.audit.as_ref() {
                        let _ = store.record(
                            &actor, name, &digest, started_at_ms,
                            t0.elapsed().as_millis() as i64,
                            Outcome::Failure, Some(&err_msg),
                        );
                    }
                    return Ok(ToolResult::failure(err_msg));
                }
            }
        }

        let result = tool.execute(args).await;
        let duration_ms = t0.elapsed().as_millis() as i64;

        let (outcome, snippet) = match &result {
            Ok(r) if r.success => {
                debug!("Tool succeeded");
                (Outcome::Success, Some(truncate_output(&r.output)))
            }
            Ok(r) => {
                warn!("Tool failed: {:?}", r.error);
                (Outcome::Failure, r.error.as_deref().map(truncate_output))
            }
            Err(e) => {
                warn!("Tool error: {}", e);
                (Outcome::Error, Some(truncate_output(&e.to_string())))
            }
        };

        if let Some(store) = self.audit.as_ref() {
            if let Err(e) = store.record(
                &actor, name, &digest, started_at_ms, duration_ms,
                outcome, snippet.as_deref(),
            ) {
                // Audit failure must not break tool execution.
                warn!("tool_audit write failed: {}", e);
            }
        }

        result
    }
    
    // Get all tool names. Includes every tier — mostly useful for tests and
    // health checks. User-facing surfaces should use [`Self::list_visible_tools`]
    // to hide system internals.
    pub fn list_tools(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tools.keys().cloned().collect();
        names.extend(self.mcp_snapshot().iter().map(|t| t.name().to_owned()));
        names
    }

    // Tool names appropriate for a user-facing palette: `User` + `Admin`
    // tiers, never `System`. `/api/tools` uses this.
    pub fn list_visible_tools(&self) -> Vec<String> {
        self.tools.values()
            .chain(self.mcp_snapshot().iter())
            .filter(|t| !matches!(t.visibility(), ToolVisibility::System { .. }))
            .map(|t| t.name().to_owned())
            .collect()
    }

    // Tool names allowed for a given conversation flow.
    //     // - `"chat"` → `User` + `Admin` tiers (the normal chat tool set).
    // - any other name → `System` tools whose `flow` matches exactly.
    //     // System tools are never mixed with user-tier tools; a flow is either
    // an internal MIRA flow or a normal chat. This keeps onboarding from
    // accidentally inheriting destructive user tools.
    pub fn list_for_flow(&self, flow: &str) -> Vec<String> {
        self.tools.values()
            .chain(self.mcp_snapshot().iter())
            .filter(|t| match t.visibility() {
                ToolVisibility::User | ToolVisibility::Admin => flow == "chat",
                ToolVisibility::System { flow: f }           => f == flow,
            })
            .map(|t| t.name().to_owned())
            .collect()
    }
    
    // Build a description of all tools for the model
    pub fn build_tool_descriptions(&self) -> String {
        let mut desc = String::from("Available Tools:\n\n");
        
        for name in self.tools.keys() {
            if let Some(tool) = self.tools.get(name) {
                desc.push_str(&format!("- {}: {}\n", name, tool.description()));
            }
        }
        
        desc
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ───  helpers ──────────────────────────────────────────────

// Pull the trusted `_agent_id` out of injected args. Returns `None`
// when the key is absent or unparseable — the policy consult skips
// in that case (no agent context = no per-Skill rules to apply).
fn parse_agent_id(args: &ToolArgs) -> Option<crate::agent::instance::AgentId> {
    args.get("_agent_id")
        .and_then(|v| v.as_str())
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(crate::agent::instance::AgentId)
}

// Pull the trusted `_skill_id` out of injected args. Returns `None`
// for built-in tool calls (no Skill in scope).
fn parse_skill_id(args: &ToolArgs) -> Option<String> {
    args.get("_skill_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

// Build a short, redaction-safe summary of `args` for a `ToolInvocation`
// event. Critically this is NOT the raw JSON — args may contain
// secrets the model passed through. Strategy:
// - List the top-level keys (excluding the injected `_*` ones,
// which are not user-visible payload).
// - Include the value for keys whose name suggests it's a query
// or path (safe-by-convention) — but truncated.
// - Otherwise just the key.
// // The summary is bounded — admin-defined rules can match against
// substrings here, but they CAN'T see secret values. Engines that
// need raw args for stricter checks should consult the audit-log
// `args_digest` instead (which is one-way hashed, also no leaks).
fn summarise_args(args: &ToolArgs) -> String {
    let Some(obj) = args.as_object() else {
        return String::from("(non-object args)");
    };
    let mut parts = Vec::new();
    for (k, v) in obj {
        if k.starts_with('_') { continue; } // skip injected metadata
        if SAFE_TO_PREVIEW_KEYS.contains(&k.as_str()) {
            if let Some(s) = v.as_str() {
                let preview: String = s.chars().take(80).collect();
                parts.push(format!("{k}={preview}"));
                continue;
            }
        }
        parts.push(k.clone());
    }
    parts.join(" ")
}

// Keys whose values are safe to include in a ToolInvocation event
// summary — query strings, URLs, and file paths are useful for
// admin rules ("deny if `url` matches *.evil.com") and don't carry
// secret material by convention. Everything else stays as just the
// key name. Order doesn't matter.
const SAFE_TO_PREVIEW_KEYS: &[&str] = &[
    "url", "path", "query", "command", "host", "domain", "name",
    "skill_id", "tool",
];

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str { "echo" }
        fn description(&self) -> &str { "Echoes the input back" }
        fn args_schema(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {"message": {"type": "string"}}})
        }
        async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
            let msg = args.get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("(empty)");
            Ok(ToolResult::success(msg.to_string()))
        }
    }

    struct FailTool;

    #[async_trait]
    impl Tool for FailTool {
        fn name(&self) -> &str { "fail_tool" }
        fn description(&self) -> &str { "Always fails" }
        fn args_schema(&self) -> serde_json::Value { json!({}) }
        async fn execute(&self, _args: ToolArgs) -> Result<ToolResult, MiraError> {
            Ok(ToolResult::failure("intentional failure"))
        }
    }

    #[test]
    fn test_tool_result_success() {
        let r = ToolResult::success("hello");
        assert!(r.success);
        assert_eq!(r.output, "hello");
        assert!(r.error.is_none());
    }

    #[test]
    fn test_tool_result_failure() {
        let r = ToolResult::failure("something went wrong");
        assert!(!r.success);
        assert!(r.output.is_empty());
        assert_eq!(r.error, Some("something went wrong".to_string()));
    }

    #[test]
    fn test_registry_register_and_list() {
        let mut reg = ToolRegistry::new();
        reg.register(EchoTool);
        let tools = reg.list_tools();
        assert!(tools.contains(&"echo".to_string()));
    }

    struct McpEcho;
    #[async_trait]
    impl Tool for McpEcho {
        fn name(&self) -> &str { "mcp__test__echo" }
        fn description(&self) -> &str { "mcp echo" }
        fn args_schema(&self) -> serde_json::Value { json!({}) }
        async fn execute(&self, _a: ToolArgs) -> Result<ToolResult, MiraError> {
            Ok(ToolResult::success("ok"))
        }
    }

    #[tokio::test]
    async fn mcp_tools_hot_swap_via_set_mcp_tools() {
        // set_mcp_tools is &self (interior-mutable) — works on a shared,
        // non-mut registry, which is the whole point of hot-reload.
        let reg = ToolRegistry::new();
        assert!(reg.get("mcp__test__echo").is_none());

        reg.set_mcp_tools(vec![std::sync::Arc::new(McpEcho)]);
        assert!(reg.get("mcp__test__echo").is_some(), "get sees the hot-added tool");
        assert!(reg.list_tools().contains(&"mcp__test__echo".to_string()));
        assert!(reg.list_for_flow("chat").contains(&"mcp__test__echo".to_string()),
            "hot-added MCP tool is exposed to the chat flow");
        assert!(reg.execute("mcp__test__echo", json!({})).await.unwrap().success);

        // Hot-remove: replacing with an empty set drops it immediately.
        reg.set_mcp_tools(vec![]);
        assert!(reg.get("mcp__test__echo").is_none(), "tool removed after reload");
        assert!(!reg.list_tools().contains(&"mcp__test__echo".to_string()));
    }

    #[tokio::test]
    async fn test_registry_execute_known_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(EchoTool);
        let result = reg.execute("echo", json!({"message": "hello world"})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output, "hello world");
    }

    #[tokio::test]
    async fn test_registry_execute_unknown_tool() {
        let reg = ToolRegistry::new();
        let err = reg.execute("nonexistent", json!({})).await.unwrap_err();
        assert!(matches!(err, MiraError::ToolError(_)));
    }

    #[tokio::test]
    async fn test_registry_failing_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(FailTool);
        let result = reg.execute("fail_tool", json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[test]
    fn test_build_tool_descriptions() {
        let mut reg = ToolRegistry::new();
        reg.register(EchoTool);
        let desc = reg.build_tool_descriptions();
        assert!(desc.contains("echo"));
        assert!(desc.contains("Echoes the input back"));
    }

    #[test]
    fn test_get_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(EchoTool);
        assert!(reg.get("echo").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    struct SystemTool;
    #[async_trait]
    impl Tool for SystemTool {
        fn name(&self) -> &str { "system_only" }
        fn description(&self) -> &str { "system-tier" }
        fn args_schema(&self) -> serde_json::Value { json!({}) }
        async fn execute(&self, _args: ToolArgs) -> Result<ToolResult, MiraError> {
            Ok(ToolResult::success(""))
        }
        fn visibility(&self) -> ToolVisibility { ToolVisibility::system("onboarding") }
    }

    struct AdminTool;
    #[async_trait]
    impl Tool for AdminTool {
        fn name(&self) -> &str { "admin_tool" }
        fn description(&self) -> &str { "admin" }
        fn args_schema(&self) -> serde_json::Value { json!({}) }
        async fn execute(&self, _args: ToolArgs) -> Result<ToolResult, MiraError> {
            Ok(ToolResult::success(""))
        }
        fn visibility(&self) -> ToolVisibility { ToolVisibility::Admin }
    }

    #[test]
    fn list_visible_tools_hides_system_tier() {
        let mut reg = ToolRegistry::new();
        reg.register(EchoTool);        // User (default)
        reg.register(AdminTool);       // Admin
        reg.register(SystemTool);      // System { flow = onboarding }

        let visible = reg.list_visible_tools();
        assert!(visible.contains(&"echo".to_string()));
        assert!(visible.contains(&"admin_tool".to_string()));
        assert!(!visible.contains(&"system_only".to_string()),
            "system-tier must not appear in user-facing list");
    }

    #[test]
    fn list_for_flow_routes_system_and_chat_disjointly() {
        let mut reg = ToolRegistry::new();
        reg.register(EchoTool);
        reg.register(AdminTool);
        reg.register(SystemTool);

        let chat = reg.list_for_flow("chat");
        assert!(chat.contains(&"echo".to_string()));
        assert!(chat.contains(&"admin_tool".to_string()));
        assert!(!chat.contains(&"system_only".to_string()));

        let onboarding = reg.list_for_flow("onboarding");
        assert_eq!(onboarding, vec!["system_only".to_string()]);

        // Unknown flow gets nothing — no silent fallback to user-tier.
        assert!(reg.list_for_flow("unknown_flow").is_empty());
    }

    #[tokio::test]
    async fn execute_writes_one_audit_row_per_call() {
        let tmp   = tempfile::tempdir().unwrap();
        let store = Arc::new(ToolAuditStore::open(&tmp.path().join("tools.db")).unwrap());
        let mut reg = ToolRegistry::new().with_audit(Arc::clone(&store));
        reg.register(EchoTool);
        reg.register(FailTool);

        // Successful call — actor read from the trusted `_user_id` key.
        reg.execute("echo", json!({"message": "hi", "_user_id": "u1"})).await.unwrap();
        // Failure path — still audited.
        reg.execute("fail_tool", json!({"_user_id": "u1"})).await.unwrap();
        // Missing actor — recorded as "unknown", not dropped.
        reg.execute("echo", json!({"message": "anon"})).await.unwrap();
        // Unknown tool — Err(..) path must also write a row.
        let _ = reg.execute("nope", json!({"_user_id": "u1"})).await;

        // Unknown-tool dispatch returns early before the audit write, so we
        // expect 3 rows (two successes + one failure). This is a
        // deliberate choice — unknown tools have no tier to attribute.
        assert_eq!(store.count().unwrap(), 3);
    }

    // ── policy engine seam ─────────────────────────────────

    use crate::policy::{
        AllowAllEngine, DenyAllEngine, PolicyDecision, PolicyEngine, PolicyEvent,
    };
    use crate::agent::instance::AgentId;
    use std::sync::Mutex as StdMutex;

    // Engine that records every event it sees + replies with a closure.
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

    fn registry_with_engine(engine: Arc<dyn PolicyEngine>) -> ToolRegistry {
        let mut reg = ToolRegistry::new().with_policy_engine(engine);
        reg.register(EchoTool);
        reg
    }

    #[test]
    fn has_policy_engine_reflects_with_policy_engine() {
        let reg = ToolRegistry::new();
        assert!(!reg.has_policy_engine());
        let reg = reg.with_policy_engine(Arc::new(AllowAllEngine));
        assert!(reg.has_policy_engine());
    }

    #[tokio::test]
    async fn engine_consult_skipped_when_args_lack_agent_id() {
        let engine = Arc::new(RecordingEngine {
            seen: StdMutex::new(Vec::new()),
            // would deny if called — verify it's NOT called.
            decide: Box::new(|_| PolicyDecision::deny("would-fire", "x")),
        });
        let reg = registry_with_engine(engine.clone());
        // No `_agent_id` → engine skipped; tool runs normally.
        let r = reg.execute("echo", json!({"message": "hi", "_user_id": "u1"}))
            .await.unwrap();
        assert!(r.success);
        assert_eq!(r.output, "hi");
        assert!(engine.seen.lock().unwrap().is_empty(),
            "engine consulted even though _agent_id was absent");
    }

    #[tokio::test]
    async fn engine_receives_tool_invocation_event_when_agent_id_present() {
        let engine = Arc::new(RecordingEngine {
            seen: StdMutex::new(Vec::new()),
            decide: Box::new(|_| PolicyDecision::Allow),
        });
        let reg = registry_with_engine(engine.clone());
        let agent_id = AgentId::new();
        let _ = reg.execute("echo", json!({
            "message":   "hi",
            "_user_id":  "u1",
            "_agent_id": agent_id.to_string(),
            "_skill_id": "com.example.x",
        })).await.unwrap();

        let seen = engine.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "expected exactly one event");
        match &seen[0] {
            PolicyEvent::ToolInvocation { agent_id: aid, skill_id, tool, args_summary, .. } => {
                assert_eq!(*aid, agent_id);
                assert_eq!(skill_id.as_deref(), Some("com.example.x"));
                assert_eq!(tool, "echo");
                // Summary mentions key names but NOT injected `_*`.
                assert!(args_summary.contains("message"),  "got: {args_summary}");
                assert!(!args_summary.contains("_user_id"), "leaked _user_id: {args_summary}");
                assert!(!args_summary.contains("_agent_id"),"leaked _agent_id: {args_summary}");
            }
            other => panic!("expected ToolInvocation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn engine_deny_short_circuits_to_failure_with_rule_in_message() {
        let reg = registry_with_engine(
            Arc::new(DenyAllEngine::new("blocked for tests"))
        );
        let r = reg.execute("echo", json!({
            "message": "hi", "_user_id": "u1",
            "_agent_id": AgentId::new().to_string(),
        })).await.unwrap();
        assert!(!r.success);
        let err = r.error.unwrap_or_default();
        assert!(err.contains("policy/test/deny-all"), "got: {err}");
        assert!(err.contains("blocked for tests"),    "got: {err}");
    }

    #[tokio::test]
    async fn engine_deny_writes_failure_audit_row() {
        let tmp   = tempfile::tempdir().unwrap();
        let store = Arc::new(ToolAuditStore::open(&tmp.path().join("tools.db")).unwrap());
        let mut reg = ToolRegistry::new()
            .with_audit(Arc::clone(&store))
            .with_policy_engine(Arc::new(DenyAllEngine::new("nope")));
        reg.register(EchoTool);

        let _ = reg.execute("echo", json!({
            "message": "hi", "_user_id": "u1",
            "_agent_id": AgentId::new().to_string(),
        })).await.unwrap();

        // Even though the tool itself never ran, the denial is
        // forensically visible — one Failure row.
        assert_eq!(store.count().unwrap(), 1);
    }

    #[tokio::test]
    async fn allow_engine_lets_tool_proceed_normally() {
        let reg = registry_with_engine(Arc::new(AllowAllEngine));
        let r = reg.execute("echo", json!({
            "message": "hi", "_user_id": "u1",
            "_agent_id": AgentId::new().to_string(),
        })).await.unwrap();
        assert!(r.success);
        assert_eq!(r.output, "hi");
    }

    #[tokio::test]
    async fn invalid_agent_id_string_treated_as_missing() {
        // A garbage `_agent_id` should NOT trigger a panic or a
        // "deny by default." The consult is simply skipped (same as
        // when `_agent_id` is absent altogether) and the tool runs.
        let engine = Arc::new(RecordingEngine {
            seen: StdMutex::new(Vec::new()),
            decide: Box::new(|_| PolicyDecision::deny("rule", "reason")),
        });
        let reg = registry_with_engine(engine.clone());
        let r = reg.execute("echo", json!({
            "message": "hi", "_user_id": "u1",
            "_agent_id": "not-a-uuid",
        })).await.unwrap();
        assert!(r.success, "tool should have run when agent_id was unparseable");
        assert!(engine.seen.lock().unwrap().is_empty());
    }

    // ── summarise_args helper unit tests ──────────────────────────────

    #[test]
    fn summarise_args_lists_user_keys_excludes_injected_metadata() {
        let s = summarise_args(&json!({
            "url":        "https://x.test",
            "max_chars":  500,
            "_user_id":   "u1",
            "_agent_id":  "a",
            "_skill_id":  "s",
        }));
        assert!(s.contains("url=https://x.test"), "got: {s}");
        assert!(s.contains("max_chars"),          "got: {s}");
        assert!(!s.contains("_user_id"),  "leaked: {s}");
        assert!(!s.contains("_agent_id"), "leaked: {s}");
        assert!(!s.contains("_skill_id"), "leaked: {s}");
    }

    #[test]
    fn summarise_args_truncates_long_safe_value_strings() {
        let long = "x".repeat(200);
        let s = summarise_args(&json!({"query": long}));
        assert!(s.starts_with("query=xxxxx"));
        // 80-char preview + "query=" prefix.
        assert!(s.len() <= "query=".len() + 80, "got: len={}", s.len());
    }

    #[test]
    fn summarise_args_omits_value_for_unsafe_keys() {
        // `secret` is NOT in SAFE_TO_PREVIEW_KEYS — only the key name
        // surfaces, never the value.
        let s = summarise_args(&json!({"secret": "shhhh"}));
        assert!(s.contains("secret"));
        assert!(!s.contains("shhhh"), "value leaked for unsafe key: {s}");
    }

    #[test]
    fn summarise_args_handles_non_object_args() {
        let s = summarise_args(&json!("just a string"));
        assert!(s.contains("non-object"));
    }

    // ── parse_agent_id / parse_skill_id ─────────────────────────────

    #[test]
    fn parse_agent_id_returns_some_on_valid_uuid() {
        let id = AgentId::new();
        let got = parse_agent_id(&json!({"_agent_id": id.to_string()}));
        assert_eq!(got, Some(id));
    }

    #[test]
    fn parse_agent_id_returns_none_on_missing_or_bad_input() {
        assert_eq!(parse_agent_id(&json!({})), None);
        assert_eq!(parse_agent_id(&json!({"_agent_id": "not-a-uuid"})), None);
        assert_eq!(parse_agent_id(&json!({"_agent_id": 42})), None);
    }

    #[test]
    fn parse_skill_id_filters_empty_string() {
        assert_eq!(parse_skill_id(&json!({"_skill_id": "com.x"})),
                   Some("com.x".into()));
        assert_eq!(parse_skill_id(&json!({"_skill_id": ""})), None);
        assert_eq!(parse_skill_id(&json!({})),                None);
    }
}
