// SPDX-License-Identifier: AGPL-3.0-or-later

//! `SkillTool` — exposes one loaded Skill as a single entry in the agent's
//! tool list.
//!
//! Why one entry per Skill instead of one entry per tool? LLMs degrade
//! sharply once their tool list grows past ~30 — splitting Skills into
//! individual tools would dump dozens of entries on the model. Bundling
//! each Skill as a single router with a `tool` arg keeps the working set
//! small while still giving the model a structured way to pick.
//!
//! When the agent calls `<skill>(tool=X, args={...})`:
//!   - **prompt** tools resolve the template and return its contents.
//!     The agent then uses that text as context for its next reasoning
//!     step. No LLM call from inside the tool — that's the agent's job.
//!   - **builtin** tools forward to an existing `src/tools/` entry via
//!     the supplied `BuiltinDispatcher`. The dispatcher is injected so
//!     this module doesn't need to carry an `Arc<ToolRegistry>` and run
//!     into ownership cycles.
//!   - **executable** tools are deferred — they need the sandbox bind-mount
//!     story finalised. Returns a clear "not yet" error so the agent
//!     surfaces it to the user.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, info, warn};

use crate::MiraError;
use crate::tools::{Tool, ToolArgs, ToolRegistry, ToolResult, ToolVisibility, Tier};
use crate::skills::loader::{LoadedSkill, SkillRegistry};
use crate::skills::manifest::ToolSpec;
use crate::skills::prefs::SkillPrefsStore;

/// What the SkillTool needs in order to forward `kind = builtin` calls
/// without holding an `Arc<ToolRegistry>` (which would create a cycle if
/// SkillTools live inside the same registry).
#[async_trait]
pub trait BuiltinDispatcher: Send + Sync {
    /// Invoke the registered tool by name. Errors propagate the same
    /// `MiraError` the original tool would have produced.
    async fn invoke_builtin(&self, name: &str, args: ToolArgs) -> Result<ToolResult, MiraError>;
}

/// Enforces the execution invariant: a skill that re-exposes a
/// `builtin` tool (Tier-1) must never hand a caller a tool their RBAC — or
/// Restricted Mode — denies directly. Given the calling user and the builtin
/// impl name, returns whether the call may proceed. Implemented over the auth
/// service in the gateway; `None` on a `SkillTool` (tests/CLI) means "allow"
/// (no multi-tenant context to enforce against).
pub trait SkillCapGate: Send + Sync {
    fn builtin_allowed(&self, user_id: Option<&str>, builtin_impl: &str) -> bool;
}

/// Tool-trait wrapper around a `LoadedSkill`. Agent sees one of these per
/// installed Skill.
pub struct SkillTool {
    /// Sanitised name surfaced to the LLM (dots → underscores). Tool
    /// naming conventions across providers are stricter than reverse-DNS.
    name: String,

    /// Cached so we don't re-render it on every `description()` call.
    description: String,

    /// JSON schema declaring `{tool: enum, args: object}`.
    args_schema: serde_json::Value,

    skill: LoadedSkill,
    dispatcher: Arc<dyn BuiltinDispatcher>,

    /// Per-user enable/disable lookup (slice A5). Optional so tests don't
    /// have to thread a store through every constructor — None is treated
    /// as "always enabled".
    prefs: Option<Arc<SkillPrefsStore>>,

    /// Owning user id for a USER-SCOPED skill.
    /// `None` = global (visible to all). `Some(uid)` = private: offered only to
    /// `uid` and refused for any other caller as a hard execution backstop.
    owner: Option<String>,

    /// Enforces the Tier-1 invariant on `builtin` dispatch. `None` =
    /// no multi-tenant context (tests/CLI) → allow.
    cap_gate: Option<Arc<dyn SkillCapGate>>,
}

impl SkillTool {
    pub fn new(skill: LoadedSkill, dispatcher: Arc<dyn BuiltinDispatcher>) -> Self {
        let name = sanitise_tool_name(&skill.manifest.skill.id);
        let description = render_description(&skill);
        let args_schema = render_args_schema(&skill);
        Self { name, description, args_schema, skill, dispatcher, prefs: None, owner: None, cap_gate: None }
    }

    /// Attach the capability gate that enforces the invariant on `builtin`
    /// tool dispatch (a skill can't hand a caller a tool their RBAC denies).
    pub fn with_cap_gate(mut self, gate: Option<Arc<dyn SkillCapGate>>) -> Self {
        self.cap_gate = gate;
        self
    }

    /// Add a per-user prefs store. Calls from disabled users get refused
    /// before any tool work happens.
    pub fn with_prefs(mut self, prefs: Arc<SkillPrefsStore>) -> Self {
        self.prefs = Some(prefs);
        self
    }

    /// Mark this skill as user-scoped (private to `owner`). A non-owner caller
    /// is refused at execution and the skill is filtered out of other users'
    /// tool sets. `None`/unset = global.
    pub fn with_owner(mut self, owner: Option<String>) -> Self {
        self.owner = owner;
        self
    }

    /// Skill ID (the original reverse-DNS form, not the sanitised tool name).
    pub fn skill_id(&self) -> &str {
        &self.skill.manifest.skill.id
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct SkillCall {
    tool: String,
    #[serde(default = "default_args")]
    args: ToolArgs,
}

fn default_args() -> ToolArgs { json!({}) }

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str { &self.name }
    fn description(&self) -> &str { &self.description }
    fn args_schema(&self) -> serde_json::Value { self.args_schema.clone() }
    fn visibility(&self) -> ToolVisibility { ToolVisibility::User }
    fn skill_owner(&self) -> Option<&str> { self.owner.as_deref() }
    fn tier(&self) -> Tier {
        // Tier surfaces "what kind of resources does this touch" to the
        // /api/tools listing. A Skill can touch any tier depending on its
        // contents — pick the strongest declared in the manifest.
        if !self.skill.manifest.permissions.network_egress.is_empty() {
            Tier::Network
        } else if !self.skill.manifest.permissions.filesystem.is_empty() {
            Tier::Filesystem
        } else if self.skill.manifest.permissions.subprocess {
            Tier::Code
        } else {
            Tier::Pure
        }
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        // User-scope backstop: a private, user-authored skill is only
        // executable by its owner. This is the HARD security guarantee —
        // enforced here even if offering-filters are bypassed — so one tenant's
        // self-authored skill can never run for another. Absent `_user_id`
        // (tests, admin /api/tools/run) fails CLOSED for an owned skill, unlike
        // the fail-open disable check below, because owner identity is a
        // security boundary, not a convenience preference.
        if let Some(owner) = self.owner.as_deref() {
            let caller = args.get("_user_id").and_then(|v| v.as_str());
            if caller != Some(owner) {
                return Err(MiraError::ToolError(format!(
                    "skill {:?}: this is a private skill belonging to another user",
                    self.skill_id(),
                )));
            }
        }

        // Per-user enable/disable check. The chat handler injects
        // `_user_id` into every tool call; absence (tests, direct
        // /api/tools/run by an admin) skips the check — same fail-open
        // policy as the audit's "unknown" actor.
        if let (Some(prefs), Some(user_id)) = (
            self.prefs.as_ref(),
            args.get("_user_id").and_then(|v| v.as_str()),
        ) {
            if prefs.is_disabled(user_id, self.skill_id()) {
                return Err(MiraError::ToolError(format!(
                    "skill {:?}: disabled by user — re-enable on the Skills page to use it",
                    self.skill_id(),
                )));
            }
        }

        let call: SkillCall = serde_json::from_value(args.clone())
            .map_err(|e| MiraError::ToolError(format!(
                "skill {:?}: invalid arguments — expected {{ tool: string, args: object }}: {e}",
                self.skill_id(),
            )))?;

        let spec = self.skill.manifest.tools.get(&call.tool).ok_or_else(|| {
            let available = self.skill.manifest.tools.keys().cloned().collect::<Vec<_>>().join(", ");
            MiraError::ToolError(format!(
                "skill {:?}: tool {:?} not found. Available: [{available}]",
                self.skill_id(), call.tool,
            ))
        })?;

        match spec {
            ToolSpec::Prompt { template } => {
                let path = self.skill.root_dir.join(template);
                resolve_prompt(&path).map(ToolResult::success).map_err(|e| {
                    MiraError::ToolError(format!(
                        "skill {:?} tool {:?}: failed to read prompt template {}: {e}",
                        self.skill_id(), call.tool, path.display(),
                    ))
                })
            }
            ToolSpec::Builtin { r#impl } => {
                // Invariant: a skill re-exposing a builtin must not run
                // it for a caller whose RBAC / Restricted Mode denies it directly.
                // Effective power = intersection(skill, caller). Enforced before
                // any work; `None` gate (tests/CLI) = allow.
                if let Some(gate) = self.cap_gate.as_ref() {
                    let caller = args.get("_user_id").and_then(|v| v.as_str());
                    if !gate.builtin_allowed(caller, r#impl) {
                        return Err(MiraError::ToolError(format!(
                            "skill {:?}: this skill uses the {:?} tool, which your permissions don't allow",
                            self.skill_id(), r#impl,
                        )));
                    }
                }
                // Forward the handler-injected context (`_user_id`,
                // `_conversation_id`, `_channel`, `_user_tz`, …) from the outer,
                // trusted envelope onto the inner args the model supplied, so
                // user-scoped builtins (wiki, memory, calendar) reached *through
                // a skill wrapper* still see the caller's identity. Without this
                // the inner `call.args` is all the builtin gets and every
                // `require_user_id` check fails.
                //
                // SECURITY: the outer values are injected by the chat handler and
                // MUST win over anything the model placed inside `args` — a model
                // must never be able to spoof `_user_id` by putting it in the
                // object it controls. We only forward `_`-prefixed keys (the
                // reserved injected-context namespace), never model-facing args.
                let mut inner = call.args;
                if !inner.is_object() {
                    // Malformed (non-object) model args — start a fresh object so
                    // the injected context still reaches the builtin.
                    inner = serde_json::Value::Object(serde_json::Map::new());
                }
                if let (Some(outer), Some(inner_obj)) = (args.as_object(), inner.as_object_mut()) {
                    for (key, value) in outer {
                        if key.starts_with('_') {
                            inner_obj.insert(key.clone(), value.clone());
                        }
                    }
                }
                self.dispatcher.invoke_builtin(r#impl, inner).await
            }
            ToolSpec::Executable { .. } => {
                // Deferred: needs the sandbox bind-mount mapping (see
                // skills::permissions::build_sandbox_limits comment) plus
                // the executable runner. Tracked for a follow-up slice.
                Err(MiraError::ToolError(format!(
                    "skill {:?} tool {:?}: executable tools are not yet supported in this MIRA build. \
                     The runner is tracked as a follow-up.",
                    self.skill_id(), call.tool,
                )))
            }
        }
    }
}

/// LLM tool naming conventions don't allow dots (Anthropic) or some other
/// punctuation across providers. Reverse-DNS Skill IDs use dots, so sanitise
/// to underscores. The `skill_id()` accessor keeps the original form for
/// logs / audit / config.
fn sanitise_tool_name(id: &str) -> String {
    id.chars().map(|c| if c == '.' || c == '-' { '_' } else { c }).collect()
}

/// Human-readable description shown to the LLM. Includes the manifest's
/// own description plus a per-tool one-line summary so the model can pick
/// the right `tool` in a single message instead of guessing.
fn render_description(skill: &LoadedSkill) -> String {
    let m = &skill.manifest;
    let mut s = String::new();
    s.push_str(&m.skill.description);
    if !m.tools.is_empty() {
        s.push_str("\n\nAvailable tools (pass one as `tool`):");
        // Keep deterministic ordering for stable LLM prompts.
        let mut tool_names: Vec<&String> = m.tools.keys().collect();
        tool_names.sort();
        for name in tool_names {
            let kind = match &m.tools[name] {
                ToolSpec::Builtin    { .. } => "builtin",
                ToolSpec::Prompt     { .. } => "prompt",
                ToolSpec::Executable { .. } => "executable",
            };
            s.push_str(&format!("\n  - {name} ({kind})"));
        }
    }
    s
}

/// `{tool: enum, args: object}` schema. Per-tool args schemas aren't on
/// the manifest yet — when they land, this becomes a `oneOf` with each
/// branch's args typed. For now `args` is open-shape and per-tool
/// validation happens inside dispatch.
fn render_args_schema(skill: &LoadedSkill) -> serde_json::Value {
    let mut tool_names: Vec<&String> = skill.manifest.tools.keys().collect();
    tool_names.sort();
    let enum_values: Vec<serde_json::Value> = tool_names.iter().map(|n| json!(n)).collect();

    json!({
        "type": "object",
        "properties": {
            "tool": {
                "type": "string",
                "enum": enum_values,
                "description": "Which tool in this Skill to invoke."
            },
            "args": {
                "type": "object",
                "description": "Arguments for the chosen tool. Shape depends on the tool selected — see the Skill's documentation for per-tool argument schemas."
            }
        },
        "required": ["tool", "args"]
    })
}

fn resolve_prompt(path: &PathBuf) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

// ── Built-in dispatchers ─────────────────────────────────────────────────

/// `BuiltinDispatcher` backed by a frozen snapshot of `Arc<dyn Tool>`s.
/// Take this snapshot *before* registering any `SkillTool`s into the same
/// `ToolRegistry`: that way the dispatcher only references builtins, and
/// the `SkillTool ↔ ToolRegistry` ownership cycle never forms.
pub struct BuiltinSnapshotDispatcher {
    table: HashMap<String, Arc<dyn Tool>>,
    // Restricted Mode gate, snapshotted from the registry. Skills invoke builtins
    // through this dispatcher, which calls `tool.execute()` DIRECTLY — bypassing
    // `ToolRegistry::execute`'s gate. So the same fail-closed check is applied
    // here too, else a Skill would be a hole straight through Restricted Mode.
    restricted: Option<Arc<crate::policy::RestrictedPolicy>>,
}

impl BuiltinSnapshotDispatcher {
    /// Snapshot every tool currently in `registry` into the dispatcher's
    /// table. Subsequent registrations into the registry are *not*
    /// reflected — that's the whole point.
    pub fn from_registry(registry: &ToolRegistry) -> Self {
        let table = registry.iter()
            .map(|(name, tool)| (name.clone(), tool.clone()))
            .collect();
        Self { table, restricted: registry.restricted_policy() }
    }

    /// Number of tools captured. Mostly useful for diagnostics.
    pub fn len(&self) -> usize { self.table.len() }
    pub fn is_empty(&self) -> bool { self.table.is_empty() }
}

#[async_trait]
impl BuiltinDispatcher for BuiltinSnapshotDispatcher {
    async fn invoke_builtin(&self, name: &str, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let tool = self.table.get(name).ok_or_else(|| {
            MiraError::ToolError(format!(
                "skill called builtin {name:?}, which isn't registered in this MIRA build",
            ))
        })?;
        // Apply the same fail-closed Restricted Mode gate the registry applies —
        // a Skill must never be a bypass around it.
        if let Some(restricted) = &self.restricted {
            if let crate::policy::RestrictedDecision::Deny { reason } =
                restricted.check_tool(name, tool.tier())
            {
                return Ok(ToolResult::failure(reason));
            }
        }
        tool.execute(args).await
    }
}

// ── Bulk registration helper ─────────────────────────────────────────────

/// Register every Skill in `skills` as a `SkillTool` in `registry`. Each
/// SkillTool dispatches `kind = builtin` calls through `dispatcher` —
/// caller is responsible for ensuring the dispatcher's snapshot was taken
/// *before* this call so the ownership graph stays acyclic.
///
/// `prefs` is optional; pass `Some(...)` to honour per-user disable
/// preferences (slice A5). Tests can pass `None` for unconditional access.
///
/// Returns the count of successfully-registered Skills. Loader-side
/// errors (malformed manifests, missing files) are already in
/// `skills.errors` and surface separately in the web UI.
/// Build the `SkillTool` router tools for every loaded skill, as owned
/// `Arc<dyn Tool>` — the shared half of startup registration and hot-reload.
/// The dispatcher must snapshot ONLY builtins (never other SkillTools) so the
/// ownership graph stays acyclic.
pub fn build_skill_tools(
    skills: &SkillRegistry,
    dispatcher: Arc<dyn BuiltinDispatcher>,
    prefs: Option<Arc<SkillPrefsStore>>,
    owner: Option<&str>,
    cap_gate: Option<Arc<dyn SkillCapGate>>,
) -> Vec<Arc<dyn Tool>> {
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();
    for skill in skills.iter() {
        let id = skill.manifest.skill.id.clone();
        let mut tool = SkillTool::new(skill.clone(), dispatcher.clone());
        if let Some(p) = prefs.as_ref() {
            tool = tool.with_prefs(p.clone());
        }
        tool = tool.with_owner(owner.map(str::to_string)).with_cap_gate(cap_gate.clone());
        debug!("Built Skill {id:?} as router tool {:?} (owner={owner:?})", tool.name());
        out.push(Arc::new(tool));
    }
    if !skills.errors.is_empty() {
        warn!(
            "Skills loader: {} built, {} skipped due to errors (see /api/skills for details)",
            out.len(), skills.errors.len(),
        );
    }
    out
}

/// Install the loaded skills onto the registry's hot-swappable skill surface
/// (`set_skill_tools`). Works at startup (before the Arc wrap, via `&mut` deref)
/// AND on hot-reload through the shared `Arc<ToolRegistry>` — so authoring /
/// approving / deleting a skill takes effect on the next turn with no restart.
pub fn register_skills(
    registry: &ToolRegistry,
    skills: &SkillRegistry,
    dispatcher: Arc<dyn BuiltinDispatcher>,
    prefs: Option<Arc<SkillPrefsStore>>,
) -> usize {
    // Global skills only (owner = None). User-scoped skills are applied by
    // [`reload_skills`], which combines both trees in one `set_skill_tools`.
    let tools = build_skill_tools(skills, dispatcher, prefs, None, registry.skill_cap_gate());
    let count = tools.len();
    registry.set_skill_tools(tools);
    count
}

/// Approve ONE proposed skill: move it from staging to the live user tree and
/// hot-reload so it's active immediately. Used by the
/// approve endpoint.
pub fn approve_proposal(
    registry: &ToolRegistry,
    data_dir: &std::path::Path,
    owner: &str,
    id: &str,
) -> Result<(), crate::MiraError> {
    let skills_dir = crate::skills::default_skills_dir(data_dir);
    let live = crate::skills::authoring::activate(&skills_dir, owner, id)?;
    sign_live_best_effort(data_dir, &live, id);
    reload_skills(registry, data_dir);
    Ok(())
}

/// Sign a freshly-activated/promoted skill with the per-instance key so it
/// reads "Signed by this MIRA". Best-effort: a signing failure must never
/// strand an otherwise-valid skill — it just stays unsigned-but-live. Logged.
fn sign_live_best_effort(data_dir: &std::path::Path, live_dir: &std::path::Path, id: &str) {
    if let Err(e) = crate::skills::instance_key::sign_skill(data_dir, live_dir) {
        warn!("instance-sign skill {id:?} failed (left unsigned): {e}");
    }
}

/// Promote ONE of a user's live user-scoped skills to global: rename it
/// out of the `user.<uid>.` namespace into `local.<slug>`, re-sign the global
/// copy with the instance key, and hot-reload so it's visible to everyone on
/// the next turn. Returns the new global id. Authorization (admin / RBAC
/// `allow_global_propose`) is enforced by the HTTP handler.
pub fn promote_skill(
    registry: &ToolRegistry,
    data_dir: &std::path::Path,
    owner: &str,
    id: &str,
) -> Result<String, crate::MiraError> {
    let skills_dir = crate::skills::default_skills_dir(data_dir);
    let new_id = crate::skills::authoring::promote(&skills_dir, owner, id)?;
    sign_live_best_effort(data_dir, &skills_dir.join(&new_id), &new_id);
    reload_skills(registry, data_dir);
    Ok(new_id)
}

/// Auto-approve ALL of a user's pending proposals (activate + a single reload),
/// returning the activated ids. Used by the chat handler's post-turn hook when
/// `agent.skill_authoring.auto_approve_tier0` is on — Tier-0 grants no
/// capability, so this is safe to do without a prompt.
pub fn auto_approve_pending(
    registry: &ToolRegistry,
    data_dir: &std::path::Path,
    owner: &str,
) -> Vec<String> {
    let skills_dir = crate::skills::default_skills_dir(data_dir);
    let mut activated = Vec::new();
    for p in crate::skills::authoring::list(&skills_dir, owner) {
        match crate::skills::authoring::activate(&skills_dir, owner, &p.id) {
            Ok(live) => {
                sign_live_best_effort(data_dir, &live, &p.id);
                activated.push(p.id);
            }
            Err(e) => warn!("auto-approve: activate {:?} for {owner:?} failed: {e}", p.id),
        }
    }
    if !activated.is_empty() {
        reload_skills(registry, data_dir);
    }
    activated
}

/// Hot-reload the skill surface from disk onto a live `Arc<ToolRegistry>` —
/// with **no restart**. Re-scans `<data_dir>/skills/`, rebuilds every skill's
/// router tool, and swaps the registry's skill surface via `set_skill_tools`.
/// Called whenever the on-disk skills change: a skill authored/approved/edited/
/// deleted, or a `.miraskill` installed/uninstalled.
/// Does NOT re-extract bundled skills (a startup-only concern) — it reflects
/// exactly what is on disk now. Returns how many skills are now active.
pub fn reload_skills(registry: &ToolRegistry, data_dir: &std::path::Path) -> usize {
    let skills_dir = crate::skills::default_skills_dir(data_dir);
    let mira_version = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is always valid semver");
    let trust = crate::skills::TrustStore::load(
        &crate::skills::TrustStore::default_path(&skills_dir),
    ).ok();
    // Global skills (owner = None, visible to all, subject to RBAC + disable).
    let global = crate::skills::load_dir_with_trust(&skills_dir, &mira_version, trust.as_ref());
    for err in &global.errors {
        warn!("Skill reload error at {}: {}", err.path.display(), err.error);
    }
    // User-scoped skills (owner = <uid>, private to that user) — S2.
    let user_scoped = crate::skills::loader::load_user_scoped(&skills_dir, &mira_version, trust.as_ref());

    let prefs = crate::skills::SkillPrefsStore::open(&data_dir.join("skill_prefs.db"))
        .ok()
        .map(Arc::new);
    // Dispatcher snapshots the CURRENT builtins (stable after startup) — never
    // other skills, keeping the ownership graph acyclic.
    let dispatcher: Arc<dyn BuiltinDispatcher> =
        Arc::new(BuiltinSnapshotDispatcher::from_registry(registry));

    let gate = registry.skill_cap_gate();
    let mut tools = build_skill_tools(&global, dispatcher.clone(), prefs.clone(), None, gate.clone());
    let global_count = tools.len();
    let mut user_count = 0;
    for (uid, reg) in &user_scoped {
        for err in &reg.errors {
            warn!("User-scoped skill reload error ({uid}) at {}: {}", err.path.display(), err.error);
        }
        let mut t = build_skill_tools(reg, dispatcher.clone(), prefs.clone(), Some(uid), gate.clone());
        user_count += t.len();
        tools.append(&mut t);
    }
    let count = tools.len();
    registry.set_skill_tools(tools);
    info!(
        "Skills hot-reloaded: {count} active ({global_count} global, {user_count} user-scoped; skills_dir={})",
        skills_dir.display(),
    );
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use crate::skills::manifest::{
        Permissions, SkillManifest, SkillMeta, ToolSpec, Verification,
    };
    use crate::skills::loader::LoadedSkill;
    use semver::Version;
    use tempfile::TempDir;

    /// Records every builtin call. Returns canned successes by name.
    struct MockDispatcher {
        calls: Mutex<Vec<(String, ToolArgs)>>,
        responses: HashMap<String, String>,
    }

    impl MockDispatcher {
        fn new(responses: &[(&str, &str)]) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                responses: responses.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            })
        }

        fn calls(&self) -> Vec<(String, ToolArgs)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl BuiltinDispatcher for MockDispatcher {
        async fn invoke_builtin(&self, name: &str, args: ToolArgs) -> Result<ToolResult, MiraError> {
            self.calls.lock().unwrap().push((name.to_string(), args));
            self.responses.get(name)
                .map(|v| ToolResult::success(v.clone()))
                .ok_or_else(|| MiraError::ToolError(format!("no mock for {name}")))
        }
    }

    fn make_skill(
        id: &str,
        tools: Vec<(&str, ToolSpec)>,
        root_dir: PathBuf,
    ) -> LoadedSkill {
        let mut tool_map = HashMap::new();
        for (name, spec) in tools {
            tool_map.insert(name.to_string(), spec);
        }
        LoadedSkill {
            manifest: SkillManifest {
                skill: SkillMeta {
                    id: id.to_string(),
                    version: Version::parse("1.0.0").unwrap(),
                    display_name: "Test".to_string(),
                    description: "A test skill that does X.".to_string(),
                    authors: vec![],
                    license: None,
                    mira_min: None,
                    system: false,
                },
                permissions: Permissions::default(),
                tools: tool_map,
                dependencies: HashMap::new(),
                verification: None::<Verification>,
            },
            root_dir,
            signed: false,
            verified: false,
            publisher_label: None,
            verification_error: None,
            system: false,
        }
    }

    #[test]
    fn name_sanitises_reverse_dns_dots_to_underscores() {
        assert_eq!(sanitise_tool_name("com.mira.research"), "com_mira_research");
        assert_eq!(sanitise_tool_name("io.example.thing-name"), "io_example_thing_name");
    }

    // The invariant: a skill re-exposing a builtin must be refused for a
    // caller the gate denies that builtin for, and run when allowed.
    struct FixedGate { allow: bool }
    impl SkillCapGate for FixedGate {
        fn builtin_allowed(&self, _user_id: Option<&str>, _impl_name: &str) -> bool { self.allow }
    }

    #[tokio::test]
    async fn builtin_skill_refused_when_cap_gate_denies_and_runs_when_allowed() {
        let dir = TempDir::new().unwrap();
        let skill = make_skill(
            "com.example.wrap",
            vec![("run", ToolSpec::Builtin { r#impl: "code_run".into() })],
            dir.path().to_path_buf(),
        );
        let disp = MockDispatcher::new(&[("code_run", "ran")]);

        // Denied: refused before dispatch, builtin never invoked.
        let denied = SkillTool::new(skill.clone(), disp.clone())
            .with_cap_gate(Some(Arc::new(FixedGate { allow: false })));
        let err = denied.execute(json!({"tool":"run","args":{},"_user_id":"restricted"})).await.unwrap_err();
        assert!(err.to_string().contains("permissions don't allow"), "{err}");
        assert!(disp.calls().is_empty(), "builtin must not be invoked when denied");

        // Allowed: dispatches to the builtin.
        let allowed = SkillTool::new(skill, disp.clone())
            .with_cap_gate(Some(Arc::new(FixedGate { allow: true })));
        let ok = allowed.execute(json!({"tool":"run","args":{},"_user_id":"privileged"})).await.unwrap();
        assert!(ok.success);
        assert_eq!(disp.calls().len(), 1);
    }

    #[test]
    fn description_lists_available_tools_alphabetically() {
        let dir = TempDir::new().unwrap();
        let skill = make_skill(
            "com.example.s",
            vec![
                ("zebra", ToolSpec::Prompt { template: "z.md".into() }),
                ("alpha", ToolSpec::Builtin { r#impl: "web_fetch".into() }),
            ],
            dir.path().to_path_buf(),
        );
        let tool = SkillTool::new(skill, MockDispatcher::new(&[]));
        let d = tool.description();
        let alpha_pos = d.find("alpha").expect("alpha listed");
        let zebra_pos = d.find("zebra").expect("zebra listed");
        assert!(alpha_pos < zebra_pos, "tools must be sorted alphabetically");
        assert!(d.contains("(builtin)"));
        assert!(d.contains("(prompt)"));
    }

    #[test]
    fn args_schema_enums_tool_names() {
        let dir = TempDir::new().unwrap();
        let skill = make_skill(
            "com.example.s",
            vec![
                ("first",  ToolSpec::Builtin { r#impl: "web_fetch".into() }),
                ("second", ToolSpec::Prompt  { template: "x.md".into() }),
            ],
            dir.path().to_path_buf(),
        );
        let tool = SkillTool::new(skill, MockDispatcher::new(&[]));
        let schema = tool.args_schema();

        let enum_values = schema.pointer("/properties/tool/enum")
            .and_then(|v| v.as_array()).expect("tool enum present");
        let names: Vec<&str> = enum_values.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(names, vec!["first", "second"]);
    }

    #[tokio::test]
    async fn prompt_tool_returns_template_contents() {
        let dir = TempDir::new().unwrap();
        let prompt_dir = dir.path().join("prompts");
        std::fs::create_dir_all(&prompt_dir).unwrap();
        let prompt_path = prompt_dir.join("synth.md");
        std::fs::write(&prompt_path, "Synthesise these sources: ...").unwrap();

        let skill = make_skill(
            "com.example.research",
            vec![("synthesize", ToolSpec::Prompt { template: "prompts/synth.md".into() })],
            dir.path().to_path_buf(),
        );
        let tool = SkillTool::new(skill, MockDispatcher::new(&[]));

        let result = tool.execute(json!({
            "tool": "synthesize",
            "args": {}
        })).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output, "Synthesise these sources: ...");
    }

    #[test]
    fn auto_approve_activates_pending_and_reloads_live() {
        // S4: a staged proposal is activated (moved to the live user tree) and
        // hot-reloaded onto the registry — live for the next turn, no restart.
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();
        let skills_dir = crate::skills::default_skills_dir(data_dir);
        let toml = "[skill]\nid = \"user.alice.notes\"\nversion = \"0.1.0\"\n\
                    display_name = \"Notes\"\ndescription = \"when noting\"\n\n\
                    [tools.guide]\nkind = \"prompt\"\ntemplate = \"prompts/main.md\"\n";
        crate::skills::authoring::stage(&skills_dir, "alice", "user.alice.notes", toml, "jot it down").unwrap();

        let reg = ToolRegistry::new();
        let activated = auto_approve_pending(&reg, data_dir, "alice");
        assert_eq!(activated, vec!["user.alice.notes".to_string()]);
        // Gone from staging, present live on the registry's skill surface.
        assert!(crate::skills::authoring::list(&skills_dir, "alice").is_empty());
        assert!(reg.list_tools().iter().any(|n| n.contains("alice")),
            "activated skill is live on the registry");
    }

    #[test]
    fn promote_skill_renames_signs_and_goes_global_live() {
        // S6: a user-scoped skill is promoted to global — renamed into the
        // `local.` namespace, re-signed with the instance key (verifies as
        // "this MIRA"), and hot-reloaded as a global (owner-less) tool.
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();
        let skills_dir = crate::skills::default_skills_dir(data_dir);
        let toml = "[skill]\nid = \"user.alice.notes\"\nversion = \"0.1.0\"\n\
                    display_name = \"Notes\"\ndescription = \"when noting\"\n\n\
                    [tools.guide]\nkind = \"prompt\"\ntemplate = \"prompts/main.md\"\n";
        crate::skills::authoring::stage(&skills_dir, "alice", "user.alice.notes", toml, "jot").unwrap();

        let reg = ToolRegistry::new();
        auto_approve_pending(&reg, data_dir, "alice");

        let new_id = promote_skill(&reg, data_dir, "alice", "user.alice.notes").unwrap();
        assert_eq!(new_id, "local.notes");

        // On disk: global copy exists, signed; user-scoped copy gone.
        let global = skills_dir.join("local.notes");
        assert!(global.join("skill.toml").is_file());
        assert!(!skills_dir.join("users").join("alice").join("user.alice.notes").exists());

        // Verifies against the pinned instance key.
        let trust = crate::skills::TrustStore::load(
            &crate::skills::TrustStore::default_path(&skills_dir)).unwrap();
        let ver = semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        let loaded = crate::skills::load_dir_with_trust(&skills_dir, &ver, Some(&trust));
        let s = loaded.iter().find(|s| s.manifest.skill.id == "local.notes").expect("global loaded");
        assert!(s.verified);
        assert_eq!(s.publisher_label.as_deref(), Some(crate::skills::INSTANCE_KEY_LABEL));
    }

    #[tokio::test]
    async fn user_scoped_skill_refuses_non_owner_and_absent_caller() {
        // S2 security backstop: a private (user-scoped) skill runs ONLY for its
        // owner; a different user is refused, and an absent caller fails CLOSED.
        let dir = TempDir::new().unwrap();
        let prompt_dir = dir.path().join("prompts");
        std::fs::create_dir_all(&prompt_dir).unwrap();
        std::fs::write(prompt_dir.join("s.md"), "secret guidance").unwrap();

        let skill = make_skill(
            "user.alice.notes",
            vec![("do", ToolSpec::Prompt { template: "prompts/s.md".into() })],
            dir.path().to_path_buf(),
        );
        let tool = SkillTool::new(skill, MockDispatcher::new(&[]))
            .with_owner(Some("alice".to_string()));

        // Owner → passes the backstop and reaches the prompt.
        let ok = tool.execute(json!({ "_user_id": "alice", "tool": "do", "args": {} }))
            .await.unwrap();
        assert!(ok.success);
        assert_eq!(ok.output, "secret guidance");

        // Different user → refused before any tool work.
        let err = tool.execute(json!({ "_user_id": "bob", "tool": "do", "args": {} }))
            .await.unwrap_err();
        assert!(err.to_string().contains("private skill belonging to another user"), "got: {err}");

        // Absent caller → fails CLOSED (owner identity is a security boundary).
        let err2 = tool.execute(json!({ "tool": "do", "args": {} })).await.unwrap_err();
        assert!(err2.to_string().contains("private skill"), "got: {err2}");
    }

    #[tokio::test]
    async fn prompt_tool_surfaces_missing_template_clearly() {
        let dir = TempDir::new().unwrap();
        let skill = make_skill(
            "com.example.research",
            vec![("missing", ToolSpec::Prompt { template: "no/such.md".into() })],
            dir.path().to_path_buf(),
        );
        let tool = SkillTool::new(skill, MockDispatcher::new(&[]));

        let err = tool.execute(json!({
            "tool": "missing",
            "args": {}
        })).await.unwrap_err();
        assert!(err.to_string().contains("failed to read prompt template"));
    }

    #[tokio::test]
    async fn builtin_tool_forwards_to_dispatcher() {
        let dir = TempDir::new().unwrap();
        let dispatcher = MockDispatcher::new(&[("web_fetch", "<html>...</html>")]);

        let skill = make_skill(
            "com.example.web",
            vec![("fetch", ToolSpec::Builtin { r#impl: "web_fetch".into() })],
            dir.path().to_path_buf(),
        );
        let tool = SkillTool::new(skill, dispatcher.clone());

        let result = tool.execute(json!({
            "tool": "fetch",
            "args": { "url": "https://example.com/" }
        })).await.unwrap();
        assert_eq!(result.output, "<html>...</html>");

        let calls = dispatcher.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "web_fetch");
        assert_eq!(calls[0].1.pointer("/url").and_then(|v| v.as_str()), Some("https://example.com/"));
    }

    #[tokio::test]
    async fn builtin_tool_receives_injected_context_from_outer_envelope() {
        // A user-scoped builtin reached through a skill wrapper must still see
        // the handler-injected `_user_id` (et al) — otherwise every
        // `require_user_id` builtin (wiki, memory, calendar) fails when routed
        // via a skill. The keys live on the OUTER envelope; the model only
        // controls the inner `args`.
        let dir = TempDir::new().unwrap();
        let dispatcher = MockDispatcher::new(&[("wiki_read", "page body")]);
        let skill = make_skill(
            "com.example.wiki",
            vec![("read", ToolSpec::Builtin { r#impl: "wiki_read".into() })],
            dir.path().to_path_buf(),
        );
        let tool = SkillTool::new(skill, dispatcher.clone());

        tool.execute(json!({
            "tool": "read",
            "args": { "path": "pages/notes.md" },
            "_user_id": "685532cc",
            "_conversation_id": "conv-1",
            "_channel": "web",
            "_user_tz": "Europe/London",
        })).await.unwrap();

        let calls = dispatcher.calls();
        let forwarded = &calls[0].1;
        // Model-supplied arg survives …
        assert_eq!(forwarded.pointer("/path").and_then(|v| v.as_str()), Some("pages/notes.md"));
        // … and every injected context key is forwarded.
        assert_eq!(forwarded.pointer("/_user_id").and_then(|v| v.as_str()), Some("685532cc"));
        assert_eq!(forwarded.pointer("/_conversation_id").and_then(|v| v.as_str()), Some("conv-1"));
        assert_eq!(forwarded.pointer("/_channel").and_then(|v| v.as_str()), Some("web"));
        assert_eq!(forwarded.pointer("/_user_tz").and_then(|v| v.as_str()), Some("Europe/London"));
    }

    #[tokio::test]
    async fn model_supplied_user_id_cannot_override_injected_one() {
        // SECURITY: a model must not be able to spoof its identity by placing
        // `_user_id` inside the inner `args` it controls. The trusted, outer,
        // handler-injected value wins.
        let dir = TempDir::new().unwrap();
        let dispatcher = MockDispatcher::new(&[("wiki_read", "ok")]);
        let skill = make_skill(
            "com.example.wiki",
            vec![("read", ToolSpec::Builtin { r#impl: "wiki_read".into() })],
            dir.path().to_path_buf(),
        );
        let tool = SkillTool::new(skill, dispatcher.clone());

        tool.execute(json!({
            "tool": "read",
            "args": { "path": "pages/notes.md", "_user_id": "attacker-spoofed" },
            "_user_id": "685532cc-trusted",
        })).await.unwrap();

        let calls = dispatcher.calls();
        assert_eq!(
            calls[0].1.pointer("/_user_id").and_then(|v| v.as_str()),
            Some("685532cc-trusted"),
            "outer (handler-injected) _user_id must win over the model-supplied one",
        );
    }

    #[tokio::test]
    async fn unknown_tool_in_skill_returns_clear_error() {
        let dir = TempDir::new().unwrap();
        let skill = make_skill(
            "com.example.s",
            vec![("known", ToolSpec::Prompt { template: "p.md".into() })],
            dir.path().to_path_buf(),
        );
        let tool = SkillTool::new(skill, MockDispatcher::new(&[]));

        let err = tool.execute(json!({
            "tool": "unknown",
            "args": {}
        })).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"));
        assert!(msg.contains("known"), "error should list available tools");
    }

    #[tokio::test]
    async fn malformed_args_surface_clearly() {
        let dir = TempDir::new().unwrap();
        let skill = make_skill(
            "com.example.s",
            vec![("any", ToolSpec::Prompt { template: "p.md".into() })],
            dir.path().to_path_buf(),
        );
        let tool = SkillTool::new(skill, MockDispatcher::new(&[]));

        let err = tool.execute(json!({"missing": "tool key"})).await.unwrap_err();
        assert!(err.to_string().contains("invalid arguments"));
    }

    #[tokio::test]
    async fn executable_tool_returns_not_yet_supported() {
        let dir = TempDir::new().unwrap();
        let skill = make_skill(
            "com.example.s",
            vec![("runner", ToolSpec::Executable {
                path: "tools/runner.py".into(),
                run_in_sandbox: true,
            })],
            dir.path().to_path_buf(),
        );
        let tool = SkillTool::new(skill, MockDispatcher::new(&[]));

        let err = tool.execute(json!({
            "tool": "runner",
            "args": {}
        })).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("executable tools are not yet supported"));
    }

    /// Smoke test for the full snapshot path: register a builtin in a
    /// registry, snapshot it, register a SkillTool that calls that
    /// builtin, dispatch through the registry. Catches integration bugs
    /// in `BuiltinSnapshotDispatcher::from_registry` and the
    /// `register_skills` helper.
    #[tokio::test]
    async fn end_to_end_snapshot_dispatcher_round_trip() {
        use crate::tools::ToolRegistry;
        use crate::skills::loader::SkillRegistry;

        struct EchoBuiltin;
        #[async_trait]
        impl Tool for EchoBuiltin {
            fn name(&self) -> &str { "echo_builtin" }
            fn description(&self) -> &str { "Echo back the args" }
            fn args_schema(&self) -> serde_json::Value { json!({}) }
            async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
                Ok(ToolResult::success(args.to_string()))
            }
        }

        // 1) Register a builtin.
        let mut registry = ToolRegistry::new();
        registry.register(EchoBuiltin);

        // 2) Snapshot — must happen BEFORE Skills register.
        let dispatcher: Arc<dyn BuiltinDispatcher> =
            Arc::new(BuiltinSnapshotDispatcher::from_registry(&registry));
        assert_eq!(
            dispatcher.invoke_builtin("echo_builtin", json!({"x": 1})).await
                .unwrap().output,
            r#"{"x":1}"#,
        );

        // 3) Register a Skill that wraps it.
        let dir = TempDir::new().unwrap();
        let skill = make_skill(
            "com.example.wrap",
            vec![("call_echo", ToolSpec::Builtin { r#impl: "echo_builtin".into() })],
            dir.path().to_path_buf(),
        );
        let mut skills_reg = SkillRegistry::default();
        skills_reg.loaded.push(skill);

        let count = register_skills(&mut registry, &skills_reg, dispatcher.clone(), None);
        assert_eq!(count, 1);

        // 4) Call through the agent's normal registry surface.
        let result = registry.execute(
            "com_example_wrap",
            json!({"tool": "call_echo", "args": {"hello": "world"}}),
        ).await.unwrap();
        assert_eq!(result.output, r#"{"hello":"world"}"#);
    }

    #[tokio::test]
    async fn snapshot_does_not_capture_skills_registered_after_it() {
        use crate::tools::ToolRegistry;
        use crate::skills::loader::SkillRegistry;

        // Empty registry → empty snapshot.
        let mut registry = ToolRegistry::new();
        let dispatcher: Arc<dyn BuiltinDispatcher> =
            Arc::new(BuiltinSnapshotDispatcher::from_registry(&registry));

        // Add a Skill *after* the snapshot.
        let dir = TempDir::new().unwrap();
        let skill = make_skill(
            "com.example.s",
            vec![("p", ToolSpec::Prompt { template: "p.md".into() })],
            dir.path().to_path_buf(),
        );
        let mut skills_reg = SkillRegistry::default();
        skills_reg.loaded.push(skill);
        register_skills(&mut registry, &skills_reg, dispatcher.clone(), None);

        // The Skill is in the registry (on the hot-swap skill surface, counted
        // by list_tools), but builtin snapshots never see it.
        assert_eq!(
            registry.list_tools().len(), 1,
            "skill present in registry (on the skill surface)",
        );
        let snapshot = match Arc::downcast::<BuiltinSnapshotDispatcher>(
            // A FRESH snapshot taken after registration still excludes the skill:
            // skills now live on the registry's separate `skills` surface, never
            // in the builtin `tools` map that `from_registry` snapshots — so the
            // SkillTool↔builtin ownership graph is acyclic by construction, not
            // just by snapshot timing.
            Arc::new(BuiltinSnapshotDispatcher::from_registry(&registry)) as Arc<dyn std::any::Any + Send + Sync>,
        ) {
            Ok(s) => s,
            Err(_) => panic!("downcast should succeed"),
        };
        assert_eq!(snapshot.len(), 0, "builtin snapshot never contains skills — they're on a separate surface");
    }

    #[test]
    fn tier_reflects_strongest_declared_permission() {
        let dir = TempDir::new().unwrap();
        let mut skill = make_skill(
            "com.example.s",
            vec![],
            dir.path().to_path_buf(),
        );
        // Pure when nothing is declared.
        let tool = SkillTool::new(
            LoadedSkill {
                manifest: skill.manifest.clone(),
                root_dir: skill.root_dir.clone(),
                signed: false,
            verified: false,
            publisher_label: None,
            verification_error: None,
            system: false,
            },
            MockDispatcher::new(&[]),
        );
        assert_eq!(tool.tier(), Tier::Pure);

        // Subprocess → Code
        skill.manifest.permissions.subprocess = true;
        let tool = SkillTool::new(
            LoadedSkill {
                manifest: skill.manifest.clone(),
                root_dir: skill.root_dir.clone(),
                signed: false,
            verified: false,
            publisher_label: None,
            verification_error: None,
            system: false,
            },
            MockDispatcher::new(&[]),
        );
        assert_eq!(tool.tier(), Tier::Code);

        // Filesystem outranks subprocess
        skill.manifest.permissions.filesystem = vec!["read:/tmp".into()];
        let tool = SkillTool::new(
            LoadedSkill {
                manifest: skill.manifest.clone(),
                root_dir: skill.root_dir.clone(),
                signed: false,
            verified: false,
            publisher_label: None,
            verification_error: None,
            system: false,
            },
            MockDispatcher::new(&[]),
        );
        assert_eq!(tool.tier(), Tier::Filesystem);

        // Network outranks both
        skill.manifest.permissions.network_egress = vec!["https://example.com".into()];
        let tool = SkillTool::new(skill, MockDispatcher::new(&[]));
        assert_eq!(tool.tier(), Tier::Network);
    }

    // ── A5: per-user enable/disable ──

    #[tokio::test]
    async fn disabled_user_is_refused_before_dispatch() {
        let dir = TempDir::new().unwrap();
        let skill = make_skill(
            "com.example.gated",
            vec![("clock", ToolSpec::Builtin { r#impl: "echo_builtin".into() })],
            dir.path().to_path_buf(),
        );
        let prefs = Arc::new(SkillPrefsStore::open_in_memory());
        prefs.set_enabled("alice", "com.example.gated", false).unwrap();

        let dispatcher = MockDispatcher::new(&[("echo_builtin", "ok")]);
        let tool = SkillTool::new(skill, dispatcher.clone()).with_prefs(prefs.clone());

        let err = tool.execute(json!({
            "_user_id": "alice",
            "tool": "clock",
            "args": {}
        })).await.unwrap_err();
        assert!(err.to_string().contains("disabled by user"));
        // Crucially the dispatcher was *not* invoked: the user's
        // preference is checked before any tool-side work.
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn enabled_user_passes_through_normally() {
        let dir = TempDir::new().unwrap();
        let skill = make_skill(
            "com.example.gated",
            vec![("clock", ToolSpec::Builtin { r#impl: "echo_builtin".into() })],
            dir.path().to_path_buf(),
        );
        let prefs = Arc::new(SkillPrefsStore::open_in_memory());
        // bob has no row → enabled by default

        let dispatcher = MockDispatcher::new(&[("echo_builtin", "ok")]);
        let tool = SkillTool::new(skill, dispatcher.clone()).with_prefs(prefs.clone());

        let result = tool.execute(json!({
            "_user_id": "bob",
            "tool": "clock",
            "args": {}
        })).await.unwrap();
        assert!(result.success);
        assert_eq!(dispatcher.calls().len(), 1);
    }

    #[tokio::test]
    async fn missing_user_id_skips_the_check() {
        // Direct /api/tools/run by an admin doesn't carry _user_id; we
        // fail-open in that case rather than blocking admin debugging.
        let dir = TempDir::new().unwrap();
        let skill = make_skill(
            "com.example.gated",
            vec![("clock", ToolSpec::Builtin { r#impl: "echo_builtin".into() })],
            dir.path().to_path_buf(),
        );
        let prefs = Arc::new(SkillPrefsStore::open_in_memory());
        prefs.set_enabled("alice", "com.example.gated", false).unwrap();

        let dispatcher = MockDispatcher::new(&[("echo_builtin", "ok")]);
        let tool = SkillTool::new(skill, dispatcher.clone()).with_prefs(prefs);

        let result = tool.execute(json!({
            "tool": "clock",
            "args": {}
        })).await.unwrap();
        assert!(result.success);
    }
}
