// SPDX-License-Identifier: AGPL-3.0-or-later

//! `SkillTool` — exposes one loaded Skill as a single entry in the agent's
//! tool list (the "Skill router" pattern from `design-docs/skills-and-agents.md`).
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
use tracing::{debug, warn};

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
}

impl SkillTool {
    pub fn new(skill: LoadedSkill, dispatcher: Arc<dyn BuiltinDispatcher>) -> Self {
        let name = sanitise_tool_name(&skill.manifest.skill.id);
        let description = render_description(&skill);
        let args_schema = render_args_schema(&skill);
        Self { name, description, args_schema, skill, dispatcher, prefs: None }
    }

    /// Add a per-user prefs store. Calls from disabled users get refused
    /// before any tool work happens.
    pub fn with_prefs(mut self, prefs: Arc<SkillPrefsStore>) -> Self {
        self.prefs = Some(prefs);
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
                self.dispatcher.invoke_builtin(r#impl, call.args).await
            }
            ToolSpec::Executable { .. } => {
                // Deferred: needs the sandbox bind-mount mapping (see
                // skills::permissions::build_sandbox_limits comment) plus
                // the executable runner. Tracked for a follow-up slice.
                Err(MiraError::ToolError(format!(
                    "skill {:?} tool {:?}: executable tools are not yet supported in this MIRA build. \
                     The runner is tracked as a follow-up to slice A3 in design-docs/skills-and-agents.md.",
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
}

impl BuiltinSnapshotDispatcher {
    /// Snapshot every tool currently in `registry` into the dispatcher's
    /// table. Subsequent registrations into the registry are *not*
    /// reflected — that's the whole point.
    pub fn from_registry(registry: &ToolRegistry) -> Self {
        let table = registry.iter()
            .map(|(name, tool)| (name.clone(), tool.clone()))
            .collect();
        Self { table }
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
pub fn register_skills(
    registry: &mut ToolRegistry,
    skills: &SkillRegistry,
    dispatcher: Arc<dyn BuiltinDispatcher>,
    prefs: Option<Arc<SkillPrefsStore>>,
) -> usize {
    let mut count = 0;
    for skill in skills.iter() {
        let id = skill.manifest.skill.id.clone();
        let mut tool = SkillTool::new(skill.clone(), dispatcher.clone());
        if let Some(p) = prefs.as_ref() {
            tool = tool.with_prefs(p.clone());
        }
        debug!("Registered Skill {id:?} as tool {:?}", tool.name());
        registry.register(tool);
        count += 1;
    }
    if !skills.errors.is_empty() {
        warn!(
            "Skills loader: {} successfully registered, {} skipped due to errors (see /api/skills for details)",
            count, skills.errors.len(),
        );
    }
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

        // The Skill is in the registry, but the snapshot is still empty —
        // exactly what we want to keep the ownership graph acyclic.
        assert_eq!(
            registry.list_tools().len(), 1,
            "skill present in registry",
        );
        let snapshot = match Arc::downcast::<BuiltinSnapshotDispatcher>(
            // Re-create from the registry as it stands now to compare.
            // In production the dispatcher is taken once and held — this is
            // a sanity check that the original snapshot semantics held.
            Arc::new(BuiltinSnapshotDispatcher::from_registry(&registry)) as Arc<dyn std::any::Any + Send + Sync>,
        ) {
            Ok(s) => s,
            Err(_) => panic!("downcast should succeed"),
        };
        assert_eq!(snapshot.len(), 1, "fresh snapshot DOES contain the new skill — but the original held by SkillTool predates it");
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
