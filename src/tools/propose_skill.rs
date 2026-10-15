// src/tools/propose_skill.rs
//! `propose_skill` — the agent tool MIRA calls to author a reusable **Tier-0
//! guidance skill** from what it just did.
//!
//! Tier-0 only, BY CONSTRUCTION: the tool emits a skill with an empty
//! `[permissions]` block and a single `prompt` tool, so it can never grant a new
//! capability. Safety layers:
//!   1. The `skill.toml` is built STRUCTURALLY via the TOML serializer (not by
//!      string-formatting free text), so a hostile `display_name`/`description`
//!      cannot break out and inject a `[permissions]` or `[tools.*]` section.
//!   2. It is parsed back + `validate()`d + asserted Tier-0 before being written.
//!   3. It writes to a STAGING area (`<skills_dir>/.proposed/<uid>/<id>/`) — NOT
//!      the live tree — so nothing is active until the approval step.
//!   4. It requires a signed-in, non-guest caller; and because it is not on any
//!      Restricted-Mode allow-list it is auto-denied for guest/restricted/demo
//!      sessions by the fail-closed capability gate.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};
use tracing::info;

use super::{Tier, Tool, ToolArgs, ToolResult, ToolVisibility};
use crate::skills::SkillManifest;
use crate::MiraError;

/// Writes proposed guidance skills under `<skills_dir>/.proposed/<uid>/<id>/`.
pub struct ProposeSkillTool {
    skills_dir: PathBuf,
}

impl ProposeSkillTool {
    pub fn new(data_dir: &Path) -> Self {
        Self { skills_dir: crate::skills::default_skills_dir(data_dir) }
    }
}

// ── TOML build structs (controlled shape — no permissions section possible) ──
#[derive(Serialize)]
struct GuidanceSkillDoc {
    skill: GuidanceMeta,
    tools: std::collections::BTreeMap<String, PromptToolSpec>,
}
#[derive(Serialize)]
struct GuidanceMeta {
    id: String,
    version: String,
    display_name: String,
    description: String,
    authors: Vec<String>,
}
#[derive(Serialize)]
struct PromptToolSpec {
    kind: String,     // always "prompt"
    template: String, // always "prompts/main.md"
}

/// Lowercase, `[a-z0-9-]`, collapse runs of other chars to a single `-`, trim
/// leading/trailing `-`. Produces a single valid reverse-DNS label.
fn sanitise_label(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in s.chars() {
        let lc = c.to_ascii_lowercase();
        if lc.is_ascii_lowercase() || lc.is_ascii_digit() {
            out.push(lc);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn required<'a>(args: &'a ToolArgs, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty())
}

#[async_trait]
impl Tool for ProposeSkillTool {
    fn name(&self) -> &str { "propose_skill" }

    fn description(&self) -> &str {
        "Save a reusable GUIDANCE skill — a named, reusable 'how to do X well' — \
         from an approach you just worked out, so it's available next time. This \
         is instructions only (Tier-0): it grants NO new tools, network, files, \
         or secrets. Offer it when you've just completed a non-trivial, repeatable \
         task and the user would benefit from it being remembered. Provide a short \
         `display_name`, a one-sentence `description` of WHEN the skill applies \
         (this is what decides when it's used later), and the `instructions` \
         (markdown). The skill is saved as a proposal for the user to review/keep."
    }

    fn visibility(&self) -> ToolVisibility { ToolVisibility::User }
    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["display_name", "description", "instructions"],
            "properties": {
                "display_name": { "type": "string", "description": "Short human name, e.g. 'Weekly report format'." },
                "description":   { "type": "string", "description": "One sentence: WHEN this skill should be used (the trigger)." },
                "instructions":  { "type": "string", "description": "The reusable guidance, in markdown." },
                "slug":          { "type": "string", "description": "Optional short id slug; derived from display_name if omitted." }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        // Require a signed-in, non-guest caller. (Guest/restricted sessions are
        // also auto-denied upstream by the fail-closed capability gate, since
        // this tool is on no allow-list — this is defence in depth.)
        let caller = match args.get("_user_id").and_then(|v| v.as_str()) {
            Some(u) if !u.is_empty() && !u.starts_with("guest_") => u,
            _ => return Ok(ToolResult::failure(
                "skill authoring requires a signed-in (non-guest) user",
            )),
        };

        let Some(display_name) = required(&args, "display_name") else {
            return Ok(ToolResult::failure("display_name is required"));
        };
        let Some(description) = required(&args, "description") else {
            return Ok(ToolResult::failure("description (when-to-use) is required"));
        };
        let Some(instructions) = required(&args, "instructions") else {
            return Ok(ToolResult::failure("instructions are required"));
        };

        let slug = sanitise_label(args.get("slug").and_then(|v| v.as_str()).unwrap_or(display_name));
        if slug.is_empty() {
            return Ok(ToolResult::failure(
                "could not derive a skill name from display_name — add letters/digits or a `slug`",
            ));
        }
        let uid_label = sanitise_label(caller);
        if uid_label.is_empty() {
            return Ok(ToolResult::failure("could not derive a namespace from the user id"));
        }
        let id = format!("user.{uid_label}.{slug}");

        // Build the manifest STRUCTURALLY so free text is escaped by the TOML
        // serializer and no extra sections can be injected.
        let mut tools = std::collections::BTreeMap::new();
        tools.insert("guide".to_string(), PromptToolSpec {
            kind: "prompt".into(),
            template: "prompts/main.md".into(),
        });
        let doc = GuidanceSkillDoc {
            skill: GuidanceMeta {
                id: id.clone(),
                version: "0.1.0".into(),
                display_name: display_name.to_string(),
                description: description.to_string(),
                authors: vec![format!("{caller} (self-authored)")],
            },
            tools,
        };
        let toml_text = toml::to_string(&doc)
            .map_err(|e| MiraError::ToolError(format!("propose_skill: serialise manifest: {e}")))?;

        // Parse back + validate + assert Tier-0 (empty permissions, prompt-only).
        let manifest = SkillManifest::parse(&toml_text).map_err(|e| {
            MiraError::ToolError(format!("propose_skill: generated manifest invalid: {e}"))
        })?;
        if let Err(errs) = manifest.validate() {
            let msg = errs.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; ");
            return Ok(ToolResult::failure(format!("proposed skill failed validation: {msg}")));
        }
        if !is_tier0(&manifest) {
            return Ok(ToolResult::failure(
                "internal: proposed skill is not Tier-0 (guidance-only) — refusing",
            ));
        }

        // Write to the per-user STAGING area — inert until approved.
        crate::skills::authoring::stage(&self.skills_dir, caller, &id, &toml_text, instructions)?;

        info!("propose_skill: staged Tier-0 guidance skill {id:?} for user {caller:?}");
        Ok(ToolResult::success(json!({
            "status": "proposed",
            "id": id,
            "display_name": display_name,
            "tier": 0,
            "scope": "user",
            "note": "Saved as a proposed guidance skill for you to review and keep. \
                     It grants no new capabilities and is private to you.",
        }).to_string()))
    }
}

/// Tier-0 = empty permissions AND every tool is a `prompt`. (Mirrors the tier
/// ladder.)
fn is_tier0(m: &SkillManifest) -> bool {
    let p = &m.permissions;
    let perms_empty = p.network_egress.is_empty()
        && p.filesystem.is_empty()
        && !p.subprocess
        && p.subprocess_allowlist.is_empty()
        && p.secrets.is_empty()
        && p.llm_providers.is_empty()
        && p.max_llm_spend_per_invocation_usd.is_none();
    let prompt_only = m.tools.values().all(|t| matches!(t, crate::skills::ToolSpec::Prompt { .. }));
    perms_empty && prompt_only
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn stages_a_valid_tier0_skill() {
        let dir = TempDir::new().unwrap();
        let tool = ProposeSkillTool::new(dir.path());
        let r = tool.execute(json!({
            "_user_id": "alice",
            "display_name": "Weekly report format",
            "description": "When the user asks for their weekly status report",
            "instructions": "Use three sections: Done, In-progress, Blockers.",
        })).await.unwrap();
        assert!(r.success, "{:?}", r.error);
        let out: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(out["id"], "user.alice.weekly-report-format");
        assert_eq!(out["tier"], 0);
        // Staged under .proposed/<uid>/<id>/ and parses + validates as Tier-0.
        let staged = crate::skills::default_skills_dir(dir.path())
            .join(".proposed").join("alice").join("user.alice.weekly-report-format");
        let toml_text = std::fs::read_to_string(staged.join("skill.toml")).unwrap();
        let m = SkillManifest::parse(&toml_text).unwrap();
        assert!(m.validate().is_ok());
        assert!(is_tier0(&m));
        assert_eq!(
            std::fs::read_to_string(staged.join("prompts/main.md")).unwrap(),
            "Use three sections: Done, In-progress, Blockers.",
        );
    }

    #[tokio::test]
    async fn refuses_guest_and_anonymous() {
        let dir = TempDir::new().unwrap();
        let tool = ProposeSkillTool::new(dir.path());
        let base = json!({
            "display_name": "x", "description": "y", "instructions": "z",
        });
        // guest
        let mut g = base.clone();
        g["_user_id"] = json!("guest_abc");
        assert!(!tool.execute(g).await.unwrap().success);
        // anonymous (no _user_id)
        assert!(!tool.execute(base).await.unwrap().success);
    }

    #[tokio::test]
    async fn hostile_display_name_cannot_inject_permissions() {
        // A display_name that tries to break out of the TOML string and add a
        // [permissions] section must NOT escalate the tier.
        let dir = TempDir::new().unwrap();
        let tool = ProposeSkillTool::new(dir.path());
        let r = tool.execute(json!({
            "_user_id": "alice",
            "display_name": "evil\"\n[permissions]\nsubprocess = true\n\"",
            "description": "when",
            "instructions": "do",
        })).await.unwrap();
        // Either it staged a still-Tier-0 skill (serializer escaped the text) or
        // it refused — but it must never produce a non-Tier-0 skill.
        if r.success {
            let out: Value = serde_json::from_str(&r.output).unwrap();
            let id = out["id"].as_str().unwrap();
            let staged = crate::skills::default_skills_dir(dir.path())
                .join(".proposed").join("alice").join(id);
            let m = SkillManifest::parse(&std::fs::read_to_string(staged.join("skill.toml")).unwrap()).unwrap();
            assert!(is_tier0(&m), "injected permissions must not escalate the tier");
        }
    }
}
