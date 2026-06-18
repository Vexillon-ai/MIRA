// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/settings.rs
//! Settings introspection tools (of MIRA self-knowledge).
//!
//! Two read-only, access-gated tools the agent uses to answer "what is my X"
//! "what does setting Y do":
//! * `settings_describe` — explain a setting (type, allowed values,
//!   description, scope, whether it's a secret). Sourced from the config
//!   schema for global settings + a small per-user catalog. Open to all
//!   (descriptions are public).
//! * `settings_get` — the *live value*, access-gated:
//!     - `me.*` paths → the caller's own per-user settings (voice prefs,
//!       companion). Any authenticated user, own data only.
//!     - anything else → a global/operator setting, **admin only**, read
//!       fresh from the on-disk config with secrets redacted.
//!
//! adds `settings_set` (mutating), with the same access model:
//! * `me.*` paths → the caller writes their own per-user settings
//!   (currently per-channel voice). No confirmation needed; own data only.
//! * anything else → a global/operator setting, **admin only**, gated by a
//!   denylist (security/providers/proxy + any secret key), a required
//!   `confirm: true`, and schema validation *before* persisting (an invalid
//!   write would otherwise break the next restart). Applied live via
//!   `LiveConfig::update`. Every call is recorded by the tool audit log.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::auth::{LocalAuthService, Role};
use crate::companion::CompanionStore;
use crate::server::handlers::config_api::redact_secrets;
use crate::tools::{Tool, ToolArgs, ToolResult};
use crate::voice::{normalise as normalise_voice, parse_user_prefs, to_storage_json,
                   ChannelVoicePrefs, ResponsePolicy};
use crate::web::LiveConfig;
use crate::MiraError;

const SCHEMA_SRC: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/config/mira_config.schema.json"));

fn schema() -> &'static Value {
    static S: OnceLock<Value> = OnceLock::new();
    S.get_or_init(|| serde_json::from_str(SCHEMA_SRC).unwrap_or(Value::Null))
}

// Shared dependencies for the settings tools.
#[derive(Clone)]
struct SettingsAccess {
    config_path: PathBuf,
    auth:        Option<Arc<LocalAuthService>>,
    companion:   Option<Arc<CompanionStore>>,
}

impl SettingsAccess {
    fn role_of(&self, user_id: &str) -> Option<Role> {
        self.auth.as_ref()
            .and_then(|a| a.get_user(user_id).ok().flatten())
            .map(|u| u.role)
    }
    fn is_admin(&self, user_id: &str) -> bool {
        matches!(self.role_of(user_id), Some(Role::Admin))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// settings_get
// ─────────────────────────────────────────────────────────────────────────────

pub struct SettingsGetTool { a: SettingsAccess }

impl SettingsGetTool {
    pub fn new(config_path: PathBuf, auth: Option<Arc<LocalAuthService>>, companion: Option<Arc<CompanionStore>>) -> Self {
        Self { a: SettingsAccess { config_path, auth, companion } }
    }
}

#[async_trait]
impl Tool for SettingsGetTool {
    fn name(&self) -> &str { "settings_get" }
    fn description(&self) -> &str {
        "Read the live value of a MIRA setting, respecting your access. Use a `me.` prefix for \
         your own per-user settings (e.g. `me.voice_prefs.telegram`, `me.companion.daily_briefing_hour`). \
         Any other path is a server-wide/operator setting (e.g. `tts.default_backend`, \
         `agent.max_tool_rounds`) — viewable by admins only. Secrets (API keys, tokens, passwords) \
         are never returned; you'll be told whether one is set. Read-only."
    }
    fn args_schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{ "path": { "type":"string", "description":"Dotted setting path. Prefix with `me.` for your own settings." } },
            "required":["path"]
        })
    }
    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = args.get("_user_id").and_then(|v| v.as_str()).unwrap_or("");
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("").trim();
        if path.is_empty() {
            return Ok(ToolResult::failure("`path` is required (e.g. `tts.default_backend` or `me.companion.enabled`)."));
        }

        // Per-user settings: own data only.
        if let Some(rest) = path.strip_prefix("me.").or_else(|| path.strip_prefix("my.")) {
            if user_id.is_empty() {
                return Ok(ToolResult::failure("Can't resolve who you are — no user context on this request."));
            }
            return Ok(self.get_per_user(user_id, rest));
        }

        // Global/operator setting: admin only.
        if !self.a.is_admin(user_id) {
            return Ok(ToolResult::success(format!(
                "`{path}` is a server-wide (operator) setting — only an administrator can view it."
            )));
        }
        let mut cfg = match std::fs::read_to_string(&self.a.config_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        {
            Some(v) => v,
            None => return Ok(ToolResult::failure("Couldn't read the live config file.")),
        };
        redact_secrets(&mut cfg);
        match navigate(&cfg, path) {
            Some(v) => Ok(ToolResult::success(format!("`{path}` = {}", render_value(v)))),
            None    => Ok(ToolResult::success(format!(
                "No setting at `{path}`. Use settings_describe to browse available settings."
            ))),
        }
    }
}

impl SettingsGetTool {
    // Server-default per-channel voice prefs (`tts.voice_prefs`), read fresh
    // from the config file. Non-secret, so usable to resolve any user's
    // effective voice without exposing the rest of the global config.
    fn server_voice_prefs(&self) -> crate::voice::VoicePrefsMap {
        std::fs::read_to_string(&self.a.config_path).ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|c| c.get("tts").and_then(|t| t.get("voice_prefs")).cloned())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }

    fn get_per_user(&self, user_id: &str, rest: &str) -> ToolResult {
        let (head, tail) = match rest.split_once('.') {
            Some((h, t)) => (h, Some(t)),
            None => (rest, None),
        };
        match head {
            "voice_prefs" | "voice" => {
                // The EFFECTIVE voice you hear on a channel = your personal
                // override layered over the server default. The Settings →
                // Voice UI writes voice ids to the server config
                // (tts.voice_prefs); a per-user override (users.voice_prefs)
                // wins when set. Report the resolved result + where it came
                // from, since that's what "what voice is set for X" means.
                let user_json = self.a.auth.as_ref()
                    .and_then(|a| a.get_user(user_id).ok().flatten())
                    .and_then(|u| u.voice_prefs);
                let user_prefs = crate::voice::parse_user_prefs(user_json.as_deref());
                let server = self.server_voice_prefs();

                let describe = |ch: &str| -> String {
                    let r = crate::voice::resolve_voice(ch, Some(&user_prefs), &server);
                    let voice = r.voice_id.clone().unwrap_or_else(|| "(backend default)".into());
                    let from_user = user_prefs.get(ch).and_then(|u| u.voice_id.as_ref())
                        .map(|v| !v.is_empty()).unwrap_or(false);
                    let src = if from_user { "your override" } else { "server default" };
                    format!("voice = {voice} ({src}), policy = {}", r.policy.as_str())
                };

                if let Some(ch) = tail {
                    return ToolResult::success(format!("`me.voice.{ch}` → {}", describe(ch)));
                }
                let mut channels: Vec<String> = ["web", "telegram", "signal", "email"]
                    .iter().map(|s| s.to_string()).collect();
                for k in server.keys().chain(user_prefs.keys()) {
                    if !channels.contains(k) { channels.push(k.clone()); }
                }
                let mut lines = vec!["Your effective per-channel voice (your preference over the server default):".to_string()];
                for ch in channels {
                    lines.push(format!("- {ch}: {}", describe(&ch)));
                }
                ToolResult::success(lines.join("\n"))
            }
            "companion" => {
                let Some(store) = self.a.companion.as_ref() else {
                    return ToolResult::failure("Companion is not available on this server.");
                };
                let settings = match store.get(user_id) {
                    Ok(Some(s)) => s,
                    Ok(None)    => return ToolResult::success("You haven't set up companion mode yet."),
                    Err(e)      => return ToolResult::failure(format!("companion read failed: {e}")),
                };
                let v = serde_json::to_value(&settings).unwrap_or(Value::Null);
                match tail {
                    Some(t) => match navigate(&v, t) {
                        Some(val) => ToolResult::success(format!("`me.companion.{t}` = {}", render_value(val))),
                        None      => ToolResult::success(format!("No companion setting `{t}`.")),
                    },
                    None => ToolResult::success(format!("Your companion settings: {}", render_value(&v))),
                }
            }
            other => ToolResult::success(format!(
                "Unknown per-user setting `me.{other}`. Available: `me.voice_prefs[.<channel>]`, `me.companion[.<field>]`."
            )),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// settings_set  (writes)
// ─────────────────────────────────────────────────────────────────────────────

// Top-level config groups that may never be written from chat, even by an
// admin: secret-bearing or security-critical surfaces. (`providers` holds
// API keys; `security`/`proxy` can lock you out or exfiltrate.)
const WRITE_DENY_PREFIXES: &[&str] = &["security", "providers", "proxy"];

pub struct SettingsSetTool {
    a:           SettingsAccess,
    // Filled by the gateway *after* `LiveConfig` exists (this tool is built
    // before it). When set, global writes apply live (validate→persist→
    // broadcast); if somehow unset we fall back to a file write + a note that
    // a restart is needed.
    live_config: Arc<OnceLock<Arc<LiveConfig>>>,
}

impl SettingsSetTool {
    pub fn new(
        config_path: PathBuf,
        auth:        Option<Arc<LocalAuthService>>,
        companion:   Option<Arc<CompanionStore>>,
        live_config: Arc<OnceLock<Arc<LiveConfig>>>,
    ) -> Self {
        Self { a: SettingsAccess { config_path, auth, companion }, live_config }
    }
}

#[async_trait]
impl Tool for SettingsSetTool {
    fn name(&self) -> &str { "settings_set" }
    fn description(&self) -> &str {
        "Change a MIRA setting, respecting your access. Use a `me.` prefix to change your own \
         settings — currently your per-channel voice: `me.voice.<channel>` sets the voice id \
         (e.g. path `me.voice.telegram`, value `Abigail`), and `me.voice.<channel>.policy` sets \
         when you get voice replies (`always` / `on_voice_input` / `never`). Your own changes \
         apply immediately. Any other path is a server-wide/operator setting (e.g. \
         `tts.default_backend`) — admins only, and you must pass `confirm: true` to apply it \
         (call once without it first to preview the change). Secrets and the \
         security/providers/proxy groups can't be written here. Use settings_describe to check a \
         setting's type and allowed values before writing."
    }
    fn args_schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "path":  { "type":"string", "description":"Dotted setting path. Prefix with `me.` for your own settings." },
                "value": { "description":"New value (string, number, or boolean). Strings are coerced to the setting's type." },
                "confirm": { "type":"boolean", "description":"Required `true` to actually apply a server-wide change. Ignored for `me.*` writes." }
            },
            "required":["path","value"]
        })
    }
    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = args.get("_user_id").and_then(|v| v.as_str()).unwrap_or("");
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if path.is_empty() {
            return Ok(ToolResult::failure("`path` is required (e.g. `me.voice.telegram` or `tts.default_backend`)."));
        }
        let Some(value) = args.get("value").cloned() else {
            return Ok(ToolResult::failure("`value` is required."));
        };
        let confirm = args.get("confirm").and_then(|v| v.as_bool()).unwrap_or(false);

        // Per-user writes: own data only, no confirmation.
        if let Some(rest) = path.strip_prefix("me.").or_else(|| path.strip_prefix("my.")) {
            if user_id.is_empty() {
                return Ok(ToolResult::failure("Can't resolve who you are — no user context on this request."));
            }
            return Ok(self.set_per_user(user_id, rest, &value));
        }

        // Global/operator write: admin only.
        if !self.a.is_admin(user_id) {
            return Ok(ToolResult::failure(format!(
                "`{path}` is a server-wide (operator) setting — only an administrator can change it."
            )));
        }
        self.set_global(&path, value, confirm).await
    }
}

impl SettingsSetTool {
    fn set_per_user(&self, user_id: &str, rest: &str, value: &Value) -> ToolResult {
        // Accept `voice.<ch>`, `voice.<ch>.voice_id`, `voice.<ch>.policy`
        // (and the `voice_prefs.` synonym).
        let body = rest.strip_prefix("voice_prefs.").or_else(|| rest.strip_prefix("voice."));
        let Some(body) = body else {
            return ToolResult::failure(
                "Only per-channel voice is writable per-user right now: \
                 `me.voice.<channel>` (value = voice id) or `me.voice.<channel>.policy` \
                 (value = always|on_voice_input|never). For companion settings use the companion tools.",
            );
        };
        let mut parts = body.splitn(2, '.');
        let Some(channel) = parts.next().filter(|c| !c.is_empty()) else {
            return ToolResult::failure("Specify a channel, e.g. `me.voice.telegram`.");
        };
        let field = parts.next().unwrap_or("voice_id");

        let Some(auth) = self.a.auth.as_ref() else {
            return ToolResult::failure("User accounts aren't available on this server.");
        };
        let user = match auth.get_user(user_id) {
            Ok(Some(u)) => u,
            Ok(None)    => return ToolResult::failure("Couldn't find your user record."),
            Err(e)      => return ToolResult::failure(format!("user read failed: {e}")),
        };

        let mut prefs = parse_user_prefs(user.voice_prefs.as_deref());
        let entry = prefs.entry(channel.to_string())
            .or_insert_with(|| ChannelVoicePrefs { response_policy: None, voice_id: None });

        let confirmation;
        match field {
            "voice_id" => {
                let Some(v) = value.as_str().map(|s| s.trim().to_string()) else {
                    return ToolResult::failure("Voice id must be a string, e.g. `Abigail`.");
                };
                if v.is_empty() {
                    entry.voice_id = None;
                    confirmation = format!("Cleared your `{channel}` voice override (back to the server default).");
                } else {
                    entry.voice_id = Some(v.clone());
                    confirmation = format!("Set your `{channel}` voice to `{v}`.");
                }
            }
            "policy" | "response_policy" => {
                let raw = value.as_str().unwrap_or("").trim().to_lowercase().replace(' ', "_");
                let policy = match raw.as_str() {
                    "always"          => ResponsePolicy::Always,
                    "on_voice_input"  => ResponsePolicy::OnVoiceInput,
                    "never"           => ResponsePolicy::Never,
                    other => return ToolResult::failure(format!(
                        "`{other}` isn't a valid voice policy. Use: always, on_voice_input, never."
                    )),
                };
                entry.response_policy = Some(policy);
                confirmation = format!("Set your `{channel}` voice policy to `{}`.", policy.as_str());
            }
            other => return ToolResult::failure(format!(
                "Unknown voice field `{other}`. Use `me.voice.{channel}` (voice id) or `me.voice.{channel}.policy`."
            )),
        }

        let stored = to_storage_json(&normalise_voice(prefs));
        match auth.update_user(
            user_id,
            user.display_name, user.email, user.role, user.is_active,
            user.phone, user.preferred_contact, user.avatar,
            stored,
        ) {
            Ok(_)  => ToolResult::success(confirmation),
            Err(e) => ToolResult::failure(format!("Couldn't save your voice settings: {e}")),
        }
    }

    async fn set_global(&self, path: &str, value: Value, confirm: bool) -> Result<ToolResult, MiraError> {
        // Denylist: secret-bearing / security-critical groups + any secret leaf.
        let top = path.split('.').next().unwrap_or("");
        let leaf = path.rsplit('.').next().unwrap_or("");
        if WRITE_DENY_PREFIXES.contains(&top) || SECRET_KEYS.contains(&leaf) {
            return Ok(ToolResult::failure(format!(
                "`{path}` can't be changed from chat — secrets and the security/providers/proxy \
                 settings are protected. Use the admin Settings UI for those."
            )));
        }

        // The path must exist in the schema (catch typos before we touch the config).
        let spec = schema_spec(path);
        if spec.is_none() && describe_global(path).is_none() {
            return Ok(ToolResult::success(format!(
                "No setting at `{path}`. Use settings_describe to browse available settings."
            )));
        }

        // Coerce the incoming value to the schema type + validate any enum.
        let new_val = match coerce_value(spec.as_ref(), value) {
            Ok(v)  => v,
            Err(e) => return Ok(ToolResult::failure(format!("`{path}`: {e}"))),
        };

        // Base config: the live in-memory snapshot if we have it, else the file.
        let live = self.live_config.get().cloned();
        let mut cfg_val: Value = if let Some(lc) = &live {
            serde_json::to_value(&*lc.get().await).unwrap_or(Value::Null)
        } else {
            std::fs::read_to_string(&self.a.config_path).ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(Value::Null)
        };
        if !cfg_val.is_object() {
            return Ok(ToolResult::failure("Couldn't read the current config to modify."));
        }

        // What's it changing from? (for the preview / confirmation)
        let old_display = navigate(&cfg_val, path)
            .map(|v| { let mut c = v.clone(); redact_secrets(&mut c); render_value(&c) })
            .unwrap_or_else(|| "(unset)".into());

        if let Err(e) = set_path(&mut cfg_val, path, new_val.clone()) {
            return Ok(ToolResult::failure(format!("`{path}`: {e}")));
        }

        // Validate the *whole* config against the embedded schema BEFORE we
        // persist — an invalid value would break the next restart.
        if let Err(errors) = crate::config::validate::validate_config_json(&cfg_val) {
            return Ok(ToolResult::failure(format!(
                "Rejected — that would make the config invalid:\n{}",
                errors.join("\n")
            )));
        }
        let mut new_config: crate::config::MiraConfig = match serde_json::from_value(cfg_val) {
            Ok(c)  => c,
            Err(e) => return Ok(ToolResult::failure(format!("`{path}`: value doesn't fit this setting ({e})."))),
        };
        // `config_path` is `#[serde(skip)]`, so the round-trip above drops it —
        // restore it or `save()` has nowhere to write ("no parent directory").
        new_config.config_path = self.a.config_path.clone();

        let new_display = render_value(&new_val);

        // First call (no confirm) → preview only, change nothing.
        if !confirm {
            return Ok(ToolResult::success(format!(
                "Ready to change server-wide setting `{path}`:\n  {old_display}  →  {new_display}\n\
                 This affects every user. Re-run with `confirm: true` to apply."
            )));
        }

        // Apply.
        match &live {
            Some(lc) => {
                lc.update(new_config).await?;
                tracing::info!(target: "settings", path, %new_display, "settings_set applied global change (live)");
                Ok(ToolResult::success(format!(
                    "Done — `{path}` is now {new_display} server-wide (applied live: {old_display} → {new_display})."
                )))
            }
            None => {
                new_config.save().map_err(|e| MiraError::ConfigError(e.to_string()))?;
                tracing::info!(target: "settings", path, %new_display, "settings_set wrote global change to disk (restart needed)");
                Ok(ToolResult::success(format!(
                    "Saved `{path}` = {new_display} to the config file ({old_display} → {new_display}). \
                     A restart is needed for it to take effect."
                )))
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// settings_describe
// ─────────────────────────────────────────────────────────────────────────────

pub struct SettingsDescribeTool;

#[async_trait]
impl Tool for SettingsDescribeTool {
    fn name(&self) -> &str { "settings_describe" }
    fn description(&self) -> &str {
        "Explain a MIRA setting: its meaning, type, allowed values, scope (server-wide vs your \
         own), and whether it's a secret. Pass a dotted `path` (e.g. `tts.default_backend`, \
         `agent.max_tool_rounds`, `me.companion`). Omit `path` to list the top-level setting \
         groups. This describes settings (open to everyone); use settings_get for live values."
    }
    fn args_schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{ "path": { "type":"string", "description":"Dotted setting path to describe. Omit to list groups." } },
            "required":[]
        })
    }
    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("").trim();

        if path.is_empty() {
            let groups = top_level_groups();
            return Ok(ToolResult::success(format!(
                "MIRA settings groups (server-wide, admin to change): {}.\n\
                 Your own settings (any user): me.voice_prefs, me.companion.\n\
                 Pass a path like `tts.default_backend` or `me.companion` to describe it.",
                groups.join(", ")
            )));
        }

        if let Some(rest) = path.strip_prefix("me.").or_else(|| path.strip_prefix("my.")) {
            let head = rest.split('.').next().unwrap_or(rest);
            let desc = match head {
                "voice_prefs" | "voice" => "Your effective per-channel voice: response policy (always / on voice input / never) + the voice id. `settings_get me.voice` resolves your personal override layered over the server default (the Settings → Voice picker writes voice ids to the server config). Scope: your own.",
                "companion" => "Your companion-mode settings: enabled, quiet hours, preferred channels, daily-briefing on/off and hour. Scope: your own. Anyone can view/change their own.",
                _ => "Unknown per-user setting. Available: me.voice_prefs, me.companion.",
            };
            return Ok(ToolResult::success(desc.to_string()));
        }

        match describe_global(path) {
            Some(s) => Ok(ToolResult::success(s)),
            None    => Ok(ToolResult::success(format!(
                "No setting at `{path}`. Top-level groups: {}.", top_level_groups().join(", ")
            ))),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// helpers
// ─────────────────────────────────────────────────────────────────────────────

fn navigate<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = value;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

fn render_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn top_level_groups() -> Vec<String> {
    schema().get("properties").and_then(|p| p.as_object())
        .map(|m| { let mut v: Vec<String> = m.keys().cloned().collect(); v.sort(); v })
        .unwrap_or_default()
}

const SECRET_KEYS: &[&str] = &[
    "api_key","auth_token","webhook_secret","hmac_key","secret_token","bot_token","jwt_secret","password_hash","password",
];

// Return the schema spec node for a dotted global path, if the schema
// describes it explicitly. Returns `None` for paths under map-typed
// (`additionalProperties`) subtrees — those are still writable, we just
// can't read their type, so the caller falls back to validation.
fn schema_spec(path: &str) -> Option<Value> {
    let mut node = schema().get("properties")?;
    let segs: Vec<&str> = path.split('.').collect();
    for (i, seg) in segs.iter().enumerate() {
        let spec = node.get(seg)?;
        if i == segs.len() - 1 { return Some(spec.clone()); }
        node = spec.get("properties")?;
    }
    None
}

// Coerce an incoming JSON value to the schema's declared type (so a model
// passing the string `"12"` for an integer setting still works) and reject
// values outside a declared `enum`.
fn coerce_value(spec: Option<&Value>, val: Value) -> Result<Value, String> {
    let typ = spec.and_then(|s| s.get("type")).and_then(|t| t.as_str());
    let out = match typ {
        Some("integer") => match &val {
            Value::Number(n) if n.is_i64() || n.is_u64() => val.clone(),
            Value::String(s) => s.trim().parse::<i64>().map(|n| json!(n))
                .map_err(|_| format!("expected an integer, got \"{s}\""))?,
            _ => return Err("expected an integer".into()),
        },
        Some("number") => match &val {
            Value::Number(_) => val.clone(),
            Value::String(s) => s.trim().parse::<f64>().map(|n| json!(n))
                .map_err(|_| format!("expected a number, got \"{s}\""))?,
            _ => return Err("expected a number".into()),
        },
        Some("boolean") => match &val {
            Value::Bool(_) => val.clone(),
            Value::String(s) => match s.trim().to_lowercase().as_str() {
                "true" | "yes" | "1" | "on"  => json!(true),
                "false" | "no" | "0" | "off" => json!(false),
                _ => return Err(format!("expected true/false, got \"{s}\"")),
            },
            _ => return Err("expected true or false".into()),
        },
        _ => val, // string / enum / array / object — pass through, validate below
    };
    if let Some(en) = spec.and_then(|s| s.get("enum")).and_then(|e| e.as_array()) {
        if !en.iter().any(|e| e == &out) {
            let allowed = en.iter().map(render_value).collect::<Vec<_>>().join(", ");
            return Err(format!("`{}` is not allowed. Allowed: {allowed}", render_value(&out)));
        }
    }
    Ok(out)
}

// Set a dotted path in a JSON object, creating intermediate objects as needed.
fn set_path(root: &mut Value, path: &str, val: Value) -> Result<(), String> {
    let segs: Vec<&str> = path.split('.').collect();
    let mut cur = root;
    for seg in &segs[..segs.len() - 1] {
        let obj = cur.as_object_mut().ok_or_else(|| format!("`{seg}` is not a settings group"))?;
        cur = obj.entry((*seg).to_string()).or_insert_with(|| json!({}));
    }
    let last = *segs.last().unwrap();
    cur.as_object_mut()
        .ok_or_else(|| "parent is not a settings group".to_string())?
        .insert(last.to_string(), val);
    Ok(())
}

// Walk the schema's nested `properties` to describe a dotted global path.
fn describe_global(path: &str) -> Option<String> {
    let mut node = schema().get("properties")?;
    let segs: Vec<&str> = path.split('.').collect();
    for (i, seg) in segs.iter().enumerate() {
        let spec = node.get(seg)?;
        if i == segs.len() - 1 {
            let typ = spec.get("type").map(render_value).unwrap_or_default();
            let desc = spec.get("description").and_then(|d| d.as_str()).unwrap_or("");
            let enumv = spec.get("enum").and_then(|e| e.as_array())
                .map(|a| a.iter().map(render_value).collect::<Vec<_>>().join(", "));
            let secret = SECRET_KEYS.contains(seg);
            let mut out = format!("`{path}` — server-wide (operator) setting; admins view/change.");
            if !typ.is_empty() { out.push_str(&format!(" Type: {typ}.")); }
            if let Some(e) = enumv { out.push_str(&format!(" Allowed: {e}.")); }
            if secret { out.push_str(" This is a SECRET — its value is never shown (you can overwrite it, not read it)."); }
            if !desc.is_empty() { out.push_str(&format!("\n{desc}")); }
            return Some(out);
        }
        // descend into nested object properties
        node = spec.get("properties")?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_global_known_setting() {
        let s = describe_global("tts.default_backend").expect("known setting");
        assert!(s.contains("server-wide"));
        assert!(s.to_lowercase().contains("backend"));
    }

    #[test]
    fn describe_flags_secrets() {
        let s = describe_global("providers.openai.api_key").unwrap_or_default();
        assert!(s.to_uppercase().contains("SECRET"), "got: {s}");
    }

    #[test]
    fn top_level_groups_present() {
        let g = top_level_groups();
        assert!(g.iter().any(|x| x == "tts"));
        assert!(g.iter().any(|x| x == "agent"));
    }

    #[test]
    fn navigate_dotted() {
        let v = json!({"a":{"b":{"c":42}}});
        assert_eq!(navigate(&v, "a.b.c"), Some(&json!(42)));
        assert_eq!(navigate(&v, "a.x"), None);
    }

    #[test]
    fn effective_voice_falls_back_to_server_default() {
        // No per-user override (auth=None) → effective voice = the server
        // default from tts.voice_prefs. This is the bug the user hit: the UI
        // writes voice ids here, not to the per-user record.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("c.json");
        std::fs::write(&cfg, r#"{"tts":{"voice_prefs":{"telegram":{"response_policy":"always","voice_id":"Abigail"}}}}"#).unwrap();
        let tool = SettingsGetTool::new(cfg, None, None);
        let single = tool.get_per_user("u1", "voice.telegram");
        assert!(single.output.contains("Abigail"), "got: {}", single.output);
        assert!(single.output.contains("server default"));
        let all = tool.get_per_user("u1", "voice");
        assert!(all.output.contains("telegram") && all.output.contains("Abigail"));
    }

    #[test]
    fn coerce_string_to_typed_value() {
        let int_spec = json!({"type":"integer"});
        assert_eq!(coerce_value(Some(&int_spec), json!("12")).unwrap(), json!(12));
        let bool_spec = json!({"type":"boolean"});
        assert_eq!(coerce_value(Some(&bool_spec), json!("on")).unwrap(), json!(true));
        assert!(coerce_value(Some(&int_spec), json!("nope")).is_err());
    }

    #[test]
    fn coerce_rejects_value_outside_enum() {
        let spec = json!({"type":"string","enum":["a","b"]});
        assert!(coerce_value(Some(&spec), json!("a")).is_ok());
        assert!(coerce_value(Some(&spec), json!("z")).is_err());
    }

    #[test]
    fn set_path_creates_intermediates() {
        let mut v = json!({"tts":{}});
        set_path(&mut v, "tts.voice_prefs.telegram.voice_id", json!("Abigail")).unwrap();
        assert_eq!(navigate(&v, "tts.voice_prefs.telegram.voice_id"), Some(&json!("Abigail")));
    }

    #[tokio::test]
    async fn global_write_blocks_denylisted_and_secret_paths() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("c.json");
        std::fs::write(&cfg, "{}").unwrap();
        let tool = SettingsSetTool::new(cfg, None, None, Arc::new(OnceLock::new()));
        for p in ["security.foo", "providers.openai.base_url", "proxy.url", "channels.telegram.bot_token"] {
            let r = tool.set_global(p, json!("x"), true).await.unwrap();
            assert!(!r.success, "expected `{p}` to be blocked, got: {}", r.output);
        }
    }
}
