// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/memory_tools.rs
//! Agent-driven long-term memory tools (Part B2c).
//!
//! `memory_search` / `memory_add` / `memory_forget` let the model recall, store,
//! and delete durable facts on demand through the provider seam — the
//! "agent-driven" usage mode, complementing the harness-driven pre-turn recall.
//! They operate on the built-in provider today; when multiple providers are
//! configured each tool is gated on the active provider's capabilities and
//! surfaces a legible refusal rather than a generic error if a verb isn't
//! supported.
//!
//! Scoping is strict: the chat handler injects `_user_id`; the tools refuse to
//! run without it, so cross-user access is impossible by construction.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::debug;

use super::{Tool, ToolArgs, ToolResult, ToolVisibility};
use crate::auth::LocalAuthService;
use crate::memory::MemorySystem;
use crate::memory::provider::{CatalogParams, MemoryProvider, MemoryQuery, Trust};
use crate::memory::provider::set::ProviderSet;
use crate::memory::router::MemoryRouter;
use crate::memory::storage::{Category, MemorySource};
use crate::MiraError;

const DEFAULT_TOP_K: usize = 5;
const MAX_TOP_K: usize = 20;

fn user_id_of(args: &ToolArgs, tool: &str) -> Result<String, MiraError> {
    args.get("_user_id").and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .ok_or_else(|| MiraError::ToolError(format!("{tool} called without _user_id (chat handler must inject)")))
}

// ── memory_search ─────────────────────────────────────────────────────────────

pub struct MemorySearchTool {
    providers: Arc<ProviderSet>,
    auth:      Option<Arc<LocalAuthService>>,
}
impl MemorySearchTool {
    pub fn new(providers: Arc<ProviderSet>, auth: Option<Arc<LocalAuthService>>) -> Self {
        Self { providers, auth }
    }
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str { "memory_search" }
    fn description(&self) -> &str {
        "Search your long-term memory of durable facts about this user (their \
         preferences, relationships, projects, ongoing situation) by meaning — \
         across ALL configured memory stores, including any external source. \
         Use it when you need a fact that isn't in the visible context — e.g. \
         \"what car does the user drive?\". Returns a JSON array of facts, each \
         with its source and trust label; relay the relevant ones in natural \
         language, and attribute (don't assert) any marked \"found in source, not \
         verified\". If it comes back empty, say you don't have that recorded — a \
         miss means it isn't in memory, not that it's false. If a store is \
         reported unavailable, say so rather than concluding there's no such memory."
    }
    fn visibility(&self) -> ToolVisibility { ToolVisibility::User }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": { "type": "string", "description": "What to look for, in natural language (or a subject/person's name)." },
                "top_k": { "type": "integer", "description": "Max facts to return (default 5, max 20)." }
            }
        })
    }
    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = user_id_of(&args, "memory_search")?;
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("").trim().to_owned();
        if query.is_empty() {
            return Ok(ToolResult::failure("memory_search: `query` is required"));
        }
        let k = args.get("top_k").and_then(|v| v.as_u64()).map(|n| n as usize)
            .unwrap_or(DEFAULT_TOP_K).clamp(1, MAX_TOP_K);

        // Identity for an external store keyed on username/email (e.g. a remote HTTP service).
        let (username, email) = self.auth.as_ref()
            .and_then(|a| a.get_user(&user_id).ok().flatten())
            .map(|u| (Some(u.username), u.email))
            .unwrap_or((None, None));
        let mut q = MemoryQuery::new(&user_id, &query, k);
        q.username = username;
        q.email    = email;
        // Pass the user's groups so group-scoped memories are found (the per-turn
        // hook injects them; the tool must match it).
        q.group_ids = self.auth.as_ref()
            .and_then(|a| a.list_user_group_ids(&user_id).ok())
            .unwrap_or_default();

        // Fan across ALL configured read providers (builtin + any external),
        // not just the builtin store — so "look in memory" actually consults
        // every store the user has configured.
        debug!("memory_search: user={user_id} k={k} providers={}", self.providers.read.len());
        let recalled = MemoryRouter::new(self.providers.read.clone()).recall(&q, 0).await;

        let hits: Vec<Value> = recalled.facts.iter().map(|f| {
            let label = f.provider_meta.get("trust_label").and_then(|v| v.as_str());
            let trust = match f.trust {
                Trust::Verified => "verified",
                Trust::Reported => "reported",
                Trust::Unknown  => "unknown",
                _               => "unverified",
            };
            // Namespace the id by its source (`builtin:5547`, `<provider>:…`) so
            // memory_forget routes to the owning store and a numeric external id
            // can never delete builtin row #N.
            let id = f.id.as_ref().map(|i| format!("{}:{}", f.source_id, i));
            json!({
                "id": id,
                "content": f.content,
                "recorded_at": f.recorded_at.map(|t| t.to_rfc3339()),
                "score": f.score,
                "source": f.source_id,
                "trust": trust,
                "trust_label": label,
                // Documented SPI descriptors, surfaced when the service sent them.
                "subject": f.provider_meta.get("subject"),
                "relation": f.provider_meta.get("relation"),
            })
        }).collect();

        let unavailable: Vec<Value> = recalled.unavailable.iter()
            .map(|(src, reason)| json!({ "source": src, "reason": reason }))
            .collect();
        // A source that WITHHELD a related result (near/ambiguous match) — surface
        // it so the model refines/asks rather than reading "nothing recorded".
        let withheld: Vec<Value> = recalled.withheld.iter()
            .map(|(src, reason)| json!({ "source": src, "reason": reason }))
            .collect();

        if hits.is_empty() {
            let note = if !withheld.is_empty() {
                "no facts returned, but a source WITHHELD a related result (see `withheld`) — not \"no such memory\"; refine the query or ask the user to be more specific"
            } else if !unavailable.is_empty() {
                "no facts, but a store was unavailable; do not conclude the user has no such memory"
            } else {
                "nothing in memory for this query (not recorded — not that it's false)"
            };
            return Ok(ToolResult::success(
                json!({ "facts": [], "unavailable": unavailable, "withheld": withheld, "note": note }).to_string()));
        }
        Ok(ToolResult::success(json!({ "facts": hits, "unavailable": unavailable, "withheld": withheld }).to_string()))
    }
}

// ── memory_add ──────────────────────────────────────────────────────────────

pub struct MemoryAddTool { memory: Arc<MemorySystem> }
impl MemoryAddTool { pub fn new(memory: Arc<MemorySystem>) -> Self { Self { memory } } }

#[async_trait]
impl Tool for MemoryAddTool {
    fn name(&self) -> &str { "memory_add" }
    fn description(&self) -> &str {
        "Store a durable fact about this user in long-term memory, on their \
         explicit request (\"remember that …\") or when they state something \
         clearly worth keeping. Write ONE concise fact in the third person \
         (\"User prefers window seats\"). If this CORRECTS a fact you already \
         hold, store the new value here with the SAME `entity` key — it \
         replaces the old one (reported as \"updated\") rather than adding a \
         duplicate. Don't store secrets, passwords, or card numbers. Category is \
         one of fact|preference|skill|relationship|project (default fact). The \
         result tells you exactly what happened (added / updated / already \
         held) — only confirm to the user what the result reports."
    }
    fn visibility(&self) -> ToolVisibility { ToolVisibility::User }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["content"],
            "properties": {
                "content": { "type": "string", "description": "One concise, third-person fact about the user." },
                "category": { "type": "string", "enum": ["fact","preference","skill","relationship","project"],
                              "description": "Defaults to 'fact'." },
                "subject": { "type": "string", "description":
                    "Who the fact is about: 'user' (default), or a specific person's lowercase first name \
                     (e.g. 'leila') — never a role word like 'sister'." },
                "attribute": { "type": "string", "description":
                    "What the fact is about, a short stable snake_case word (e.g. 'favourite_colour', \
                     'employer', 'location', 'language'). Use the SAME attribute (and subject) when \
                     correcting a fact so the old value is replaced, not duplicated." },
                "cardinality": { "type": "string", "enum": ["single","multi"], "description":
                    "'single' if the subject has one such value (a new value replaces the old); 'multi' \
                     if many coexist (languages, pets, siblings — kept alongside). Default 'multi'." }
            }
        })
    }
    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = user_id_of(&args, "memory_add")?;
        let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("").trim().to_owned();
        if content.is_empty() {
            return Ok(ToolResult::failure("memory_add: `content` is required"));
        }
        // Refuse credential-like content rather than storing a secret in plain
        // text (N3). The model is told not to, but enforce it in code too.
        if crate::memory::auto_extract::looks_like_secret(&content) {
            return Ok(ToolResult::failure(
                "memory_add: refusing to store what looks like a credential (password/API key/token). \
                 Don't save secrets to memory.",
            ));
        }
        let category = match args.get("category").and_then(|v| v.as_str()) {
            Some("preference")   => Category::Preference,
            Some("skill")        => Category::Skill,
            Some("relationship") => Category::Relationship,
            Some("project")      => Category::Project,
            _                    => Category::Fact,
        };
        // Accept `attribute` (preferred) or the legacy `entity` alias.
        let attribute = args.get("attribute").or_else(|| args.get("entity"))
            .and_then(|v| v.as_str()).map(|s| s.trim()).filter(|s| !s.is_empty());
        let subject = args.get("subject").and_then(|v| v.as_str())
            .map(|s| s.trim()).filter(|s| !s.is_empty());
        let cardinality = args.get("cardinality").and_then(|v| v.as_str()).unwrap_or("");
        // Conflict-aware, UserExplicit write: a correction (same subject.attribute,
        // single-valued, changed value) supersedes; a restatement is a no-op
        // duplicate; an unkeyed / multi-valued fact adds alongside — so the model's
        // confirmation can be truthful (N1/N2).
        //
        // Ground the model's chosen subject against the turn's user message
        // (injected as `_user_message` by the agent). A mis-attributed fact ("I
        // heard Sara moved to Doha" stored as the user's own location) then keeps
        // both instead of overwriting. When there's no turn message (non-chat
        // caller), grounding is `None` and the model's subject is trusted as
        // before. The fact is always STORED either way — grounding only gates the
        // supersede, never the save.
        use crate::memory::WriteOutcome;
        let grounding = args.get("_user_message").and_then(|v| v.as_str());
        let outcome = self.memory.remember_fact(
            &user_id, content.clone(), category, subject, attribute, cardinality,
            grounding,
            &["tool".to_owned()], true, MemorySource::UserExplicit("memory_add".into()),
            None, None, None,
        ).await?;
        debug!("memory_add: user={user_id} outcome={outcome:?}");
        let body = match outcome {
            WriteOutcome::Added(id) =>
                json!({ "status": "added", "id": id.to_string() }),
            WriteOutcome::Updated { new_id, superseded } =>
                json!({ "status": "updated", "id": new_id.to_string(), "replaced_id": superseded.to_string() }),
            WriteOutcome::Duplicate(id) =>
                json!({ "status": "already_held", "id": id.to_string() }),
            WriteOutcome::Skipped(reason) =>
                json!({ "status": "skipped", "reason": reason }),
        };
        Ok(ToolResult::success(body.to_string()))
    }
}

// ── memory_forget ─────────────────────────────────────────────────────────────

pub struct MemoryForgetTool {
    providers: Arc<ProviderSet>,
    memory:    Arc<MemorySystem>,
    auth:      Option<Arc<LocalAuthService>>,
}
impl MemoryForgetTool {
    pub fn new(providers: Arc<ProviderSet>, memory: Arc<MemorySystem>) -> Self {
        Self { providers, memory, auth: None }
    }
    pub fn with_auth(mut self, auth: Option<Arc<LocalAuthService>>) -> Self {
        self.auth = auth; self
    }
}

#[async_trait]
impl Tool for MemoryForgetTool {
    fn name(&self) -> &str { "memory_forget" }
    fn description(&self) -> &str {
        "Delete a durable fact from long-term memory by its id, on the user's \
         request (\"forget that …\"). Use the id exactly as memory_search returned \
         it (it is namespaced by source, e.g. `builtin:123`). Only removes the one \
         fact you name; a fact in a read-only source can't be deleted from here."
    }
    fn visibility(&self) -> ToolVisibility { ToolVisibility::User }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["id"],
            "properties": { "id": { "type": "string", "description": "The namespaced fact id from memory_search (e.g. `builtin:123`)." } }
        })
    }
    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = user_id_of(&args, "memory_forget")?;
        let raw = args.get("id").and_then(|v| v.as_str()).unwrap_or("").trim().to_owned();
        if raw.is_empty() {
            return Ok(ToolResult::failure("memory_forget: `id` is required"));
        }
        // Namespaced id from memory_search ("builtin:5547", "<provider>:…"). A bare
        // value (no namespace) defaults to builtin for backward compatibility —
        // and so a numeric id can only ever hit builtin, never a coincidentally-
        // numbered external row.
        let (prov_id, item_id) = raw.split_once(':').unwrap_or(("builtin", raw.as_str()));
        let Some(provider) = self.providers.read.iter().find(|p| p.id() == prov_id) else {
            return Ok(ToolResult::failure(format!("memory_forget: no memory source '{prov_id}'")));
        };
        if !provider.capabilities().forget {
            return Ok(ToolResult::success(json!({
                "forgotten": false,
                "note": format!("'{prov_id}' is read-only or managed elsewhere — nothing was deleted"),
            }).to_string()));
        }
        // Built-in: scope-policy ownership gate. The chat tool may only forget the
        // caller's OWN user-scoped memory — a system- or group-scoped memory is
        // visible to the user but must NOT be deletable from chat (that's the
        // admin/creator-only DELETE endpoint). Empty groups so group rows aren't
        // even fetched here.
        if prov_id == "builtin" {
            let Ok(id) = item_id.parse::<u64>() else {
                return Ok(ToolResult::failure("memory_forget: builtin id must be numeric"));
            };
            match self.memory.get_visible(id, &user_id, &[]) {
                Ok(Some(m)) if m.scope == crate::memory::Scope::User => {}
                Ok(Some(_)) => return Ok(ToolResult::success(json!({
                    "forgotten": false,
                    "note": "that's a shared (group/system) memory — manage it from the Memory page, not chat",
                }).to_string())),
                Ok(None) => return Ok(ToolResult::success(
                    json!({ "forgotten": false, "note": "no such fact for you" }).to_string())),
                Err(e) => return Ok(ToolResult::failure(format!("memory_forget: {e}"))),
            }
        }
        // The authenticated caller's id travels in the selector; providers
        // authorize against it and never trust any user part of the item id. The
        // caller's username/email ride along so an HTTP store keyed on those
        // (its `user_id_field`) forgets on the right identity, not the raw UUID.
        let (caller_username, caller_email) = self.auth.as_ref()
            .and_then(|a| a.get_user(&user_id).ok().flatten())
            .map(|u| (Some(u.username), u.email))
            .unwrap_or((None, None));
        let selector = crate::memory::provider::ForgetSelector::Id {
            caller_user_id: user_id.clone(),
            item: item_id.to_string(),
            caller_username,
            caller_email,
        };
        match provider.forget(&selector).await {
            Ok(n) if n > 0 => Ok(ToolResult::success(json!({ "forgotten": true, "id": raw }).to_string())),
            Ok(_)          => Ok(ToolResult::success(json!({ "forgotten": false, "note": "already gone" }).to_string())),
            Err(e)         => Ok(ToolResult::failure(format!("memory_forget: {e}"))),
        }
    }
}

// ── memory_catalog ──────────────────────────────────────────────────────────
//
// Browse WHAT an external memory source HOLDS for this user — counts per store
// and trust level, the subject/record names to ask by, and (for a named subject)
// its served facts WITH their trust level — rather than reading facts for a query.
// Only registered when a catalog-capable provider (e.g. a remote HTTP memory service)
// is configured; owner-scoped like the other memory tools via the injected
// `_user_id`. Holds the one catalog provider directly (shared with the agent's
// ProviderSet, so no second HTTP client) plus auth for identity resolution.

pub struct MemoryCatalogTool {
    providers: Arc<ProviderSet>,
    auth:      Option<Arc<LocalAuthService>>,
}
impl MemoryCatalogTool {
    pub fn new(providers: Arc<ProviderSet>, auth: Option<Arc<LocalAuthService>>) -> Self {
        Self { providers, auth }
    }
    /// The catalog-capable provider right now (capabilities can change after a
    /// late re-probe), or None — so the tool is always registered and simply
    /// refuses when no source offers catalog (X10).
    fn catalog_provider(&self) -> Option<&Arc<dyn MemoryProvider>> {
        self.providers.read.iter().find(|p| p.capabilities().catalog)
    }
}

#[async_trait]
impl Tool for MemoryCatalogTool {
    fn name(&self) -> &str { "memory_catalog" }
    fn description(&self) -> &str {
        "Browse WHAT your external memory source holds about a subject, rather than \
         recalling facts for a query. Use it for \"what do you know about <person/topic>?\" \
         or to see which subjects are on file. With no `subject`, returns counts (by trust \
         level) and the matching subject names to ask by; with a `subject`, returns that \
         subject's held facts WITH their trust level — an unverified item is a source \
         passage, so attribute it, don't assert it. Owner-scoped: only ever your own holdings."
    }
    fn visibility(&self) -> ToolVisibility { ToolVisibility::User }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subject":  { "type": "string", "description": "A subject/person/title to get the held facts (with their trust level) for." },
                "prefix":   { "type": "string", "description": "List only subjects whose name/alias/title starts with this." },
                "contains": { "type": "string", "description": "List only subjects whose name/alias/title contains this (don't combine with prefix)." },
                "page":     { "type": "integer", "description": "1-based page of the subject/record list (default 1)." }
            }
        })
    }
    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = user_id_of(&args, "memory_catalog")?;
        let Some(provider) = self.catalog_provider() else {
            return Ok(ToolResult::success(json!({
                "available": false,
                "note": "no catalog-capable memory source is configured or reachable right now",
            }).to_string()));
        };
        let s = |k: &str| args.get(k).and_then(|v| v.as_str())
            .map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
        let page = args.get("page").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as u32;

        let (username, email) = self.auth.as_ref()
            .and_then(|a| a.get_user(&user_id).ok().flatten())
            .map(|u| (Some(u.username), u.email))
            .unwrap_or((None, None));
        let mut params = CatalogParams::new(&user_id);
        params.username = username;
        params.email    = email;
        params.subject  = s("subject");
        params.prefix   = s("prefix");
        params.contains = s("contains");
        params.page     = page;
        debug!("memory_catalog: user={user_id} subject={:?} page={page}", params.subject);

        match provider.catalog(&params).await {
            Ok(res) => {
                let cat = &res.catalog;
                // Compact the subject list to the names a follow-up can ask by.
                let subjects: Vec<&str> = cat.get("subjects").and_then(|v| v.as_array())
                    .map(|a| a.iter()
                        .filter_map(|r| r.get("name").and_then(|v| v.as_str()))
                        .collect())
                    .unwrap_or_default();
                Ok(ToolResult::success(json!({
                    "state":          res.state,
                    "source":         provider.id(),
                    "counts":         cat.get("counts"),
                    "subjects":       subjects,
                    "page":           cat.get("page"),
                    "subject_detail": cat.get("subject_detail"),
                }).to_string()))
            }
            Err(e) => Ok(ToolResult::failure(format!("memory_catalog: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::provider::MemoryRetrieval;
    use tempfile::tempdir;

    fn mem() -> Arc<MemorySystem> {
        let dir = tempdir().unwrap();
        let path = dir.path().join("m.db");
        std::mem::forget(dir);
        Arc::new(MemorySystem::new(path).unwrap())
    }

    // A builtin-only provider set wrapping `m` (default config declares no
    // external providers).
    fn providers_for(m: &Arc<MemorySystem>) -> Arc<ProviderSet> {
        Arc::new(ProviderSet::build(&crate::config::MiraConfig::default(), Arc::clone(m)))
    }
    fn search_for(m: &Arc<MemorySystem>) -> MemorySearchTool {
        MemorySearchTool::new(providers_for(m), None)
    }

    #[tokio::test]
    async fn add_search_forget_round_trip() {
        let m = mem();
        let providers = providers_for(&m);
        let add = MemoryAddTool::new(Arc::clone(&m));
        let search = MemorySearchTool::new(Arc::clone(&providers), None);
        let forget = MemoryForgetTool::new(Arc::clone(&providers), Arc::clone(&m));

        // add
        let r = add.execute(json!({ "_user_id": "alice", "content": "User drives a Subaru" })).await.unwrap();
        assert!(r.success, "add failed: {:?}", r.error);
        let id = serde_json::from_str::<Value>(&r.output).unwrap()["id"].as_str().unwrap().to_owned();

        // search finds it (keyword fallback works without embeddings), with a
        // source-namespaced id.
        let r = search.execute(json!({ "_user_id": "alice", "query": "Subaru" })).await.unwrap();
        assert!(r.output.contains("Subaru"), "search missed it: {}", r.output);
        assert!(r.output.contains("\"builtin:"), "id must be namespaced: {}", r.output);

        // another user doesn't see it
        let r = search.execute(json!({ "_user_id": "bob", "query": "Subaru" })).await.unwrap();
        assert!(!r.output.contains("Subaru"), "cross-user leak: {}", r.output);

        // forget via the namespaced id removes it
        let r = forget.execute(json!({ "_user_id": "alice", "id": format!("builtin:{id}") })).await.unwrap();
        assert!(r.output.contains("\"forgotten\":true"), "forget failed: {}", r.output);
    }

    #[tokio::test]
    async fn forget_refuses_read_only_source_and_bad_namespace() {
        let m = mem();
        // A provider set with builtin + a read-only external source.
        let providers = Arc::new(ProviderSet {
            read: vec![
                Arc::new(crate::memory::provider::builtin::BuiltinMemoryProvider::new(Arc::clone(&m))),
                Arc::new(SrcMock),
            ],
            harness_writers: vec![],
            build_failures: vec![],
        });
        let forget = MemoryForgetTool::new(providers, Arc::clone(&m));
        // Read-only external id → refused, nothing deleted. (SrcMock id = "ext-http",
        // read-only → capabilities().forget is false.)
        let r = forget.execute(json!({ "_user_id": "tarek", "id": "ext-http:abc" })).await.unwrap();
        assert!(r.output.contains("\"forgotten\":false"));
        assert!(r.output.contains("read-only") || r.output.contains("managed elsewhere"));
        // Unknown source → clear failure.
        let r = forget.execute(json!({ "_user_id": "tarek", "id": "nope:1" })).await.unwrap();
        assert!(!r.success);
    }

    // H2 regression: a non-owner must NOT delete a SYSTEM-scoped memory via chat.
    #[tokio::test]
    async fn forget_refuses_system_scope_via_chat_tool() {
        let m = mem();
        let sys_id = m.store_scoped(
            "system-wide policy".into(), Category::Fact, vec![],
            Some(MemorySource::UserExplicit("seed".into())),
            crate::memory::Scope::System, None, "admin", &[], None, None, None,
        ).await.unwrap();
        let forget = MemoryForgetTool::new(providers_for(&m), Arc::clone(&m));
        let r = forget.execute(json!({ "_user_id": "regular", "id": format!("builtin:{sys_id}") })).await.unwrap();
        assert!(r.output.contains("\"forgotten\":false"), "system forget must be refused: {}", r.output);
        assert!(r.output.contains("shared") || r.output.contains("Memory page"));
        assert!(m.get_visible(sys_id, "regular", &[]).unwrap().is_some(), "system memory must survive");
    }

    // The owner CAN forget their own user-scoped memory via chat.
    #[tokio::test]
    async fn forget_allows_own_user_scope_via_chat_tool() {
        let m = mem();
        let id = m.store_scoped(
            "my own note".into(), Category::Fact, vec![],
            Some(MemorySource::UserExplicit("seed".into())),
            crate::memory::Scope::User, Some("alice"), "alice", &[], None, None, None,
        ).await.unwrap();
        let forget = MemoryForgetTool::new(providers_for(&m), Arc::clone(&m));
        let r = forget.execute(json!({ "_user_id": "alice", "id": format!("builtin:{id}") })).await.unwrap();
        assert!(r.output.contains("\"forgotten\":true"), "owner forget should succeed: {}", r.output);
    }

    #[tokio::test]
    async fn tools_refuse_without_user_id() {
        let m = mem();
        let search = search_for(&m);
        assert!(search.execute(json!({ "query": "x" })).await.is_err());
    }

    // A minimal catalog-capable provider that echoes the params it received, so
    // the tool's owner-scope injection and response compacting are testable.
    struct CatMock { seen: std::sync::Mutex<Option<CatalogParams>> }
    #[async_trait]
    impl MemoryProvider for CatMock {
        fn id(&self) -> &str { "ext-http" }
        fn capabilities(&self) -> crate::memory::provider::MemoryCapabilities {
            crate::memory::provider::MemoryCapabilities { catalog: true, ..crate::memory::provider::MemoryCapabilities::read_only() }
        }
        fn locality(&self) -> crate::memory::provider::MemoryLocality { crate::memory::provider::MemoryLocality::Loopback }
        async fn health(&self) -> crate::memory::provider::MemoryHealth { crate::memory::provider::MemoryHealth::Ready }
        async fn retrieve(&self, _q: &MemoryQuery) -> Result<MemoryRetrieval, crate::memory::provider::MemoryError> {
            Ok(MemoryRetrieval::Absent { reason: None })
        }
        async fn ingest(&self, _t: &crate::memory::provider::TurnRecord) -> Result<usize, crate::memory::provider::MemoryError> { Ok(0) }
        async fn forget(&self, _s: &crate::memory::provider::ForgetSelector) -> Result<usize, crate::memory::provider::MemoryError> { Ok(0) }
        async fn catalog(&self, p: &CatalogParams) -> Result<crate::memory::provider::CatalogResult, crate::memory::provider::MemoryError> {
            *self.seen.lock().unwrap() = Some(p.clone());
            Ok(crate::memory::provider::CatalogResult {
                state: "served".into(),
                catalog: json!({
                    "counts": { "subjects": 12, "facts": 40, "by_trust": { "verified": 3, "unverified": 5 } },
                    "subjects": [ { "name": "Jordan Vale", "aliases": [] } ],
                    "subject_detail": { "name": p.subject, "n_facts": 5 },
                }),
            })
        }
    }

    #[tokio::test]
    async fn catalog_tool_injects_user_and_compacts_subjects() {
        let mock = Arc::new(CatMock { seen: std::sync::Mutex::new(None) });
        let set = Arc::new(ProviderSet {
            read: vec![mock.clone() as Arc<dyn MemoryProvider>],
            harness_writers: vec![], build_failures: vec![],
        });
        let tool = MemoryCatalogTool::new(set, None);
        let r = tool.execute(json!({ "_user_id": "tarek", "subject": "Jordan Vale" })).await.unwrap();
        assert!(r.success, "catalog failed: {:?}", r.error);
        // Owner-scope: the asking user's id reached the provider.
        assert_eq!(mock.seen.lock().unwrap().as_ref().unwrap().user_id, "tarek");
        let out: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(out["source"], "ext-http");
        assert_eq!(out["counts"]["by_trust"]["unverified"], 5);
        // Subjects compacted to names to ask by.
        assert_eq!(out["subjects"][0], "Jordan Vale");
        assert_eq!(out["subject_detail"]["n_facts"], 5);
    }

    #[tokio::test]
    async fn catalog_tool_refuses_without_user_id() {
        let set = Arc::new(ProviderSet {
            read: vec![Arc::new(CatMock { seen: std::sync::Mutex::new(None) }) as Arc<dyn MemoryProvider>],
            harness_writers: vec![], build_failures: vec![],
        });
        let tool = MemoryCatalogTool::new(set, None);
        assert!(tool.execute(json!({ "subject": "x" })).await.is_err());
    }

    // An external provider that serves one unverified, attributed fact, like a
    // remote HTTP memory service.
    struct SrcMock;
    #[async_trait]
    impl MemoryProvider for SrcMock {
        fn id(&self) -> &str { "ext-http" }
        fn capabilities(&self) -> crate::memory::provider::MemoryCapabilities {
            crate::memory::provider::MemoryCapabilities { citations: true, ..crate::memory::provider::MemoryCapabilities::read_only() }
        }
        fn locality(&self) -> crate::memory::provider::MemoryLocality { crate::memory::provider::MemoryLocality::Loopback }
        async fn health(&self) -> crate::memory::provider::MemoryHealth { crate::memory::provider::MemoryHealth::Ready }
        async fn retrieve(&self, _q: &MemoryQuery) -> Result<MemoryRetrieval, crate::memory::provider::MemoryError> {
            Ok(MemoryRetrieval::Facts(vec![crate::memory::provider::MemoryFact {
                id: Some("subj/row1".into()),
                content: "Jordan Vale was a community organizer and social worker.".into(),
                score: Some(0.9), recorded_at: None, valid_at: None,
                source_id: "ext-http".into(), provenance: None, confidence: None,
                speaker: crate::memory::provider::Speaker::External,
                trust: Trust::Unverified, complete_set: false,
                provider_meta: json!({ "trust": "unverified", "trust_label": "found in source, not verified" }),
            }]))
        }
        async fn ingest(&self, _t: &crate::memory::provider::TurnRecord) -> Result<usize, crate::memory::provider::MemoryError> { Ok(0) }
        async fn forget(&self, _s: &crate::memory::provider::ForgetSelector) -> Result<usize, crate::memory::provider::MemoryError> { Ok(0) }
    }

    #[tokio::test]
    async fn memory_search_fans_to_external_provider_with_tier_labels() {
        let providers = Arc::new(ProviderSet { read: vec![Arc::new(SrcMock)], harness_writers: vec![], build_failures: vec![] });
        let tool = MemorySearchTool::new(providers, None);
        let r = tool.execute(json!({ "_user_id": "tarek", "query": "Jordan Vale" })).await.unwrap();
        assert!(r.success);
        // The external fact is surfaced (not just builtin), with its source + trust label.
        assert!(r.output.contains("Jordan"), "external fact missing: {}", r.output);
        assert!(r.output.contains("ext-http"), "source missing");
        assert!(r.output.contains("found in source, not verified"), "trust label missing");
        assert!(r.output.contains("unverified"), "trust level missing");
    }
}
