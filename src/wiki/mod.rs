// SPDX-License-Identifier: AGPL-3.0-or-later

// src/wiki/mod.rs
//! Wiki — per-user (and system) markdown knowledge base.
//!
//! Companion to the structured memory DB (`src/memory/`). Where memory
//! stores atomic facts indexed for semantic retrieval, the wiki stores
//! narrative pages on disk that the agent can read into context and the
//! user can edit by hand. See `design-docs/wiki-feature.md` for the full design.
//!
//! # Layers
//! - **Storage** — atomic markdown files with YAML frontmatter
//!   (`page.rs`, `paths.rs`, `frontmatter.rs`).
//! - **Mutation** — every change goes through a [`WikiOp`] (`ops.rs`)
//!   applied deterministically by [`WikiApplier`] (`applier.rs`).
//! - **Audit** — every submitted / applied / rejected op is recorded in
//!   `wiki_audit` (`audit.rs`) per-user SQLite DB.
//! - **Read** — [`WikiStore`] (`store.rs`) is the read-side façade used
//!   by the context-injection hook, the web UI, and the MCP server.
//!
//! # Lifecycle of a write
//! 1. Caller (UI, extractor, agent tool) builds a `WikiOp`.
//! 2. Caller submits it to `WikiSystem::submit_op` → audit row, status
//!    = `pending`.
//! 3. Caller (or a reviewer in Slice C) calls `apply_op` (or
//!    `approve_op`) which runs the applier and flips status to
//!    `applied` / `failed` / `rejected`.
//!
//! Slices B–H build on this foundation; see `design-docs/wiki-feature.md`.

// TODO(wiki-v2): multi-user shared wikis at wikis/groups/<id>/
// TODO(wiki-v2): encryption at rest using ~/.mira/data/master.key
// TODO(wiki-v2): CRDT multi-device sync (v1 uses git, Slice G)

pub mod applier;
pub mod audit;
pub mod extractor;
pub mod frontmatter;
pub mod git;
pub mod import_export;
pub mod mcp;
pub mod ops;
pub mod page;
pub mod paths;
pub mod store;

pub use applier::WikiApplier;
pub use audit::WikiAuditDb;
pub use extractor::extract_wiki_ops;
pub use frontmatter::{PageFrontmatter, ProvenanceEntry, Writer};
pub use ops::{LogKind, OpStatus, Provenance, WikiOp, WikiOpEnvelope, WikiScope};
pub use page::WikiPage;
pub use paths::WikiPath;
pub use store::WikiStore;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use tracing::{debug, info};

/// Errors surfaced by the wiki module.
#[derive(Debug, thiserror::Error)]
pub enum WikiError {
    #[error("invalid wiki path: {0}")]
    InvalidPath(String),
    #[error("invalid frontmatter: {0}")]
    InvalidFrontmatter(String),
    #[error("page not found: {0}")]
    PageNotFound(String),
    #[error("writer policy: page allows '{actual}', op requires '{required}'")]
    WriterPolicy { required: String, actual: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, WikiError>;

/// Public facade — one instance per scope (per user or the system).
///
/// Holds the [`WikiStore`] (for reads) and the audit DB (for mutations).
/// All mutating methods go through `submit_op` → `apply_op`, which keeps
/// every change auditable. Cheap to wrap in `Arc<>` and share.
pub struct WikiSystem {
    store: WikiStore,
    audit: StdMutex<WikiAuditDb>,
    scope: WikiScope,
    /// Whether to auto-commit applied ops to git. `None` means git is
    /// disabled or unavailable; we never even try.
    git_auto_commit: bool,
}

impl WikiSystem {
    /// Open (or create) the per-user wiki at
    /// `{data_dir}/wikis/users/<user_id>/`.
    ///
    /// On first call, scaffolds the default skeleton (SCHEMA, profile,
    /// index, log + `pages/`, `sources/`, `.pending/`) and migrates the
    /// legacy `profile.md` from `{data_dir}/profiles/<user_id>/` if it
    /// exists.
    pub fn for_user(data_dir: &Path, user_id: &str) -> Result<Self> {
        let root = user_wiki_root(data_dir, user_id);
        let created = ensure_wiki_dir(&root, /* is_system = */ false)?;
        if created {
            migrate_legacy_profile(data_dir, user_id, &root)?;
            info!("wiki: created per-user wiki at {}", root.display());
        } else {
            debug!("wiki: opened existing per-user wiki at {}", root.display());
        }
        let audit_path = data_dir.join(format!("wiki_{}.db", sanitize_id(user_id)));
        let audit = WikiAuditDb::open(&audit_path)?;
        Ok(Self {
            store: WikiStore::new(root),
            audit: StdMutex::new(audit),
            scope: WikiScope::User(user_id.to_string()),
            git_auto_commit: false,
        })
    }

    /// Open (or create) the system wiki at `{data_dir}/wikis/system/`.
    /// Admin-only writes are enforced at the API layer in Slice E; this
    /// constructor itself is permissive.
    pub fn for_system(data_dir: &Path) -> Result<Self> {
        let root = system_wiki_root(data_dir);
        let created = ensure_wiki_dir(&root, /* is_system = */ true)?;
        if created {
            info!("wiki: created system wiki at {}", root.display());
        } else {
            // Migrate the Slice E placeholder content to the canonical
            // DEFAULT_SYSTEM_PROMPT. Idempotent — once the body looks
            // like a real persona, this is a no-op.
            migrate_legacy_persona_placeholder(&root)?;
        }
        let audit_path = data_dir.join("wiki_system.db");
        let audit = WikiAuditDb::open(&audit_path)?;
        Ok(Self {
            store: WikiStore::new(root),
            audit: StdMutex::new(audit),
            scope: WikiScope::System,
            git_auto_commit: false,
        })
    }

    /// Initialise `<root>/.git` and turn on auto-commit for future
    /// applied ops. Idempotent — opening an already-initialised repo
    /// is a no-op besides re-stamping the identity. Returns the
    /// `git::GitStatus` after init.
    pub fn enable_git(
        &mut self, auto_commit: bool,
    ) -> std::result::Result<git::GitStatus, git::GitError> {
        let (email, name) = match &self.scope {
            WikiScope::User(uid) => git::default_identity(uid),
            WikiScope::System    => git::system_identity(),
        };
        git::ensure_repo(self.store.root(), &email, &name)?;
        self.git_auto_commit = auto_commit;
        git::status(self.store.root())
    }

    /// Whether git auto-commit is currently active on this wiki.
    pub fn git_enabled(&self) -> bool { self.git_auto_commit }

    /// Snapshot of the wiki's git state, if git is initialised.
    pub fn git_status(&self) -> std::result::Result<git::GitStatus, git::GitError> {
        git::status(self.store.root())
    }

    /// Manually commit any pending changes (e.g. before an export or
    /// when auto-commit is off).
    pub fn git_commit_all(&self, message: &str)
        -> std::result::Result<bool, git::GitError>
    {
        git::commit_changes(self.store.root(), message)
    }

    /// Set or replace the `origin` remote.
    pub fn git_set_remote(&self, url: &str) -> std::result::Result<(), git::GitError> {
        git::set_remote(self.store.root(), url)
    }

    pub fn git_push(&self) -> std::result::Result<String, git::GitError> {
        git::push(self.store.root())
    }
    pub fn git_pull(&self) -> std::result::Result<String, git::GitError> {
        git::pull(self.store.root())
    }

    pub fn root(&self) -> &Path { self.store.root() }

    pub fn scope(&self) -> &WikiScope { &self.scope }

    pub fn store(&self) -> &WikiStore { &self.store }

    /// Submit an op for audit. Lands in the table with status=pending.
    /// Caller decides whether to call `apply_op` immediately or hand to
    /// the review queue (Slice C).
    pub fn submit_op(&self, op: WikiOp, provenance: Provenance) -> Result<String> {
        self.submit_op_conf(op, provenance, None)
    }

    /// Like [`submit_op`], recording the extractor's confidence on the
    /// envelope (drives tiered auto-apply + "approve all ≥ X").
    pub fn submit_op_conf(
        &self,
        op: WikiOp,
        provenance: Provenance,
        confidence: Option<f32>,
    ) -> Result<String> {
        let env = WikiOpEnvelope::new(self.scope.clone(), op, provenance)
            .with_confidence(confidence);
        let op_id = env.op_id.clone();
        self.audit.lock().expect("audit poisoned").insert(&env)?;
        Ok(op_id)
    }

    /// Apply a previously-submitted op. Reads the envelope, runs the
    /// applier, then flips the audit row to `applied` or `failed`.
    /// When `git_auto_commit` is on, a successful apply is followed by
    /// a `git commit` carrying a one-line message derived from the op.
    pub fn apply_op(&self, op_id: &str) -> Result<()> {
        let env = {
            let audit = self.audit.lock().expect("audit poisoned");
            audit.get(op_id)?
                .ok_or_else(|| WikiError::Other(format!("op {op_id} not found")))?
        };
        let res = WikiApplier::new(&self.store).apply(&env.op, &env.provenance);
        {
            let audit = self.audit.lock().expect("audit poisoned");
            match &res {
                Ok(()) => audit.mark_applied(op_id)?,
                Err(e) => audit.mark_failed(op_id, &e.to_string())?,
            }
        }
        if res.is_ok() && self.git_auto_commit {
            let msg = git::auto_commit_message(
                env.op.kind(),
                env.op.target_path().as_str(),
                &env.provenance.actor,
            );
            // Auto-commit failures must not break op application — the
            // file write already happened. Log and continue.
            if let Err(e) = git::commit_changes(self.store.root(), &msg) {
                tracing::warn!("wiki: git auto-commit failed: {e}");
            }
        }
        res
    }

    /// Submit and apply in one step. Used by direct UI writes where the
    /// review queue is bypassed.
    pub fn submit_and_apply(&self, op: WikiOp, provenance: Provenance) -> Result<String> {
        let op_id = self.submit_op(op, provenance)?;
        self.apply_op(&op_id)?;
        Ok(op_id)
    }

    /// Submit (recording confidence) and apply in one step. Used by the
    /// extractor's confidence-tiered auto-apply path.
    pub fn submit_and_apply_conf(
        &self,
        op: WikiOp,
        provenance: Provenance,
        confidence: Option<f32>,
    ) -> Result<String> {
        let op_id = self.submit_op_conf(op, provenance, confidence)?;
        self.apply_op(&op_id)?;
        Ok(op_id)
    }

    /// Reviewer approves a pending op → apply it.
    pub fn approve_op(&self, op_id: &str, reviewer: &str) -> Result<()> {
        self.audit.lock().expect("audit poisoned")
            .mark_reviewed(op_id, reviewer, true)?;
        self.apply_op(op_id)
    }

    /// Reviewer rejects a pending op → never applied.
    pub fn reject_op(&self, op_id: &str, reviewer: &str, reason: &str) -> Result<()> {
        let audit = self.audit.lock().expect("audit poisoned");
        audit.mark_reviewed(op_id, reviewer, false)?;
        audit.mark_rejected(op_id, reason)?;
        Ok(())
    }

    /// All ops currently awaiting review.
    pub fn list_pending_ops(&self) -> Result<Vec<WikiOpEnvelope>> {
        self.audit.lock().expect("audit poisoned").list_by_status(OpStatus::Pending)
    }

    /// Bulk-approve pending ops. When `min_confidence` is `Some(t)`, only ops
    /// whose recorded confidence is `>= t` are approved; ops with no recorded
    /// confidence (e.g. pre-existing rows, or direct writes) are skipped under
    /// a threshold. When `None`, every pending op is approved. Returns the
    /// count actually applied; an op that fails to apply is logged and skipped
    /// rather than aborting the batch.
    pub fn approve_pending_bulk(&self, reviewer: &str, min_confidence: Option<f32>) -> Result<usize> {
        let pending = self.list_pending_ops()?;
        let mut n = 0usize;
        for env in pending {
            if let Some(t) = min_confidence {
                if env.confidence.map_or(true, |c| c < t) { continue; }
            }
            match self.approve_op(&env.op_id, reviewer) {
                Ok(()) => n += 1,
                Err(e) => tracing::warn!("wiki bulk-approve: op {} failed: {e}", env.op_id),
            }
        }
        Ok(n)
    }

    /// Bulk-reject pending ops. When `max_confidence` is `Some(t)`, only ops
    /// whose confidence is `< t` (or unrecorded) are rejected — the inverse of
    /// the approve threshold, useful for "discard the low-confidence noise".
    /// When `None`, every pending op is rejected. Returns the count rejected.
    pub fn reject_pending_bulk(
        &self,
        reviewer: &str,
        reason: &str,
        max_confidence: Option<f32>,
    ) -> Result<usize> {
        let pending = self.list_pending_ops()?;
        let mut n = 0usize;
        for env in pending {
            if let Some(t) = max_confidence {
                if env.confidence.map_or(false, |c| c >= t) { continue; }
            }
            match self.reject_op(&env.op_id, reviewer, reason) {
                Ok(()) => n += 1,
                Err(e) => tracing::warn!("wiki bulk-reject: op {} failed: {e}", env.op_id),
            }
        }
        Ok(n)
    }

    /// Ops created since `since`, newest first.
    pub fn list_recent_ops(
        &self,
        since: chrono::DateTime<chrono::Utc>,
        limit: usize,
    ) -> Result<Vec<WikiOpEnvelope>> {
        self.audit.lock().expect("audit poisoned").list_recent(since, limit)
    }
}

// ── Multi-tenant registry ────────────────────────────────────────────────────

/// Resolves a [`WikiSystem`] per user — the multi-tenant façade held by
/// [`AgentCore`]. One `WikiRegistry` exists per MIRA instance; it lazily
/// creates and caches per-user wikis so the per-turn injection hook stays
/// O(1) after the first turn for a given user.
pub struct WikiRegistry {
    data_dir: PathBuf,
    user_wikis: StdMutex<HashMap<String, Arc<WikiSystem>>>,
    system_wiki: OnceLock<Arc<WikiSystem>>,
    /// When set, every new `WikiSystem` from this registry gets git
    /// initialised + auto-commit turned on according to the policy.
    git_policy: Option<GitPolicy>,
}

/// Default git settings the registry applies to every wiki it builds.
#[derive(Debug, Clone, Copy)]
pub struct GitPolicy {
    pub auto_commit: bool,
}

impl WikiRegistry {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            user_wikis: StdMutex::new(HashMap::new()),
            system_wiki: OnceLock::new(),
            git_policy: None,
        }
    }

    /// Turn on git for every wiki this registry produces. Idempotent
    /// per wiki — repo init is a no-op once `.git` exists. Best-effort:
    /// if `git` is not on PATH we log a warning and the policy is not
    /// installed (other wiki operations keep working).
    pub fn with_git(mut self, policy: GitPolicy) -> Self {
        if git::is_available() {
            self.git_policy = Some(policy);
        } else {
            tracing::warn!("wiki: `git` not found on PATH; git sync disabled");
        }
        self
    }

    pub fn data_dir(&self) -> &Path { &self.data_dir }

    /// Get-or-create the wiki for `user_id`. Cached after first call.
    pub fn for_user(&self, user_id: &str) -> Result<Arc<WikiSystem>> {
        if let Some(w) = self.user_wikis.lock().expect("wiki cache poisoned").get(user_id) {
            return Ok(w.clone());
        }
        let mut wiki = WikiSystem::for_user(&self.data_dir, user_id)?;
        if let Some(p) = self.git_policy {
            if let Err(e) = wiki.enable_git(p.auto_commit) {
                tracing::warn!("wiki: git init failed for user '{user_id}': {e}");
            }
        }
        let wiki = Arc::new(wiki);
        self.user_wikis
            .lock().expect("wiki cache poisoned")
            .insert(user_id.to_string(), wiki.clone());
        Ok(wiki)
    }

    /// Get-or-create the system wiki. Cached on first call.
    pub fn system(&self) -> Result<Arc<WikiSystem>> {
        if let Some(w) = self.system_wiki.get() {
            return Ok(w.clone());
        }
        let mut wiki = WikiSystem::for_system(&self.data_dir)?;
        if let Some(p) = self.git_policy {
            if let Err(e) = wiki.enable_git(p.auto_commit) {
                tracing::warn!("wiki: git init failed for system wiki: {e}");
            }
        }
        let wiki = Arc::new(wiki);
        let _ = self.system_wiki.set(wiki.clone());
        Ok(wiki)
    }
}

// ── Internals: path resolution + scaffolding ─────────────────────────────────

fn user_wiki_root(data_dir: &Path, user_id: &str) -> PathBuf {
    data_dir.join("wikis").join("users").join(sanitize_id(user_id))
}

fn system_wiki_root(data_dir: &Path) -> PathBuf {
    data_dir.join("wikis").join("system")
}

fn sanitize_id(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// Returns `true` if the wiki was created (first-boot scaffold), `false`
/// if it already existed.
fn ensure_wiki_dir(root: &Path, is_system: bool) -> Result<bool> {
    if root.exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(root.join("pages"))?;
    std::fs::create_dir_all(root.join("sources"))?;
    std::fs::create_dir_all(root.join(".pending"))?;

    page::write_raw(&root.join("SCHEMA.md"),
        if is_system { DEFAULT_SYSTEM_SCHEMA } else { DEFAULT_USER_SCHEMA })?;
    if is_system {
        page::write_raw(&root.join("persona.md"), &default_system_persona_doc())?;
    } else {
        page::write_raw(&root.join("profile.md"), DEFAULT_USER_PROFILE)?;
    }
    page::write_raw(&root.join("index.md"), DEFAULT_INDEX)?;
    page::write_raw(&root.join("log.md"), DEFAULT_LOG)?;

    Ok(true)
}

/// Replace a pre-Slice-F `persona.md` that still holds the
/// `(Externalized in Slice F…)` placeholder with the canonical default
/// prompt. Skips files that already have real content so admin edits
/// are never clobbered.
fn migrate_legacy_persona_placeholder(root: &Path) -> Result<()> {
    let persona_path = root.join("persona.md");
    if !persona_path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&persona_path)?;
    // The placeholder body was unique enough that a substring match is safe;
    // we only rewrite when it's the literal placeholder.
    if text.contains("(Externalized in Slice F") {
        page::write_raw(&persona_path, &default_system_persona_doc())?;
        info!("wiki: migrated system persona.md placeholder to default prompt");
    }
    Ok(())
}

/// Build the seed `persona.md` for a brand-new system wiki: YAML
/// frontmatter wrapping the canonical [`crate::system_prompt::DEFAULT_SYSTEM_PROMPT`].
/// Admins edit this in place to retune MIRA's behaviour without
/// recompiling.
fn default_system_persona_doc() -> String {
    let header = "---\n\
                  title: MIRA persona\n\
                  writer: user\n\
                  ---\n\
                  \n";
    let mut out = String::with_capacity(header.len() + crate::system_prompt::DEFAULT_SYSTEM_PROMPT.len());
    out.push_str(header);
    out.push_str(crate::system_prompt::DEFAULT_SYSTEM_PROMPT);
    if !out.ends_with('\n') { out.push('\n'); }
    out
}

/// If the legacy `{data_dir}/profiles/<user_id>/profile.md` exists and
/// has content, copy it into the new wiki location (replacing the
/// default profile.md just written) and leave the legacy file in place
/// (it will be removed in a future release).
fn migrate_legacy_profile(data_dir: &Path, user_id: &str, wiki_root: &Path) -> Result<()> {
    let legacy = data_dir.join("profiles").join(sanitize_id(user_id)).join("profile.md");
    if !legacy.exists() {
        return Ok(());
    }
    let legacy_content = std::fs::read_to_string(&legacy)?;
    if !legacy_content.trim().is_empty() {
        let new = wiki_root.join("profile.md");
        page::write_raw(&new, &legacy_content)?;
        info!("wiki: migrated legacy profile.md for {user_id}");
    }
    Ok(())
}

// ── Default scaffolding content ──────────────────────────────────────────────

const DEFAULT_USER_SCHEMA: &str = r#"---
title: Wiki schema
writer: user
---

# How this wiki is organized

This file tells MIRA how to read and write your wiki.

## Layout

- `profile.md` — always loaded into the model's context. Keep it short
  (a few hundred tokens). Things that are true about you most of the
  time: name, pronouns, timezone, how you like to be addressed, what
  you ask the assistant to do or not do.
- `index.md` — the navigation file. Lists every other page with a one-line
  summary. MIRA reads this first when it needs more context than `profile.md`
  provides, then drills into the pages it thinks are relevant.
- `log.md` — append-only timeline of events. New pages, supersessions, lint
  passes are appended here.
- `pages/` — topical pages. Subdirectories are allowed; reorganize as you
  like.
- `sources/` — raw documents (notes, papers, transcripts) the assistant
  treats as read-only references.

## Conventions

- Every page has YAML frontmatter declaring `writer:` (`user`, `agent`,
  or `both`). MIRA enforces this — if a page says `writer: user`,
  the assistant won't edit it without your approval.
- Pages have `valid_from` and `valid_to` dates. When something changes,
  MIRA supersedes the old page rather than deleting it, preserving
  history.
- The assistant records its own writes with provenance (which conversation
  produced them) so you can audit them.

## Editing

You can edit any file by hand. MIRA reads them on the fly. Restructure
the layout if it suits you better — just keep `index.md` current.
"#;

const DEFAULT_USER_PROFILE: &str = r#"---
title: Profile
writer: both
---

# Profile

This file is always loaded into MIRA's context. Keep it concise — a few
hundred tokens of the most important things to know about you.

## Communication style

(Filled in during onboarding, or edit here.)

## Autonomy

(How much initiative the assistant should take.)

## How to address me

(Name, pronouns, etc.)

## What to call me (the assistant)

(The assistant's name, if you've given it one.)

## Goals

(What you're working on, broadly.)

## Off-limits

(Topics or actions the assistant should not act on.)
"#;

const DEFAULT_INDEX: &str = r#"---
title: Index
writer: agent
---

# Wiki index

This file lists every page in the wiki with a one-line summary.
The assistant updates it as it adds, renames, or supersedes pages.

- [profile.md](profile.md) — always-loaded core profile.
- [SCHEMA.md](SCHEMA.md) — how this wiki is organized.
- [log.md](log.md) — append-only timeline of events.
"#;

const DEFAULT_LOG: &str = r#"---
title: Log
writer: agent
---

# Log

Append-only timeline of changes to this wiki.
"#;

const DEFAULT_SYSTEM_SCHEMA: &str = r#"---
title: System wiki schema
writer: user
---

# System wiki

This wiki holds MIRA's own knowledge: persona, operational runbooks,
glossary, and any other shared facts that are not per-user data.

Admin-only writes. Users see this content through the assistant's
behaviour, not directly.
"#;

// `persona.md` content is built from `crate::system_prompt::DEFAULT_SYSTEM_PROMPT`
// by `default_system_persona_doc()` above so the on-disk file mirrors
// the runtime default exactly.

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn for_user_creates_skeleton() {
        let dir = tempdir().unwrap();
        let wiki = WikiSystem::for_user(dir.path(), "u1").unwrap();
        let root = wiki.root();
        assert!(root.join("SCHEMA.md").exists());
        assert!(root.join("profile.md").exists());
        assert!(root.join("index.md").exists());
        assert!(root.join("log.md").exists());
        assert!(root.join("pages").is_dir());
        assert!(root.join("sources").is_dir());
        assert!(root.join(".pending").is_dir());
    }

    #[test]
    fn for_user_is_idempotent() {
        let dir = tempdir().unwrap();
        let _ = WikiSystem::for_user(dir.path(), "u2").unwrap();
        let _ = WikiSystem::for_user(dir.path(), "u2").unwrap();
        // No panic, files still present.
        assert!(dir.path().join("wikis/users/u2/profile.md").exists());
    }

    #[test]
    fn for_system_creates_persona_not_profile() {
        let dir = tempdir().unwrap();
        let wiki = WikiSystem::for_system(dir.path()).unwrap();
        let root = wiki.root();
        assert!(root.join("persona.md").exists());
        assert!(!root.join("profile.md").exists());
    }

    #[test]
    fn fresh_system_persona_contains_default_prompt() {
        let dir = tempdir().unwrap();
        let wiki = WikiSystem::for_system(dir.path()).unwrap();
        let body = std::fs::read_to_string(wiki.root().join("persona.md")).unwrap();
        // Frontmatter + canonical prompt — verify the prompt body landed.
        assert!(body.contains("Multi-tasking Intelligent Responsive Assistant"));
        assert!(body.contains("## Memory"));
        assert!(body.starts_with("---\n"));
    }

    #[test]
    fn legacy_placeholder_persona_is_migrated() {
        let dir = tempdir().unwrap();
        // Simulate a pre-Slice-F deployment: scaffold the system wiki,
        // then overwrite persona.md with the old placeholder content.
        let _ = WikiSystem::for_system(dir.path()).unwrap();
        let persona = dir.path().join("wikis/system/persona.md");
        std::fs::write(&persona,
            "---\ntitle: MIRA persona\nwriter: user\n---\n\n# Persona\n\n\
             (Externalized in Slice F. Until then, the system prompt is loaded\n\
             from `src/agent/core.rs::DEFAULT_SYSTEM_PROMPT`.)\n"
        ).unwrap();
        // Re-open — migration runs because the dir already exists.
        let _ = WikiSystem::for_system(dir.path()).unwrap();
        let body = std::fs::read_to_string(&persona).unwrap();
        assert!(!body.contains("(Externalized in Slice F"),
            "placeholder should be replaced; got:\n{body}");
        assert!(body.contains("Multi-tasking Intelligent"));
    }

    #[test]
    fn migration_leaves_real_persona_alone() {
        let dir = tempdir().unwrap();
        let _ = WikiSystem::for_system(dir.path()).unwrap();
        let persona = dir.path().join("wikis/system/persona.md");
        // An admin-edited persona.md — no placeholder substring.
        let custom = "---\ntitle: Custom\nwriter: user\n---\n\nYou are Helga, a stern librarian.\n";
        std::fs::write(&persona, custom).unwrap();
        let _ = WikiSystem::for_system(dir.path()).unwrap();
        let body = std::fs::read_to_string(&persona).unwrap();
        assert_eq!(body, custom);
    }

    #[test]
    fn legacy_profile_is_migrated() {
        let dir = tempdir().unwrap();
        let legacy_dir = dir.path().join("profiles").join("u3");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(legacy_dir.join("profile.md"),
            "---\nuser_id: u3\n---\n\n# Profile\n\n## Goals\n\n- Migrate me\n").unwrap();

        let wiki = WikiSystem::for_user(dir.path(), "u3").unwrap();
        let p = std::fs::read_to_string(wiki.root().join("profile.md")).unwrap();
        assert!(p.contains("Migrate me"));
    }

    #[test]
    fn submit_and_apply_round_trip() {
        let dir = tempdir().unwrap();
        let wiki = WikiSystem::for_user(dir.path(), "u4").unwrap();

        let op = WikiOp::WritePage {
            path: WikiPath::parse("pages/proj.md").unwrap(),
            frontmatter: PageFrontmatter::default(),
            body: "# Proj\nfirst line\n".into(),
        };
        let op_id = wiki.submit_and_apply(op, Provenance::user_ui("u4")).unwrap();

        // Status flipped to applied.
        let recent = wiki.list_recent_ops(chrono::Utc::now() - chrono::Duration::hours(1), 10).unwrap();
        let row = recent.iter().find(|e| e.op_id == op_id).unwrap();
        assert_eq!(row.status, OpStatus::Applied);
        assert!(row.applied_at.is_some());

        // File exists with the content.
        let body = std::fs::read_to_string(wiki.root().join("pages/proj.md")).unwrap();
        assert!(body.contains("first line"));
    }

    #[test]
    fn confidence_is_persisted_and_bulk_approve_respects_threshold() {
        let dir = tempdir().unwrap();
        let wiki = WikiSystem::for_user(dir.path(), "u6").unwrap();

        let mk = |name: &str| WikiOp::WritePage {
            path: WikiPath::parse(&format!("pages/{name}.md")).unwrap(),
            frontmatter: PageFrontmatter::default(),
            body: format!("# {name}\n"),
        };
        // Three pending: high conf, low conf, and no recorded confidence.
        wiki.submit_op_conf(mk("hi"),  Provenance::user_ui("u6"), Some(0.95)).unwrap();
        wiki.submit_op_conf(mk("lo"),  Provenance::user_ui("u6"), Some(0.40)).unwrap();
        wiki.submit_op(mk("none"), Provenance::user_ui("u6")).unwrap();

        // Confidence round-trips through the audit DB.
        let pending = wiki.list_pending_ops().unwrap();
        assert_eq!(pending.len(), 3);
        let hi = pending.iter().find(|e| e.op.target_path().as_str() == "pages/hi.md").unwrap();
        assert_eq!(hi.confidence, Some(0.95));

        // Approve all ≥ 0.85 → only the high-confidence op applies; the
        // low-confidence and unrecorded ones stay pending.
        let n = wiki.approve_pending_bulk("u6", Some(0.85)).unwrap();
        assert_eq!(n, 1);
        assert!(wiki.root().join("pages/hi.md").exists());
        assert!(!wiki.root().join("pages/lo.md").exists());
        assert_eq!(wiki.list_pending_ops().unwrap().len(), 2);

        // Reject-all (no threshold) clears the rest.
        let r = wiki.reject_pending_bulk("u6", "noise", None).unwrap();
        assert_eq!(r, 2);
        assert_eq!(wiki.list_pending_ops().unwrap().len(), 0);
        assert!(!wiki.root().join("pages/none.md").exists());
    }

    #[test]
    fn approve_and_reject_flow() {
        let dir = tempdir().unwrap();
        let wiki = WikiSystem::for_user(dir.path(), "u5").unwrap();

        let op = WikiOp::WritePage {
            path: WikiPath::parse("pages/x.md").unwrap(),
            frontmatter: PageFrontmatter::default(),
            body: "x\n".into(),
        };
        let id = wiki.submit_op(op, Provenance::user_ui("u5")).unwrap();
        assert_eq!(wiki.list_pending_ops().unwrap().len(), 1);

        wiki.approve_op(&id, "u5").unwrap();
        assert_eq!(wiki.list_pending_ops().unwrap().len(), 0);
        assert!(wiki.root().join("pages/x.md").exists());

        // Reject path.
        let op2 = WikiOp::WritePage {
            path: WikiPath::parse("pages/y.md").unwrap(),
            frontmatter: PageFrontmatter::default(),
            body: "y\n".into(),
        };
        let id2 = wiki.submit_op(op2, Provenance::user_ui("u5")).unwrap();
        wiki.reject_op(&id2, "u5", "not needed").unwrap();
        assert!(!wiki.root().join("pages/y.md").exists());
    }
}
