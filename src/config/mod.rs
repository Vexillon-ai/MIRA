// SPDX-License-Identifier: AGPL-3.0-or-later

// src/config/mod.rs
//! MIRA configuration — loading, validation, saving, and first-run setup.
//!
//! # Config file location (platform-appropriate, under `~/.mira/`)
//!
//! | Platform      | Path                                        |
//! |---------------|---------------------------------------------|
//! | Linux / macOS | `~/.mira/config/mira_config.json`           |
//! | Windows       | `%USERPROFILE%\.mira\config\mira_config.json` |
//!
//! On first run the directory and file are created automatically. An example
//! template (`mira_config.example.json`) is also written there for reference.
//!
//! # Config file format
//! Plain JSON (no comments). A JSON Schema lives at
//! `config/mira_config.schema.json` (repo root) and is also embedded in the
//! binary. The schema is validated at every startup.
//!
//! # Override
//! Pass `--config <path>` on the command line to use a different file.

pub mod migrate;
pub mod schema;
pub mod validate;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::info;

use crate::MiraError;
use migrate::{find_legacy_toml, prompt_and_migrate};
use schema::EXAMPLE_JSONC;
use validate::validate_config_json;

// ── Top-level config ─────────────────────────────────────────────────────────

// Root MIRA configuration struct.
// // Deserialised from `mira_config.json` and validated against the embedded
// JSON Schema on every startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiraConfig {
    // Runtime-only: path from which this config was loaded. Not serialised.
    #[serde(skip)]
    pub config_path: PathBuf,

    // Schema version — must be `"1"`. Managed by MIRA.
    #[serde(default = "default_config_version")]
    pub config_version: String,

    // Root data directory (SQLite DBs, history, exports). Supports `~`.
    #[serde(default = "default_data_dir")]
    pub data_dir: String,

    // Active AI provider: `"ollama"` | `"lmstudio"` | `"openrouter"`.
    #[serde(default = "default_primary_provider")]
    pub primary_provider: String,

    // Ordered list of provider slugs used as AUTOMATIC failover after the
    // primary — presence = enabled for fallback, order = priority. `None`
    // (default) is **fail-closed local-only**: only local providers
    // (lmstudio, ollama, a loopback/LAN openai_compat) receive conversations
    // automatically when the primary fails; cloud providers never do. Cloud
    // providers remain available for EXPLICIT model selection regardless of
    // this list — it governs only the silent auto-failover chain, so a local
    // "heart" can't leak the family's chats off-box on a crash/timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failover_providers: Option<Vec<String>>,

    #[serde(default)]
    pub providers: ProvidersConfig,

    #[serde(default)]
    pub cli: CliConfig,

    #[serde(default)]
    pub tui: TuiConfig,

    #[serde(default)]
    pub server: ServerConfig,

    #[serde(default)]
    pub channels: ChannelsConfig,

    #[serde(default)]
    pub logging: LoggingConfig,

    #[serde(default)]
    pub memory: MemoryConfig,

    #[serde(default)]
    pub session: SessionConfig,

    #[serde(default)]
    pub agent: AgentConfig,

    #[serde(default)]
    pub guardian: GuardianConfig,

    #[serde(default)]
    pub security: SecurityPolicyConfig,

    #[serde(default)]
    pub proxy: ProxyConfig,

    #[serde(default)]
    pub calendar: CalendarConfig,

    #[serde(default)]
    pub sandbox: SandboxConfig,

    #[serde(default)]
    pub tts: TtsConfig,

    #[serde(default)]
    pub stt: SttConfig,

    #[serde(default)]
    pub automations: AutomationsConfig,

    // 0.269.0 — companion-mode (proactive check-in) tuning.
    #[serde(default)]
    pub companion: CompanionConfig,

    // 0.111.0 — where subagent-spawned task deliverables land.
    // Default `~/mira-artifacts/`. Each task gets a per-skill subdir
    // with a slug + task_id name.
    #[serde(default)]
    pub artifacts: ArtifactsConfig,

    // 0.115.0 — per-user wiki (markdown knowledge base) settings.
    // Companion to memory: stores narrative pages on disk that the
    // agent reads into context. See `design-docs/wiki-feature.md`.
    #[serde(default)]
    pub wiki: WikiConfig,

    // 0.154.x (Q2 #7) — MCP host registry. External Model Context
    // Protocol servers MIRA should connect to at startup; each
    // server's tools are surfaced as native MIRA tools under the
    // `mcp__<server>__<tool>` namespace. Empty by default.
    #[serde(default)]
    pub mcp: McpConfig,

    // 0.163.x (Q2 #8 E4) — OAuth client configuration for the email
    // channel. Operator brings their own Google + Microsoft OAuth
    // apps (one-time setup at each provider's developer console);
    // MIRA only needs the public `client_id`s and a publicly-
    // reachable callback URL. PKCE-only flow — no client secrets
    // stored or expected. See design-docs/email-channel.md for the
    // step-by-step provider-side setup.
    #[serde(default)]
    pub email_oauth: EmailOAuthConfig,

    // SSO / OIDC web login. Off by default; when a provider is
    // configured + enabled, the web login page shows a "Sign in with
    // …" button per provider. See src/auth/oidc.rs + docs.
    #[serde(default)]
    pub auth: AuthConfig,

    // 0.164.x (Q2 #8 E5) — system email account. Application-
    // initiated mail (password reset, admin alerts, waitlist
    // confirmations, etc.) goes through this SMTP config rather
    // than borrowing a user's per-user email account. Disabled by
    // default; only matters once a feature pulls it in.
    #[serde(default)]
    pub system_email: SystemEmailConfig,

    // Backup behaviour — on-demand download/upload (always on, Q1.5
    // since 0.148.1) plus optional scheduled rotation (this section).
    // All off-by-default; see `BackupConfig`.
    #[serde(default)]
    pub backup: BackupConfig,

    // 0.282.0 — proactive-notification transports. Web Push (VAPID) is
    // always available; this section adds opt-in Firebase Cloud Messaging
    // for the native mobile app. Off by default — with `fcm.enabled=false`
    // the server behaves exactly as before.
    #[serde(default)]
    pub notifications: NotificationsConfig,

    // 0.284.0 — built-in weather. Defaults to keyless Open-Meteo (global, no
    // setup); an admin can switch to a keyed provider for richer data.
    #[serde(default)]
    pub weather: WeatherConfig,

    // 0.292.0 — image generation backends (OpenAI, local Automatic1111 / SD
    // WebUI, local ComfyUI). The `image_generate` tool dispatches through this.
    #[serde(default)]
    pub image: ImageConfig,

    // 0.292.0 — video generation (OpenAI Videos / Sora). The `video_generate`
    // tool reads its defaults from here; key/endpoint come from providers.openai.
    #[serde(default)]
    pub video: VideoConfig,
}

// Backup runtime knobs. The on-demand `GET /api/admin/backup` and the
// restore endpoint are always available regardless; this only gates
// the *scheduled* nightly snapshot loop and its retention/rotation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupConfig {
    // Run a scheduled snapshot loop in the background. Off by default.
    // When on, writes `<data_dir>/backups/mira-backup-…tar.gz` every
    // `scheduled_interval_secs` and keeps `scheduled_retention_count`
    // most-recent files.
    #[serde(default)]
    pub scheduled_enabled: bool,
    // Interval between scheduled snapshots, in seconds. Minimum 60
    // (anything lower is treated as 60). Default 86400 = once a day.
    #[serde(default = "default_backup_interval_secs")]
    pub scheduled_interval_secs: u64,
    // How many scheduled snapshots to retain on disk. Older snapshots
    // are pruned after each write. 0 = keep all (use with care).
    // Default 7.
    #[serde(default = "default_backup_retention_count")]
    pub scheduled_retention_count: u32,
}

fn default_backup_interval_secs() -> u64 { 86_400 }
fn default_backup_retention_count() -> u32 { 7 }

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            scheduled_enabled:        false,
            scheduled_interval_secs:  default_backup_interval_secs(),
            scheduled_retention_count: default_backup_retention_count(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactsConfig {
    #[serde(default = "default_artifacts_root")]
    pub root_dir: String,
}

impl Default for ArtifactsConfig {
    fn default() -> Self { Self { root_dir: default_artifacts_root() } }
}

fn default_artifacts_root() -> String { "~/mira-artifacts".to_string() }

// ── Wiki config ──────────────────────────────────────────────────────────────

// Per-user wiki feature settings. Disabled = no scaffolding, no context
// injection, no auto-extraction. Enabled-by-default; downstream
// behaviour is controlled by [`WikiAutoExtractConfig`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub auto_extract: WikiAutoExtractConfig,
    #[serde(default)]
    pub agent_tools: WikiAgentToolsConfig,
    #[serde(default)]
    pub git: WikiGitConfig,
    #[serde(default)]
    pub mcp: WikiMcpConfig,
}

impl Default for WikiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_extract: WikiAutoExtractConfig::default(),
            agent_tools: WikiAgentToolsConfig::default(),
            git: WikiGitConfig::default(),
            mcp: WikiMcpConfig::default(),
        }
    }
}

// Git-backed durability for the wiki (Slice G).
// // When `enabled`, the wiki gateway initialises `<wiki_root>/.git` on
// startup (idempotent). With `auto_commit = true`, every successful
// op is followed by a `git commit` carrying a one-line message
// derived from the op kind + target. Push / pull are manual via the
// HTTP API or web UI — we deliberately don't auto-push so the user
// stays in control of when their wiki leaves the machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiGitConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub auto_commit: bool,
}

impl Default for WikiGitConfig {
    fn default() -> Self { Self { enabled: true, auto_commit: true } }
}

// MCP (Model Context Protocol) server settings (Slice G).
// // When `enabled`, the `mira wiki mcp-serve --user-id <id>` CLI starts
// a stdio JSON-RPC server that exposes the named user's wiki pages
// as MCP resources, suitable for connection by Claude Desktop and
// other MCP clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiMcpConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for WikiMcpConfig {
    fn default() -> Self { Self { enabled: true } }
}

// Controls the model-callable `wiki` skill (Slice D).
// // Reads (search, read) are always available when `enabled = true`; the
// `write_mode` field gates the three write tools (append_section,
// write_page, log_entry):
// - `"review"` (default) — agent writes submit as `pending` to the audit DB;
// user approves them on the Wiki page (Slice E). Mirrors the auto-extract
// default so the model and the extractor have the same safety posture.
// - `"auto"` — agent writes apply immediately. Use when you trust the
// model to mutate the wiki without supervision.
// - `"off"` — write tools are not registered. The model sees the read
// tools but cannot mutate the wiki.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiAgentToolsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_wiki_agent_write_mode")]
    pub write_mode: String,
}

impl Default for WikiAgentToolsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            write_mode: default_wiki_agent_write_mode(),
        }
    }
}

fn default_wiki_agent_write_mode() -> String { "review".to_string() }

// How the post-turn wiki extractor behaves.
// // `mode`:
// - `"review"` (default) — extracted ops land in the audit DB with
// status=pending; user approves/edits/rejects before they're applied.
// This is the ChatGPT-memory-lessons mitigation against silent writes.
// - `"auto"` — ops are applied immediately. Trusts the extractor.
// - `"off"` — extractor never runs. Wiki context injection still happens
// over whatever content the user has authored manually.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiAutoExtractConfig {
    #[serde(default = "default_wiki_extract_mode")]
    pub mode: String,
    #[serde(default = "default_wiki_min_confidence")]
    pub min_confidence: f32,
    #[serde(default = "default_wiki_max_ops_per_turn")]
    pub max_ops_per_turn: usize,
    /// In `mode="review"`, ops with extractor confidence at or above this
    /// threshold are applied immediately (skip the review queue); the rest
    /// still land as pending. `None` (default) preserves the old behaviour —
    /// every extracted op waits for review. Ignored when `mode` is `"auto"`
    /// (everything applies) or `"off"` (nothing extracts).
    #[serde(default)]
    pub auto_apply_above: Option<f32>,
}

impl Default for WikiAutoExtractConfig {
    fn default() -> Self {
        Self {
            mode: default_wiki_extract_mode(),
            min_confidence: default_wiki_min_confidence(),
            max_ops_per_turn: default_wiki_max_ops_per_turn(),
            auto_apply_above: None,
        }
    }
}

fn default_wiki_extract_mode() -> String { "review".to_string() }
fn default_wiki_min_confidence() -> f32 { 0.6 }
fn default_wiki_max_ops_per_turn() -> usize { 3 }

// ── Provider configs ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub ollama: OllamaConfig,

    #[serde(default)]
    pub lmstudio: LmStudioConfig,

    #[serde(default)]
    pub openrouter: OpenRouterConfig,

    // ── OpenAI-compatible cloud providers ────────────────────────────
    // All share the same wire shape (`/v1/chat/completions`) and only
    // differ in base URL + default model. Wired through the shared
    // `providers::openai_compat::OpenAiCompatClient`. Each provider is
    // registered in the chain only when its `api_key` is set so an
    // empty config block is a no-op.
    #[serde(default)]
    pub openai: OpenAiConfig,

    #[serde(default)]
    pub deepseek: DeepSeekConfig,

    // Moonshot AI (Kimi).
    #[serde(default)]
    pub moonshot: MoonshotConfig,

    #[serde(default)]
    pub groq: GroqConfig,

    // xAI (Grok).
    #[serde(default)]
    pub xai: XaiConfig,

    // Catch-all for any other OpenAI-compatible gateway (Together,
    // Fireworks, Perplexity, Mistral, DeepInfra, vLLM, LocalAI,
    // Azure OpenAI, …). The user supplies `base_url` and a
    // distinguishing `name` so logs and the `ProviderId` slug are
    // readable; pick `auth_style = "azure"` for Azure deployments,
    // otherwise leave it `"bearer"` (the default).
    #[serde(default)]
    pub openai_compat: OpenAiCompatProviderConfig,

    // Anthropic native (Claude). Uses /v1/messages, not OpenAI's
    // /v1/chat/completions, so it has its own dedicated client. Set
    // `api_key` to register; default model is the most-capable Claude
    // at time of writing — override with claude-haiku-* for cheap/fast
    // or claude-opus-* for top-tier reasoning.
    #[serde(default)]
    pub anthropic: AnthropicConfig,

    // Google Gemini native. Uses :generateContent (not OpenAI-shaped),
    // with role:"user"/"model", systemInstruction at top level, and
    // functionCall / functionResponse parts. Set `api_key` to register.
    // Default model is the latest top-tier; gemini-2.5-flash for
    // faster/cheaper.
    #[serde(default)]
    pub gemini: GeminiConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OllamaConfig {
    // When false, the provider is omitted from the failover chain at
    // startup AND from the chat UI's model dropdown. Defaults to true
    // so existing configs (which predate this field) continue to
    // behave as they did before — the gate still falls on whether
    // the underlying URL / api_key is actually reachable.
    #[serde(default = "default_true")]
    pub enabled: bool,

    #[serde(default = "default_ollama_url")]
    pub url: String,

    #[serde(default = "default_ollama_model")]
    pub default_model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<String>,

    #[serde(default = "default_ollama_timeout")]
    pub timeout_secs: u64,
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            enabled:       true,
            url:           default_ollama_url(),
            default_model: default_ollama_model(),
            available_models: Vec::new(),
            timeout_secs:  default_ollama_timeout(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LmStudioConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    #[serde(default = "default_lmstudio_url")]
    pub url: String,

    #[serde(default = "default_lmstudio_model")]
    pub default_model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<String>,

    #[serde(default = "default_lmstudio_timeout")]
    pub timeout_secs: u64,
}

impl Default for LmStudioConfig {
    fn default() -> Self {
        Self {
            enabled:       true,
            url:           default_lmstudio_url(),
            default_model: default_lmstudio_model(),
            available_models: Vec::new(),
            timeout_secs:  default_lmstudio_timeout(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenRouterConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,

    #[serde(default = "default_openrouter_url")]
    pub base_url: String,

    #[serde(default = "default_openrouter_model")]
    pub default_model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<String>,

    // How often the on-disk OpenRouter model catalog cache may be served
    // without re-fetching. Cache lives at
    // `<data_dir>/cache/openrouter-models.json`. `0` means always re-fetch.
    #[serde(default = "default_openrouter_catalog_refresh_hours")]
    pub catalog_refresh_hours: u64,
}

impl Default for OpenRouterConfig {
    fn default() -> Self {
        Self {
            enabled:               true,
            api_key:               None,
            base_url:              default_openrouter_url(),
            default_model:         default_openrouter_model(),
            available_models: Vec::new(),
            catalog_refresh_hours: default_openrouter_catalog_refresh_hours(),
        }
    }
}

// ── OpenAI-compatible cloud provider configs ────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default = "default_openai_url")]
    pub base_url: String,
    #[serde(default = "default_openai_model")]
    pub default_model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<String>,
    #[serde(default = "default_provider_timeout")]
    pub timeout_secs: u64,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            enabled:       true,
            api_key:       None,
            base_url:      default_openai_url(),
            default_model: default_openai_model(),
            available_models: Vec::new(),
            timeout_secs:  default_provider_timeout(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeepSeekConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default = "default_deepseek_url")]
    pub base_url: String,
    #[serde(default = "default_deepseek_model")]
    pub default_model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<String>,
    #[serde(default = "default_provider_timeout")]
    pub timeout_secs: u64,
}

impl Default for DeepSeekConfig {
    fn default() -> Self {
        Self {
            enabled:       true,
            api_key:       None,
            base_url:      default_deepseek_url(),
            default_model: default_deepseek_model(),
            available_models: Vec::new(),
            timeout_secs:  default_provider_timeout(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoonshotConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default = "default_moonshot_url")]
    pub base_url: String,
    #[serde(default = "default_moonshot_model")]
    pub default_model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<String>,
    #[serde(default = "default_provider_timeout")]
    pub timeout_secs: u64,
}

impl Default for MoonshotConfig {
    fn default() -> Self {
        Self {
            enabled:       true,
            api_key:       None,
            base_url:      default_moonshot_url(),
            default_model: default_moonshot_model(),
            available_models: Vec::new(),
            timeout_secs:  default_provider_timeout(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroqConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default = "default_groq_url")]
    pub base_url: String,
    #[serde(default = "default_groq_model")]
    pub default_model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<String>,
    #[serde(default = "default_provider_timeout")]
    pub timeout_secs: u64,
}

impl Default for GroqConfig {
    fn default() -> Self {
        Self {
            enabled:       true,
            api_key:       None,
            base_url:      default_groq_url(),
            default_model: default_groq_model(),
            available_models: Vec::new(),
            timeout_secs:  default_provider_timeout(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XaiConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default = "default_xai_url")]
    pub base_url: String,
    #[serde(default = "default_xai_model")]
    pub default_model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<String>,
    #[serde(default = "default_provider_timeout")]
    pub timeout_secs: u64,
}

impl Default for XaiConfig {
    fn default() -> Self {
        Self {
            enabled:       true,
            api_key:       None,
            base_url:      default_xai_url(),
            default_model: default_xai_model(),
            available_models: Vec::new(),
            timeout_secs:  default_provider_timeout(),
        }
    }
}

// Catch-all for OpenAI-compatible gateways that don't have a dedicated
// config block above. The user picks a `name` slug used in logs and
// `ProviderId` (e.g. `"together"`, `"fireworks"`, `"perplexity"`,
// `"azure"`, `"local_vllm"`), sets the `base_url`, key, and model,
// and the gateway registers the provider when `api_key` is set OR the
// `auth_style` is `"none"` (anonymous self-hosted endpoints).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiCompatProviderConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    // Slug used for logs + `ProviderId`. Empty = the catch-all is
    // disabled (the registration step skips it).
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub default_model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<String>,
    #[serde(default = "default_provider_timeout")]
    pub timeout_secs: u64,
    // `"bearer"` (default), `"azure"` (api-key header), or `"none"`
    // (no auth header — for unsecured self-hosted endpoints).
    #[serde(default = "default_auth_style")]
    pub auth_style: String,
}

impl Default for OpenAiCompatProviderConfig {
    fn default() -> Self {
        Self {
            enabled:       true,
            name:          String::new(),
            api_key:       None,
            base_url:      String::new(),
            default_model: String::new(),
            available_models: Vec::new(),
            timeout_secs:  default_provider_timeout(),
            auth_style:    default_auth_style(),
        }
    }
}

// ── Defaults ────────────────────────────────────────────────────────────────

fn default_provider_timeout() -> u64 { 120 }
fn default_auth_style()       -> String { "bearer".into() }

fn default_openai_url()       -> String { "https://api.openai.com/v1".into() }
fn default_openai_model()     -> String { "gpt-4o-mini".into() }

fn default_deepseek_url()     -> String { "https://api.deepseek.com/v1".into() }
fn default_deepseek_model()   -> String { "deepseek-chat".into() }

fn default_moonshot_url()     -> String { "https://api.moonshot.ai/v1".into() }
fn default_moonshot_model()   -> String { "kimi-k2-0905-preview".into() }

fn default_groq_url()         -> String { "https://api.groq.com/openai/v1".into() }
fn default_groq_model()       -> String { "llama-3.3-70b-versatile".into() }

fn default_xai_url()          -> String { "https://api.x.ai/v1".into() }
fn default_xai_model()        -> String { "grok-4".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default = "default_anthropic_url")]
    pub base_url: String,
    #[serde(default = "default_anthropic_model")]
    pub default_model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<String>,
    #[serde(default = "default_provider_timeout")]
    pub timeout_secs: u64,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            enabled:       true,
            api_key:       None,
            base_url:      default_anthropic_url(),
            default_model: default_anthropic_model(),
            available_models: Vec::new(),
            timeout_secs:  default_provider_timeout(),
        }
    }
}

fn default_anthropic_url()    -> String { "https://api.anthropic.com".into() }
fn default_anthropic_model()  -> String { "claude-sonnet-4-5".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default = "default_gemini_url")]
    pub base_url: String,
    #[serde(default = "default_gemini_model")]
    pub default_model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<String>,
    #[serde(default = "default_provider_timeout")]
    pub timeout_secs: u64,
}

impl Default for GeminiConfig {
    fn default() -> Self {
        Self {
            enabled:       true,
            api_key:       None,
            base_url:      default_gemini_url(),
            default_model: default_gemini_model(),
            available_models: Vec::new(),
            timeout_secs:  default_provider_timeout(),
        }
    }
}

fn default_gemini_url()       -> String { "https://generativelanguage.googleapis.com".into() }
fn default_gemini_model()     -> String { "gemini-2.5-pro".into() }

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliConfig {
    #[serde(default = "default_true")]
    pub colored_output: bool,

    #[serde(default = "default_true")]
    pub streaming: bool,

    #[serde(default = "default_prompt")]
    pub prompt: String,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            colored_output: true,
            streaming:      true,
            prompt:         default_prompt(),
        }
    }
}

// ── TUI ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiConfig {
    #[serde(default = "default_tui_theme")]
    pub theme: String,

    #[serde(default = "default_tui_layout")]
    pub layout: String,

    #[serde(default = "default_true")]
    pub show_timestamps: bool,

    #[serde(default = "default_true")]
    pub show_token_count: bool,

    // Backend the TUI talks to: `"auto"` | `"local"` | `"server"`.
    // `auto` picks `server` when `server.enabled` is true and the URL is
    // reachable, else `local`. 
    #[serde(default = "default_tui_mode")]
    pub mode: String,

    // Base URL of the MIRA HTTP server for server-mode TUI. Used when
    // `mode=server` or `mode=auto` with `server.enabled=true`.
    #[serde(default = "default_tui_server_url")]
    pub server_url: String,

    // Path to the local bearer token the server mints for same-host TUI use.
    // Tilde-expanded. Ignored for remote TUI (where `MIRA_TOKEN` env is used).
    #[serde(default = "default_tui_token_path")]
    pub auto_token_path: String,

    // When true, on startup the TUI loads the tail of the most recent `tui`
    // conversation and continues it instead of starting empty. 
    #[serde(default)]
    pub resume_last: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            theme:            default_tui_theme(),
            layout:           default_tui_layout(),
            show_timestamps:  true,
            show_token_count: true,
            mode:             default_tui_mode(),
            server_url:       default_tui_server_url(),
            auto_token_path:  default_tui_token_path(),
            resume_last:      false,
        }
    }
}

// ── Server ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default = "default_server_host")]
    pub host: String,

    #[serde(default = "default_server_port")]
    pub port: u16,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_cert_path: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_key_path: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,

    #[serde(default = "default_max_connections")]
    pub max_connections: u32,

    #[serde(default = "default_request_timeout")]
    pub request_timeout_secs: u32,

    #[serde(default)]
    pub allowed_origins: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_secret: Option<String>,

    // Human-readable label for this instance ("Tarek's MIRA"), shown by the
    // mobile app and returned in the device-pairing payload + /api/status.
    // None → falls back to the hostname.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,

    // Canonical public base URL (scheme+host[+port]) the outside world — phones,
    // pairing QR codes — should use to reach this instance. None → derive from
    // the incoming request. Set this when behind a reverse proxy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_base_url: Option<String>,

    // Externally-reachable base URL for REMOTE access (away from the LAN) —
    // e.g. a Tailscale MagicDNS name (https://mira.my-tailnet.ts.net) or a
    // Cloudflare Tunnel / DDNS hostname. Distinct from `public_base_url`
    // (the LAN/current address): this is the "away" endpoint embedded as
    // `remote_url` in the pairing QR so the app can auto-select it when the
    // LAN one is unreachable. None → fall back to Tailscale auto-detection,
    // then omit. Must be an absolute http/https URL when set.
    // Also settable via the `MIRA_REMOTE_URL` env var.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,

    // periodic check against a Releases API for a newer
    // MIRA version. Renders a banner in the admin web UI when one
    // is available. Off by default ("skippable per-version, off by
    // default behind a config flag for paranoid users", per the
    // install-and-supervisor design).
    #[serde(default)]
    pub update_check: UpdateCheckConfig,

    // Serving of coding-agent-built web apps (a completed task's
    // output/index.html) at an isolated per-app origin, so "open the
    // game you built" returns a real clickable link instead of the
    // model confabulating a browser-open it cannot perform.
    #[serde(default)]
    pub web_apps: WebAppsConfig,
}

/// Serve web apps/games that MIRA's coding agent builds. Each app is served
/// at `http://<task_id>.<host_suffix>:<port>/`, a distinct browser origin from
/// the MIRA app itself — so a model-built app cannot read MIRA's session token
/// or call its authenticated API. `task_id` (a high-entropy UUIDv7) is the
/// unguessable capability; only requests whose `Host` is `<label>.<host_suffix>`
/// are treated as app requests, everything else routes to MIRA normally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebAppsConfig {
    /// Master switch. When off, no app is served and the URL helper still
    /// resolves (so the tool can explain the app exists on disk).
    #[serde(default = "default_web_apps_enabled")]
    pub enabled: bool,

    /// How built apps are exposed — a security/reachability trade-off the
    /// deployer picks (the server can't auto-detect how a browser will reach
    /// it, nor fall back at runtime — it hands back one URL and never sees
    /// whether the client's connection succeeded):
    /// - `subdomain` (default): `<task_id>.<host_suffix>:<port>` — a distinct
    ///   origin per app (isolates cookies AND localStorage), no extra port.
    ///   Works when the browser resolves the suffix to the box MIRA runs on
    ///   (same machine, or WSL reached via `localhost`).
    /// - `port`: a separate listener (`web_apps.port`) at `/a/<task_id>/`.
    ///   Reachable over any host incl. a LAN / WSL-gateway IP, at the cost of
    ///   weaker isolation (all apps share one origin; port is not a cookie
    ///   boundary on the same host).
    /// - `both`: serve via both; the subdomain is the primary link, the port
    ///   URL an alternate.
    #[serde(default = "default_web_apps_mode")]
    pub mode: String,

    /// Host suffix for the per-app subdomain origin (`subdomain`/`both`).
    /// `localhost` resolves to loopback natively in every major browser
    /// (RFC 6761) — origin-isolated, no extra port. Only works when the
    /// browser reaches MIRA's box via that name (same machine, or WSL via
    /// `localhost`).
    #[serde(default = "default_web_app_host_suffix")]
    pub host_suffix: String,

    /// Listener port for `port`/`both` mode. `0` means `server.port + 1`.
    #[serde(default)]
    pub port: u16,

    /// Host clients use to reach the `port`-mode listener — used only to build
    /// the returned URL (e.g. a LAN or WSL-gateway IP like `198.51.100.10`).
    /// `None` derives it from `server.public_base_url`, then `server.host`
    /// (when concrete), then `localhost`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertised_host: Option<String>,
}

impl Default for WebAppsConfig {
    fn default() -> Self {
        Self {
            enabled:         default_web_apps_enabled(),
            mode:            default_web_apps_mode(),
            host_suffix:     default_web_app_host_suffix(),
            port:            0,
            advertised_host: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateCheckConfig {
    // Passive version check against the Releases API. ON by default — it's a
    // single lightweight request that only COMPARES versions; it never
    // downloads or installs anything (upgrading is always an explicit action,
    // via the Settings "Upgrade now" button or `mira upgrade`). Set false to
    // stop MIRA contacting the release host at all.
    #[serde(default = "default_true")]
    pub enabled: bool,
    // Releases API URL. Default points at the public MIRA GitHub releases
    // endpoint; forks should override with their own GitHub / GitLab Releases
    // endpoint. Empty string disables (same as `enabled: false`).
    #[serde(default = "default_update_check_url")]
    pub source_url: String,
    // How often the server refreshes its cached check result: "daily" |
    // "weekly" | "monthly". The UI's "Check now" always forces an immediate
    // refresh regardless of this.
    #[serde(default = "default_update_check_frequency")]
    pub frequency: String,
}

fn default_update_check_url() -> String {
    "https://api.github.com/repos/Vexillon-ai/MIRA/releases".to_string()
}

fn default_update_check_frequency() -> String {
    "daily".to_string()
}

impl UpdateCheckConfig {
    /// The cache-refresh interval implied by `frequency`. Unknown values fall
    /// back to daily.
    pub fn refresh_interval(&self) -> std::time::Duration {
        match self.frequency.trim().to_ascii_lowercase().as_str() {
            "weekly"  => std::time::Duration::from_secs(7  * 86_400),
            "monthly" => std::time::Duration::from_secs(30 * 86_400),
            _         => std::time::Duration::from_secs(86_400),
        }
    }
}

impl Default for UpdateCheckConfig {
    fn default() -> Self {
        Self {
            enabled:    true,
            source_url: default_update_check_url(),
            frequency:  default_update_check_frequency(),
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            enabled:              false,
            host:                 default_server_host(),
            port:                 default_server_port(),
            tls_cert_path:        None,
            tls_key_path:         None,
            auth_token:           None,
            max_connections:      default_max_connections(),
            request_timeout_secs: default_request_timeout(),
            allowed_origins:      vec![],
            webhook_secret:       None,
            display_name:         None,
            public_base_url:      None,
            remote_url:           None,
            update_check:         UpdateCheckConfig::default(),
            web_apps:             WebAppsConfig::default(),
        }
    }
}

// ── Channels ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChannelsConfig {
    #[serde(default)]
    pub signal: SignalConfig,

    #[serde(default)]
    pub telegram: TelegramConfig,

    #[serde(default)]
    pub discord: DiscordConfig,

    #[serde(default)]
    pub matrix: MatrixConfig,

    #[serde(default)]
    pub whatsapp: WhatsAppConfig,

    #[serde(default)]
    pub slack: SlackConfig,

    #[serde(default)]
    pub external: ExternalConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone_number: Option<String>,

    #[serde(default = "default_signal_port")]
    pub rest_port: u16,

    #[serde(default = "default_signal_socket")]
    pub socket_path: String,

    #[serde(default = "default_signal_binary")]
    pub cli_binary: String,

    #[serde(default = "default_signal_data_dir")]
    pub data_dir: String,

    // HMAC-SHA256 key used to verify the `X-Signal-Signature` header on incoming
    // webhook requests from signal-cli. None = signature verification disabled
    // (warn at startup).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hmac_key: Option<String>,
}

impl Default for SignalConfig {
    fn default() -> Self {
        Self {
            enabled:      false,
            phone_number: None,
            rest_port:    default_signal_port(),
            socket_path:  default_signal_socket(),
            cli_binary:   default_signal_binary(),
            data_dir:     default_signal_data_dir(),
            hmac_key:     None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,

    #[serde(default)]
    pub polling: bool,
    // `secret_token` was removed in 0.152.x — the per-account
    // `secret_token` on each ChannelAccount row is the only enforced
    // value (verified inline by `telegram_handler`). The schema still
    // accepts the old field on existing configs so they keep loading;
    // serde drops it on the next save.
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled:     false,
            bot_token:   None,
            webhook_url: None,
            polling:     false,
        }
    }
}

// Discord channel global config. Per-bot credentials live on the
// `channel_accounts` row (each MIRA user registers their own Discord
// application). This block only holds the MIRA-wide kill switch that
// the gateway connection and outbound dispatchers honour at request
// time — flipping it to `false` halts every Discord account without
// having to disable each row individually.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiscordConfig {
    #[serde(default)]
    pub enabled: bool,
}

// Matrix channel kill switch. Per-account credentials (homeserver +
// access token) live on each `channel_accounts` row; this block only
// holds the MIRA-wide on/off the sync loop + outbound dispatchers honour.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MatrixConfig {
    #[serde(default)]
    pub enabled: bool,
}

// WhatsApp channel kill switch. Per-account credentials (phone-number id +
// tokens) live on each `channel_accounts` row; this block holds the
// MIRA-wide on/off the webhook handler + outbound dispatchers honour.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WhatsAppConfig {
    #[serde(default)]
    pub enabled: bool,
}

// Slack channel kill switch. Per-account credentials (bot token + signing
// secret) live on each `channel_accounts` row; this block holds the
// MIRA-wide on/off the webhook handler + outbound dispatchers honour.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SlackConfig {
    #[serde(default)]
    pub enabled: bool,
}

// External (CPP plugin channel) kill switch. Per-account config (provider
// kind, send_url, secrets) lives on each `channel_accounts` row; this
// block holds the MIRA-wide on/off the `/webhook/external/{id}` handler +
// outbound dispatchers honour.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExternalConfig {
    #[serde(default)]
    pub enabled: bool,
}

// ── Logging ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,

    #[serde(default = "default_log_format")]
    pub format: String,

    #[serde(default = "default_log_file")]
    pub file: String,

    #[serde(default = "default_log_max_size")]
    pub max_file_size_mb: u32,

    #[serde(default = "default_log_max_files")]
    pub max_files: u32,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level:            default_log_level(),
            format:           default_log_format(),
            file:             default_log_file(),
            max_file_size_mb: default_log_max_size(),
            max_files:        default_log_max_files(),
        }
    }
}

// ── Memory ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_vector_backend")]
    pub vector_backend: String,

    #[serde(default)]
    pub embedding: EmbeddingConfig,

    #[serde(default = "default_embedding_dim")]
    pub embedding_dim: usize,

    #[serde(default = "default_embedding_cache_size")]
    pub embedding_cache_size: usize,

    #[serde(default = "default_similarity_threshold")]
    pub similarity_threshold: f32,

    // How many memories the per-turn context hook retrieves and injects.
    // Higher helps aggregation/"how many"/multi-session questions (which need
    // many facts at once) at the cost of a larger prompt.
    #[serde(default = "default_context_top_k")]
    pub context_top_k: usize,

    #[serde(default = "default_true")]
    pub per_user_isolation: bool,

    #[serde(default = "default_true")]
    pub share_across_channels: bool,

    #[serde(default = "default_qdrant_url")]
    pub qdrant_url: String,

    #[serde(default)]
    pub auto_extract: AutoExtractConfig,

    #[serde(default)]
    pub indexer: IndexerConfig,

    #[serde(default)]
    pub rollup: RollupConfig,

    #[serde(default)]
    pub graph: GraphConfig,

    #[serde(default)]
    pub consolidation: ConsolidationConfig,

    #[serde(default)]
    pub recency: RecencyConfig,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            vector_backend:       default_vector_backend(),
            embedding:            EmbeddingConfig::default(),
            embedding_dim:        default_embedding_dim(),
            embedding_cache_size: default_embedding_cache_size(),
            similarity_threshold: default_similarity_threshold(),
            context_top_k:        default_context_top_k(),
            per_user_isolation:   true,
            share_across_channels:true,
            qdrant_url:           default_qdrant_url(),
            auto_extract:         AutoExtractConfig::default(),
            indexer:              IndexerConfig::default(),
            rollup:               RollupConfig::default(),
            graph:                GraphConfig::default(),
            consolidation:        ConsolidationConfig::default(),
            recency:              RecencyConfig::default(),
        }
    }
}

// Recency tuning for semantic recall. Retrieval blends similarity with an
// age-based freshness boost so recently-formed memories can surface ahead of
// older but frequently-reinforced ones: `score' = (1-weight)·similarity +
// weight·2^(-age_days / half_life_days)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecencyConfig {
    // Weight of the recency term in `[0.0, 1.0]`. `0.0` = pure similarity
    // (the pre-0.244 behaviour); higher values favour fresher memories.
    #[serde(default = "default_recency_weight")]
    pub weight: f32,
    // Half-life in days: a memory this old contributes half the recency boost
    // of a brand-new one. Larger = recency decays more slowly.
    #[serde(default = "default_recency_half_life_days")]
    pub half_life_days: f32,
}

impl Default for RecencyConfig {
    fn default() -> Self {
        Self {
            weight:         default_recency_weight(),
            half_life_days: default_recency_half_life_days(),
        }
    }
}

fn default_recency_weight() -> f32 { 0.25 }
fn default_recency_half_life_days() -> f32 { 30.0 }

// Companion-mode tuning. Companion is otherwise configured per-user (enable /
// quiet hours / briefing in the companion settings DB); this holds the global
// scheduler knobs that aren't per-user yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompanionConfig {
    // Pause proactive check-ins after this many go unanswered in a row. The
    // counter resets on any user message, so check-ins resume automatically
    // when the user replies. `0` disables the cap (check-ins are then bounded
    // only by min-gap, daily cap, and quiet hours). Default 3 — stops
    // talking into the void without going silent on an engaged user.
    #[serde(default = "default_max_unanswered_checkins")]
    pub max_unanswered_checkins: u32,

    // Maximum proactive check-ins per user-local day (hard ceiling, separate
    // from the unanswered cap). Default 6.
    #[serde(default = "default_checkin_max_per_day")]
    pub max_per_day: u32,

    // Minimum minutes between consecutive check-ins (frequency floor).
    // Default 90.
    #[serde(default = "default_checkin_min_gap_minutes")]
    pub min_gap_minutes: i64,
}

impl Default for CompanionConfig {
    fn default() -> Self {
        Self {
            max_unanswered_checkins: default_max_unanswered_checkins(),
            max_per_day:             default_checkin_max_per_day(),
            min_gap_minutes:         default_checkin_min_gap_minutes(),
        }
    }
}

fn default_max_unanswered_checkins() -> u32 { 3 }
fn default_checkin_max_per_day() -> u32 { 6 }
fn default_checkin_min_gap_minutes() -> i64 { 90 }

// MIRA-Guardian — the built-in, code-defined system watchdog agent. Its
// identity (prompt + tools) is immutable and lives in the binary; this config
// only controls whether it runs and at what authority. See
// `design-docs/guardian-agent.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardianConfig {
    // `"off"` — disabled (default; opt-in). `"monitor"` — observe + alert
    // only, no actions. `"active"` — monitor plus gated/isolation remediation
    // actions. Identity is fixed regardless; only authority changes.
    #[serde(default = "default_guardian_mode")]
    pub mode: String,

    // How often (seconds) the proactive watch loop checks the latest health
    // snapshot and, on a *new* non-green state, fires a Guardian alert. Only
    // active when `mode != "off"`. Default 900 (15 min).
    #[serde(default = "default_guardian_watch_interval")]
    pub watch_interval_secs: u64,

    // Isolation autonomy (§4.5) dry-run. When `true` (default) the Guardian,
    // on detecting it cannot reach you (a configured channel's delivery
    // failed) and that a bounded fix is warranted, only **logs + audits what
    // it would do** — it does not execute. Flip to `false` to permit real
    // autonomous remediation under isolation. Only relevant in `active` mode.
    #[serde(default = "default_true")]
    pub isolation_dry_run: bool,

    // Grace period (seconds) after a failed approval delivery before the
    // Guardian may act autonomously — a window for any web-side decision.
    // Default 180 (3 min). Only relevant when `isolation_dry_run = false`.
    #[serde(default = "default_guardian_isolation_grace")]
    pub isolation_grace_secs: u64,

    // The Ollama-registry model the provisioning flow (P2b) pulls + binds the
    // Guardian to when no local provider is already configured — so a fresh
    // install can run the Guardian without manually setting up an LLM. A small,
    // reliable-tool-calling default; override to taste.
    #[serde(default = "default_guardian_provision_model")]
    pub provision_model: String,

    // ── Tiered model (design-docs/guardian-scope.md §6) ──────────────────────
    // The Guardian runs a *tiered* local model: a light always-on model for
    // routine ticks (low-severity notes), escalating to a stronger model only
    // for real triage (a Red detector). Each tier is (provider, model); when a
    // tier's provider is unset/empty it falls back to the `guardian` llm-alias,
    // then the primary provider — so existing installs (alias only) are
    // unchanged. Both tiers are still subject to the fail-closed local-only
    // `model_check` (cloud is refused). Provider must be a local one
    // (`lmstudio`/`ollama`).
    //
    // Routine tier — light model for low-severity ticks. Empty = use the
    // `guardian` alias / primary.
    #[serde(default)]
    pub routine_provider: Option<String>,
    #[serde(default)]
    pub routine_model: Option<String>,
    // Triage tier — stronger model reached only when a detector goes Red.
    // Empty = use the `guardian` alias / primary.
    #[serde(default)]
    pub triage_provider: Option<String>,
    #[serde(default)]
    pub triage_model: Option<String>,

    // ── Separate-process independence (design-docs/guardian-separate-process.md) ─
    // The out-of-process liveness sentinel (`mira guardian-watch`): a sibling
    // process that watches whether MIRA itself is alive and raises a DIRECT
    // alarm if MIRA goes down — the one failure the co-resident watch can't
    // catch (it shares MIRA's fate). Off by default.
    #[serde(default)]
    pub process: GuardianProcessConfig,
}

impl Default for GuardianConfig {
    fn default() -> Self {
        Self {
            mode: default_guardian_mode(),
            watch_interval_secs: default_guardian_watch_interval(),
            isolation_dry_run: true,
            isolation_grace_secs: default_guardian_isolation_grace(),
            provision_model: default_guardian_provision_model(),
            routine_provider: None,
            routine_model: None,
            triage_provider: None,
            triage_model: None,
            process: GuardianProcessConfig::default(),
        }
    }
}

fn default_guardian_mode() -> String { "off".to_owned() }
fn default_guardian_watch_interval() -> u64 { 900 }
fn default_guardian_isolation_grace() -> u64 { 180 }
fn default_guardian_provision_model() -> String { "qwen2.5:3b-instruct".to_owned() }

/// The out-of-process Guardian liveness sentinel (`mira guardian-watch`). A
/// separate supervised process that probes MIRA's `/health` and, if MIRA is
/// unreachable for a sustained window, delivers a DIRECT web-push alarm to the
/// household (cold from the shared data dir — no dependency on the down MIRA).
/// Observe-and-alarm only in this increment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardianProcessConfig {
    // Master switch for the sentinel process. Off by default; the sentinel is a
    // separate service the operator enables + supervises.
    #[serde(default)]
    pub enabled: bool,
    // How often (seconds) to probe MIRA's liveness. Default 30; minimum 5.
    #[serde(default = "default_sentinel_probe_interval")]
    pub probe_interval_secs: u64,
    // Consecutive failed probes before declaring MIRA down + alarming. Default 3
    // (so a normal restart, which recovers within one window, doesn't alarm).
    #[serde(default = "default_sentinel_down_after")]
    pub down_after_failures: u32,
    // Explicit liveness URL to probe. Empty/absent = derive
    // `http://127.0.0.1:<server.port>/health` (the unauthenticated readiness
    // route). Override for a non-default bind / reverse-proxy.
    #[serde(default)]
    pub probe_url: Option<String>,
    // The user id whose registered push devices receive the "MIRA is down"
    // alarm. Empty/absent = no push target (the sentinel still logs + audits);
    // set it to the household admin so the phone actually buzzes.
    #[serde(default)]
    pub notify_user_id: Option<String>,

    // When true, the out-of-process sentinel OWNS health watch + triage: it
    // also triages non-green health snapshots while MIRA is up (surfacing
    // through MIRA), and MIRA's co-resident watch loop stands down (no-ops its
    // health triage) so the two don't double-alert. Default false = the
    // co-resident loop still owns health triage; the sentinel only watches
    // liveness. Requires `enabled = true`.
    #[serde(default)]
    pub owns_watch: bool,

    // Separate log file for the out-of-process sentinel. Empty/absent = the
    // sentinel shares MIRA's main log file (`logging.file`) — the default, so
    // both processes' lines land together. Set an explicit path to keep the
    // sentinel's logs in their own file (easier to read, especially when MIRA
    // is down). `~` is expanded.
    #[serde(default)]
    pub log_file: Option<String>,
}

impl Default for GuardianProcessConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            probe_interval_secs: default_sentinel_probe_interval(),
            down_after_failures: default_sentinel_down_after(),
            probe_url: None,
            notify_user_id: None,
            owns_watch: false,
            log_file: None,
        }
    }
}

fn default_sentinel_probe_interval() -> u64 { 30 }
fn default_sentinel_down_after() -> u32 { 3 }

// Temporal knowledge-graph memory (see `design-docs/graph-memory.md`). Additive and
// **off by default**: when enabled, the post-turn extractor also writes typed,
// timestamped triples to `kg_entities`/`kg_edges` so aggregation/counting
// questions resolve against exact set membership instead of fuzzy top-k.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphConfig {
    // Master switch. Off = no graph extraction or retrieval (flat memory only).
    #[serde(default)]
    pub enabled: bool,
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self { enabled: false }
    }
}

// Sleep-like consolidation (see `design-docs/memory-research-2026.md` §5). Phased
// nightly passes over the graph that clean up duplicates, resolve
// contradictions, and score importance — all deterministic, all MIRA-side
// (no LLM-as-policy). Each phase is independently togglable so a phase that
// hurts in your environment can be turned off without disabling the others.
// LongMemEval can't validate this work (its haystacks don't exercise the
// contradictions/sprawl consolidation is for); ship and observe in production.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationConfig {
    // Phase C — resolve single-valued-predicate contradictions (`works_at`,
    // `lives_in`, `married_to`, …): keep the newest, close older edges via
    // `valid_to`. Pure SQL + a curated predicate list. Off by default.
    #[serde(default)]
    pub contradictions_enabled: bool,

    // Phase A — merge near-duplicate entities within the same `entity_type`
    // via strict-token-subset + size-ratio rule (e.g. "navy blazer" / "navy
    // blue blazer"). Re-points edges, rolls alias, marks superseded.
    // Pure SQL, no LLM. Off by default.
    #[serde(default)]
    pub entity_dedup_enabled: bool,

    // Size-ratio threshold for the Phase A merge rule (smaller / larger token
    // counts). 0.6 catches `{navy, blazer} ⊂ {navy, blue, blazer}` (2/3) and
    // rejects `{plant} ⊂ {peace, lily, plant}` (1/3). Range 0.0–1.0.
    #[serde(default = "default_entity_dedup_ratio")]
    pub entity_dedup_ratio: f64,

    // Phase D — nightly importance scoring on graph edges. When on, scores
    // every live edge as `ln(1 + access_count) × exp(-age_days / half_life)`
    // and stores it in `kg_edges.importance`. Retrieval already sorts by
    // `importance DESC` (no-op until Phase D writes non-zero scores), so
    // enabling this biases context toward frequently-reinforced + recent
    // facts. Off by default.
    #[serde(default)]
    pub importance_enabled: bool,

    // Half-life (in days) for the Phase D decay term. After this many days
    // of no reinforcement, an edge's score decays to ~50% of its un-decayed
    // strength. 30 default — month-scale half-life matches typical personal
    // context drift. Range 1.0–365.0.
    #[serde(default = "default_importance_half_life_days")]
    pub importance_half_life_days: f64,
}

fn default_entity_dedup_ratio() -> f64 { 0.6 }
fn default_importance_half_life_days() -> f64 { 30.0 }

impl Default for ConsolidationConfig {
    fn default() -> Self {
        Self {
            contradictions_enabled:   false,
            entity_dedup_enabled:     false,
            entity_dedup_ratio:       default_entity_dedup_ratio(),
            importance_enabled:       false,
            importance_half_life_days: default_importance_half_life_days(),
        }
    }
}

// Background transcript indexer config. The indexer embeds historical
// chat messages into `message_vectors` so the (future) `recall_history`
// tool can semantic-search the transcript. Defaults match the sizing
// assumptions in `src/history/indexer.rs` — keep them in sync when tuning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexerConfig {
    // Enable the background indexer. Off disables transcript semantic
    // search entirely (the table still exists; it's just never populated).
    #[serde(default = "default_indexer_enabled")]
    pub enabled: bool,

    // Seconds between idle polls. Busy passes run much faster — this
    // interval only applies when the previous batch found zero rows.
    #[serde(default = "default_indexer_interval_secs")]
    pub interval_secs: u64,

    // Max messages processed per pass. Higher values backfill faster on
    // first run but tie up the embedding provider for longer stretches.
    #[serde(default = "default_indexer_batch_size")]
    pub batch_size: u32,

    // Roles the indexer skips — usually `["tool", "system"]`. The `user`
    // and `assistant` roles should virtually never appear here.
    #[serde(default = "default_indexer_skip_roles")]
    pub skip_roles: Vec<String>,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            enabled:       default_indexer_enabled(),
            interval_secs: default_indexer_interval_secs(),
            batch_size:    default_indexer_batch_size(),
            skip_roles:    default_indexer_skip_roles(),
        }
    }
}

fn default_indexer_enabled() -> bool { true }
fn default_indexer_interval_secs() -> u64 { 60 }
fn default_indexer_batch_size() -> u32 { 32 }
fn default_indexer_skip_roles() -> Vec<String> {
    vec!["tool".to_owned(), "system".to_owned()]
}

// Daily memory rollup config. The rollup job periodically summarises each
// active user's previous UTC day into one memory, so patterns that only
// emerge across a whole day (projects, moods, recurring topics) aren't
// lost when per-turn extraction misses them. Off by default — it costs
// one extra LLM call per user per day and users should opt in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollupConfig {
    // Enable the rollup job. Off disables daily consolidation entirely.
    #[serde(default = "default_rollup_enabled")]
    pub enabled: bool,

    // Seconds between polls. The loop wakes this often, figures out the
    // target day (today - `day_lag_days` in UTC), and runs rollups for
    // any user whose summary for that day is still missing. One hour is
    // a sensible default: it closes a late-day server restart's gap
    // quickly, and the work is idempotent so repeats cost one DB check.
    #[serde(default = "default_rollup_interval_secs")]
    pub interval_secs: u64,

    // How many UTC days back to summarise. `1` = yesterday, the safe
    // default — today is still happening, so summarising it would be
    // premature. Bump this to `0` only if you want mid-day partials.
    #[serde(default = "default_rollup_day_lag_days")]
    pub day_lag_days: u64,

    // Hard cap on messages fed to one summarizer call. Oldest-first
    // truncation keeps the prompt bounded on heavy days.
    #[serde(default = "default_rollup_max_messages")]
    pub max_messages: u32,

    // Per-message character cap before concatenation. Long pastes
    // rarely help a day summary; truncating keeps the prompt short.
    #[serde(default = "default_rollup_max_chars_per_message")]
    pub max_chars_per_message: u32,
}

impl Default for RollupConfig {
    fn default() -> Self {
        Self {
            enabled:               default_rollup_enabled(),
            interval_secs:         default_rollup_interval_secs(),
            day_lag_days:          default_rollup_day_lag_days(),
            max_messages:          default_rollup_max_messages(),
            max_chars_per_message: default_rollup_max_chars_per_message(),
        }
    }
}

fn default_rollup_enabled() -> bool { false }
fn default_rollup_interval_secs() -> u64 { 3600 }
fn default_rollup_day_lag_days() -> u64 { 1 }
fn default_rollup_max_messages() -> u32 { 200 }
fn default_rollup_max_chars_per_message() -> u32 { 800 }

// Post-turn memory extraction config.
// // Extraction is the decision to *proactively* persist facts from a
// conversation without the user explicitly asking. It's useful but
// privacy-sensitive, so defaults are conservative: heuristic-only, medium
// confidence floor, and only the factual/utility categories. Relationships
// and health-adjacent content are NOT extracted by default — a user who
// wants that behaviour has to opt in explicitly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoExtractConfig {
    // `"off"` — never extract. `"heuristic"` — regex-based extractor only
    // (the historical default). `"llm"` — run a structured extraction pass
    // through a model after each turn; writes go through the same storage
    // path but with conflict detection via `supersede`.
    #[serde(default = "default_auto_extract_mode")]
    pub mode: String,

    // Minimum confidence tier required to persist an LLM-extracted candidate.
    // `"low"` | `"medium"` | `"high"` (applies only when `mode = "llm"`).
    #[serde(default = "default_auto_extract_min_confidence")]
    pub min_confidence: String,

    // Which memory categories are eligible for LLM extraction. Restricting
    // this is the main privacy knob — `"relationship"` in particular can
    // involve third parties who haven't consented and is off by default.
    #[serde(default = "default_auto_extract_allowed_categories")]
    pub allowed_categories: Vec<String>,

    // Channels that should use the richer **LLM** extractor instead of the
    // cheap heuristic one — independent of `mode` (as long as `mode` isn't
    // `"off"`). Names match the channel id a turn arrives on: `"web"`,
    // `"telegram"`, `"signal"`, `"discord"`, `"slack"`, `"matrix"`,
    // `"whatsapp"`, `"email"`. Empty (the default) means only `mode` decides:
    // `"llm"` runs the LLM extractor everywhere, otherwise the heuristic runs.
    // Use this to turn the LLM extractor on for, say, just Telegram while
    // keeping everything else heuristic.
    #[serde(default = "default_auto_extract_llm_channels")]
    pub llm_channels: Vec<String>,
}

// Which memory extractor a turn runs after completing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractorKind {
    // No auto-extraction this turn.
    Off,
    // Regex/pattern-based extractor (cheap, no model call).
    Heuristic,
    // Structured extraction via a model call (richer, conflict-aware).
    Llm,
}

impl AutoExtractConfig {
    // Resolve which extractor `channel` should run. `mode = "off"` disables
    // all extraction. Otherwise a channel listed in `llm_channels` — or every
    // channel when `mode = "llm"` — uses the LLM extractor; all others fall
    // back to the heuristic.
    pub fn effective_extractor(&self, channel: &str) -> ExtractorKind {
        if self.mode.eq_ignore_ascii_case("off") {
            return ExtractorKind::Off;
        }
        if self.mode.eq_ignore_ascii_case("llm")
            || self.llm_channels.iter().any(|c| c.eq_ignore_ascii_case(channel))
        {
            return ExtractorKind::Llm;
        }
        ExtractorKind::Heuristic
    }
}

impl Default for AutoExtractConfig {
    fn default() -> Self {
        Self {
            mode:               default_auto_extract_mode(),
            min_confidence:     default_auto_extract_min_confidence(),
            allowed_categories: default_auto_extract_allowed_categories(),
            llm_channels:       default_auto_extract_llm_channels(),
        }
    }
}

fn default_auto_extract_mode() -> String { "heuristic".to_owned() }
fn default_auto_extract_llm_channels() -> Vec<String> { Vec::new() }
fn default_auto_extract_min_confidence() -> String { "medium".to_owned() }
fn default_auto_extract_allowed_categories() -> Vec<String> {
    vec![
        "fact".to_owned(),
        "preference".to_owned(),
        "skill".to_owned(),
        "project".to_owned(),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    // `"internal"` | `"ollama"` | `"lmstudio"` | `"openai"` | `"openrouter"`
    #[serde(default = "default_embedding_provider")]
    pub provider: String,

    #[serde(default = "default_embedding_model")]
    pub model: String,

    // Base URL for external embedding servers. `None` uses the provider default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_url: Option<String>,

    // Local cache directory for models downloaded by the `internal` provider.
    #[serde(default = "default_model_cache_dir")]
    pub model_cache_dir: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            provider:        default_embedding_provider(),
            model:           default_embedding_model(),
            provider_url:    None, // derived at runtime from the active provider config
            model_cache_dir: default_model_cache_dir(),
            api_key:         None,
        }
    }
}

// ── Agent ─────────────────────────────────────────────────────────────────────

// Reasoning-model auto-routing (roadmap #13). Opt-in. When `enabled`, a
// per-turn router decides whether the turn is "hard" and, if so, routes it to
// the `provider` slot (which the operator points at their strong reasoning
// model) instead of the default provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningConfig {
    // Master switch. Off by default.
    #[serde(default)]
    pub enabled: bool,
    // Provider id (as in `providers.*`, matched like `primary_provider`) to
    // route hard turns to. Its configured model should be your strong
    // reasoning model (e.g. an `anthropic` provider on Claude Opus, or an
    // o-series `openai_compat`). Empty → routing stays inert even if enabled.
    #[serde(default)]
    pub provider: String,
    // Input length (chars) at/above which a turn is treated as a routing
    // signal on its own. Tunable; pairs with the content heuristics.
    #[serde(default = "default_reasoning_min_chars")]
    pub min_chars: usize,
    // Reasoning effort applied to the routed-to provider for hard turns:
    // `"low"` | `"medium"` | `"high"`. Sent as OpenAI `reasoning_effort`;
    // mapped to an Anthropic `thinking` token budget. Default `"medium"`.
    #[serde(default = "default_reasoning_effort")]
    pub effort: String,
    // Provider id for the cheap classifier consulted on *ambiguous* turns
    // (the hybrid router's fallback — Slice C). Should be a small/fast model.
    // Empty → use the default provider for classification.
    #[serde(default)]
    pub classifier_provider: String,
}

impl Default for ReasoningConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: String::new(),
            min_chars: default_reasoning_min_chars(),
            effort: default_reasoning_effort(),
            classifier_provider: String::new(),
        }
    }
}

fn default_reasoning_min_chars() -> usize { 600 }
fn default_reasoning_effort() -> String { "medium".to_string() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    // Maximum number of tool-call/observe rounds before giving up.
    #[serde(default = "default_max_tool_rounds")]
    pub max_tool_rounds: usize,

    // Tool-calling protocol: `"auto"` | `"openai"` | `"react"` | `"disabled"`.
    // `"auto"` tries OpenAI structured tool_calls first, falls back to ReAct.
    #[serde(default = "default_tool_mode")]
    pub tool_mode: String,

    // Path to `agent.md` persona file. Empty string → built-in default prompt.
    #[serde(default)]
    pub system_prompt_file: String,

    // Suppress model "thinking" by appending the `/no_think` directive to the
    // system prompt. Reasoning models (the qwen3 family, etc.) otherwise burn
    // the per-round token budget on chain-of-thought before acting, which
    // stalls MIRA's tool loops. Off by default; flip on when the active model
    // is a reasoning model. The web chat can override this per-conversation.
    #[serde(default)]
    pub disable_reasoning: bool,

    // Playful "easter eggs" personality layer. When on, MIRA recognises famous
    // pop-culture references and playful prompts (mirror-mirror, "open the pod
    // bay doors", "meaning of life", magic-8-ball "should I…", "marco", etc.)
    // and plays along — improvised, in the user's own tone, scaled by their
    // playfulness — without hijacking genuine requests. LLM-driven (no canned
    // strings). On by default; a low-playfulness persona only gets subtle winks.
    #[serde(default = "default_true")]
    pub playful_easter_eggs: bool,

    // Maximum history turns kept in context per session (1 turn = user + assistant).
    #[serde(default = "default_max_context_turns")]
    pub max_context_turns: usize,

    // Phase-1 token-aware context budgeting (design-docs/context-compaction.md).
    // The model's context window in tokens. `0` (default) keeps the legacy
    // fixed `max_context_turns` window (no behaviour change). When set (e.g.
    // 128000), MIRA fills the window by token budget instead — carrying far
    // more history when it fits — reserving room for the response + margin.
    // Set this to your primary model's real context length.
    #[serde(default)]
    pub context_length_tokens: usize,

    // Tokens held back from the context budget as headroom (only used when
    // `context_length_tokens > 0`). Guards against token-estimate drift so a
    // packed prompt doesn't overflow the model.
    #[serde(default = "default_context_safety_margin")]
    pub context_safety_margin_tokens: usize,

    // Phase-0 prompt caching: when true, keep the system-prompt PREFIX
    // byte-stable turn-to-turn by moving per-turn retrieved context (memory +
    // wiki) OUT of the system prompt and folding it into the current user
    // message. A stable prefix lets providers (and local backends' KV cache)
    // reuse it — ~90% cheaper/faster on cloud, free speedup locally. Default
    // false (unchanged prompt shape) while validating; flip on to enable.
    #[serde(default)]
    pub prompt_cache_enabled: bool,

    // Phase-2 auto-compaction. When token budgeting (`context_length_tokens`)
    // is on and the oldest turns overflow the window, compact them into a
    // rolling anchored summary instead of dropping them. Inert unless token
    // budgeting is enabled, so the default is behaviour-preserving.
    #[serde(default)]
    pub compaction: CompactionConfig,

    #[serde(default)]
    pub tools: ToolsConfig,

    // Just-in-Time Tools (adaptive tool selection). When `mode="adaptive"`,
    // each turn carries only the tools it plausibly needs (core set + semantic
    // top-K of the message + conversation-sticky tools), plus a `find_tools`
    // meta-tool the model can call to pull in anything else on demand — instead
    // of sending every enabled tool's schema on every request. Default
    // `mode="all"` preserves today's behaviour. See design-docs/just-in-time-tools.md.
    #[serde(default)]
    pub tool_selection: ToolSelectionConfig,

    // Reasoning-model auto-routing (roadmap #13). When enabled, MIRA inspects
    // each turn and routes "hard" ones to a stronger reasoning provider/model
    // instead of the default — so users don't flip a manual toggle.
    #[serde(default)]
    pub reasoning: ReasoningConfig,

    // Assistant avatar, encoded like user avatars: `"preset:<key>"` for
    // bundled icons or `"upload:<ext>"` for a file at
    // `{data_dir}/avatars/agent.{ext}`. None → MIRA logo fallback.
    #[serde(default)]
    pub avatar: Option<String>,

    // Unix-ms timestamp of the last avatar change. Used by the web UI to
    // cache-bust `<img>` src when an upload is replaced.
    #[serde(default)]
    pub avatar_updated_at: Option<i64>,

    // Multi-agent LLM aliases (slice B8). Maps a logical alias —
    // `"primary"`, `"coding"`, `"research"`, `"cheap"` — to a concrete
    // `(provider, model)`. Skill manifests' `permissions.llm_providers`
    // list selects from these aliases at spawn time, so the user can
    // steer "all coding agents to GPT-5" or "all research agents to
    // Claude 4.7" by editing a single map instead of every Skill.
    //     // Empty by default — the supervisor falls back to the configured
    // `primary_provider` when an alias isn't found.
    #[serde(default)]
    pub llm_aliases: std::collections::HashMap<String, LlmAlias>,

    // Per-call token cap applied to non-streaming tool-loop rounds (the
    // rounds where the model is supposed to emit a structured tool call,
    // not prose). Tight on purpose: reasoning-distilled fine-tunes will
    // happily loop on duplicated tool-call XML for thousands of tokens
    // when they're given enough rope. 2048 fits ~12 tool calls plus a
    // short cover note, which is enough headroom for any real round.
    #[serde(default = "default_max_tool_round_tokens")]
    pub max_tool_round_tokens: u32,
    // Per-call token cap applied to the final-answer streaming path. Set
    // large enough that long markdown plans (or models that prepend a few
    // hundred tokens of reasoning before answering) don't truncate
    // mid-sentence.
    #[serde(default = "default_max_response_tokens")]
    pub max_response_tokens:   u32,

    // 0.113.0 — agent detail page defaults. Controls whether the
    // /agents/{id} view uses poll or SSE by default, the poll
    // interval in milliseconds, and whether to prettify the raw
    // adapter stdout for display.
    #[serde(default)]
    pub detail: AgentDetailConfig,

    // Show a "Thinking" rollup on each assistant message in the
    // chat UI containing the agent's tool calls, tool results,
    // model reasoning (when emitted), and wiki context fetched for
    // the turn. Lives behind a config flag so users who only care
    // about final answers can hide the noise. Server still
    // collects + persists the thinking events either way (cheap),
    // so flipping this back on doesn't lose past activity.
    #[serde(default = "default_true")]
    pub show_thinking: bool,

    // Shared USD budget across a root agent's whole multi-agent tree. When the
    // combined LLM spend of all agents under a session exceeds this, further
    // work is cut off with a `session_budget_exceeded` fault. Raise it for
    // long research runs (this cap — previously a hardcoded $5 — is the usual
    // cause of a lengthy run "failing with a timeout").
    #[serde(default = "default_session_budget_usd")]
    pub session_budget_usd: f64,
    // Per-task budget the `spawn_background_task` tool assigns a worker when
    // the caller omits one.
    #[serde(default = "default_task_budget_usd")]
    pub default_task_budget_usd: f64,
    // Hard ceiling a single spawned task's budget is clamped to.
    #[serde(default = "default_max_task_budget_usd")]
    pub max_task_budget_usd: f64,
}

fn default_session_budget_usd() -> f64 { 5.0 }
fn default_task_budget_usd() -> f64 { 2.0 }
fn default_max_task_budget_usd() -> f64 { 10.0 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDetailConfig {
    // `"poll"` or `"sse"`. Default `"poll"` — simpler, no streaming
    // connection state to mind. Switch to `"sse"` for sub-second
    // updates on long-running agents.
    #[serde(default = "default_agent_detail_view_mode")]
    pub view_mode: String,
    // Frontend poll interval (ms). Only consulted when `view_mode`
    // is `"poll"`. Reasonable range: 500–5000.
    #[serde(default = "default_agent_detail_poll_ms")]
    pub poll_interval_ms: u64,
    // When true, the detail page attempts to JSON-prettify each
    // stdout line it recognises as JSON (claudecode's stream-json
    // format, opencode's events). False = render raw, which is
    // honest about what the adapter actually emitted.
    #[serde(default)]
    pub prettify_output: bool,
}

impl Default for AgentDetailConfig {
    fn default() -> Self {
        Self {
            view_mode:        default_agent_detail_view_mode(),
            poll_interval_ms: default_agent_detail_poll_ms(),
            prettify_output:  false,
        }
    }
}

fn default_agent_detail_view_mode() -> String { "poll".to_string() }
fn default_agent_detail_poll_ms()   -> u64    { 1500 }

// Concrete (provider, model) tuple a logical alias resolves to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmAlias {
    // Name of a provider in `[providers]` (e.g. `"openrouter"`,
    // `"lmstudio"`). Falling outside the configured providers is a
    // runtime resolution error, not a config-load error — that way
    // users can stage alias edits before the matching provider is
    // fully configured.
    pub provider: String,
    // Model id within that provider. None = use the provider's
    // `default_model`.
    #[serde(default)]
    pub model: Option<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_tool_rounds:   default_max_tool_rounds(),
            tool_mode:         default_tool_mode(),
            system_prompt_file: String::new(),
            disable_reasoning: false,
            playful_easter_eggs: default_true(),
            max_context_turns: default_max_context_turns(),
            context_length_tokens: 0,
            context_safety_margin_tokens: default_context_safety_margin(),
            prompt_cache_enabled: false,
            compaction: CompactionConfig::default(),
            tools:             ToolsConfig::default(),
            tool_selection:    ToolSelectionConfig::default(),
            reasoning:         ReasoningConfig::default(),
            avatar:            None,
            avatar_updated_at: None,
            llm_aliases:       std::collections::HashMap::new(),
            max_tool_round_tokens: default_max_tool_round_tokens(),
            max_response_tokens:   default_max_response_tokens(),
            detail:                AgentDetailConfig::default(),
            show_thinking:         true,
            session_budget_usd:      default_session_budget_usd(),
            default_task_budget_usd: default_task_budget_usd(),
            max_task_budget_usd:     default_max_task_budget_usd(),
        }
    }
}

// Per-tool enable/disable switches.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolsConfig {
    #[serde(default)]
    pub shell: ToolToggle,

    #[serde(default)]
    pub filesystem: ToolToggle,

    #[serde(default)]
    pub web_fetch: WebFetchConfig,

    #[serde(default)]
    pub url_preview: UrlPreviewConfig,

    #[serde(default)]
    pub web_search: WebSearchConfig,
}

// Just-in-Time Tools — adaptive per-turn tool selection.
// See design-docs/just-in-time-tools.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSelectionConfig {
    /// "all" (default — send every enabled tool, today's behaviour) or
    /// "adaptive" (semantic top-K + core set + stickiness + find_tools).
    #[serde(default = "default_tool_selection_mode")]
    pub mode: String,
    /// Always-on tools, even when unmatched. Supports trailing-`*` globs
    /// (e.g. "memory_*"). Keeps flow-critical/baseline tools present.
    #[serde(default = "default_core_tools")]
    pub core_tools: Vec<String>,
    /// Max number of semantically-matched tools to add per turn.
    #[serde(default = "default_tool_top_k")]
    pub top_k: usize,
    /// Minimum cosine similarity for a tool to be included by semantic match.
    #[serde(default = "default_tool_min_similarity")]
    pub min_similarity: f32,
    /// Tools used earlier in a conversation stay active for this many turns.
    #[serde(default = "default_tool_stickiness_turns")]
    pub stickiness_turns: usize,
    /// Expose the `find_tools` meta-tool so the model can pull in any tool
    /// on demand (progressive disclosure). Strongly recommended on.
    #[serde(default = "default_true")]
    pub expose_find_tools: bool,
}

fn default_tool_selection_mode() -> String { "all".to_string() }
fn default_core_tools() -> Vec<String> {
    vec!["memory_*".to_string(), "wiki_*".to_string(), "now".to_string()]
}
fn default_tool_top_k() -> usize { 8 }
fn default_tool_min_similarity() -> f32 { 0.30 }
fn default_tool_stickiness_turns() -> usize { 6 }

impl Default for ToolSelectionConfig {
    fn default() -> Self {
        Self {
            mode:             default_tool_selection_mode(),
            core_tools:       default_core_tools(),
            top_k:            default_tool_top_k(),
            min_similarity:   default_tool_min_similarity(),
            stickiness_turns: default_tool_stickiness_turns(),
            expose_find_tools: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolToggle {
    #[serde(default)]
    pub enabled: bool,
}

impl Default for ToolToggle {
    fn default() -> Self { Self { enabled: false } }
}

// `web_fetch` tool — Tier 2 network. See design-docs/phase7-tier2-web-tools.md §3.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebFetchConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    // Hard cap on bytes read from the origin (pre-readability).
    #[serde(default = "default_web_fetch_body_bytes")]
    pub max_body_bytes: u64,
    // Hard cap on characters returned to the model (post-readability).
    #[serde(default = "default_web_fetch_text_chars")]
    pub max_text_chars: usize,
    #[serde(default = "default_web_fetch_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_web_fetch_redirects")]
    pub max_redirects: usize,
}

impl Default for WebFetchConfig {
    fn default() -> Self {
        Self {
            enabled:        true,
            max_body_bytes: default_web_fetch_body_bytes(),
            max_text_chars: default_web_fetch_text_chars(),
            timeout_secs:   default_web_fetch_timeout(),
            max_redirects:  default_web_fetch_redirects(),
        }
    }
}

fn default_web_fetch_body_bytes() -> u64   { 5 * 1024 * 1024 }
fn default_web_fetch_text_chars() -> usize { 256 * 1024 }
fn default_web_fetch_timeout()   -> u64    { 30 }
fn default_web_fetch_redirects() -> usize  { 5 }

// `url_preview` tool — Tier 2 network. See design-docs/phase7-tier2-web-tools.md §4.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UrlPreviewConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    // OG tags live in `<head>` — cap tightly to save bandwidth.
    #[serde(default = "default_url_preview_body_bytes")]
    pub max_body_bytes: u64,
}

impl Default for UrlPreviewConfig {
    fn default() -> Self {
        Self {
            enabled:        true,
            max_body_bytes: default_url_preview_body_bytes(),
        }
    }
}

fn default_url_preview_body_bytes() -> u64 { 128 * 1024 }

// `web_search` tool — Tier 2 network. See design-docs/phase7-tier2-web-tools.md §5.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    // Backend to try first. One of `"ddg"`, `"brave"`, `"searxng"`.
    #[serde(default = "default_web_search_default")]
    pub default: String,
    // Ordered fallback list. `"ddg"` is a sensible last entry since it
    // needs no key.
    #[serde(default = "default_web_search_failover")]
    pub failover: Vec<String>,
    // Default hit count when the caller doesn't specify `top_k`.
    #[serde(default = "default_web_search_top_k")]
    pub top_k: usize,

    #[serde(default)]
    pub brave: BraveSearchConfig,

    #[serde(default)]
    pub searxng: SearxngConfig,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            enabled:  true,
            default:  default_web_search_default(),
            failover: default_web_search_failover(),
            top_k:    default_web_search_top_k(),
            brave:    BraveSearchConfig::default(),
            searxng:  SearxngConfig::default(),
        }
    }
}

fn default_web_search_default()  -> String       { "ddg".to_string() }
fn default_web_search_failover() -> Vec<String>  { vec!["ddg".to_string()] }
fn default_web_search_top_k()    -> usize        { 10 }

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BraveSearchConfig {
    // API key. If empty, also reads `BRAVE_SEARCH_API_KEY` env.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearxngConfig {
    // Full URL to a SearXNG instance, e.g. `http://searxng.example.com:8080`.
    // If set and the URL points at a private IP, the HTTP policy will
    // auto-whitelist that single host:port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

// ── Security policy ───────────────────────────────────────────────────────────

// Security policy applied by the middleware layer.
// Distinct from the server's `auth_token` which is stored under `server`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityPolicyConfig {
    // Maximum requests per minute per IP address. 0 = unlimited.
    #[serde(default = "default_rate_limit_rpm")]
    pub rate_limit_rpm: u32,

    // CORS allowed origins. Empty = deny all cross-origin requests.
    #[serde(default)]
    pub cors_allowed_origins: Vec<String>,

    // IP addresses permanently blocked from all endpoints.
    #[serde(default)]
    pub blocked_ips: Vec<String>,

    // HS256 secret for signing/verifying JWT access tokens.
    // If None, a random 32-byte hex secret is generated at first run and saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwt_secret: Option<String>,

    // Refresh token lifetime in days. Default: 7.
    #[serde(default = "default_session_days")]
    pub session_days: u64,

    // Outbound HTTP policy shared by all Tier 2 network tools.
    #[serde(default)]
    pub http: HttpSecurityConfig,
}

impl Default for SecurityPolicyConfig {
    fn default() -> Self {
        Self {
            rate_limit_rpm:      default_rate_limit_rpm(),
            cors_allowed_origins: vec![],
            blocked_ips:          vec![],
            jwt_secret:           None,
            session_days:         default_session_days(),
            http:                 HttpSecurityConfig::default(),
        }
    }
}

// Outbound HTTP policy for Tier 2 tools. See `design-docs/phase7-tier2-web-tools.md` §1.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HttpSecurityConfig {
    // Domains (exact or suffix at label boundary) the policy layer refuses to reach.
    #[serde(default)]
    pub denylist: Vec<String>,

    // When `allowlist_only=true`, only hosts in this list may be reached.
    #[serde(default)]
    pub allowlist: Vec<String>,

    // Enterprise-paranoid mode. Default: false (open internet is reachable).
    #[serde(default)]
    pub allowlist_only: bool,

    // One user-configured SearXNG `host:port` exempted from the private-IP
    // block. Format: `"searxng.example.com:8080"`. Other checks still apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub searxng_exception: Option<String>,

    #[serde(default)]
    pub rate: HttpRateConfig,
}

// Per-user token-bucket rate limits for Tier 2 HTTP traffic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpRateConfig {
    #[serde(default = "default_http_rate_user_per_min")]
    pub user_per_min: u32,
    #[serde(default = "default_http_rate_user_per_hour")]
    pub user_per_hour: u32,
    #[serde(default = "default_http_rate_user_per_domain_per_min")]
    pub user_per_domain_per_min: u32,
    #[serde(default = "default_http_rate_search_per_min")]
    pub search_per_min: u32,
}

impl Default for HttpRateConfig {
    fn default() -> Self {
        Self {
            user_per_min:            default_http_rate_user_per_min(),
            user_per_hour:           default_http_rate_user_per_hour(),
            user_per_domain_per_min: default_http_rate_user_per_domain_per_min(),
            search_per_min:          default_http_rate_search_per_min(),
        }
    }
}

fn default_http_rate_user_per_min()            -> u32 { 60 }
fn default_http_rate_user_per_hour()           -> u32 { 600 }
fn default_http_rate_user_per_domain_per_min() -> u32 { 10 }
fn default_http_rate_search_per_min()          -> u32 { 30 }

// ── Proxy ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    // Enable nginx reverse proxy management.
    #[serde(default)]
    pub enabled: bool,

    // Path to the nginx binary.
    #[serde(default = "default_nginx_binary")]
    pub nginx_binary: String,

    // Where MIRA writes the generated nginx.conf.
    #[serde(default = "default_nginx_config_path")]
    pub config_path: String,

    // nginx PID file location.
    #[serde(default = "default_nginx_pid_path")]
    pub pid_path: String,

    // nginx worker_processes directive value.
    #[serde(default = "default_nginx_workers")]
    pub worker_processes: String,

    // Enable WebSocket proxying (needed for the future browser streaming client).
    #[serde(default = "default_true")]
    pub websocket_support: bool,

    #[serde(default)]
    pub tls: TlsConfig,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled:            false,
            nginx_binary:       default_nginx_binary(),
            config_path:        default_nginx_config_path(),
            pid_path:           default_nginx_pid_path(),
            worker_processes:   default_nginx_workers(),
            websocket_support:  true,
            tls:                TlsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    // Enable TLS. Requires cert_path and key_path to be set.
    #[serde(default)]
    pub enabled: bool,

    // Path to the TLS certificate (PEM).
    #[serde(default)]
    pub cert_path: String,

    // Path to the TLS private key (PEM).
    #[serde(default)]
    pub key_path: String,

    // Port nginx listens on for HTTPS.
    #[serde(default = "default_tls_port")]
    pub listen_port: u16,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            enabled:     false,
            cert_path:   String::new(),
            key_path:    String::new(),
            listen_port: default_tls_port(),
        }
    }
}

// ── Calendar ──────────────────────────────────────────────────────────────────
//
// MIRA-native storage is always on. When `sync_provider` is set to something
// other than `"none"`, a background task polls the external source and
// mirrors events into the native store. Write-back to external sources is
// not in scope for this initial implementation — event CRUD from the UI /
// agent tools writes to MIRA-native only.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarConfig {
    // Enable MIRA-native calendar storage + agent tools. Defaults to true.
    #[serde(default = "default_true")]
    pub enabled: bool,

    // Which external source to sync from. One of:
    // - `"none"`     — native only (default)
    // - `"caldav"`   — CalDAV server (user-supplied URL + basic auth)
    // - `"google"`   — Google Calendar via OAuth
    // - `"outlook"`  — Microsoft Outlook / 365 via OAuth
    #[serde(default = "default_calendar_sync_provider")]
    pub sync_provider: String,

    // How often the sync engine pulls from the external source. Floor: 5 min.
    #[serde(default = "default_calendar_sync_interval")]
    pub sync_interval_mins: u64,

    #[serde(default)]
    pub caldav: CalDavConfig,

    #[serde(default)]
    pub google: CalendarOAuthConfig,

    #[serde(default)]
    pub outlook: CalendarOAuthConfig,
}

impl Default for CalendarConfig {
    fn default() -> Self {
        Self {
            enabled:            true,
            sync_provider:      default_calendar_sync_provider(),
            sync_interval_mins: default_calendar_sync_interval(),
            caldav:             CalDavConfig::default(),
            google:             CalendarOAuthConfig::default(),
            outlook:            CalendarOAuthConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CalDavConfig {
    // Full URL to the user's CalDAV calendar collection, e.g.
    // `https://cal.example.com/dav/calendars/alice/primary/`.
    #[serde(default)]
    pub url: String,

    // Basic-auth username. Common deployments use email-as-username.
    #[serde(default)]
    pub username: String,

    // Password or app-specific token. Stored in the config file — the user
    // is expected to protect it with filesystem perms on the config dir.
    #[serde(default)]
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CalendarOAuthConfig {
    // OAuth client id issued when the user registered a MIRA app in their
    // Google / Azure tenant. Leave blank to leave the provider unconfigured.
    #[serde(default)]
    pub client_id: String,

    #[serde(default)]
    pub client_secret: String,

    // Where the provider should redirect after authorisation. Must match
    // the URL registered in the provider console.
    #[serde(default = "default_calendar_redirect_uri")]
    pub redirect_uri: String,

    // Comma-separated scopes. Providers default to read-only calendar access.
    #[serde(default)]
    pub scopes: String,
}

fn default_calendar_sync_provider() -> String { "none".to_string() }
fn default_calendar_sync_interval() -> u64    { 15 }
fn default_calendar_redirect_uri()  -> String { "http://localhost:8080/api/calendar/oauth/callback".to_string() }

// ── Sandbox (5) ──────────────────────────────────────────────────────
//
// Tier 4 sandboxed code execution. Disabled by default — admins opt in by
// flipping `sandbox.enabled` and at least one per-tool toggle. The runtime
// also requires a prebaked rootfs (see `mira sandbox install python`).

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SandboxConfig {
    // Master switch for the entire sandbox subsystem. When false, no
    // Tier 4 tool is registered regardless of per-tool toggles.
    #[serde(default)]
    pub enabled: bool,

    // Which seccomp filter the backend installs in the child.
    // `"allowlist"` (default since 7a-5 iter C) permits only the syscalls a
    // Python interpreter needs; `"denylist"` blocks a curated set of escape
    // primitives and allows everything else (the 7a-4 default — kept as an
    // opt-out for scripts that need a syscall not yet in the allowlist).
    #[serde(default)]
    pub seccomp_mode: SeccompModeConfig,

    // `code_run` agent tool — runs short scripts in the prebaked rootfs.
    #[serde(default)]
    pub code_run: CodeRunConfig,

    // Path overrides for installed rootfs entries. Empty = use the manager's
    // default location under `<data_dir>/sandbox/rootfs/...`.
    #[serde(default)]
    pub python: PythonRootfsConfig,

    // Code-execution backend: "" / "auto" (namespace on Linux when a rootfs is
    // installed, else the cross-platform WASM backend), "namespace" (Linux
    // only), or "wasm" (WASM/WASI everywhere). 0.284.x+.
    #[serde(default)]
    pub backend: String,

    // WASM/WASI backend (cross-platform code_run).
    #[serde(default)]
    pub wasm: WasmRootfsConfig,

    // Pyodide-on-Node backend — scientific Python (numpy/pandas/matplotlib).
    // 0.286.x+.
    #[serde(default)]
    pub pyodide: PyodideConfig,
}

// WASM/WASI sandbox settings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WasmRootfsConfig {
    // Optional override for the WASI CPython module path. Empty = use the
    // managed copy under `<data_dir>/deps/wasm/python-<ver>.wasm`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub python_path: String,
}

// Pyodide-on-Node sandbox settings. Pyodide is CPython-on-emscripten run by
// the Node runtime MIRA already provisions for MCP; it brings the full
// scientific stack (numpy/pandas/matplotlib) with on-demand wheel loading —
// things the pure-WASI backend can't do. Python still runs in wasm (V8): no
// syscalls, no host FS except a single granted scratch dir, no network from
// user code. The Node *host* process is privileged, so this is a weaker
// isolation boundary than wasmtime — fine for semi-trusted code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PyodideConfig {
    // Enable the Pyodide backend. Off by default; auto-selected by the
    // "auto" backend router when a script imports a scientific package or
    // `code_run(packages=...)` is given and Pyodide is provisioned.
    #[serde(default)]
    pub enabled: bool,

    // Packages to pre-warm into the local wheel cache at provision time so
    // the first scientific run is offline-fast. Empty = use the built-in
    // default trio (numpy, pandas, matplotlib).
    #[serde(default)]
    pub prewarm: Vec<String>,
}

impl Default for PyodideConfig {
    fn default() -> Self {
        Self { enabled: false, prewarm: Vec::new() }
    }
}

// Wire format for `[sandbox] seccomp_mode`. Mirrors `sandbox::SeccompMode`
// but lives in `config` so the config crate doesn't have to depend on the
// sandbox module's internal enum (the sandbox module is feature-gated; the
// config is always compiled).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeccompModeConfig {
    Denylist,
    Allowlist,
}

impl Default for SeccompModeConfig {
    fn default() -> Self { Self::Allowlist }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeRunConfig {
    // Per-tool toggle. Both `sandbox.enabled` and this must be true for the
    // tool to be registered. Defaults to off.
    #[serde(default)]
    pub enabled: bool,

    // Languages the tool will accept. Anything outside this set is refused
    // at the schema layer. Iteration B ships `python` only.
    #[serde(default = "default_code_run_languages")]
    pub allowed_languages: Vec<String>,

    // Hard ceiling on per-call wall-clock seconds. Callers can ask for
    // less; anything more is clamped down.
    #[serde(default = "default_code_run_wall_clock_seconds")]
    pub max_wall_clock_seconds: u64,

    // Memory cap (RLIMIT_AS) per call, in megabytes.
    #[serde(default = "default_code_run_max_memory_mb")]
    pub max_memory_mb: u64,
}

impl Default for CodeRunConfig {
    fn default() -> Self {
        Self {
            enabled:                false,
            allowed_languages:      default_code_run_languages(),
            max_wall_clock_seconds: default_code_run_wall_clock_seconds(),
            max_memory_mb:          default_code_run_max_memory_mb(),
        }
    }
}

fn default_code_run_languages()           -> Vec<String> { vec!["python".to_string()] }
fn default_code_run_wall_clock_seconds()  -> u64 { 10 }
fn default_code_run_max_memory_mb()       -> u64 { 256 }

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PythonRootfsConfig {
    // Optional override for the python rootfs pivot path. Empty = use the
    // manager's default (`<data_dir>/sandbox/rootfs/python-<ver>/python`).
    // Useful for shared installations or a manually-built rootfs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rootfs_path: String,
}

// ── TTS ──────────────────────────────────────────────────────────────────────

// Text-to-Speech subsystem. See `design-docs/phase8-tts.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    // Backend used when no per-channel route or per-request override applies.
    // One of: `internal` | `openai` | `openai_compat` | `elevenlabs` | `cartesia`.
    #[serde(default = "default_tts_backend")]
    pub default_backend: String,

    // Voice id used when the request does not specify one. Empty = backend default.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default_voice: String,

    // Speech rate. 1.0 = natural; 0.5..=2.0 is the supported band.
    #[serde(default = "default_tts_speed")]
    pub default_speed: f32,

    // Hint for the encoder. `wav` | `mp3` | `ogg-opus`.
    #[serde(default = "default_tts_format")]
    pub default_format: String,

    // Sentence-chunked streaming for chat.  honours this; 
    // returns the full buffer regardless.
    #[serde(default = "default_true")]
    pub streaming: bool,

    // Safety cap on a single TTS call.
    #[serde(default = "default_tts_max_chars")]
    pub max_chars_per_request: usize,

    #[serde(default = "default_tts_request_timeout")]
    pub request_timeout_secs: u64,

    #[serde(default)]
    pub cache: TtsCacheConfig,

    #[serde(default)]
    pub internal: TtsInternalConfig,

    // K1 (Q2 #10) — native Kokoro backend. Only active in a build with
    // `--features kokoro`; inert otherwise.
    #[serde(default)]
    pub kokoro: TtsKokoroConfig,

    // K3 (Q2 #10) — Chatterbox AMD Vulkan TTS server integration.
    #[serde(default)]
    pub chatterbox: TtsChatterboxConfig,

    #[serde(default)]
    pub openai: TtsOpenaiConfig,

    #[serde(default)]
    pub openai_compat: TtsOpenaiCompatConfig,

    #[serde(default)]
    pub elevenlabs: TtsElevenlabsConfig,

    #[serde(default)]
    pub cartesia: TtsCartesiaConfig,

    #[serde(default)]
    pub routing: TtsRoutingConfig,

    // Server-default voice prefs keyed by channel id. Each user can
    // override these per-channel from the Profile dialog. Missing channel
    // keys (or null fields) fall back to "Never" / no voice override.
    // The map is intentionally untyped at the channel-id level so plugin
    // channels can register entries without a config schema bump.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub voice_prefs: std::collections::HashMap<String, crate::voice::ChannelVoicePrefs>,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            enabled:               true,
            default_backend:       default_tts_backend(),
            default_voice:         String::new(),
            default_speed:         default_tts_speed(),
            default_format:        default_tts_format(),
            streaming:             true,
            max_chars_per_request: default_tts_max_chars(),
            request_timeout_secs:  default_tts_request_timeout(),
            cache:                 TtsCacheConfig::default(),
            internal:              TtsInternalConfig::default(),
            kokoro:                TtsKokoroConfig::default(),
            chatterbox:            TtsChatterboxConfig::default(),
            openai:                TtsOpenaiConfig::default(),
            openai_compat:         TtsOpenaiCompatConfig::default(),
            elevenlabs:            TtsElevenlabsConfig::default(),
            cartesia:              TtsCartesiaConfig::default(),
            routing:               TtsRoutingConfig::default(),
            voice_prefs:           std::collections::HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsCacheConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_tts_cache_max_disk_mb")]
    pub max_disk_mb: u64,
    #[serde(default = "default_tts_cache_ttl_days")]
    pub ttl_days: u64,
}

impl Default for TtsCacheConfig {
    fn default() -> Self {
        Self {
            enabled:     true,
            max_disk_mb: default_tts_cache_max_disk_mb(),
            ttl_days:    default_tts_cache_ttl_days(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsInternalConfig {
    // `piper` (default) | `espeak` | `kokoro`.
    #[serde(default = "default_tts_internal_engine")]
    pub engine: String,

    // Override for `<data_dir>/tts/voices`. Empty = derive.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub voices_dir: String,

    #[serde(default = "default_tts_internal_voice")]
    pub default_voice: String,

    #[serde(default = "default_true")]
    pub auto_download_voices: bool,

    // Override for the auto-installed Piper executable. Empty = derive.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub binary_path: String,

    // Web playback gain. 1.0 = unaltered, 2.0 = doubled. See
    // `default_tts_volume`.
    #[serde(default = "default_tts_volume")]
    pub volume: f32,
}

impl Default for TtsInternalConfig {
    fn default() -> Self {
        Self {
            engine:               default_tts_internal_engine(),
            voices_dir:           String::new(),
            default_voice:        default_tts_internal_voice(),
            auto_download_voices: true,
            binary_path:          String::new(),
            volume:               default_tts_volume(),
        }
    }
}

// K1 (Q2 #10) — native Kokoro TTS backend (`any-tts` / Candle). Runs the
// Kokoro-82M model in-process: no separate server, no ONNX native lib, no
// system espeak-ng (pure-Rust phonemizer). American English only for now.
// Every field here is inert unless MIRA was built with `--features kokoro`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsKokoroConfig {
    // Register the backend. Defaults off so a stock build never spends
    // memory loading the model unless the operator opts in — even when
    // the binary was compiled with the feature.
    #[serde(default)]
    pub enabled: bool,

    // Override the model directory. Empty = `<data_dir>/tts/kokoro/Kokoro-82M`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model_path: String,

    // Voice used when a request doesn't specify one (a Kokoro preset id
    // such as `af_heart`, `am_michael`, `bf_emma`).
    #[serde(default = "default_kokoro_voice")]
    pub default_voice: String,

    // Pull missing model weights + voices from HuggingFace on first use.
    #[serde(default = "default_true")]
    pub auto_download: bool,

    // Device preference: `auto` | `cpu` | `cuda` | `metal`. GPU options
    // take effect only in a build with the matching any-tts GPU feature;
    // otherwise they degrade to CPU.
    #[serde(default = "default_kokoro_device")]
    pub device: String,
}

impl Default for TtsKokoroConfig {
    fn default() -> Self {
        Self {
            enabled:       false,
            model_path:    String::new(),
            default_voice: default_kokoro_voice(),
            auto_download: true,
            device:        default_kokoro_device(),
        }
    }
}

fn default_kokoro_voice()  -> String { "af_heart".to_string() }
fn default_kokoro_device() -> String { "auto".to_string() }

// K3 (Q2 #10) — Chatterbox AMD Vulkan TTS server integration. Chatterbox
// (https://github.com/tarekedOz/Chatterbox_AMDVulkan) is an OpenAI-compatible
// TTS server that's very fast on AMD Radeon GPUs via Vulkan. When enabled,
// MIRA registers it as a `chatterbox` TTS backend pointed at the local
// server and (optionally) supervises the process: spawn, health-check,
// restart. Recommended automatically when an AMD GPU is detected (K2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsChatterboxConfig {
    // Register the `chatterbox` TTS backend (OpenAI-compatible client
    // pointed at `http://127.0.0.1:{port}/v1`).
    #[serde(default)]
    pub enabled: bool,

    // Local port the Chatterbox server listens on. Default 8087.
    #[serde(default = "default_chatterbox_port")]
    pub port: u16,

    // Default Chatterbox voice (its preset names, e.g. `Adrian`).
    #[serde(default = "default_chatterbox_voice")]
    pub default_voice: String,

    // Have MIRA spawn + supervise the server process (spawn, poll
    // `/health`, restart on crash). Off = the operator runs Chatterbox
    // themselves and MIRA only talks to the URL. On WSL2 with a Windows-
    // side Chatterbox this stays off — MIRA can't manage a cross-OS process.
    #[serde(default)]
    pub supervise: bool,

    // Path to the Chatterbox server executable. Empty = rely on the
    // installer's default location / PATH. Required when `supervise = true`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub binary_path: String,

    // Extra args passed to the server on spawn (e.g. model `--*-gguf`
    // paths or `--config`). Empty = run the binary with its own defaults
    // (the installer sets up a working config).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
}

impl Default for TtsChatterboxConfig {
    fn default() -> Self {
        Self {
            enabled:       false,
            port:          default_chatterbox_port(),
            default_voice: default_chatterbox_voice(),
            supervise:     false,
            binary_path:   String::new(),
            extra_args:    Vec::new(),
        }
    }
}

fn default_chatterbox_port()  -> u16    { 8087 }
fn default_chatterbox_voice() -> String { "Adrian".to_string() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsOpenaiConfig {
    // Falls back to `providers.openai.api_key` then `OPENAI_API_KEY`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default = "default_tts_openai_url")]
    pub base_url: String,
    #[serde(default = "default_tts_openai_model")]
    pub model: String,
    #[serde(default = "default_tts_openai_voice")]
    pub default_voice: String,
    #[serde(default = "default_tts_volume")]
    pub volume: f32,
}

impl Default for TtsOpenaiConfig {
    fn default() -> Self {
        Self {
            api_key:       None,
            base_url:      default_tts_openai_url(),
            model:         default_tts_openai_model(),
            default_voice: default_tts_openai_voice(),
            volume:        default_tts_volume(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsOpenaiCompatConfig {
    #[serde(default = "default_tts_openai_compat_url")]
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default = "default_tts_openai_model")]
    pub model: String,
    #[serde(default = "default_tts_openai_voice")]
    pub default_voice: String,
    #[serde(default = "default_tts_volume")]
    pub volume: f32,
}

impl Default for TtsOpenaiCompatConfig {
    fn default() -> Self {
        Self {
            url:           default_tts_openai_compat_url(),
            api_key:       None,
            model:         default_tts_openai_model(),
            default_voice: default_tts_openai_voice(),
            volume:        default_tts_volume(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsElevenlabsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default = "default_tts_elevenlabs_model")]
    pub model: String,
    #[serde(default = "default_tts_elevenlabs_voice")]
    pub default_voice_id: String,
    #[serde(default = "default_tts_volume")]
    pub volume: f32,
}

impl Default for TtsElevenlabsConfig {
    fn default() -> Self {
        Self {
            api_key:          None,
            model:            default_tts_elevenlabs_model(),
            default_voice_id: default_tts_elevenlabs_voice(),
            volume:           default_tts_volume(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsCartesiaConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default = "default_tts_cartesia_model")]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default_voice_id: String,
    #[serde(default = "default_tts_volume")]
    pub volume: f32,
}

impl Default for TtsCartesiaConfig {
    fn default() -> Self {
        Self {
            api_key:          None,
            model:            default_tts_cartesia_model(),
            default_voice_id: String::new(),
            volume:           default_tts_volume(),
        }
    }
}

// Per-channel backend pinning. Each value is a backend id or `"internal"`
// (which resolves to whichever engine is configured under `tts.internal`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsRoutingConfig {
    #[serde(default = "default_tts_route")]
    pub web: String,
    #[serde(default = "default_tts_route")]
    pub tui: String,
    #[serde(default = "default_tts_route")]
    pub telegram: String,
    #[serde(default = "default_tts_route")]
    pub signal: String,
    // Native mobile app (channel id `mobile`). 0.287.x+.
    #[serde(default = "default_tts_route")]
    pub mobile: String,
}

impl Default for TtsRoutingConfig {
    fn default() -> Self {
        Self {
            web:      default_tts_route(),
            tui:      default_tts_route(),
            telegram: default_tts_route(),
            signal:   default_tts_route(),
            mobile:   default_tts_route(),
        }
    }
}

// ── STT (speech-to-text) ─────────────────────────────────────────────────────

// Speech-to-text subsystem. Mirrors the TTS layout: a default backend, an
// internal in-process whisper.cpp engine for "out of the box" use, plus
// optional OpenAI-compatible (self-hosted or cloud) backends via the same
// `/v1/audio/transcriptions` shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    // Backend used when no per-channel route or per-request override applies.
    // One of: `internal` | `openai` | `openai_compat`.
    #[serde(default = "default_stt_backend")]
    pub default_backend: String,

    // BCP-47 language hint passed to the backend. Empty = let the backend
    // auto-detect (whisper handles this natively; OpenAI defaults to English
    // when omitted).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default_language: String,

    // Hard cap on a single transcription's audio length. Protects the
    // internal backend from runaway transcripts and bounds upload size on
    // the cloud backends.
    #[serde(default = "default_stt_max_audio_seconds")]
    pub max_audio_seconds: u32,

    // Per-request timeout against the backend. Internal backend ignores this
    // (synchronous CPU work) but the HTTP backends honour it.
    #[serde(default = "default_stt_request_timeout")]
    pub request_timeout_secs: u64,

    #[serde(default)]
    pub internal: SttInternalConfig,

    #[serde(default)]
    pub openai: SttOpenaiConfig,

    #[serde(default)]
    pub openai_compat: SttOpenaiCompatConfig,

    #[serde(default)]
    pub routing: SttRoutingConfig,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            enabled:              true,
            default_backend:      default_stt_backend(),
            default_language:     String::new(),
            max_audio_seconds:    default_stt_max_audio_seconds(),
            request_timeout_secs: default_stt_request_timeout(),
            internal:             SttInternalConfig::default(),
            openai:               SttOpenaiConfig::default(),
            openai_compat:        SttOpenaiCompatConfig::default(),
            routing:              SttRoutingConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttInternalConfig {
    // Whisper.cpp ggml model id. Maps to a download URL via the manifest.
    // Examples: `tiny.en`, `base.en`, `small.en`, `tiny`, `base`, `small`,
    // `medium`, `large-v3`. The `.en` variants are English-only and ~2x
    // faster; multilingual variants drop the suffix.
    #[serde(default = "default_stt_internal_model")]
    pub model: String,

    // Override for `<data_dir>/stt/models`. Empty = use the default.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub models_dir: String,

    // Auto-fetch the model file from huggingface.co on first use. Off = the
    // admin must place the file under `models_dir` manually.
    #[serde(default = "default_true")]
    pub auto_download_model: bool,

    // Inference threads. 0 = num_cpus.
    #[serde(default)]
    pub threads: u32,

    // Run the encoder on GPU when whisper-rs was built with a GPU feature.
    // Ignored on CPU-only builds.
    #[serde(default)]
    pub use_gpu: bool,
}

impl Default for SttInternalConfig {
    fn default() -> Self {
        Self {
            model:               default_stt_internal_model(),
            models_dir:          String::new(),
            auto_download_model: true,
            threads:             0,
            use_gpu:             false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttOpenaiConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default = "default_tts_openai_url")]
    pub base_url: String,
    #[serde(default = "default_stt_openai_model")]
    pub model: String,
}

impl Default for SttOpenaiConfig {
    fn default() -> Self {
        Self {
            api_key:  None,
            base_url: default_tts_openai_url(),
            model:    default_stt_openai_model(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttOpenaiCompatConfig {
    #[serde(default = "default_stt_openai_compat_url")]
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default = "default_stt_openai_compat_model")]
    pub model: String,
}

impl Default for SttOpenaiCompatConfig {
    fn default() -> Self {
        Self {
            url:     default_stt_openai_compat_url(),
            api_key: None,
            model:   default_stt_openai_compat_model(),
        }
    }
}

// Per-channel backend pinning for STT — the inverse of [`TtsRoutingConfig`].
// Empty = fall through to `stt.default_backend`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttRoutingConfig {
    #[serde(default = "default_stt_route")]
    pub web: String,
    #[serde(default = "default_stt_route")]
    pub tui: String,
    #[serde(default = "default_stt_route")]
    pub telegram: String,
    #[serde(default = "default_stt_route")]
    pub signal: String,
}

impl Default for SttRoutingConfig {
    fn default() -> Self {
        Self {
            web:      default_stt_route(),
            tui:      default_stt_route(),
            telegram: default_stt_route(),
            signal:   default_stt_route(),
        }
    }
}

// ── Session ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    #[serde(default = "default_cleanup_interval")]
    pub cleanup_interval_secs: u64,

    #[serde(default = "default_session_timeout")]
    pub timeout_secs: u64,

    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            cleanup_interval_secs: default_cleanup_interval(),
            timeout_secs:          default_session_timeout(),
            max_turns:             default_max_turns(),
        }
    }
}

// ── Automations (/) ─────────────────────────────────────────

// Per-user limits + agent-creation gating for the automations subsystem.
// Quotas are enforced at create-time; status `pending_approval` lets the
// user gate agent-authored rows before they go live.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutomationsConfig {
    #[serde(default)]
    pub quota_per_user: AutomationQuota,

    // When true, agent-authored rows (`owner_kind = "agent"`) land in
    // `pending_approval` instead of `active`. The user approves or rejects
    // from the UI. Defaults to true — agent autonomy is opt-out, not opt-in.
    #[serde(default = "default_true")]
    pub agent_creates_pending: bool,

    // When true, agent-authored rows must include a non-empty `rationale`.
    // Surfaces the agent's reasoning to the user for approval. Defaults
    // to true — the audit value is the entire point.
    #[serde(default = "default_true")]
    pub agent_rationale_required: bool,

    // Hard cap on how many activations may be linked in a single chain
    // before the dispatcher refuses. Guards against runaway loops where
    // an action emits an event that fires another subscription that
    // emits another event, etc. Default 5 — generous enough for normal
    // fan-out, tight enough to fail fast on a real loop.
    #[serde(default = "default_max_chain_depth")]
    pub max_chain_depth: u32,

    // Per-channel cap on `channel_message` actions per minute, scoped to
    // the row's owning user. Spam guard for both user-authored and
    // agent-authored automations. Map keys are channel ids
    // (`web`, `signal`, `telegram`, `email`); the special key `*` is the
    // fallback when a channel has no explicit entry. A value of `0`
    // disables the limit for that channel. Defaults are conservative
    // enough to absorb real cases (e.g. a 1-minute joke schedule) while
    // stopping a runaway loop in seconds.
    #[serde(default = "default_channel_rate_limits")]
    pub channel_rate_limits: std::collections::HashMap<String, u32>,

    // Slice W1 — log/audit watchdog. Tails configured sources, fires
    // `watchdog.alert` events on matched lines. Off by default;
    // admins opt in by editing config or using the Settings UI when
    // it lands.
    #[serde(default)]
    pub watchdog: WatchdogConfig,
}

impl Default for AutomationsConfig {
    fn default() -> Self {
        Self {
            quota_per_user:           AutomationQuota::default(),
            agent_creates_pending:    true,
            agent_rationale_required: true,
            max_chain_depth:          default_max_chain_depth(),
            channel_rate_limits:      default_channel_rate_limits(),
            watchdog:                 WatchdogConfig::default(),
        }
    }
}

// Slice W1 watchdog config. Off by default — operators opt in by
// flipping `enabled` and (typically) populating `notify_user_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchdogConfig {
    // Master switch. When false, the heartbeat never registers and
    // no schedule is seeded. Default false.
    #[serde(default)]
    pub enabled: bool,

    // How often the heartbeat ticks. Default 60s — log lines from
    // the last 60s are scanned each fire. Smaller values catch
    // problems faster but increase CPU; larger values save power.
    #[serde(default = "default_watchdog_interval_secs")]
    pub interval_secs: u64,

    // Lowest tracing level treated as an alert. One of `WARN` |
    // `ERROR`. INFO/DEBUG are never alerted on (would drown out
    // real signal). Default `WARN`.
    #[serde(default = "default_watchdog_severity")]
    pub severity_threshold: String,

    // How long the same fingerprint stays suppressed after firing
    // once. Prevents an error-per-second condition from flooding
    // the channel. Default 600s (10 min).
    #[serde(default = "default_watchdog_dedup_ttl_secs")]
    pub dedup_ttl_secs: u64,

    // Hard global cap on alerts per minute across all sources.
    // When exceeded, additional alerts within the same minute are
    // silently dropped (logged at debug). Default 10.
    #[serde(default = "default_watchdog_rate_limit_per_min")]
    pub rate_limit_per_min: u32,

    // W4 — storm pause. When a single source emits more than
    // `storm_threshold` distinct alerts within `storm_window_secs`,
    // that source is paused for `storm_cooldown_secs` and a one-shot
    // "storm detected" alert is emitted in its place. Per-source so
    // a runaway schedule failure doesn't crowd out an unrelated
    // log alert. Set `storm_threshold = 0` to disable.
    #[serde(default = "default_watchdog_storm_threshold")]
    pub storm_threshold:     u32,
    #[serde(default = "default_watchdog_storm_window_secs")]
    pub storm_window_secs:   u64,
    #[serde(default = "default_watchdog_storm_cooldown_secs")]
    pub storm_cooldown_secs: u64,

    // Lines matching any of these regexes are skipped before
    // fingerprinting. Use to silence known-noisy WARN patterns
    // (e.g. "Signal SSE error"). Default empty.
    #[serde(default)]
    pub ignore_patterns: Vec<String>,

    // Recipient for the `watchdog.alert` ChannelMessage. When None,
    // the watchdog still emits the event but no subscription
    // auto-routes to a channel (alerts only land in run history).
    // Set to a user UUID — typically the admin account.
    #[serde(default)]
    pub notify_user_id: Option<String>,

    // Channel for the auto-seeded notification subscription. Must
    // be one of `web` | `signal` | `telegram` | `email`. Default
    // `web` (lands in the per-user "Notifications" thread).
    #[serde(default = "default_watchdog_channel")]
    pub channel: String,

    // Override the log-file path the watchdog tails. None falls
    // back to `logging.file`. Useful for testing against a fixture.
    #[serde(default)]
    pub log_file: Option<String>,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            enabled:             false,
            interval_secs:       default_watchdog_interval_secs(),
            severity_threshold:  default_watchdog_severity(),
            dedup_ttl_secs:      default_watchdog_dedup_ttl_secs(),
            rate_limit_per_min:  default_watchdog_rate_limit_per_min(),
            ignore_patterns:     Vec::new(),
            notify_user_id:      None,
            channel:             default_watchdog_channel(),
            log_file:            None,
            storm_threshold:     default_watchdog_storm_threshold(),
            storm_window_secs:   default_watchdog_storm_window_secs(),
            storm_cooldown_secs: default_watchdog_storm_cooldown_secs(),
        }
    }
}

fn default_watchdog_interval_secs()       -> u64    { 60 }
fn default_watchdog_severity()            -> String { "WARN".into() }
fn default_watchdog_dedup_ttl_secs()      -> u64    { 600 }
fn default_watchdog_rate_limit_per_min()  -> u32    { 10 }
fn default_watchdog_channel()             -> String { "web".into() }
fn default_watchdog_storm_threshold()     -> u32    { 30  }
fn default_watchdog_storm_window_secs()   -> u64    { 300 }
fn default_watchdog_storm_cooldown_secs() -> u64    { 900 }

fn default_channel_rate_limits() -> std::collections::HashMap<String, u32> {
    let mut m = std::collections::HashMap::new();
    // Web is internal (an entry in a thread + a NotificationBus event).
    // Cheap, no external API; cap is tuned to catch loops, not user noise.
    m.insert("web".into(),      60);
    // External user-facing channels — push to a phone. Tight by design;
    // a legitimate "every 5 minutes" reminder fits well under 3/min.
    m.insert("signal".into(),    3);
    m.insert("telegram".into(),  3);
    // Email is the noisiest if it goes wrong. Default to 1/min per user.
    m.insert("email".into(),     1);
    // Fallback for unknown channels. Conservative on purpose.
    m.insert("*".into(),        10);
    m
}

// Per-user caps on row counts. Enforced for `User` and `Agent` owners only;
// `System` rows are seeded by the platform and bypass quotas. Counts are
// computed from `*_active*` rows + `pending_approval` rows so a user with a
// huge backlog of paused rows isn't blocked from creating new active ones.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutomationQuota {
    #[serde(default = "default_quota_schedules")]
    pub schedules: usize,
    #[serde(default = "default_quota_webhooks")]
    pub webhooks: usize,
    #[serde(default = "default_quota_event_subs")]
    pub event_subscriptions: usize,
}

impl Default for AutomationQuota {
    fn default() -> Self {
        Self {
            schedules:           default_quota_schedules(),
            webhooks:            default_quota_webhooks(),
            event_subscriptions: default_quota_event_subs(),
        }
    }
}

fn default_quota_schedules()  -> usize { 50 }
fn default_quota_webhooks()   -> usize { 20 }
fn default_quota_event_subs() -> usize { 50 }
fn default_max_chain_depth()  -> u32   { 5  }

// ── Default helpers ───────────────────────────────────────────────────────────

fn default_config_version()  -> String  { "1".to_string() }
fn default_data_dir()        -> String  { "~/.mira/data".to_string() }
fn default_primary_provider()-> String  { "lmstudio".to_string() }
fn default_true()            -> bool    { true }

fn default_ollama_url()      -> String  { "http://localhost:11434".to_string() }
fn default_ollama_model()    -> String  { "llama3.2".to_string() }
fn default_ollama_timeout()  -> u64     { 120 }

// LM Studio's OpenAI-compatible API is served under `/v1`. Include it in the
// default so the stored config is self-consistent and the chat/model endpoints
// resolve correctly. (The client also normalizes a `/v1`-less URL, so existing
// configs and hand-typed URLs without it still work.)
fn default_lmstudio_url()    -> String  { "http://localhost:1234/v1".to_string() }
fn default_lmstudio_model()  -> String  { "local-model".to_string() }
fn default_lmstudio_timeout()-> u64     { 300 }

fn default_openrouter_url()  -> String  { "https://openrouter.ai/api/v1".to_string() }
fn default_openrouter_model()-> String  { "meta-llama/llama-3.2-3b-instruct".to_string() }
fn default_openrouter_catalog_refresh_hours() -> u64 { 24 }

fn default_prompt()          -> String  { "> ".to_string() }
fn default_tui_theme()       -> String  { "mira-dark".to_string() }
fn default_tui_layout()      -> String  { "standard".to_string() }
fn default_tui_mode()        -> String  { "auto".to_string() }
fn default_tui_server_url()  -> String  { "http://127.0.0.1:8082".to_string() }
fn default_tui_token_path()  -> String  { "~/.mira/data/local.token".to_string() }

fn default_server_host()     -> String  { "127.0.0.1".to_string() }
fn default_server_port()     -> u16     { 8080 }
fn default_web_apps_enabled()     -> bool   { true }
fn default_web_apps_mode()        -> String { "subdomain".to_string() }
fn default_web_app_host_suffix()  -> String { "localhost".to_string() }
fn default_max_connections() -> u32     { 100 }
fn default_request_timeout() -> u32     { 30 }

fn default_signal_port()     -> u16     { 8080 }
fn default_signal_socket()   -> String  { "/run/signald/signald.sock".to_string() }
fn default_signal_binary()   -> String  { "signal-cli".to_string() }
fn default_signal_data_dir() -> String  {
    dirs::home_dir()
        .map(|h| h.join(".local/share/signal-cli").to_string_lossy().to_string())
        .unwrap_or_else(|| "~/.local/share/signal-cli".to_string())
}

fn default_log_level()       -> String  { "info".to_string() }
fn default_log_format()      -> String  { "compact".to_string() }
pub(crate) fn default_log_file() -> String  { "~/.mira/logs/mira.log".to_string() }
fn default_log_max_size()    -> u32     { 10 }
fn default_log_max_files()   -> u32     { 5 }

fn default_vector_backend()      -> String  { "sqlite".to_string() }
// Default to the in-process fastembed engine: no external server, no model to
// load in LM Studio, works on a fresh install with no setup. (The previous
// lmstudio default named a model that doesn't exist in a stock LM Studio, so
// memory silently fell back anyway.) BGE-small is 384-dim, matching
// default_embedding_dim below.
fn default_embedding_provider()  -> String  { "internal".to_string() }
fn default_embedding_model()     -> String  { "BGE-small-en-v1.5".to_string() }
fn default_model_cache_dir()     -> String  { "~/.mira/models".to_string() }
fn default_embedding_dim()       -> usize   { 384 }
fn default_embedding_cache_size()-> usize   { 1000 }
fn default_similarity_threshold()-> f32     { 0.6 }
fn default_context_top_k()       -> usize   { 15 }
fn default_qdrant_url()          -> String  { "http://localhost:6333".to_string() }

fn default_cleanup_interval()    -> u64     { 600 }
fn default_session_timeout()     -> u64     { 3600 }
fn default_max_turns()           -> usize   { 50 }

fn default_max_tool_rounds()     -> usize   { 8 }
fn default_tool_mode()           -> String  { "auto".to_string() }
fn default_max_context_turns()   -> usize   { 20 }
fn default_context_safety_margin() -> usize { 2048 }
fn default_compaction_enabled()    -> bool  { true }
fn default_keep_last_turns()       -> usize { 6 }
fn default_max_summary_tokens()    -> usize { 1024 }

/// Phase-2 auto-compaction settings (`agent.compaction`). Compaction only
/// runs when token budgeting is active (`agent.context_length_tokens > 0`)
/// AND the oldest turns overflow the window; otherwise it's inert, so these
/// defaults preserve today's behaviour.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// Master switch. When off, overflowing turns are dropped (Phase-1
    /// behaviour) instead of compacted. Default true.
    #[serde(default = "default_compaction_enabled")]
    pub enabled: bool,

    /// How many of the most recent turns (1 turn = user + assistant) are kept
    /// verbatim and never summarized, so recent detail is preserved exactly.
    /// Default 6.
    #[serde(default = "default_keep_last_turns")]
    pub keep_last_turns: usize,

    /// Model used to produce the summary. Empty = use the cheap classifier
    /// provider when one is configured, else the primary model. A named model
    /// is reserved for a future per-model resolver; today a non-empty value
    /// behaves the same as empty (documented in settings-reference).
    #[serde(default)]
    pub summary_model: String,

    /// Soft cap on the rolling summary's size in tokens; the summarizer is
    /// asked to stay within it so the compacted block itself can't grow
    /// unbounded. Default 1024.
    #[serde(default = "default_max_summary_tokens")]
    pub max_summary_tokens: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled:            default_compaction_enabled(),
            keep_last_turns:    default_keep_last_turns(),
            summary_model:      String::new(),
            max_summary_tokens: default_max_summary_tokens(),
        }
    }
}
fn default_max_tool_round_tokens() -> u32   { 2048 }
fn default_max_response_tokens()   -> u32   { 16384 }

fn default_rate_limit_rpm()      -> u32     { 60 }
fn default_session_days()        -> u64     { 7 }

fn default_tts_backend()              -> String { "internal".to_string() }
// Empty so unset per-channel routes fall through to `tts.default_backend`.
// Pinning to "internal" by default made the Settings UI's default-backend
// dropdown a no-op for the chat 🔊 button — the route always won.
fn default_tts_route()                -> String { String::new() }
fn default_tts_speed()                -> f32    { 1.0 }
fn default_tts_format()               -> String { "wav".to_string() }
fn default_tts_max_chars()            -> usize  { 4000 }
fn default_tts_request_timeout()      -> u64    { 180 }
fn default_tts_cache_max_disk_mb()    -> u64    { 100 }
fn default_tts_cache_ttl_days()       -> u64    { 30 }
fn default_tts_internal_engine()      -> String { "piper".to_string() }
fn default_tts_internal_voice()       -> String { "en_US-amy-medium".to_string() }
fn default_tts_openai_url()           -> String { "https://api.openai.com/v1".to_string() }
fn default_tts_openai_model()         -> String { "tts-1".to_string() }
fn default_tts_openai_voice()         -> String { "alloy".to_string() }
fn default_tts_openai_compat_url()    -> String { "http://localhost:8000/v1".to_string() }
fn default_tts_elevenlabs_model()     -> String { "eleven_turbo_v2_5".to_string() }
fn default_tts_elevenlabs_voice()     -> String { "21m00Tcm4TlvDq8ikWAM".to_string() }
fn default_tts_cartesia_model()       -> String { "sonic-english".to_string() }
// Per-backend playback gain. 1.0 = unaltered. The web client applies it via
// a Web Audio API GainNode at playback time; other channels currently
// ignore it.
fn default_tts_volume()               -> f32    { 1.0 }

fn default_stt_backend()              -> String { "internal".to_string() }
fn default_stt_route()                -> String { String::new() }
fn default_stt_max_audio_seconds()    -> u32    { 300 }
fn default_stt_request_timeout()      -> u64    { 60 }
fn default_stt_internal_model()       -> String { "base.en".to_string() }
// OpenAI's hosted Whisper transcription model id — `whisper-1` is the only
// one publicly available right now; if that changes, the user can override
// in the Voice settings tab.
fn default_stt_openai_model()         -> String { "whisper-1".to_string() }
// Self-hosted whisper-cpp servers conventionally listen on port 8080;
// faster-whisper-server defaults to 8000. Both speak the OpenAI shape at
// `/v1/audio/transcriptions`. We pick 8080 to avoid clashing with the TTS
// `openai_compat` placeholder (which uses 8000).
fn default_stt_openai_compat_url()    -> String { "http://localhost:8080/v1".to_string() }
fn default_stt_openai_compat_model()  -> String { "whisper-1".to_string() }

fn default_nginx_binary()        -> String  { "/usr/sbin/nginx".to_string() }
fn default_nginx_config_path()   -> String  { "~/.mira/nginx/nginx.conf".to_string() }
fn default_nginx_pid_path()      -> String  { "~/.mira/nginx/nginx.pid".to_string() }
fn default_nginx_workers()       -> String  { "auto".to_string() }
fn default_tls_port()            -> u16     { 443 }

// ── Path helpers ──────────────────────────────────────────────────────────────

// Returns `~/.mira/config/` on all platforms.
// // We deliberately stay under `~/.mira/` rather than the XDG `~/.config/`
// tree to keep every MIRA artefact (data, config, logs, models) in one
// place and avoid scattering files across the file system.
pub fn default_config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mira")
        .join("config")
}

// Full path to the live config file.
pub fn default_config_path() -> PathBuf {
    default_config_dir().join("mira_config.json")
}

// Default data dir path, resolved without loading the config. Used
// by pre-config hooks (Q1.5 startup restore swap) that need to touch
// data files before any config-driven path can be trusted. The
// String-returning `default_data_dir()` further down is the legacy
// serde default for the `MiraConfig.data_dir` field.
pub fn default_data_dir_path() -> PathBuf {
    if let Some(d) = data_dir_env_override() {
        return d;
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mira")
        .join("data")
}

// The `MIRA_DATA_DIR` runtime override. When set (and non-empty) it wins over
// the config `data_dir` field and the built-in default; `~` is expanded.
// // Set it directly, or indirectly via the global `--data-dir` flag (which `main`
// wires into this env so one resolver covers the flag, the env, and the service
// launch args). This is the mechanism that keeps a *supervised* service —
// systemd `--system` (runs as the `mira` user), a launchd agent, or a Windows
// LocalSystem service — reading the SAME data dir the installer chose, instead
// of re-expanding `~` against whatever account the supervisor runs it under
// (which is how the data dir used to silently diverge from where setup wrote).
pub fn data_dir_env_override() -> Option<PathBuf> {
    std::env::var("MIRA_DATA_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| expand_path(&v))
}

/// Process-wide lock any test that sets/clears `MIRA_DATA_DIR` must hold for the
/// duration of its env mutation. `MIRA_DATA_DIR` is global mutable state, and
/// cargo runs tests in parallel threads within one process — without this, a
/// test asserting the default layout can observe an override another test set
/// mid-run (and vice-versa). Acquire it as the first line of any such test:
/// `let _env = crate::config::ENV_TEST_LOCK.lock().unwrap();`
#[cfg(test)]
pub(crate) static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// Expand a leading `~` to the home directory.
pub fn expand_path(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(rest)
    } else if s == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
    } else {
        PathBuf::from(s)
    }
}

// Resolve a config path that may be a `~/.mira/...` default naming MIRA's own
// state (logs, the model cache, the local-TUI token, …).
// // When a data-dir override is active (`MIRA_DATA_DIR` / the `--data-dir` flag —
// i.e. a relocated or service-baked data dir), a `~/.mira/<rest>` value resolves
// under that dir's *parent* (the "MIRA home") so this state follows the data dir
// instead of being re-expanded against the runtime user's `~`. That `~` diverges
// for a supervised service — Windows LocalSystem → `…\systemprofile\.mira\…`,
// systemd `--system` → the `mira` user — scattering logs/cache where the operator
// can't find them. With NO override (the normal same-user case) this is identical
// to [`expand_path`], so the default layout (`~/.mira/logs`, `~/.mira/models`, …)
// is unchanged. A non-`~/.mira` value (an explicit absolute/custom path) is always
// honored as-is.
pub fn resolve_state_path(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/.mira/") {
        if let Some(home) = data_dir_env_override().as_deref().and_then(|d| d.parent()) {
            return home.join(rest);
        }
    }
    expand_path(p)
}

// ── Load / save ───────────────────────────────────────────────────────────────

impl MiraConfig {
    // ── Default ──────────────────────────────────────────────────────────────

    // Construct a default config with the standard config path set.
    pub fn default_with_path() -> Self {
        let mut cfg = Self::default();
        cfg.config_path = default_config_path();
        cfg
    }

    // ── Load ─────────────────────────────────────────────────────────────────

    // Load configuration.
    //     // Order of precedence:
    // 1. `override_path` — explicit `--config` argument
    // 2. Default location (`~/.mira/config/mira_config.json`)
    //     // On first run the config is created from defaults. If a legacy
    // `~/.mira/config.toml` is found the user is offered a migration.
    pub fn load(override_path: Option<PathBuf>) -> Result<Self, MiraError> {
        let config_path = override_path.unwrap_or_else(default_config_path);

        if config_path.exists() {
            Self::from_file(&config_path)
        } else {
            Self::first_run(&config_path)
        }
    }

    // Parse and validate a config file at `path`.
    pub fn from_file(path: &Path) -> Result<Self, MiraError> {
        info!("Loading config from {:?}", path);

        let content = std::fs::read_to_string(path)
            .map_err(|e| MiraError::ConfigError(
                format!("Cannot read config file '{}': {}", path.display(), e)
            ))?;

        // Parse JSON
        let json_value: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| MiraError::ConfigError(
                format!(
                    "Config file '{}' is not valid JSON:\n  {}",
                    path.display(), e
                )
            ))?;

        // Validate against embedded schema
        if let Err(errors) = validate_config_json(&json_value) {
            let joined = errors.join("\n");
            return Err(MiraError::ConfigError(format!(
                "Config file '{}' failed schema validation:\n{}",
                path.display(),
                joined
            )));
        }

        // Deserialise into struct
        let mut cfg: MiraConfig = serde_json::from_value(json_value)
            .map_err(|e| MiraError::ConfigError(
                format!("Cannot deserialise config '{}': {}", path.display(), e)
            ))?;

        cfg.config_path = path.to_path_buf();

        // Apply environment variable overrides
        cfg.apply_env_overrides();

        info!("Config loaded successfully from {:?}", path);
        Ok(cfg)
    }

    // First-run setup: check for legacy TOML, then create a new config.
    fn first_run(config_path: &Path) -> Result<Self, MiraError> {
        // Check for legacy TOML and offer migration
        let cfg = if let Some(toml_path) = find_legacy_toml() {
            prompt_and_migrate(&toml_path, config_path).unwrap_or_else(MiraConfig::default_with_path)
        } else {
            eprintln!();
            eprintln!("─────────────────────────────────────────────────────────");
            eprintln!("  MIRA: First run — creating configuration file");
            eprintln!("─────────────────────────────────────────────────────────");
            MiraConfig::default_with_path()
        };

        let mut cfg = cfg;
        cfg.config_path = config_path.to_path_buf();
        cfg.save()?;

        // Write the example template alongside the live config
        Self::write_example_template(config_path.parent().unwrap_or(Path::new(".")))?;

        eprintln!("  Config : {}", config_path.display());
        eprintln!("  Example: {}", config_path.with_file_name("mira_config.example.json").display());
        eprintln!("─────────────────────────────────────────────────────────");
        eprintln!();

        Ok(cfg)
    }

    // ── Save ─────────────────────────────────────────────────────────────────

    // Serialise and write the config to `self.config_path`.
    pub fn save(&self) -> Result<(), MiraError> {
        let dir = self.config_path.parent()
            .ok_or_else(|| MiraError::ConfigError("Config path has no parent directory".into()))?;

        std::fs::create_dir_all(dir)
            .map_err(|e| MiraError::ConfigError(
                format!("Cannot create config directory '{}': {}", dir.display(), e)
            ))?;

        let json = serde_json::to_string_pretty(self)
            .map_err(|e| MiraError::ConfigError(format!("Cannot serialise config: {}", e)))?;

        std::fs::write(&self.config_path, json)
            .map_err(|e| MiraError::ConfigError(
                format!("Cannot write config file '{}': {}", self.config_path.display(), e)
            ))?;

        info!("Config saved to {:?}", self.config_path);
        Ok(())
    }

    // ── Example template ─────────────────────────────────────────────────────

    // Write `mira_config.example.json` into `dir` (usually the config dir).
    pub fn write_example_template(dir: &Path) -> Result<(), MiraError> {
        let dest = dir.join("mira_config.example.json");
        std::fs::write(&dest, EXAMPLE_JSONC)
            .map_err(|e| MiraError::ConfigError(
                format!("Cannot write example template '{}': {}", dest.display(), e)
            ))?;
        info!("Example template written to {:?}", dest);
        Ok(())
    }

    /// The `(provider slug, model id)` a new chat uses when the caller hasn't
    /// picked one — the configured `primary_provider`'s **default model**.
    /// Falls back to LMStudio's default (the historical hard-wired default)
    /// when the primary provider isn't a known slug or has no default set.
    pub fn default_chat_model(&self) -> (String, String) {
        let primary = self.primary_provider.trim();
        if let Some(m) = self.provider_default_model(primary) {
            let m = m.trim().to_string();
            if !m.is_empty() {
                return (primary.to_string(), m);
            }
        }
        ("lmstudio".to_string(), self.providers.lmstudio.default_model.clone())
    }

    /// A provider slug's configured `default_model`, or `None` if the slug
    /// isn't a known built-in provider. The `openai_compat` catch-all matches
    /// on its user-set `name`.
    pub fn provider_default_model(&self, slug: &str) -> Option<String> {
        let p = &self.providers;
        let m = match slug {
            "ollama"     => &p.ollama.default_model,
            "lmstudio"   => &p.lmstudio.default_model,
            "openrouter" => &p.openrouter.default_model,
            "openai"     => &p.openai.default_model,
            "deepseek"   => &p.deepseek.default_model,
            "moonshot"   => &p.moonshot.default_model,
            "groq"       => &p.groq.default_model,
            "xai"        => &p.xai.default_model,
            "anthropic"  => &p.anthropic.default_model,
            "gemini"     => &p.gemini.default_model,
            other if other == p.openai_compat.name => &p.openai_compat.default_model,
            _ => return None,
        };
        Some(m.clone())
    }

    // ── Environment overrides ─────────────────────────────────────────────────

    fn apply_env_overrides(&mut self) {
        if let Ok(key) = std::env::var("OPENROUTER_API_KEY") {
            if !key.is_empty() {
                self.providers.openrouter.api_key = Some(key);
                info!("OpenRouter API key loaded from OPENROUTER_API_KEY env var");
            }
        }
        if let Ok(url) = std::env::var("OLLAMA_HOST") {
            if !url.is_empty() {
                self.providers.ollama.url = url;
                info!("Ollama URL overridden by OLLAMA_HOST env var");
            }
        }
        if let Ok(url) = std::env::var("MIRA_REMOTE_URL") {
            let url = url.trim().to_string();
            if !url.is_empty() {
                self.server.remote_url = Some(url);
                info!("Remote access URL set from MIRA_REMOTE_URL env var");
            }
        }
    }

    // ── Convenience accessors ─────────────────────────────────────────────────

    // Returns the resolved data directory as a `PathBuf`. The `MIRA_DATA_DIR`
    // env / `--data-dir` flag override wins; otherwise the config `data_dir`
    // field is used (with `~` expanded). See [`data_dir_env_override`].
    pub fn data_dir_path(&self) -> PathBuf {
        data_dir_env_override().unwrap_or_else(|| expand_path(&self.data_dir))
    }

    // Resolved artifacts root. Follows the data dir for a relocated/service
    // install (like log_file_path) so a LocalSystem service's artifacts land
    // next to its data, not under the supervisor account's `~`.
    pub fn artifacts_root_path(&self) -> PathBuf {
        resolve_state_path(&self.artifacts.root_dir)
    }

    // Returns the resolved log file path. Follows the data dir for a relocated
    // or service-baked install (see [`resolve_state_path`]) so a supervised
    // service's logs land next to its data, not under the supervisor account's
    // `~`.
    pub fn log_file_path(&self) -> PathBuf {
        resolve_state_path(&self.logging.file)
    }
}

impl Default for MiraConfig {
    fn default() -> Self {
        Self {
            config_path:     default_config_path(),
            config_version:  default_config_version(),
            data_dir:        default_data_dir(),
            primary_provider:default_primary_provider(),
            failover_providers: None,
            providers:       ProvidersConfig::default(),
            cli:             CliConfig::default(),
            tui:             TuiConfig::default(),
            server:          ServerConfig::default(),
            channels:        ChannelsConfig::default(),
            logging:         LoggingConfig::default(),
            memory:          MemoryConfig::default(),
            session:         SessionConfig::default(),
            agent:           AgentConfig::default(),
            guardian:        GuardianConfig::default(),
            security:        SecurityPolicyConfig::default(),
            proxy:           ProxyConfig::default(),
            calendar:        CalendarConfig::default(),
            sandbox:         SandboxConfig::default(),
            tts:             TtsConfig::default(),
            stt:             SttConfig::default(),
            automations:     AutomationsConfig::default(),
            companion:       CompanionConfig::default(),
            artifacts:       ArtifactsConfig::default(),
            wiki:            WikiConfig::default(),
            mcp:             McpConfig::default(),
            email_oauth:     EmailOAuthConfig::default(),
            auth:            AuthConfig::default(),
            system_email:    SystemEmailConfig::default(),
            backup:          BackupConfig::default(),
            notifications:   NotificationsConfig::default(),
            weather:         WeatherConfig::default(),
            image:           ImageConfig::default(),
            video:           VideoConfig::default(),
        }
    }
}

// ── Weather (built-in tool, 0.284.0) ────────────────────────────────────

/// Built-in weather. Defaults to keyless **Open-Meteo** (global, no API key,
/// includes free geocoding). An admin may switch to a keyed provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeatherConfig {
    /// "open_meteo" (default, keyless) | "openweathermap" (needs api_key).
    #[serde(default = "default_weather_provider")]
    pub provider: String,
    /// API key for keyed providers (openweathermap). Treated as a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// "metric" (°C, mm, km/h — default) | "imperial" (°F, in, mph).
    #[serde(default = "default_weather_units")]
    pub units: String,
}

fn default_weather_provider() -> String { "open_meteo".to_string() }
fn default_weather_units() -> String { "metric".to_string() }

impl Default for WeatherConfig {
    fn default() -> Self {
        Self {
            provider: default_weather_provider(),
            api_key:  None,
            units:    default_weather_units(),
        }
    }
}

// ── Image generation (0.292.0) ──────────────────────────────────────────
/// Image-generation backends. The `image_generate` tool dispatches through a
/// router (mirroring the TTS subsystem): one `default_backend`, plus
/// per-backend config. Backends: `openai` (uses `providers.openai`, on when a
/// key resolves), `automatic1111` (local SD WebUI), `comfyui` (local ComfyUI).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImageConfig {
    /// Which backend to use by default. Empty/`auto` = first enabled, preferring
    /// a configured local backend, else OpenAI. One of: `openai` |
    /// `automatic1111` | `comfyui`.
    #[serde(default)]
    pub default_backend: String,
    /// OpenAI Images (or OpenAI-compatible) backend settings. Key + endpoint
    /// come from `providers.openai`; this just picks the model.
    #[serde(default)]
    pub openai: ImageOpenAiConfig,
    /// Local Automatic1111 / Stable Diffusion WebUI backend.
    #[serde(default)]
    pub automatic1111: Automatic1111Config,
    /// Local ComfyUI backend.
    #[serde(default)]
    pub comfyui: ComfyUiConfig,
}

/// OpenAI image backend knobs. The API key + base URL live under
/// `providers.openai` (shared with chat/embeddings); only the default image
/// model is image-specific.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageOpenAiConfig {
    /// Default image model, e.g. `dall-e-3` or `gpt-image-1`.
    #[serde(default = "default_openai_image_model")]
    pub default_model: String,
}

fn default_openai_image_model() -> String { "dall-e-3".to_string() }

impl Default for ImageOpenAiConfig {
    fn default() -> Self { Self { default_model: default_openai_image_model() } }
}

/// Automatic1111 / SD WebUI (`/sdapi/v1/txt2img`). Local, no key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Automatic1111Config {
    #[serde(default)]
    pub enabled: bool,
    /// Base URL of the WebUI, e.g. `http://127.0.0.1:7860`.
    #[serde(default = "default_a1111_url")]
    pub base_url: String,
    /// Optional checkpoint to switch to (override_settings.sd_model_checkpoint).
    /// Empty = leave the WebUI's current model.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default = "default_a1111_steps")]
    pub steps: u32,
    #[serde(default = "default_a1111_sampler")]
    pub sampler: String,
    #[serde(default = "default_image_dim")]
    pub width: u32,
    #[serde(default = "default_image_dim")]
    pub height: u32,
    #[serde(default = "default_a1111_cfg")]
    pub cfg_scale: f32,
    /// Default negative prompt applied when the call doesn't pass one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub negative_prompt: String,
}

fn default_a1111_url() -> String { "http://127.0.0.1:7860".to_string() }
fn default_a1111_steps() -> u32 { 25 }
fn default_a1111_sampler() -> String { "Euler a".to_string() }
fn default_a1111_cfg() -> f32 { 7.0 }
fn default_image_dim() -> u32 { 1024 }

impl Default for Automatic1111Config {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: default_a1111_url(),
            model: String::new(),
            steps: default_a1111_steps(),
            sampler: default_a1111_sampler(),
            width: default_image_dim(),
            height: default_image_dim(),
            cfg_scale: default_a1111_cfg(),
            negative_prompt: String::new(),
        }
    }
}

/// ComfyUI (`POST /prompt` with a workflow graph → poll `/history` → fetch
/// `/view`). Local, no key. The workflow is a ComfyUI **API-format** JSON with
/// placeholder tokens the backend substitutes: `{{prompt}}`, `{{negative}}`,
/// `{{seed}}`, `{{width}}`, `{{height}}`, `{{steps}}`, `{{cfg}}`, `{{ckpt}}`.
/// Empty = use the built-in default SD txt2img workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComfyUiConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Base URL, e.g. `http://127.0.0.1:8188`.
    #[serde(default = "default_comfyui_url")]
    pub base_url: String,
    /// Inline workflow JSON (API format) with placeholder tokens. Empty = the
    /// built-in default workflow.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workflow_json: String,
    /// Checkpoint filename for the default workflow's `{{ckpt}}` (e.g.
    /// `sd_xl_base_1.0.safetensors`). Ignored when a custom `workflow_json`
    /// hardcodes its own checkpoint.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default = "default_comfy_steps")]
    pub steps: u32,
    #[serde(default = "default_image_dim")]
    pub width: u32,
    #[serde(default = "default_image_dim")]
    pub height: u32,
    #[serde(default = "default_comfy_cfg")]
    pub cfg_scale: f32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub negative_prompt: String,
}

fn default_comfyui_url() -> String { "http://127.0.0.1:8188".to_string() }
fn default_comfy_steps() -> u32 { 20 }
fn default_comfy_cfg() -> f32 { 7.0 }

impl Default for ComfyUiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: default_comfyui_url(),
            workflow_json: String::new(),
            model: String::new(),
            steps: default_comfy_steps(),
            width: default_image_dim(),
            height: default_image_dim(),
            cfg_scale: default_comfy_cfg(),
            negative_prompt: String::new(),
        }
    }
}

// ── Video generation (0.292.0) ──────────────────────────────────────────
/// Video generation. Today: OpenAI Videos / Sora (key + endpoint from
/// `providers.openai`). `default_backend` is reserved for future local
/// backends (e.g. ComfyUI video workflows).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VideoConfig {
    /// Default backend. Empty/`auto` = first enabled (local preferred). One of:
    /// `openai` | `comfyui` | `wan2gp`.
    #[serde(default)]
    pub default_backend: String,
    /// OpenAI Videos (Sora) settings.
    #[serde(default)]
    pub openai: VideoOpenAiConfig,
    /// Local ComfyUI video backend (a video workflow over the same /prompt API).
    #[serde(default)]
    pub comfyui: VideoComfyUiConfig,
    /// Local WAN2GP (Gradio) video backend.
    #[serde(default)]
    pub wan2gp: Wan2gpConfig,
}

/// Local ComfyUI **video** backend. Unlike images, there's no universal default
/// workflow — you supply a ComfyUI API-format video workflow (Wan / AnimateDiff
/// / SVD, ending in a video-combine node) with placeholder tokens: `{{prompt}}`
/// `{{negative}}` `{{seed}}` `{{width}}` `{{height}}` `{{frames}}` `{{fps}}`
/// `{{steps}}` `{{cfg}}` `{{ckpt}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoComfyUiConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_comfyui_url")]
    pub base_url: String,
    /// REQUIRED: the video workflow JSON (API format) with placeholder tokens.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workflow_json: String,
    /// Checkpoint/model for `{{ckpt}}` (workflow-dependent).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default = "default_comfy_steps")]
    pub steps: u32,
    #[serde(default = "default_video_dim")]
    pub width: u32,
    #[serde(default = "default_video_dim")]
    pub height: u32,
    /// Frames per second; `{{frames}}` = seconds × fps.
    #[serde(default = "default_video_fps")]
    pub fps: u32,
    #[serde(default = "default_comfy_cfg")]
    pub cfg_scale: f32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub negative_prompt: String,
}

fn default_video_dim() -> u32 { 512 }
fn default_video_fps() -> u32 { 16 }

impl Default for VideoComfyUiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: default_comfyui_url(),
            workflow_json: String::new(),
            model: String::new(),
            steps: default_comfy_steps(),
            width: default_video_dim(),
            height: default_video_dim(),
            fps: default_video_fps(),
            cfg_scale: default_comfy_cfg(),
            negative_prompt: String::new(),
        }
    }
}

/// Local WAN2GP (deepbeepmeep's Wan2GP) — a Gradio app for Wan video models.
/// MIRA drives its Gradio API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Wan2gpConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Base URL of the Gradio app, e.g. `http://127.0.0.1:7862`.
    #[serde(default = "default_wan2gp_url")]
    pub base_url: String,
    /// Gradio API endpoint name to call (the named `api_name`, e.g.
    /// `/generate_video`). Discoverable from the app's `/config`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_name: String,
}

fn default_wan2gp_url() -> String { "http://127.0.0.1:7862".to_string() }

impl Default for Wan2gpConfig {
    fn default() -> Self {
        Self { enabled: false, base_url: default_wan2gp_url(), api_name: String::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoOpenAiConfig {
    /// Default video model, e.g. `sora-2` or `sora-2-pro`.
    #[serde(default = "default_video_model")]
    pub default_model: String,
    /// Default frame size `WIDTHxHEIGHT` (larger sizes need sora-2-pro).
    #[serde(default = "default_video_size")]
    pub default_size: String,
    /// Default clip length in seconds.
    #[serde(default = "default_video_seconds")]
    pub default_seconds: u32,
}

fn default_video_model() -> String { "sora-2".to_string() }
fn default_video_size() -> String { "1280x720".to_string() }
fn default_video_seconds() -> u32 { 4 }

impl Default for VideoOpenAiConfig {
    fn default() -> Self {
        Self {
            default_model:   default_video_model(),
            default_size:    default_video_size(),
            default_seconds: default_video_seconds(),
        }
    }
}

// ── Notifications (proactive push transports, 0.282.0) ──────────────────

/// Proactive-notification transports. Web Push (VAPID) is always on; this
/// section adds opt-in Firebase Cloud Messaging for the native mobile app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationsConfig {
    #[serde(default)]
    pub fcm: FcmConfig,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self { fcm: FcmConfig::default() }
    }
}

/// Firebase Cloud Messaging (HTTP v1). Off by default. When enabled, the
/// notification dispatcher also fans proactive events out to registered
/// FCM device tokens. Auth is an OAuth2 service-account JWT minted from
/// the service-account JSON — that file is a secret (redacted in the API).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FcmConfig {
    /// Master switch. `false` → behaves exactly as before (web push only).
    #[serde(default)]
    pub enabled: bool,
    /// Firebase project id (the `project_id` in the service-account JSON).
    /// Required when `enabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Filesystem path to the Google service-account JSON. Required when
    /// `enabled`. The file itself is the credential — keep it readable
    /// only by the MIRA process user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_account_json_path: Option<String>,
}

impl Default for FcmConfig {
    fn default() -> Self {
        Self { enabled: false, project_id: None, service_account_json_path: None }
    }
}

// ── MCP host (Q2 #7, stdio) ─────────────────────────────────────────

// Registry of external MCP servers MIRA connects to as a client at
// startup. Each enabled server's tools are exposed under
// `mcp__<server_name>__<tool_name>`, so a builtin tool with the same
// name never collides with a remote one. Disabled servers are kept in
// the file (toggle, don't delete) so credentials survive a quick
// on/off round-trip from the Settings UI.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

// One MCP server entry. The `transport` field picks between:
// - `"stdio"` (default): MIRA spawns a child process described by
// `command` + `args` + `env` and speaks JSON-RPC over its stdio.
// - `"http"`: MIRA connects to a remote Streamable-HTTP
// MCP endpoint at `url`. Used for cloud-hosted servers like the
// Notion or GitHub remote endpoints.
// // `name` doubles as both the user-facing label and the tool-namespace
// prefix (`mcp__<name>__<tool>`), so keep it short and unique.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name:    String,

    // `"stdio"` (default, requires `command`) or `"http"` (requires
    // `url`). Anything else is rejected at connect time with a
    // per-server error visible in `/api/mcp/status`.
    #[serde(default = "default_mcp_transport")]
    pub transport: String,

    // Stdio-only: executable to spawn. Ignored when transport=http.
    // Optional in the struct so HTTP entries don't need a placeholder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub args:    Vec<String>,
    #[serde(default)]
    pub env:     std::collections::HashMap<String, String>,

    // HTTP-only: full URL of the Streamable-HTTP MCP endpoint, e.g.
    // `https://mcp.example.com/v1`. Ignored when transport=stdio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url:     Option<String>,

    // Per-server kill switch. Disabling skips the connect on startup
    // (tools disappear from the registry until re-enable + restart).
    #[serde(default = "default_true")]
    pub enabled: bool,

    // opt-in to MCP **sampling**: server-initiated
    // `sampling/createMessage` requests that ask MIRA to make an LLM
    // call on the server's behalf. Off by default because third-
    // party MCP servers running with sampling on can burn through
    // the user's provider quota; the user must explicitly enable
    // per server. When false, MIRA doesn't advertise the sampling
    // capability at initialize time, so well-behaved servers won't
    // even try.
    #[serde(default)]
    pub sampling_enabled: bool,
}

fn default_mcp_transport() -> String { "stdio".to_string() }

// ── Email OAuth (Q2 #8 E4) ──────────────────────────────────────────────────

// OAuth client configuration for the email channel. The operator
// registers a "Desktop / Public client" app at Google Cloud Console
// (Gmail) and another at Azure Portal / Entra ID (Outlook + 365),
// then drops the resulting `client_id` values here. PKCE flow means
// no client_secret is stored or required — proof-of-possession
// happens via the per-flow code challenge.
// // `public_base_url` is what MIRA prepends to the callback path when
// building the OAuth redirect URI. Must exactly match the redirect
// URI registered at the provider (case-sensitive, trailing slash
// matters). Self-hosted MIRA running on localhost uses
// `http://127.0.0.1:8082`; reverse-proxied deployments set their
// public origin (e.g. `https://mira.example.com`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EmailOAuthConfig {
    // Origin (scheme + host + port) MIRA serves on, as reachable
    // from the user's browser. Used to build the OAuth redirect
    // URI — exact match required at the provider. Empty defaults
    // to `http://127.0.0.1:<server.port>` at runtime.
    #[serde(default)]
    pub public_base_url: String,

    // Google Cloud OAuth `client_id` for the Gmail integration.
    // Empty disables the "Connect Gmail" UI button.
    #[serde(default)]
    pub google_client_id: String,

    // Microsoft Entra ID OAuth `client_id` for the Outlook /
    // Microsoft 365 integration. Empty disables the
    // "Connect Outlook" UI button.
    #[serde(default)]
    pub microsoft_client_id: String,
}

// ── SSO / OIDC web login ────────────────────────────────────────────────────

// Top-level auth config. Today only carries OIDC; future auth knobs
// (session lifetime overrides, LDAP, …) nest here too.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthConfig {
    #[serde(default)]
    pub oidc: OidcConfig,

    #[serde(default)]
    pub signup: SignupConfig,

    #[serde(default)]
    pub ldap: LdapConfig,
}

// LDAP / Active Directory authentication (Q2 #11). When enabled, a username
// + password that fails local auth is tried against the directory via
// search-then-bind. Local accounts (incl. the bootstrap admin) always work,
// so a directory outage never locks everyone out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LdapConfig {
    #[serde(default)]
    pub enabled: bool,

    // Directory URL, e.g. `ldap://dc.example.com:389` or
    // `ldaps://dc.example.com:636`.
    #[serde(default)]
    pub url: String,

    // Upgrade a plaintext `ldap://` connection to TLS via STARTTLS before
    // binding. Ignored for `ldaps://`.
    #[serde(default)]
    pub starttls: bool,

    // Service-account DN used to search for the user before binding as them.
    // Empty → anonymous search.
    #[serde(default)]
    pub bind_dn: String,
    // Password for `bind_dn`.
    #[serde(default)]
    pub bind_password: String,

    // Base DN to search under, e.g. `ou=people,dc=example,dc=com`.
    #[serde(default)]
    pub user_base_dn: String,
    // User search filter; `{username}` is substituted (and LDAP-escaped).
    // Defaults to `(uid={username})`; AD typically uses
    // `(sAMAccountName={username})`.
    #[serde(default = "default_ldap_filter")]
    pub user_filter: String,

    // Attribute carrying the user's email. Default `mail`.
    #[serde(default = "default_ldap_attr_email")]
    pub attr_email: String,
    // Attribute carrying the display name. Default `cn`.
    #[serde(default = "default_ldap_attr_display")]
    pub attr_display_name: String,

    // If set, the user must be a member of this group DN (checked via the
    // user's `memberOf` attribute). Empty = no group requirement.
    #[serde(default)]
    pub required_group: String,

    // Create a MIRA account on first successful LDAP login for a user with
    // no existing match. Off by default (link-to-existing only).
    #[serde(default)]
    pub auto_provision: bool,
    // When auto-provisioning, only allow these email domains. Empty = any.
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    // Role for auto-provisioned users: "user" (default) or "admin".
    #[serde(default = "default_signup_role")]
    pub default_role: String,
}

impl Default for LdapConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            starttls: false,
            bind_dn: String::new(),
            bind_password: String::new(),
            user_base_dn: String::new(),
            user_filter: default_ldap_filter(),
            attr_email: default_ldap_attr_email(),
            attr_display_name: default_ldap_attr_display(),
            required_group: String::new(),
            auto_provision: false,
            allowed_domains: Vec::new(),
            default_role: "user".into(),
        }
    }
}

fn default_ldap_filter() -> String { "(uid={username})".into() }
fn default_ldap_attr_email() -> String { "mail".into() }
fn default_ldap_attr_display() -> String { "cn".into() }

// Self-service onboarding (Q2 #11). Admin invite links always work
// regardless of these knobs; this governs *open* (un-invited) signup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignupConfig {
    // Allow anyone to create an account from the signup page without an
    // invite. Off by default — onboarding is invite-only until enabled.
    #[serde(default)]
    pub enabled: bool,

    // New open-signup accounts land in a pending state until an admin
    // approves them. On by default — never silently grant access.
    #[serde(default = "default_true")]
    pub require_approval: bool,

    // Role granted to open-signup accounts. "user" (default) or "admin".
    #[serde(default = "default_signup_role")]
    pub default_role: String,

    // When set, open signup only accepts these email domains (lowercase,
    // no `@`). Empty = any domain.
    #[serde(default)]
    pub allowed_domains: Vec<String>,
}

impl Default for SignupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            require_approval: true,
            default_role: "user".into(),
            allowed_domains: Vec::new(),
        }
    }
}

fn default_signup_role() -> String { "user".into() }

// SSO via OpenID Connect. Generic + discovery-driven — one provider
// shape covers Google / Microsoft Entra / Keycloak / Authentik / Okta.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OidcConfig {
    // Master switch. Even with providers listed, OIDC is inert unless on.
    #[serde(default)]
    pub enabled: bool,

    // Origin (scheme + host + port) MIRA is reachable on from the user's
    // browser — used to build the redirect URI
    // (`<base>/api/auth/oidc/callback`), which must match the value
    // registered at the IdP exactly. Empty → `http://127.0.0.1:<port>`.
    #[serde(default)]
    pub public_base_url: String,

    // Configured identity providers. Each renders a login button.
    #[serde(default)]
    pub providers: Vec<OidcProvider>,
}

// One OIDC identity provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcProvider {
    // Stable slug used in URLs + identity binding (e.g. "google").
    pub id: String,

    // Button label, e.g. "Google" or "Company SSO".
    #[serde(default)]
    pub display_name: String,

    // Issuer / discovery base URL. MIRA fetches
    // `<issuer>/.well-known/openid-configuration` to resolve the
    // authorization / token / userinfo endpoints.
    #[serde(default)]
    pub issuer: String,

    // OAuth client id registered at the IdP.
    #[serde(default)]
    pub client_id: String,

    // OAuth client secret. Confidential-client code exchange.
    #[serde(default)]
    pub client_secret: String,

    // Requested scopes. Defaults to `openid email profile` when empty.
    #[serde(default)]
    pub scopes: Vec<String>,

    // Create a MIRA account on first login for a user with no existing
    // match. Off by default (link-to-existing-email only); scope it with
    // `allowed_domains` before turning on.
    #[serde(default)]
    pub auto_provision: bool,

    // When auto-provisioning, only allow these email domains (lowercase,
    // no `@`). Empty = any domain (use with care).
    #[serde(default)]
    pub allowed_domains: Vec<String>,

    // Role granted to auto-provisioned users. "user" (default) or "admin".
    #[serde(default)]
    pub default_role: String,
}

impl OidcProvider {
    // Effective scopes, defaulting to the OIDC basics.
    pub fn effective_scopes(&self) -> Vec<String> {
        if self.scopes.is_empty() {
            vec!["openid".into(), "email".into(), "profile".into()]
        } else {
            self.scopes.clone()
        }
    }
}

// ── System email (Q2 #8 E5) ─────────────────────────────────────────────────

// Application-initiated email config. Distinct from the per-user
// email accounts in `email_accounts` — this is MIRA-the-software
// sending mail (e.g. "your password reset link", "MIRA noticed an
// incident overnight"), not "Tarek's MIRA replying to Mom on
// Tarek's behalf".
// // Disabled by default. Only matters once a feature actually pulls
// it in; until then it's pure infrastructure with no live consumer.
// Password auth only in this slice — OAuth for the system account
// is a later concern (transactional senders like Postmark/
// SendGrid/AWS-SES via SMTP-relay creds cover the common case).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemEmailConfig {
    // Hard switch. Send calls error out when false.
    #[serde(default)]
    pub enabled: bool,

    // `From` address that goes on every outbound. e.g.
    // `mira@example.com` or `noreply@example.com`.
    #[serde(default)]
    pub from_address: String,

    // Display name in the From header. Empty falls back to "MIRA".
    #[serde(default)]
    pub from_name: String,

    #[serde(default)]
    pub smtp_host: String,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    #[serde(default = "default_true")]
    pub smtp_use_tls: bool,
    #[serde(default)]
    pub smtp_username: String,
    #[serde(default)]
    pub smtp_password: String,
}

fn default_smtp_port() -> u16 { 465 }

impl Default for SystemEmailConfig {
    // Manual (not derived) so `Default` matches the serde defaults: a derived
    // Default gives smtp_port = 0 (violates the schema's `minimum: 1`) and
    // smtp_use_tls = false (serde default is true). Keeping them in step means
    // `MiraConfig::default()` validates against the embedded schema.
    fn default() -> Self {
        Self {
            enabled:      false,
            from_address: String::new(),
            from_name:    String::new(),
            smtp_host:    String::new(),
            smtp_port:    default_smtp_port(),
            smtp_use_tls: true,
            smtp_username: String::new(),
            smtp_password: String::new(),
        }
    }
}

// ── Backward-compat type alias ────────────────────────────────────────────────

// Alias kept so that existing code using `Config` continues to compile.
pub type Config = MiraConfig;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_minimal(dir: &TempDir) -> PathBuf {
        let p = dir.path().join("mira_config.json");
        std::fs::write(&p, r#"{"config_version":"1","primary_provider":"lmstudio"}"#).unwrap();
        p
    }

    #[test]
    fn effective_extractor_resolves_per_channel() {
        use ExtractorKind::*;
        let mk = |mode: &str, ch: &[&str]| AutoExtractConfig {
            mode: mode.to_owned(),
            min_confidence: "medium".into(),
            allowed_categories: vec![],
            llm_channels: ch.iter().map(|s| s.to_string()).collect(),
        };
        // off wins everywhere, even for a listed channel.
        assert_eq!(mk("off", &["telegram"]).effective_extractor("telegram"), Off);
        // default heuristic, empty list → heuristic for all.
        assert_eq!(mk("heuristic", &[]).effective_extractor("web"), Heuristic);
        assert_eq!(mk("heuristic", &[]).effective_extractor("telegram"), Heuristic);
        // listed channel → LLM even when mode is heuristic; others stay heuristic.
        assert_eq!(mk("heuristic", &["telegram"]).effective_extractor("telegram"), Llm);
        assert_eq!(mk("heuristic", &["telegram"]).effective_extractor("web"), Heuristic);
        // mode=llm → LLM everywhere regardless of the list. Case-insensitive.
        assert_eq!(mk("llm", &[]).effective_extractor("signal"), Llm);
        assert_eq!(mk("heuristic", &["Telegram"]).effective_extractor("telegram"), Llm);
    }

    #[test]
    fn default_config_is_valid_json() {
        let cfg = MiraConfig::default_with_path();
        let json = serde_json::to_value(&cfg).unwrap();
        assert!(validate_config_json(&json).is_ok());
    }

    #[test]
    fn load_minimal_config() {
        let dir = TempDir::new().unwrap();
        let path = write_minimal(&dir);
        let cfg = MiraConfig::from_file(&path).unwrap();
        assert_eq!(cfg.primary_provider, "lmstudio");
        assert_eq!(cfg.config_version, "1");
    }

    #[test]
    fn save_and_reload_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("mira_config.json");
        let mut cfg = MiraConfig::default_with_path();
        cfg.config_path = path.clone();
        cfg.primary_provider = "ollama".to_string();
        cfg.providers.ollama.url = "http://my-ollama:11434".to_string();
        cfg.save().unwrap();

        let loaded = MiraConfig::from_file(&path).unwrap();
        assert_eq!(loaded.primary_provider, "ollama");
        assert_eq!(loaded.providers.ollama.url, "http://my-ollama:11434");
    }

    #[test]
    fn default_chat_model_uses_primary_providers_default_not_first_available() {
        let mut c = MiraConfig::default();
        c.primary_provider = "lmstudio".into();
        c.providers.lmstudio.default_model = "openai/gpt-oss-20b".into();
        // First in available_models is a DIFFERENT model — the default must win.
        c.providers.lmstudio.available_models = vec!["some/other-model".into(), "openai/gpt-oss-20b".into()];
        let (prov, model) = c.default_chat_model();
        assert_eq!(prov, "lmstudio");
        assert_eq!(model, "openai/gpt-oss-20b");
    }

    #[test]
    fn default_chat_model_follows_the_primary_provider() {
        let mut c = MiraConfig::default();
        c.primary_provider = "anthropic".into();
        c.providers.anthropic.default_model = "claude-x".into();
        assert_eq!(c.default_chat_model(), ("anthropic".into(), "claude-x".into()));
    }

    #[test]
    fn default_chat_model_falls_back_to_lmstudio_for_unknown_primary() {
        let mut c = MiraConfig::default();
        c.primary_provider = "nonesuch".into();
        c.providers.lmstudio.default_model = "fallback-model".into();
        let (prov, model) = c.default_chat_model();
        assert_eq!(prov, "lmstudio");
        assert_eq!(model, "fallback-model");
    }

    #[test]
    fn invalid_json_returns_error() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("bad.json");
        std::fs::write(&p, "not json at all {{{").unwrap();
        assert!(MiraConfig::from_file(&p).is_err());
    }

    #[test]
    fn schema_violation_returns_error() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("bad.json");
        std::fs::write(&p, r#"{"config_version":"1","primary_provider":"bad-provider"}"#).unwrap();
        let err = MiraConfig::from_file(&p).unwrap_err();
        assert!(err.to_string().contains("schema validation"));
    }

    #[test]
    fn expand_path_tilde() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_path("~/.mira/data"), home.join(".mira/data"));
        assert_eq!(expand_path("/absolute/path"), PathBuf::from("/absolute/path"));
    }

    #[test]
    fn env_override_openrouter_key() {
        let dir = TempDir::new().unwrap();
        let path = write_minimal(&dir);
        unsafe { std::env::set_var("OPENROUTER_API_KEY", "test-key-from-env"); }
        let cfg = MiraConfig::from_file(&path).unwrap();
        assert_eq!(cfg.providers.openrouter.api_key, Some("test-key-from-env".to_string()));
        unsafe { std::env::remove_var("OPENROUTER_API_KEY"); }
    }

    #[test]
    fn default_config_path_contains_mira() {
        let p = default_config_path();
        assert!(p.to_string_lossy().contains(".mira"));
        assert!(p.to_string_lossy().contains("mira_config.json"));
    }

    #[test]
    fn data_dir_env_override_wins_over_config_field() {
        let _env = ENV_TEST_LOCK.lock().unwrap();
        // The fix for the supervised-service data-dir divergence: MIRA_DATA_DIR
        // (set by the --data-dir flag / baked into the service launch) must win
        // over the config's `data_dir` field so the service reads the operator-
        // chosen dir regardless of which account's `~` it would otherwise expand.
        let mut cfg = MiraConfig::default_with_path();
        cfg.data_dir = "~/.mira/data".to_string();
        // Without the override, the config field (with ~ expanded) is used, and
        // state paths keep the default ~/.mira layout (unchanged behavior).
        unsafe { std::env::remove_var("MIRA_DATA_DIR"); }
        assert_eq!(cfg.data_dir_path(), expand_path("~/.mira/data"));
        assert_eq!(resolve_state_path("~/.mira/logs/mira.log"), expand_path("~/.mira/logs/mira.log"));
        // With it, the override wins for both the method and the pre-config helper.
        unsafe { std::env::set_var("MIRA_DATA_DIR", "/srv/backed-up/mira/data"); }
        assert_eq!(cfg.data_dir_path(), PathBuf::from("/srv/backed-up/mira/data"));
        assert_eq!(default_data_dir_path(), PathBuf::from("/srv/backed-up/mira/data"));
        assert_eq!(data_dir_env_override(), Some(PathBuf::from("/srv/backed-up/mira/data")));
        // State paths follow the data dir's parent ("mira home") under the override
        // so a supervised service's logs/cache/token land next to its data, not
        // under the supervisor account's `~`. (The #1 follow-up fix.)
        assert_eq!(resolve_state_path("~/.mira/logs/mira.log"), PathBuf::from("/srv/backed-up/mira/logs/mira.log"));
        assert_eq!(resolve_state_path("~/.mira/models"), PathBuf::from("/srv/backed-up/mira/models"));
        // A non-~/.mira value (explicit custom path) is honored as-is regardless.
        assert_eq!(resolve_state_path("/var/log/mira.log"), PathBuf::from("/var/log/mira.log"));
        unsafe { std::env::remove_var("MIRA_DATA_DIR"); }
    }
}
