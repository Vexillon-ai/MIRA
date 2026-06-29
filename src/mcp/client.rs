// SPDX-License-Identifier: AGPL-3.0-or-later

// src/mcp/client.rs
//! Wraps a single rmcp client connection plus the tool inventory we
//! discovered on its first handshake. The inventory is captured at
//! connect time and cached in [`McpClient::tools`] because the
//! [`crate::tools::Tool`] trait needs the JSON schema synchronously
//! from `args_schema()` — we can't re-fetch it on every adapter
//! construction.
//!
//! additions:
//! * The live rmcp service handle is held in a
//!   `tokio::sync::RwLock` so a tool-call failure can re-spawn the
//!   transport (stdio child crashed, HTTP endpoint blipped) and
//!   swap the handle without dropping the surrounding adapters.
//! * Per-server `supports_resources` is captured from the server's
//!   initialize response so the registry only synthesises the
//!   resource tools when the server actually offers them.
//! * `list_resources` / `read_resource` proxy the matching MCP
//!   methods, used by the `mcp__<server>__list_resources` and
//!   `mcp__<server>__read_resource` adapter pair.

use std::sync::Arc;

use rmcp::model::{CallToolRequestParams, GetPromptRequestParams, ReadResourceRequestParams};
use rmcp::service::{RoleClient, RunningService, ServiceExt};
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::warn;

use crate::MiraError;
use crate::config::McpServerConfig;
use crate::mcp::handler::McpClientHandler;
use crate::providers::ModelProvider;

// Metadata captured from the remote server's `tools/list` response.
// Only the fields the adapter actually needs are stored.
#[derive(Debug, Clone)]
pub struct McpToolMeta {
    // Remote tool name as the server reports it (no namespace prefix).
    pub name:        String,
    // Human description from the server, surfaced in `/api/tools`.
    pub description: String,
    // `inputSchema` from the server, kept verbatim — `args_schema()`
    // returns this so the model sees the same shape the server expects.
    pub input_schema: Value,
}

// One live MCP server connection plus its tool inventory. The
// rmcp service handle sits behind a `RwLock` so [`McpClient::reconnect`]
// can swap it without invalidating the surrounding `Arc<McpClient>`
// that every adapter holds.
pub struct McpClient {
    pub server_name: String,
    pub tools:       Vec<McpToolMeta>,
    // Whether the server's initialize response declared a `resources`
    // capability — gates registration of the resources adapter pair.
    pub supports_resources: bool,
    // Whether the server declared a `prompts` capability.
    // Gates registration of the `list_prompts` / `get_prompt`
    // adapter pair.
    pub supports_prompts: bool,
    // The original config we connected with, kept so reconnect can
    // rebuild the transport with the same parameters.
    pub config:      McpServerConfig,
    // Owning user — recorded at connect time so the handler can
    // attribute sampling calls in logs / audit. Also used by the
    // registry to drive the per-user `allowed_tool_names` filter.
    pub owner_user_id: String,
    // Provider routed to by server-initiated sampling. Cloned from
    // the gateway's primary provider once at connect time so the
    // handler doesn't have to re-resolve config each call.
    provider:        Arc<dyn ModelProvider>,
    service:         Arc<RwLock<RunningService<RoleClient, McpClientHandler>>>,
}

impl McpClient {
    // Spawn the configured child process (or open the HTTP
    // transport), run the MCP handshake, and snapshot the tool list.
    // The returned `McpClient` is ready for adapter construction.
    //     // Returns `Err` with the underlying error string on any failure
    // connection, handshake, or tools/list — so the registry can
    // surface it in the per-server status without taking down
    // startup.
    pub async fn connect(
        cfg:           &McpServerConfig,
        owner_user_id: &str,
        provider:      Arc<dyn ModelProvider>,
    ) -> Result<Self, MiraError> {
        let service = Self::open_service(cfg, owner_user_id, Arc::clone(&provider)).await?;

        let supports_resources = service.peer_info()
            .map(|info| info.capabilities.resources.is_some())
            .unwrap_or(false);
        let supports_prompts = service.peer_info()
            .map(|info| info.capabilities.prompts.is_some())
            .unwrap_or(false);

        // Snapshot the tool list. Empty cursor → first page; we don't
        // paginate in  (no public MCP server we've seen returns
        // pagination cursors for tools/list, and the rmcp client API
        // would yield all pages anyway in 's reconnect loop).
        let listed = service.list_tools(Default::default()).await
            .map_err(|e| MiraError::ConfigError(format!(
                "mcp server '{}': tools/list failed: {e}", cfg.name
            )))?;

        let tools = listed.tools.into_iter().map(|t| McpToolMeta {
            name:         t.name.to_string(),
            description:  t.description.map(|s| s.to_string()).unwrap_or_default(),
            // `input_schema` is `Arc<Map<String, Value>>` in rmcp; clone
            // into a plain serde_json::Value the Tool trait expects.
            input_schema: Value::Object((*t.input_schema).clone()),
        }).collect();

        Ok(Self {
            server_name:        cfg.name.clone(),
            tools,
            supports_resources,
            supports_prompts,
            config:             cfg.clone(),
            owner_user_id:      owner_user_id.to_owned(),
            provider,
            service:            Arc::new(RwLock::new(service)),
        })
    }

    // Re-run the transport handshake using the stored config and
    // atomically swap the live service handle. Only called by
    // `with_retry` after a tool-call failure; the inventory is
    // **not** refreshed (the original adapter set keeps the schema
    // it was registered with — see the  design note in
    // `src/mcp/mod.rs` about post-reconnect schema drift).
    async fn reconnect(&self) -> Result<(), MiraError> {
        let fresh = Self::open_service(
            &self.config,
            &self.owner_user_id,
            Arc::clone(&self.provider),
        ).await?;
        let mut slot = self.service.write().await;
        *slot = fresh;
        Ok(())
    }

    // Forward a tool call to the remote server. Used by
    // [`super::McpToolAdapter::execute`]. On a transport-level error
    // (process died, HTTP endpoint blipped), we attempt one
    // reconnect + retry — same pattern as a TCP client treating a
    // fresh `connect()` as the recovery action. A second failure
    // surfaces as the original error to the caller.
    pub async fn call_tool(&self, tool_name: &str, args: Value) -> Result<Value, MiraError> {
        // The MCP spec requires arguments to be a JSON object (or
        // absent). Anything else is the caller's bug — surface it
        // before crossing the wire.
        let args_obj = match args {
            Value::Object(m) => Some(m),
            Value::Null      => None,
            other => return Err(MiraError::ConfigError(format!(
                "mcp tool '{tool_name}': arguments must be a JSON object, got {}",
                type_name_of(&other),
            ))),
        };

        // `CallToolRequestParams` is `#[non_exhaustive]`, so build via
        // the `new` + builder helpers rather than a struct literal.
        let build_req = || {
            let mut req = CallToolRequestParams::new(tool_name.to_string());
            if let Some(obj) = args_obj.clone() {
                req = req.with_arguments(obj);
            }
            req
        };

        let first = {
            let svc = self.service.read().await;
            svc.call_tool(build_req()).await
        };
        let result = match first {
            Ok(r)  => r,
            Err(e) => {
                warn!("mcp '{}/{}': call failed ({e}); attempting reconnect", self.server_name, tool_name);
                if let Err(re) = self.reconnect().await {
                    return Err(MiraError::ProviderError(format!(
                        "mcp '{}/{}': call failed ({e}); reconnect also failed ({re})",
                        self.server_name, tool_name,
                    )));
                }
                let svc = self.service.read().await;
                svc.call_tool(build_req()).await
                    .map_err(|e2| MiraError::ProviderError(format!(
                        "mcp '{}/{}': call failed after reconnect: {e2}",
                        self.server_name, tool_name,
                    )))?
            }
        };

        Ok(serde_json::to_value(result).unwrap_or(Value::Null))
    }

    // `resources/list` — used by the `mcp__<server>__list_resources`
    // adapter. Same retry-once-on-failure pattern as `call_tool`.
    pub async fn list_resources(&self) -> Result<Value, MiraError> {
        let first = {
            let svc = self.service.read().await;
            svc.list_resources(Default::default()).await
        };
        let result = match first {
            Ok(r)  => r,
            Err(e) => {
                warn!("mcp '{}': resources/list failed ({e}); attempting reconnect", self.server_name);
                self.reconnect().await.map_err(|re| MiraError::ProviderError(format!(
                    "mcp '{}': resources/list failed ({e}); reconnect also failed ({re})",
                    self.server_name,
                )))?;
                let svc = self.service.read().await;
                svc.list_resources(Default::default()).await
                    .map_err(|e2| MiraError::ProviderError(format!(
                        "mcp '{}': resources/list failed after reconnect: {e2}",
                        self.server_name,
                    )))?
            }
        };
        Ok(serde_json::to_value(result).unwrap_or(Value::Null))
    }

    // `resources/read` — used by the `mcp__<server>__read_resource`
    // adapter. `uri` is the resource identifier from the server's
    // `resources/list` output (e.g. `file:///etc/hosts`,
    // `notion://page/abc123`).
    pub async fn read_resource(&self, uri: &str) -> Result<Value, MiraError> {
        let build = || ReadResourceRequestParams::new(uri.to_string());

        let first = {
            let svc = self.service.read().await;
            svc.read_resource(build()).await
        };
        let result = match first {
            Ok(r)  => r,
            Err(e) => {
                warn!("mcp '{}': resources/read({uri}) failed ({e}); attempting reconnect", self.server_name);
                self.reconnect().await.map_err(|re| MiraError::ProviderError(format!(
                    "mcp '{}': resources/read failed ({e}); reconnect also failed ({re})",
                    self.server_name,
                )))?;
                let svc = self.service.read().await;
                svc.read_resource(build()).await
                    .map_err(|e2| MiraError::ProviderError(format!(
                        "mcp '{}': resources/read failed after reconnect: {e2}",
                        self.server_name,
                    )))?
            }
        };
        Ok(serde_json::to_value(result).unwrap_or(Value::Null))
    }

    // `prompts/list` — used by the `mcp__<server>__list_prompts`
    // adapter. Same retry-once-on-failure pattern.
    pub async fn list_prompts(&self) -> Result<Value, MiraError> {
        let first = {
            let svc = self.service.read().await;
            svc.list_prompts(Default::default()).await
        };
        let result = match first {
            Ok(r)  => r,
            Err(e) => {
                warn!("mcp '{}': prompts/list failed ({e}); attempting reconnect", self.server_name);
                self.reconnect().await.map_err(|re| MiraError::ProviderError(format!(
                    "mcp '{}': prompts/list failed ({e}); reconnect also failed ({re})",
                    self.server_name,
                )))?;
                let svc = self.service.read().await;
                svc.list_prompts(Default::default()).await
                    .map_err(|e2| MiraError::ProviderError(format!(
                        "mcp '{}': prompts/list failed after reconnect: {e2}",
                        self.server_name,
                    )))?
            }
        };
        Ok(serde_json::to_value(result).unwrap_or(Value::Null))
    }

    // `prompts/get` — used by the `mcp__<server>__get_prompt`
    // adapter. `name` is a prompt id from list_prompts; `arguments`
    // is the optional template-variable object the server's prompt
    // schema declares.
    pub async fn get_prompt(&self, name: &str, arguments: Option<Value>)
        -> Result<Value, MiraError>
    {
        let args_obj = match arguments {
            Some(Value::Object(m)) => Some(m),
            None | Some(Value::Null) => None,
            Some(other) => return Err(MiraError::ConfigError(format!(
                "mcp prompt '{name}': arguments must be a JSON object, got {}",
                match other {
                    Value::Bool(_)   => "boolean",
                    Value::Number(_) => "number",
                    Value::String(_) => "string",
                    Value::Array(_)  => "array",
                    _ => "non-object",
                }
            ))),
        };
        let build = || {
            let mut req = GetPromptRequestParams::new(name.to_string());
            if let Some(obj) = args_obj.clone() { req = req.with_arguments(obj); }
            req
        };

        let first = {
            let svc = self.service.read().await;
            svc.get_prompt(build()).await
        };
        let result = match first {
            Ok(r)  => r,
            Err(e) => {
                warn!("mcp '{}': prompts/get({name}) failed ({e}); attempting reconnect", self.server_name);
                self.reconnect().await.map_err(|re| MiraError::ProviderError(format!(
                    "mcp '{}': prompts/get failed ({e}); reconnect also failed ({re})",
                    self.server_name,
                )))?;
                let svc = self.service.read().await;
                svc.get_prompt(build()).await
                    .map_err(|e2| MiraError::ProviderError(format!(
                        "mcp '{}': prompts/get failed after reconnect: {e2}",
                        self.server_name,
                    )))?
            }
        };
        Ok(serde_json::to_value(result).unwrap_or(Value::Null))
    }

    // Build a fresh rmcp service handle from `cfg`. Shared by the
    // initial `connect` and `reconnect`. The local handler is an
    // [`McpClientHandler`] now so the server can ask MIRA
    // to fulfil `sampling/createMessage` when the row was configured
    // with `sampling_enabled = true`.
    async fn open_service(
        cfg:           &McpServerConfig,
        owner_user_id: &str,
        provider:      Arc<dyn ModelProvider>,
    ) -> Result<RunningService<RoleClient, McpClientHandler>, MiraError>
    {
        let handler = McpClientHandler {
            server_name:      cfg.name.clone(),
            owner_user_id:    owner_user_id.to_owned(),
            sampling_enabled: cfg.sampling_enabled,
            provider,
        };
        match cfg.transport.as_str() {
            "stdio" => {
                let cmd_str = cfg.command.as_deref().ok_or_else(|| MiraError::ConfigError(
                    format!("mcp server '{}': stdio transport requires `command`", cfg.name)
                ))?;
                // Prefer a MIRA-managed runtime: if the command is a bare
                // `npx`/`uvx` and we've installed the bundled Node/uv, resolve
                // it to that absolute launcher — deterministic, and it works on
                // a Windows service (LocalSystem) PATH that can't see a user
                // Node install. Falls back to the configured command otherwise.
                let resolved = crate::install::deps::resolve_mcp_command(cmd_str);
                let runtime_dirs = crate::install::deps::managed_runtime_bin_dirs();
                let args = cfg.args.clone();
                let mut env  = cfg.env.clone();

                // Puppeteer browser server: point it at MIRA's managed Chrome
                // (provisioned under ~/.mira/deps/puppeteer) so browser
                // automation works on a Windows service where Puppeteer's own
                // self-download lands in an unreadable cache. A user-set value
                // always wins. If Chrome isn't provisioned yet the registry
                // kicks a background download + reconnect (see registry.rs); the
                // server still spawns now (cache dir set), gaining the executable
                // path on the reconnect.
                if crate::mcp::browser::server_uses_puppeteer(cfg.command.as_deref(), &args, &env) {
                    if let Some(dir) = crate::mcp::browser::cache_dir() {
                        env.entry("PUPPETEER_CACHE_DIR".to_string())
                            .or_insert_with(|| dir.to_string_lossy().into_owned());
                    }
                    if let Some(chrome) = crate::mcp::browser::chrome_path() {
                        env.entry("PUPPETEER_EXECUTABLE_PATH".to_string())
                            .or_insert_with(|| chrome.to_string_lossy().into_owned());
                    }
                }

                // On Windows the common MCP launchers (`npx`, `uvx`, `pnpm`,
                // `yarn`, `bunx`) are `.cmd`/`.bat` shims, which `CreateProcess`
                // can't execute directly — `Command::new("npx")` fails with
                // "program not found". Route a non-`.exe` command through
                // `cmd /C` so PATHEXT resolution finds the shim; a real `.exe`
                // launches directly. The MCP args are appended after the command
                // by the `.configure` closure below either way. Unix unchanged.
                #[cfg(windows)]
                let command = if resolved.to_ascii_lowercase().ends_with(".exe") {
                    Command::new(&resolved)
                } else {
                    let mut c = Command::new("cmd");
                    c.arg("/C").arg(&resolved);
                    c
                };
                #[cfg(not(windows))]
                let command = Command::new(&resolved);

                let child_transport = TokioChildProcess::new(command.configure(move |cmd| {
                    cmd.args(&args);
                    for (k, v) in &env {
                        cmd.env(k, v);
                    }
                    // Prepend managed runtime bin dirs to PATH so `npx` can find
                    // `node` (and uvx resolves) regardless of the service PATH.
                    // Built on the MCP server's own PATH override if it set one,
                    // else the process PATH. Runs last so it's the final PATH.
                    if !runtime_dirs.is_empty() {
                        let base = env.get("PATH").map(std::ffi::OsString::from)
                            .unwrap_or_else(|| std::env::var_os("PATH").unwrap_or_default());
                        let mut paths = runtime_dirs.clone();
                        paths.extend(std::env::split_paths(&base));
                        if let Ok(joined) = std::env::join_paths(paths) {
                            cmd.env("PATH", joined);
                        }
                    }
                }))
                .map_err(|e| MiraError::ConfigError(format!(
                    "mcp server '{}': spawn failed: {e}", cfg.name
                )))?;

                handler.serve(child_transport).await
                    .map_err(|e| MiraError::ConfigError(format!(
                        "mcp server '{}': handshake failed: {e}", cfg.name
                    )))
            }
            "http" => {
                let url = cfg.url.as_deref().ok_or_else(|| MiraError::ConfigError(
                    format!("mcp server '{}': http transport requires `url`", cfg.name)
                ))?;
                let transport = StreamableHttpClientTransport::from_uri(url.to_string());
                handler.serve(transport).await
                    .map_err(|e| MiraError::ConfigError(format!(
                        "mcp server '{}': http handshake failed: {e}", cfg.name
                    )))
            }
            other => Err(MiraError::ConfigError(format!(
                "mcp server '{}': unknown transport {other:?} \
                 (expected \"stdio\" or \"http\")", cfg.name
            ))),
        }
    }
}

// Best-effort JSON value type name for error messages. Kept local
// because none of the upstream error chains want to pull in this
// dependency-free helper.
fn type_name_of(v: &Value) -> &'static str {
    match v {
        Value::Null      => "null",
        Value::Bool(_)   => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_)  => "array",
        Value::Object(_) => "object",
    }
}
