// SPDX-License-Identifier: AGPL-3.0-or-later

// src/mcp/registry.rs
//! Connects every enabled `mcp_servers` row at startup, holds the
//! resulting [`McpClient`]s, and exposes a [`status_for_user`]
//! snapshot for the per-user `/api/mcp/status` view. The registry is
//! **not** the agent's [`ToolRegistry`] — the gateway calls
//! [`Self::register_tools_into`] to splat the discovered adapters
//! onto the shared tool registry so the agent treats them
//! identically to builtins.
//!
//! per-user isolation. Each row carries the owning
//! `user_id`, and [`Self::allowed_tools_for`] gives a chat handler
//! the explicit allow-list to pass via `TurnContext.allowed_tool_names`.
//!
//! collision-safe naming. When two users own servers with
//! the same `name`, both would naively register every tool under
//! identical `mcp__<server>__<tool>` keys; the global `ToolRegistry`
//! hashmap would last-write-win and silently lose one user's tools.
//! The registry now does a pre-pass: the first owner of each
//! `(server, tool)` combo keeps the clean name; later owners' tools
//! get a `__u<short_id>` suffix so every entry remains addressable.
//! The per-user filter (`allowed_tools_for`) returns the resolved
//! names per user so the agent always sees the right set.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use tracing::{info, warn};

use crate::mcp::{
    McpClient, McpGetPromptTool, McpListPromptsTool,
    McpListResourcesTool, McpReadResourceTool, McpToolAdapter,
};
use crate::artifacts::ArtifactStore;
use crate::mcp::store::{McpServerRow, McpServerStore};
use crate::providers::ModelProvider;
use crate::tools::{Tool, ToolRegistry};

// Per-server status surfaced to the UI.
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpServerStatus {
    pub id:         String,
    pub owner_user_id: String,
    pub name:       String,
    pub transport:  String,
    pub enabled:    bool,
    // `"connected"`, `"disabled"`, or `"error"`.
    pub state:      String,
    pub tool_count: usize,
    pub tools:      Vec<McpToolInfo>,
    pub supports_resources: bool,
    // `true` when the server's initialize handshake
    // declared a `prompts` capability, so the registry installed
    // the list_prompts / get_prompt adapter pair.
    pub supports_prompts: bool,
    // `true` when at least one of this server's tools had
    // its name suffixed with `__u<short_id>` because another user
    // already owned the unprefixed name. The UI uses this to
    // surface a "suffixed for uniqueness" note so the operator
    // understands the unusual names.
    pub renamed_for_collision: bool,
    // `true` when the row opted in via
    // `sampling_enabled = true`. The handler advertises the
    // sampling capability at initialize time and fulfils
    // `sampling/createMessage` via the gateway's primary provider.
    // UI shows a badge so operators know which servers can spend
    // their provider quota.
    pub sampling_enabled: bool,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct McpToolInfo {
    pub name:        String,
    pub description: String,
}

// One resolved adapter slot — what name to register under and which
// client + remote tool it wraps. Built in `connect_all_from_store`
// after collision resolution, consumed by `register_tools_into`.
struct ResolvedAdapter {
    kind:           AdapterKind,
    // The final (collision-resolved) registry key.
    qualified_name: String,
    client_idx:     usize,
    // For `AdapterKind::Tool`, the remote tool meta to wrap.
    tool_meta_idx:  Option<usize>,
}

#[derive(Copy, Clone)]
enum AdapterKind { Tool, ListResources, ReadResource, ListPrompts, GetPrompt }

// The connected-server state, swapped wholesale on reload. Held behind a
// `RwLock` so the agent's shared `Arc<McpServerRegistry>` can be refreshed
// in place when servers are added/edited/removed — no restart.
#[derive(Default)]
struct RegistryState {
    clients:       Vec<Arc<McpClient>>,
    statuses:      Vec<McpServerStatus>,
    resolved:      Vec<ResolvedAdapter>,
    tools_by_user: HashMap<String, Vec<String>>,
}

pub struct McpServerRegistry {
    state: RwLock<RegistryState>,
    // Dependencies for [`Self::reload`]. `None` in the empty/test
    // registry, which then can't reconnect (reload is a no-op).
    store:    Option<Arc<McpServerStore>>,
    provider: Option<Arc<dyn ModelProvider>>,
    // Artifact store for saving image content returned by tools (e.g.
    // browser screenshots) so the UI can render them. Threaded into each
    // tool adapter at build time.
    artifacts: Option<ArtifactStore>,
    // The agent's tool registry, so reload can push the fresh MCP tool
    // surface in via `set_mcp_tools`. Attached after construction (the
    // tool registry Arc is built slightly later in the gateway).
    tools: RwLock<Option<Arc<ToolRegistry>>>,
}

impl McpServerRegistry {
    // Empty registry — used when the store isn't wired (tests,
    // minimal builds). Equivalent to "no MCP servers"; `reload` is a no-op.
    pub fn empty() -> Self {
        Self {
            state:     RwLock::new(RegistryState::default()),
            store:     None,
            provider:  None,
            artifacts: None,
            tools:     RwLock::new(None),
        }
    }

    // A reload-capable registry. Holds the store + provider so it can
    // reconnect on demand; call [`Self::attach_tool_registry`] then
    // [`Self::reload`] to do the initial connect. `artifacts` lets tool
    // adapters save image results (screenshots) for the UI.
    pub fn new(
        store:     Arc<McpServerStore>,
        provider:  Arc<dyn ModelProvider>,
        artifacts: Option<ArtifactStore>,
    ) -> Self {
        Self {
            state:    RwLock::new(RegistryState::default()),
            store:    Some(store),
            provider: Some(provider),
            artifacts,
            tools:    RwLock::new(None),
        }
    }

    // Wire the agent's tool registry so reloads can refresh the live MCP
    // tool surface.
    pub fn attach_tool_registry(&self, tools: Arc<ToolRegistry>) {
        if let Ok(mut g) = self.tools.write() {
            *g = Some(tools);
        }
    }

    // Reconnect every enabled server from the store, rebuild the per-user
    // filter + status snapshot, and replace the live MCP tool surface —
    // all without a restart. No-op when the store/provider aren't wired.
    // Called once at startup and after every MCP-server CRUD change.
    pub async fn reload(&self) {
        let (Some(store), Some(provider)) = (self.store.as_ref(), self.provider.as_ref()) else {
            return;
        };
        let new_state = Self::connect_state(store, Arc::clone(provider)).await;
        let adapters = build_adapters(&new_state, &self.artifacts);
        // Swap connected state first, then push the tool surface, so a
        // concurrent status read never sees tools that aren't in `state`.
        if let Ok(mut g) = self.state.write() {
            *g = new_state;
        }
        if let Some(tools) = self.tools.read().ok().and_then(|g| g.clone()) {
            tools.set_mcp_tools(adapters);
        }
        info!("mcp: registry reloaded ({} connected client(s))",
            self.state.read().map(|s| s.clients.len()).unwrap_or(0));
    }

    // Connect to every enabled row across every user, resolve
    // cross-user name collisions, then build the status snapshot
    // and per-user filter tables. Per-server failures are logged at
    // WARN and recorded in `statuses` rather than propagated — one
    // broken row must not block the rest of the gateway from
    // coming up.
    //     // `provider` is the gateway's primary provider chain — it
    // fulfils server-initiated sampling for any row that opted in
    // via `sampling_enabled`.  routes every sampling call
    // through this single provider regardless of owner; per-user
    // provider routing is a follow-up.
    async fn connect_state(
        store:    &McpServerStore,
        provider: Arc<dyn ModelProvider>,
    ) -> RegistryState {
        let rows = match store.list_all_enabled() {
            Ok(r)  => r,
            Err(e) => {
                warn!("mcp: list_all_enabled failed: {e}");
                return RegistryState::default();
            }
        };
        let all_rows = match store.list_all() {
            Ok(r)  => r,
            Err(_) => rows.clone(),
        };

        // ── Pass 1: connect each enabled row. ───────────────────────────
        // Track `(client_arc, owning_row)` so the collision pre-pass
        // below can iterate them with both pieces of context.
        let mut clients: Vec<Arc<McpClient>> = Vec::new();
        let mut owners:  Vec<McpServerRow>   = Vec::new();
        let mut statuses: Vec<McpServerStatus> = Vec::new();

        for row in &rows {
            let cfg = match row.to_config() {
                Ok(c)  => c,
                Err(e) => {
                    warn!("mcp: row '{}/{}': bad config: {e}", row.user_id, row.name);
                    statuses.push(status_error(row, e.to_string()));
                    continue;
                }
            };
            match McpClient::connect(&cfg, &row.user_id, std::sync::Arc::clone(&provider)).await {
                Ok(c) => {
                    let n = c.tools.len();
                    info!(
                        "mcp: connected '{}' for user '{}' ({} tool{}{}{})",
                        row.name, row.user_id, n,
                        if n == 1 { "" } else { "s" },
                        if c.supports_resources { " + resources" } else { "" },
                        if c.supports_prompts   { " + prompts" }   else { "" },
                    );
                    clients.push(Arc::new(c));
                    owners.push(row.clone());
                }
                Err(e) => {
                    warn!("mcp: '{}/{}' failed to connect: {e}", row.user_id, row.name);
                    statuses.push(status_error(row, e.to_string()));
                }
            }
        }

        // ── Pass 2: resolve cross-user name collisions. ─────────────────
        // First owner of each canonical `mcp__<server>__<tool>` keeps
        // the clean name. Every subsequent owner of the same canonical
        // name gets `__u<short_id>` appended. The result is that solo
        // and non-overlapping installs see exactly the original names,
        // and only the second-and-later owners of a colliding name
        // see the longer disambiguated form — visible in the UI and
        // documented in the McpPage.
        let mut taken: HashSet<String> = HashSet::new();
        let mut resolved: Vec<ResolvedAdapter> = Vec::new();
        let mut renamed_clients: HashSet<usize> = HashSet::new();
        let mut tools_by_user: HashMap<String, Vec<String>> = HashMap::new();
        let mut per_client_tools: Vec<Vec<McpToolInfo>> = vec![Vec::new(); clients.len()];

        for (ci, client) in clients.iter().enumerate() {
            let owner_id = &owners[ci].user_id;
            let short    = short_user_id(owner_id);
            let server   = &client.server_name;

            // Helper closure — pick the registry key for one tool
            // name on this client, suffixing on conflict and noting
            // the rename in `renamed_clients` so the status snapshot
            // can surface it.
            let mut pick = |kind: AdapterKind, canonical: String, tool_idx: Option<usize>| {
                let key = if taken.contains(&canonical) {
                    renamed_clients.insert(ci);
                    format!("{canonical}__u{short}")
                } else {
                    canonical.clone()
                };
                taken.insert(key.clone());
                resolved.push(ResolvedAdapter {
                    kind, qualified_name: key.clone(),
                    client_idx: ci, tool_meta_idx: tool_idx,
                });
                key
            };

            // Tools
            for (ti, meta) in client.tools.iter().enumerate() {
                let canonical = format!("mcp__{server}__{}", meta.name);
                let key = pick(AdapterKind::Tool, canonical, Some(ti));
                per_client_tools[ci].push(McpToolInfo {
                    name:        key.clone(),
                    description: meta.description.clone(),
                });
                tools_by_user.entry(owner_id.clone()).or_default().push(key);
            }

            // Resources adapter pair
            if client.supports_resources {
                let lk = pick(AdapterKind::ListResources, format!("mcp__{server}__list_resources"), None);
                per_client_tools[ci].push(McpToolInfo {
                    name:        lk.clone(),
                    description: "(MIRA-synthesised) list every resource this server exposes".into(),
                });
                tools_by_user.entry(owner_id.clone()).or_default().push(lk);

                let rk = pick(AdapterKind::ReadResource, format!("mcp__{server}__read_resource"), None);
                per_client_tools[ci].push(McpToolInfo {
                    name:        rk.clone(),
                    description: "(MIRA-synthesised) read one resource by URI".into(),
                });
                tools_by_user.entry(owner_id.clone()).or_default().push(rk);
            }

            // Prompts adapter pair
            if client.supports_prompts {
                let lk = pick(AdapterKind::ListPrompts, format!("mcp__{server}__list_prompts"), None);
                per_client_tools[ci].push(McpToolInfo {
                    name:        lk.clone(),
                    description: "(MIRA-synthesised) list every prompt template this server exposes".into(),
                });
                tools_by_user.entry(owner_id.clone()).or_default().push(lk);

                let gk = pick(AdapterKind::GetPrompt, format!("mcp__{server}__get_prompt"), None);
                per_client_tools[ci].push(McpToolInfo {
                    name:        gk.clone(),
                    description: "(MIRA-synthesised) render one prompt template by name".into(),
                });
                tools_by_user.entry(owner_id.clone()).or_default().push(gk);
            }
        }

        // ── Pass 3: build the status snapshot. ──────────────────────────
        for (ci, client) in clients.iter().enumerate() {
            let row = &owners[ci];
            let tools = std::mem::take(&mut per_client_tools[ci]);
            statuses.push(McpServerStatus {
                id:                row.id.clone(),
                owner_user_id:     row.user_id.clone(),
                name:              row.name.clone(),
                transport:         row.transport.clone(),
                enabled:           true,
                state:             "connected".into(),
                tool_count:        tools.len(),
                tools,
                supports_resources: client.supports_resources,
                supports_prompts:   client.supports_prompts,
                renamed_for_collision: renamed_clients.contains(&ci),
                sampling_enabled: row.to_config().ok()
                    .map(|c| c.sampling_enabled).unwrap_or(false),
                last_error: None,
            });
        }

        // Disabled rows — placeholder entries so the UI still shows them.
        for row in &all_rows {
            if !row.enabled && !statuses.iter().any(|s| s.id == row.id) {
                statuses.push(McpServerStatus {
                    id:             row.id.clone(),
                    owner_user_id:  row.user_id.clone(),
                    name:           row.name.clone(),
                    transport:      row.transport.clone(),
                    enabled:        false,
                    state:          "disabled".into(),
                    tool_count:     0,
                    tools:          Vec::new(),
                    supports_resources: false,
                    supports_prompts:   false,
                    renamed_for_collision: false,
                    sampling_enabled: false,
                    last_error: None,
                });
            }
        }

        RegistryState { clients, statuses, resolved, tools_by_user }
    }

    // Build the `TurnContext.allowed_tool_names` allow-list for a
    // given user: every non-MCP tool plus the (collision-resolved)
    // MCP tools that user owns.
    pub fn allowed_tools_for(&self, user_id: &str, all_tools: &[String]) -> Option<Vec<String>> {
        let any_mcp = all_tools.iter().any(|n| n.starts_with("mcp__"));
        if !any_mcp { return None; }

        let owned = self.state.read().ok()
            .and_then(|s| s.tools_by_user.get(user_id).cloned())
            .unwrap_or_default();
        let mut allowed: Vec<String> = all_tools.iter()
            .filter(|n| !n.starts_with("mcp__"))
            .cloned()
            .collect();
        allowed.extend(owned);
        Some(allowed)
    }

    // Status snapshot scoped to one user. Used by `/api/mcp/status`.
    pub fn status_for_user(&self, user_id: &str) -> Vec<McpServerStatus> {
        self.state.read().ok()
            .map(|s| s.statuses.iter()
                .filter(|st| st.owner_user_id == user_id)
                .cloned()
                .collect())
            .unwrap_or_default()
    }
}

// Build the MCP tool adapters from a connected state — the same set
// `register_tools_into` used to splat into the registry, now returned as
// owned `Arc`s so they can be handed to `ToolRegistry::set_mcp_tools`.
fn build_adapters(state: &RegistryState, artifacts: &Option<ArtifactStore>) -> Vec<Arc<dyn Tool>> {
    let mut out: Vec<Arc<dyn Tool>> = Vec::with_capacity(state.resolved.len());
    for r in &state.resolved {
        let client = Arc::clone(&state.clients[r.client_idx]);
        let adapter: Arc<dyn Tool> = match r.kind {
            AdapterKind::Tool => {
                let meta = client.tools[r.tool_meta_idx.expect("tool meta")].clone();
                Arc::new(McpToolAdapter::with_name(client, meta, r.qualified_name.clone(), artifacts.clone()))
            }
            AdapterKind::ListResources =>
                Arc::new(McpListResourcesTool::with_name(client, r.qualified_name.clone())),
            AdapterKind::ReadResource =>
                Arc::new(McpReadResourceTool::with_name(client, r.qualified_name.clone())),
            AdapterKind::ListPrompts =>
                Arc::new(McpListPromptsTool::with_name(client, r.qualified_name.clone())),
            AdapterKind::GetPrompt =>
                Arc::new(McpGetPromptTool::with_name(client, r.qualified_name.clone())),
        };
        out.push(adapter);
    }
    out
}

fn status_error(row: &McpServerRow, err: String) -> McpServerStatus {
    McpServerStatus {
        id:             row.id.clone(),
        owner_user_id:  row.user_id.clone(),
        name:           row.name.clone(),
        transport:      row.transport.clone(),
        enabled:        true,
        state:          "error".into(),
        tool_count:     0,
        tools:          Vec::new(),
        supports_resources: false,
        supports_prompts:   false,
        renamed_for_collision: false,
        sampling_enabled: false,
        last_error: Some(err),
    }
}

// First 8 hex chars of a UUID user_id, dashes stripped. Used as the
// collision-disambiguation suffix in `__u<short>`. Short enough to
// keep tool names readable; ~4 billion combinations is plenty for a
// single MIRA install's user count.
fn short_user_id(user_id: &str) -> String {
    user_id.chars().filter(|c| *c != '-').take(8).collect()
}
