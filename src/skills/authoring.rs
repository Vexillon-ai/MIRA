// src/skills/authoring.rs
//! Skill self-authoring file operations.
//!
//! A proposed skill is staged under `<skills_dir>/.proposed/<owner>/<id>/`
//! (inert — the loader skips `.`-dirs). Approving it moves the dir to the live
//! user-scoped tree `<skills_dir>/users/<owner>/<id>/`; the caller then triggers
//! a `runtime::reload_skills` so it goes live with no restart. These functions
//! are pure filesystem ops (no registry/config) so they're easy to test; the
//! `SkillAuthoringService` layers registry reload + config + notifications on top.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::SkillManifest;
use crate::MiraError;

/// Hidden staging root under the skills dir.
pub const PROPOSED_DIR: &str = ".proposed";
/// Live user-scoped root under the skills dir.
pub const USERS_DIR: &str = "users";

/// A staged, not-yet-active skill proposal (read from its `skill.toml`).
#[derive(Debug, Clone, Serialize)]
pub struct Proposal {
    pub id: String,
    pub owner: String,
    pub display_name: String,
    pub description: String,
    /// Tier is always 0 for self-authored proposals today; surfaced for the UI.
    pub tier: u8,
}

fn proposed_root(skills_dir: &Path, owner: &str) -> PathBuf {
    skills_dir.join(PROPOSED_DIR).join(owner)
}
fn live_root(skills_dir: &Path, owner: &str) -> PathBuf {
    skills_dir.join(USERS_DIR).join(owner)
}

/// Reject a malformed id/owner that could escape the skills tree. Ids are
/// reverse-DNS (dots allowed, no slashes); owners are user ids. Defence in depth
/// — callers already sanitise, but path writes must never trust input.
fn safe_segment(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('/')
        && !s.contains('\\')
        && !s.split('.').any(|p| p.is_empty() || p == "..")
        && s != ".."
}

/// Write a proposed skill to staging. `toml_text` is the full validated
/// `skill.toml`; `instructions` becomes `prompts/main.md`. Returns the dir.
pub fn stage(
    skills_dir: &Path,
    owner: &str,
    id: &str,
    toml_text: &str,
    instructions: &str,
) -> Result<PathBuf, MiraError> {
    if !safe_segment(owner) || !safe_segment(id) {
        return Err(MiraError::ToolError("unsafe skill owner/id".into()));
    }
    let dir = proposed_root(skills_dir, owner).join(id);
    let prompts = dir.join("prompts");
    std::fs::create_dir_all(&prompts)
        .map_err(|e| MiraError::ToolError(format!("stage {}: {e}", prompts.display())))?;
    std::fs::write(dir.join("skill.toml"), toml_text)
        .map_err(|e| MiraError::ToolError(format!("stage skill.toml: {e}")))?;
    std::fs::write(prompts.join("main.md"), instructions)
        .map_err(|e| MiraError::ToolError(format!("stage prompt: {e}")))?;
    Ok(dir)
}

/// List a user's staged proposals (parsing each `skill.toml`).
pub fn list(skills_dir: &Path, owner: &str) -> Vec<Proposal> {
    let root = proposed_root(skills_dir, owner);
    let Ok(entries) = std::fs::read_dir(&root) else { return Vec::new() };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() { continue; }
        let Ok(text) = std::fs::read_to_string(p.join("skill.toml")) else { continue };
        let Ok(m) = SkillManifest::parse(&text) else { continue };
        out.push(Proposal {
            id: m.skill.id.clone(),
            owner: owner.to_string(),
            display_name: m.skill.display_name.clone(),
            description: m.skill.description.clone(),
            tier: 0,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Approve a proposal: move it from staging to the live user-scoped tree.
/// Returns the live dir. The caller reloads the skill surface afterwards.
pub fn activate(skills_dir: &Path, owner: &str, id: &str) -> Result<PathBuf, MiraError> {
    if !safe_segment(owner) || !safe_segment(id) {
        return Err(MiraError::ToolError("unsafe skill owner/id".into()));
    }
    let src = proposed_root(skills_dir, owner).join(id);
    if !src.is_dir() {
        return Err(MiraError::NotFound(format!("no proposed skill {id:?} for {owner:?}")));
    }
    let dst = live_root(skills_dir, owner).join(id);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| MiraError::ToolError(format!("activate mkdir {}: {e}", parent.display())))?;
    }
    // Replace any existing live version (re-approve after an edit).
    if dst.exists() {
        std::fs::remove_dir_all(&dst)
            .map_err(|e| MiraError::ToolError(format!("activate replace {}: {e}", dst.display())))?;
    }
    // Same filesystem (both under skills_dir) → rename is atomic + cheap.
    std::fs::rename(&src, &dst)
        .map_err(|e| MiraError::ToolError(format!("activate move {} -> {}: {e}", src.display(), dst.display())))?;
    Ok(dst)
}

/// Promote a live user-scoped skill to **global** (admin / global-propose).
///
/// Moves `users/<owner>/<id>/` → `<skills_dir>/<new_id>/`, renaming the id from
/// its `user.<owner>.<slug>` namespace to `local.<slug>` (the loader requires
/// the directory name to equal the manifest id, so both change together), and
/// strips any stale `[verification]` block — the caller re-signs the global copy
/// with the instance key. Returns the new global id. Errors (without touching
/// anything) if a global skill with the derived id already exists.
pub fn promote(skills_dir: &Path, owner: &str, id: &str) -> Result<String, MiraError> {
    if !safe_segment(owner) || !safe_segment(id) {
        return Err(MiraError::ToolError("unsafe skill owner/id".into()));
    }
    let src = live_root(skills_dir, owner).join(id);
    if !src.is_dir() {
        return Err(MiraError::NotFound(format!("no user-scoped skill {id:?} for {owner:?}")));
    }
    let new_id = global_id_for(owner, id);
    if !safe_segment(&new_id) {
        return Err(MiraError::ToolError(format!("derived global id {new_id:?} is invalid")));
    }
    let dst = skills_dir.join(&new_id);
    if dst.exists() {
        return Err(MiraError::ToolError(format!(
            "a global skill {new_id:?} already exists — rename or remove it first"
        )));
    }

    // Rewrite the manifest id to the global id and drop any stale signature
    // (the caller re-signs the global copy). toml_edit preserves comments.
    let manifest_path = src.join("skill.toml");
    let original = std::fs::read_to_string(&manifest_path)
        .map_err(|e| MiraError::ToolError(format!("promote read {}: {e}", manifest_path.display())))?;
    let mut doc: toml_edit::DocumentMut = original.parse()
        .map_err(|e| MiraError::ToolError(format!("promote parse {}: {e}", manifest_path.display())))?;
    doc["skill"]["id"] = toml_edit::value(new_id.clone());
    doc.as_table_mut().remove("verification");
    std::fs::write(&manifest_path, doc.to_string())
        .map_err(|e| MiraError::ToolError(format!("promote write {}: {e}", manifest_path.display())))?;

    std::fs::rename(&src, &dst).map_err(|e| {
        MiraError::ToolError(format!("promote move {} -> {}: {e}", src.display(), dst.display()))
    })?;
    Ok(new_id)
}

/// Derive the global id for a promoted user-scoped skill: strip a leading
/// `user.<owner>.` (or bare `user.`) namespace and re-root under `local.` so it
/// reads as on-box-authored and stays reverse-DNS shaped.
fn global_id_for(owner: &str, id: &str) -> String {
    let stripped = id
        .strip_prefix(&format!("user.{owner}."))
        .or_else(|| id.strip_prefix("user."))
        .unwrap_or(id);
    format!("local.{stripped}")
}

/// Discard a staged proposal.
pub fn reject(skills_dir: &Path, owner: &str, id: &str) -> Result<(), MiraError> {
    if !safe_segment(owner) || !safe_segment(id) {
        return Err(MiraError::ToolError("unsafe skill owner/id".into()));
    }
    let dir = proposed_root(skills_dir, owner).join(id);
    if dir.is_dir() {
        std::fs::remove_dir_all(&dir)
            .map_err(|e| MiraError::ToolError(format!("reject {}: {e}", dir.display())))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn toml_for(id: &str) -> String {
        format!("[skill]\nid = \"{id}\"\nversion = \"0.1.0\"\n\
                 display_name = \"T\"\ndescription = \"when\"\n\n\
                 [tools.guide]\nkind = \"prompt\"\ntemplate = \"prompts/main.md\"\n")
    }

    #[test]
    fn stage_list_activate_reject_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let skills = tmp.path().join("skills");
        let id = "user.alice.notes";

        // stage → appears in list
        stage(&skills, "alice", id, &toml_for(id), "do the thing").unwrap();
        let props = list(&skills, "alice");
        assert_eq!(props.len(), 1);
        assert_eq!(props[0].id, id);
        assert_eq!(props[0].tier, 0);

        // activate → moved to users/<owner>/<id>/, gone from staging
        let live = activate(&skills, "alice", id).unwrap();
        assert!(live.join("skill.toml").is_file());
        assert_eq!(live, skills.join("users").join("alice").join(id));
        assert!(list(&skills, "alice").is_empty());
        assert_eq!(std::fs::read_to_string(live.join("prompts/main.md")).unwrap(), "do the thing");

        // reject a (new) staged proposal → gone
        stage(&skills, "alice", "user.alice.two", &toml_for("user.alice.two"), "x").unwrap();
        reject(&skills, "alice", "user.alice.two").unwrap();
        assert!(list(&skills, "alice").is_empty());
    }

    #[test]
    fn promote_moves_user_skill_to_global_and_renames_id() {
        let tmp = TempDir::new().unwrap();
        let skills = tmp.path().join("skills");
        let id = "user.alice.notes";
        stage(&skills, "alice", id, &toml_for(id), "do the thing").unwrap();
        activate(&skills, "alice", id).unwrap();

        let new_id = promote(&skills, "alice", id).unwrap();
        assert_eq!(new_id, "local.notes");

        // User-scoped copy is gone; a global copy exists with the renamed id.
        assert!(!skills.join("users").join("alice").join(id).exists());
        let global = skills.join(&new_id);
        assert!(global.join("skill.toml").is_file());
        assert!(global.join("prompts/main.md").is_file());

        let text = std::fs::read_to_string(global.join("skill.toml")).unwrap();
        let m = SkillManifest::parse(&text).unwrap();
        assert_eq!(m.skill.id, new_id, "manifest id must match the new dir name");
        assert!(m.verification.is_none(), "stale signature must be stripped before re-signing");
    }

    #[test]
    fn promote_refuses_when_global_id_already_exists() {
        let tmp = TempDir::new().unwrap();
        let skills = tmp.path().join("skills");
        let id = "user.alice.notes";
        stage(&skills, "alice", id, &toml_for(id), "x").unwrap();
        activate(&skills, "alice", id).unwrap();
        // A pre-existing global skill at the derived id.
        std::fs::create_dir_all(skills.join("local.notes")).unwrap();

        let err = promote(&skills, "alice", id).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        // The user-scoped original is untouched on refusal.
        assert!(skills.join("users").join("alice").join(id).join("skill.toml").is_file());
    }

    #[test]
    fn rejects_path_traversal() {
        let tmp = TempDir::new().unwrap();
        let skills = tmp.path().join("skills");
        assert!(stage(&skills, "alice", "../escape", "x", "y").is_err());
        assert!(activate(&skills, "..", "user.a.b").is_err());
    }
}
