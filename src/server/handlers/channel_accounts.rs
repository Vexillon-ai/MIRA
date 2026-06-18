// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/channel_accounts.rs
//! Per-user Signal / Telegram account management.
//!
//! Regular users can CRUD their own accounts; admins see all. Config blobs
//! are transported as typed JSON objects on the wire and serialised to
//! strings in the DB so the backend stays agnostic of channel internals.
//!
//! All mutating endpoints require a server restart to take effect
//! (daemons are started at Gateway boot). The frontend surfaces a
//! "Restart server" button — see [`super::admin::restart_handler`].

use std::sync::Arc;

use axum::{
    extract::{Json, Path},
    http::StatusCode,
    response::IntoResponse,
    Extension,
};
use serde::{Deserialize, Serialize};

use crate::auth::{AuthUser, LocalAuthService, Role};
use crate::channel_accounts::{
    ChannelAccount, ChannelAccountStore, ChannelKind, NewChannelAccount,
    DiscordAccountConfig, ExternalAccountConfig, MatrixAccountConfig, SignalAccountConfig,
    SlackAccountConfig, TelegramAccountConfig, UpdateChannelAccount, WhatsAppAccountConfig,
};
use crate::MiraError;

// ── Wire types ───────────────────────────────────────────────────────────────

/// Response shape for one channel account. `config` is typed so the frontend
/// doesn't need to parse an arbitrary string; the server decodes
/// `config_json` based on `channel`.
#[derive(Serialize)]
pub struct ChannelAccountResponse {
    pub id:            String,
    pub user_id:       String,
    pub channel:       String,
    pub account_label: String,
    pub external_id:   Option<String>,
    pub enabled:       bool,
    pub routing_mode:  String,
    pub created_at:    i64,
    pub updated_at:    i64,
    pub config:        serde_json::Value,
}

impl ChannelAccountResponse {
    fn from_row(acct: ChannelAccount) -> Result<Self, MiraError> {
        let config = match acct.channel {
            ChannelKind::Signal   => serde_json::to_value(&acct.signal_config()?),
            ChannelKind::Telegram => {
                // Never leak the bot token to the frontend — the owner can
                // still see and overwrite it, but we strip it on read so
                // a casual screenshot or shoulder-surf doesn't exfiltrate.
                let mut cfg = acct.telegram_config()?;
                cfg.bot_token = redact(&cfg.bot_token);
                if let Some(ref mut s) = cfg.secret_token {
                    *s = redact(s);
                }
                serde_json::to_value(&cfg)
            }
            ChannelKind::Discord => {
                // Same redaction posture as Telegram — the bot token is
                // the keys to the kingdom, never echo it in responses.
                let mut cfg = acct.discord_config()?;
                cfg.bot_token = redact(&cfg.bot_token);
                serde_json::to_value(&cfg)
            }
            ChannelKind::Matrix => {
                // Redact the access token (homeserver is not a secret).
                let mut cfg = acct.matrix_config()?;
                cfg.access_token = redact(&cfg.access_token);
                serde_json::to_value(&cfg)
            }
            ChannelKind::WhatsApp => {
                // Redact the access token + app secret; phone_number_id and
                // verify_token are not secrets (the latter is operator-chosen
                // and also entered into Meta's config).
                let mut cfg = acct.whatsapp_config()?;
                cfg.access_token = redact(&cfg.access_token);
                if let Some(ref mut s) = cfg.app_secret { *s = redact(s); }
                serde_json::to_value(&cfg)
            }
            ChannelKind::Slack => {
                // Redact both the bot token and the signing secret.
                let mut cfg = acct.slack_config()?;
                cfg.bot_token = redact(&cfg.bot_token);
                cfg.signing_secret = redact(&cfg.signing_secret);
                serde_json::to_value(&cfg)
            }
            ChannelKind::External => {
                // Redact both CPP secrets (provider_kind + send_url are not
                // secret). The create path uses `from_row_unredacted` so the
                // operator can copy the secrets into the provider once.
                let mut cfg = acct.external_config()?;
                cfg.inbound_secret  = redact(&cfg.inbound_secret);
                cfg.outbound_secret = redact(&cfg.outbound_secret);
                serde_json::to_value(&cfg)
            }
        }.map_err(|e| MiraError::ConfigError(e.to_string()))?;

        Ok(Self {
            id:            acct.id,
            user_id:       acct.user_id,
            channel:       acct.channel.as_str().to_owned(),
            account_label: acct.account_label,
            external_id:   acct.external_id,
            enabled:       acct.enabled,
            routing_mode:  acct.routing_mode.as_str().to_owned(),
            created_at:    acct.created_at,
            updated_at:    acct.updated_at,
            config,
        })
    }

    /// Like `from_row` but does NOT redact secrets — used ONLY by the
    /// create endpoint so a CPP (External) account's freshly-generated
    /// inbound/outbound secrets are shown to the operator exactly once.
    /// Every subsequent read goes through `from_row` (redacted). For all
    /// non-External channels this is identical to `from_row`'s output
    /// shape except the config is the raw stored blob.
    fn from_row_unredacted(acct: ChannelAccount) -> Result<Self, MiraError> {
        let config: serde_json::Value = serde_json::from_str(&acct.config_json)
            .map_err(|e| MiraError::ConfigError(e.to_string()))?;
        Ok(Self {
            id:            acct.id,
            user_id:       acct.user_id,
            channel:       acct.channel.as_str().to_owned(),
            account_label: acct.account_label,
            external_id:   acct.external_id,
            enabled:       acct.enabled,
            routing_mode:  acct.routing_mode.as_str().to_owned(),
            created_at:    acct.created_at,
            updated_at:    acct.updated_at,
            config,
        })
    }
}

/// Mask a secret: keep the first 4 and last 2 characters, dots in between.
/// Short strings (< 10 chars) collapse to all dots.
fn redact(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 10 {
        "•".repeat(chars.len().max(1))
    } else {
        format!(
            "{}{}{}",
            chars[..4].iter().collect::<String>(),
            "•".repeat(chars.len() - 6),
            chars[chars.len() - 2..].iter().collect::<String>(),
        )
    }
}

#[derive(Deserialize)]
pub struct CreateAccountRequest {
    pub channel:       String,
    pub account_label: String,
    pub external_id:   Option<String>,
    pub enabled:       Option<bool>,
    /// R1+R2: "personal" (default — every inbound runs as bot owner),
    /// "shared" (look up sender in user_channel_links), or "guest_ok"
    /// (look up + fall through to a guest identity on miss).
    pub routing_mode:  Option<String>,
    /// Typed config — the shape must match the channel. The server encodes
    /// into `config_json` after validation.
    pub config:        serde_json::Value,
}

#[derive(Deserialize)]
pub struct UpdateAccountRequest {
    pub account_label: Option<String>,
    pub external_id:   Option<Option<String>>,
    pub enabled:       Option<bool>,
    pub routing_mode:  Option<String>,
    /// Optional typed config replacement. `None` fields in the supplied
    /// config are merged with the existing values so clients can partial-
    /// update a single secret without re-entering the whole blob.
    pub config:        Option<serde_json::Value>,
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn err_resp(e: MiraError) -> axum::response::Response {
    match e {
        MiraError::NotFound(m)  => (StatusCode::NOT_FOUND, m).into_response(),
        MiraError::Forbidden    => StatusCode::FORBIDDEN.into_response(),
        MiraError::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
        MiraError::ConfigError(m) => (StatusCode::BAD_REQUEST, m).into_response(),
        _                       => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Capability RBAC — may `caller` use `kind`? Admins (and any caller whose
/// effective profile doesn't restrict the channel axis) are always allowed; a
/// capability-lookup error fails open so a transient DB hiccup never strands a
/// user out of their own channels.
fn channel_allowed(
    auth: &LocalAuthService,
    caller: &crate::auth::User,
    kind: ChannelKind,
) -> bool {
    match auth.effective_capabilities(&caller.id, &caller.role) {
        Ok(caps) => caps.allows_channel(kind.as_str()),
        Err(_)   => true,
    }
}

fn parse_kind(s: &str) -> Result<ChannelKind, MiraError> {
    use std::str::FromStr;
    ChannelKind::from_str(s)
}

/// Serialise a typed wire-level config into the `config_json` string stored
/// in the DB. Enforces shape matching so a malformed blob never reaches the
/// daemon fan-out at startup.
fn encode_config(kind: ChannelKind, value: &serde_json::Value) -> Result<String, MiraError> {
    match kind {
        ChannelKind::Signal => {
            let cfg: SignalAccountConfig = serde_json::from_value(value.clone())
                .map_err(|e| MiraError::ConfigError(format!("invalid signal config: {}", e)))?;
            serde_json::to_string(&cfg).map_err(|e| MiraError::ConfigError(e.to_string()))
        }
        ChannelKind::Telegram => {
            let cfg: TelegramAccountConfig = serde_json::from_value(value.clone())
                .map_err(|e| MiraError::ConfigError(format!("invalid telegram config: {}", e)))?;
            serde_json::to_string(&cfg).map_err(|e| MiraError::ConfigError(e.to_string()))
        }
        ChannelKind::Discord => {
            let cfg: DiscordAccountConfig = serde_json::from_value(value.clone())
                .map_err(|e| MiraError::ConfigError(format!("invalid discord config: {}", e)))?;
            serde_json::to_string(&cfg).map_err(|e| MiraError::ConfigError(e.to_string()))
        }
        ChannelKind::Matrix => {
            let cfg: MatrixAccountConfig = serde_json::from_value(value.clone())
                .map_err(|e| MiraError::ConfigError(format!("invalid matrix config: {}", e)))?;
            serde_json::to_string(&cfg).map_err(|e| MiraError::ConfigError(e.to_string()))
        }
        ChannelKind::WhatsApp => {
            let cfg: WhatsAppAccountConfig = serde_json::from_value(value.clone())
                .map_err(|e| MiraError::ConfigError(format!("invalid whatsapp config: {}", e)))?;
            serde_json::to_string(&cfg).map_err(|e| MiraError::ConfigError(e.to_string()))
        }
        ChannelKind::Slack => {
            let cfg: SlackAccountConfig = serde_json::from_value(value.clone())
                .map_err(|e| MiraError::ConfigError(format!("invalid slack config: {}", e)))?;
            serde_json::to_string(&cfg).map_err(|e| MiraError::ConfigError(e.to_string()))
        }
        ChannelKind::External => {
            let mut cfg: ExternalAccountConfig = serde_json::from_value(value.clone())
                .map_err(|e| MiraError::ConfigError(format!("invalid external config: {}", e)))?;
            // Auto-generate the two CPP HMAC secrets if the client didn't
            // supply them (it normally won't — the UI leaves them blank on
            // create). 32 random bytes hex each. On edit, an empty field
            // means "keep" — but since edit re-encodes the merged config
            // which still carries the existing secrets, they survive.
            if cfg.inbound_secret.trim().is_empty() {
                cfg.inbound_secret = gen_secret();
            }
            if cfg.outbound_secret.trim().is_empty() {
                cfg.outbound_secret = gen_secret();
            }
            serde_json::to_string(&cfg).map_err(|e| MiraError::ConfigError(e.to_string()))
        }
    }
}

/// 32 random bytes, hex — the CPP inbound/outbound HMAC secrets.
fn gen_secret() -> String {
    use rand::RngCore;
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    hex::encode(raw)
}

// ── GET /api/channel-accounts ────────────────────────────────────────────────

pub async fn list_accounts(
    AuthUser(caller): AuthUser,
    Extension(store): Extension<Arc<ChannelAccountStore>>,
) -> impl IntoResponse {
    let result = if caller.role == Role::Admin {
        store.list_all()
    } else {
        store.list_for_user(&caller.id)
    };
    match result {
        Ok(list) => {
            let mut out = Vec::with_capacity(list.len());
            for a in list {
                match ChannelAccountResponse::from_row(a) {
                    Ok(r)  => out.push(r),
                    Err(e) => return err_resp(e),
                }
            }
            axum::Json(out).into_response()
        }
        Err(e) => err_resp(e),
    }
}

// ── POST /api/channel-accounts ───────────────────────────────────────────────

pub async fn create_account(
    AuthUser(caller): AuthUser,
    Extension(store): Extension<Arc<ChannelAccountStore>>,
    Extension(auth):  Extension<Arc<LocalAuthService>>,
    Json(req): Json<CreateAccountRequest>,
) -> impl IntoResponse {
    let kind = match parse_kind(&req.channel) {
        Ok(k)  => k,
        Err(e) => return err_resp(e),
    };

    // Capability RBAC — channel-axis enforcement. Refuse to add a channel the
    // caller's profile doesn't permit (admins resolve to an unrestricted
    // profile, so they're never blocked).
    if !channel_allowed(&auth, &caller, kind) {
        return (StatusCode::FORBIDDEN, format!(
            "Your account is not permitted to use the '{}' channel.", kind.as_str()
        )).into_response();
    }

    let config_json = match encode_config(kind, &req.config) {
        Ok(s)  => s,
        Err(e) => return err_resp(e),
    };

    let routing_mode = match req.routing_mode.as_deref() {
        None     => crate::channel_accounts::RoutingMode::default(),
        Some(s)  => match std::str::FromStr::from_str(s) {
            Ok(rm) => rm,
            Err(e) => return err_resp(e),
        },
    };
    let new = NewChannelAccount {
        user_id:       caller.id.clone(),
        channel:       kind,
        account_label: req.account_label,
        external_id:   req.external_id,
        config_json,
        enabled:       req.enabled.unwrap_or(true),
        routing_mode,
    };
    match store.create(new) {
        // External (CPP) accounts return their freshly-generated secrets
        // un-redacted exactly once, at create, so the operator can copy
        // them into the provider. All other channels + all later reads
        // redact. (A re-fetch of an External account via GET is redacted.)
        Ok(acct) => {
            let resp = if kind == ChannelKind::External {
                ChannelAccountResponse::from_row_unredacted(acct)
            } else {
                ChannelAccountResponse::from_row(acct)
            };
            match resp {
                Ok(r)  => (StatusCode::CREATED, axum::Json(r)).into_response(),
                Err(e) => err_resp(e),
            }
        }
        Err(e) => err_resp(e),
    }
}

// ── GET /api/channel-accounts/{id} ───────────────────────────────────────────

pub async fn get_account(
    AuthUser(caller): AuthUser,
    Extension(store): Extension<Arc<ChannelAccountStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    match store.get(&id) {
        Ok(Some(a)) if caller.role == Role::Admin || a.user_id == caller.id => {
            match ChannelAccountResponse::from_row(a) {
                Ok(r)  => axum::Json(r).into_response(),
                Err(e) => err_resp(e),
            }
        }
        Ok(Some(_)) => StatusCode::FORBIDDEN.into_response(),
        Ok(None)    => StatusCode::NOT_FOUND.into_response(),
        Err(e)      => err_resp(e),
    }
}

// ── PUT /api/channel-accounts/{id} ───────────────────────────────────────────

pub async fn update_account(
    AuthUser(caller): AuthUser,
    Extension(store): Extension<Arc<ChannelAccountStore>>,
    Extension(auth):  Extension<Arc<LocalAuthService>>,
    Path(id):         Path<String>,
    Json(req):        Json<UpdateAccountRequest>,
) -> impl IntoResponse {
    let existing = match store.get(&id) {
        Ok(Some(a)) if caller.role == Role::Admin || a.user_id == caller.id => a,
        Ok(Some(_)) => return StatusCode::FORBIDDEN.into_response(),
        Ok(None)    => return StatusCode::NOT_FOUND.into_response(),
        Err(e)      => return err_resp(e),
    };

    // Capability RBAC — block (re-)enabling a channel the caller is no longer
    // permitted to use. Disabling / editing other fields stays allowed so a
    // user can always wind a now-forbidden channel down.
    if req.enabled == Some(true) && !channel_allowed(&auth, &caller, existing.channel) {
        return (StatusCode::FORBIDDEN, format!(
            "Your account is not permitted to use the '{}' channel.", existing.channel.as_str()
        )).into_response();
    }

    // Re-encode config if provided, else keep the existing string as-is.
    let config_json = if let Some(cfg) = req.config {
        match encode_config(existing.channel, &cfg) {
            Ok(s)  => Some(s),
            Err(e) => return err_resp(e),
        }
    } else {
        None
    };

    let routing_mode = match req.routing_mode.as_deref() {
        None    => None,
        Some(s) => match std::str::FromStr::from_str(s) {
            Ok(rm) => Some(rm),
            Err(e) => return err_resp(e),
        },
    };
    let upd = UpdateChannelAccount {
        account_label: req.account_label,
        external_id:   req.external_id,
        config_json,
        enabled:       req.enabled,
        routing_mode,
    };

    match store.update(&id, upd) {
        Ok(acct) => match ChannelAccountResponse::from_row(acct) {
            Ok(r)  => axum::Json(r).into_response(),
            Err(e) => err_resp(e),
        },
        Err(e) => err_resp(e),
    }
}

// ── DELETE /api/channel-accounts/{id} ────────────────────────────────────────

pub async fn delete_account(
    AuthUser(caller): AuthUser,
    Extension(store): Extension<Arc<ChannelAccountStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    match store.get(&id) {
        Ok(Some(a)) if caller.role == Role::Admin || a.user_id == caller.id => {}
        Ok(Some(_)) => return StatusCode::FORBIDDEN.into_response(),
        Ok(None)    => return StatusCode::NOT_FOUND.into_response(),
        Err(e)      => return err_resp(e),
    }

    match store.delete(&id) {
        Ok(())  => StatusCode::NO_CONTENT.into_response(),
        Err(e)  => err_resp(e),
    }
}

// ── GET /api/channel-accounts/health ─────────────────────────────────────────
//
// Per-account liveness for the Channel Accounts UI. Probes Signal
// daemons over their REST port; treats Telegram as alive whenever the
// row is enabled (the channel is webhook-driven, no daemon to ping).
//
// Read-only. No mutation, no side effects beyond the outbound probe.
// Unlike the /api/channel-accounts list endpoint above, admins see
// every user's account here too — they need cross-user visibility to
// triage "the Signal daemon for user X is dead" without impersonating
// the user. Non-admins still only see their own.

#[derive(Debug, serde::Serialize)]
pub struct AccountHealth {
    pub account_id: String,
    pub channel:    String,
    pub alive:      bool,
    /// Probe latency in ms. None for channels where no probe runs
    /// (Telegram, or accounts disabled at the row level).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u128>,
    /// Short reason when `alive == false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error:      Option<String>,
}

pub async fn account_health(
    AuthUser(caller): AuthUser,
    Extension(store): Extension<Arc<ChannelAccountStore>>,
) -> impl IntoResponse {
    let list = if caller.role == Role::Admin {
        store.list_all()
    } else {
        store.list_for_user(&caller.id)
    };
    let accounts = match list {
        Ok(l)  => l,
        Err(e) => return err_resp(e),
    };

    // Probe each account concurrently. 1.5s per probe is plenty for
    // a localhost call; longer would let a wedged signal-cli stall
    // the whole response.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(1500))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let probes = accounts.into_iter().map(|a| {
        let client = client.clone();
        async move {
            match a.channel {
                ChannelKind::Signal   => signal_probe(&client, &a).await,
                ChannelKind::Telegram => telegram_probe(&a),
                // Discord — no cheap "is gateway alive?" probe (the
                // connection is a long-lived task on the ChannelManager,
                // not an HTTP endpoint we can ping). Report enabled rows
                // as up; the channel page surfaces real status from the
                // ChannelManager's `discord` Vec in a separate endpoint
                // when we add one in D3+ polish.
                ChannelKind::Discord  => discord_probe(&a),
                // Matrix — like Discord, the sync loop is a long-lived
                // task, not a pingable endpoint; report enabled = up.
                ChannelKind::Matrix   => matrix_probe(&a),
                // WhatsApp — webhook-driven (no daemon); report enabled = up.
                ChannelKind::WhatsApp => whatsapp_probe(&a),
                // Slack — webhook-driven; report enabled = up.
                ChannelKind::Slack    => slack_probe(&a),
                // External (CPP) — webhook-driven; report enabled = up.
                ChannelKind::External => external_probe(&a),
            }
        }
    });
    let results: Vec<AccountHealth> = futures::future::join_all(probes).await;
    axum::Json(results).into_response()
}

async fn signal_probe(client: &reqwest::Client, acct: &crate::channel_accounts::ChannelAccount)
    -> AccountHealth
{
    let id = acct.id.clone();
    let channel = "signal".to_string();
    if !acct.enabled {
        return AccountHealth {
            account_id: id, channel, alive: false,
            latency_ms: None,
            error: Some("disabled".into()),
        };
    }
    // Pull the rest_port out of the per-account config blob.
    let port = match acct.signal_config() {
        Ok(c)  => c.rest_port,
        Err(_) => None,
    };
    let Some(port) = port else {
        return AccountHealth {
            account_id: id, channel, alive: false,
            latency_ms: None,
            error: Some("no rest_port allocated yet (account hasn't started)".into()),
        };
    };
    let start = std::time::Instant::now();
    let url = format!("http://127.0.0.1:{port}/v1/health");
    let resp = client.get(&url).send().await;
    let latency_ms = start.elapsed().as_millis();
    match resp {
        Ok(r) if r.status().as_u16() < 500 => AccountHealth {
            account_id: id, channel, alive: true,
            latency_ms: Some(latency_ms),
            error: None,
        },
        Ok(r) => AccountHealth {
            account_id: id, channel, alive: false,
            latency_ms: Some(latency_ms),
            error: Some(format!("daemon returned {}", r.status())),
        },
        Err(e) => AccountHealth {
            account_id: id, channel, alive: false,
            latency_ms: Some(latency_ms),
            error: Some(connect_err_message(&e)),
        },
    }
}

fn telegram_probe(acct: &crate::channel_accounts::ChannelAccount) -> AccountHealth {
    AccountHealth {
        account_id: acct.id.clone(),
        channel:    "telegram".into(),
        alive:      acct.enabled,
        latency_ms: None,
        error:      if acct.enabled { None } else { Some("disabled".into()) },
    }
}

fn discord_probe(acct: &crate::channel_accounts::ChannelAccount) -> AccountHealth {
    // The actual gateway-connection liveness lives on the ChannelManager
    // (`discord: Vec<DiscordRuntime>`). Surfacing it here would require
    // an Extension to the manager — for now we report the row's `enabled`
    // flag (matches Telegram's posture). D3+ polish adds a real probe.
    AccountHealth {
        account_id: acct.id.clone(),
        channel:    "discord".into(),
        alive:      acct.enabled,
        latency_ms: None,
        error:      if acct.enabled { None } else { Some("disabled".into()) },
    }
}

fn matrix_probe(acct: &crate::channel_accounts::ChannelAccount) -> AccountHealth {
    // Same posture as discord_probe — sync-loop liveness lives on the
    // ChannelManager (`matrix: Vec<MatrixRuntime>`); report `enabled`.
    AccountHealth {
        account_id: acct.id.clone(),
        channel:    "matrix".into(),
        alive:      acct.enabled,
        latency_ms: None,
        error:      if acct.enabled { None } else { Some("disabled".into()) },
    }
}

fn whatsapp_probe(acct: &crate::channel_accounts::ChannelAccount) -> AccountHealth {
    // Webhook-driven (Meta pushes to us) — no endpoint to ping; report
    // the row's `enabled` flag, same as telegram_probe.
    AccountHealth {
        account_id: acct.id.clone(),
        channel:    "whatsapp".into(),
        alive:      acct.enabled,
        latency_ms: None,
        error:      if acct.enabled { None } else { Some("disabled".into()) },
    }
}

fn slack_probe(acct: &crate::channel_accounts::ChannelAccount) -> AccountHealth {
    // Webhook-driven (Slack pushes to us) — report `enabled`.
    AccountHealth {
        account_id: acct.id.clone(),
        channel:    "slack".into(),
        alive:      acct.enabled,
        latency_ms: None,
        error:      if acct.enabled { None } else { Some("disabled".into()) },
    }
}

fn external_probe(acct: &crate::channel_accounts::ChannelAccount) -> AccountHealth {
    // Webhook-driven (provider pushes to us) — report `enabled`.
    AccountHealth {
        account_id: acct.id.clone(),
        channel:    "external".into(),
        alive:      acct.enabled,
        latency_ms: None,
        error:      if acct.enabled { None } else { Some("disabled".into()) },
    }
}

/// Map a reqwest error into a one-line UI-friendly reason. Distinguishes
/// "nothing listening" (daemon never came up / crashed) from "timed
/// out" (daemon wedged / GC pause / busy) so the operator knows which
/// way to debug.
fn connect_err_message(e: &reqwest::Error) -> String {
    if e.is_timeout()        { return "probe timed out".into(); }
    if e.is_connect()        { return "no daemon listening".into(); }
    e.to_string().chars().take(200).collect()
}

// ── Per-account lifecycle (start/stop/restart) ───────────────────────────────
//
// Admin click on the Start/Stop/Restart buttons in the ChannelAccountsPage
// hits these endpoints. They take a brief write lock on the
// ChannelManager, run the lifecycle method, and return a small JSON
// status. Idempotent — `start` on a running daemon is an error,
// `stop` on a stopped one is a no-op success, `restart` is
// `stop` then `start`.

/// Newtype wrapper so the Extension lookup is unambiguous (raw
/// Arc<RwLock<…>> would collide with future RwLock-wrapped extensions).
#[derive(Clone)]
pub struct ChannelManagerExt(
    pub Arc<tokio::sync::RwLock<crate::gateway::channel_manager::ChannelManager>>,
);

#[derive(serde::Serialize)]
struct LifecycleResp {
    ok:      bool,
    action:  &'static str,
    account_id: String,
    /// One-line human-readable detail. Useful for surfacing in toast.
    message: String,
}

fn lifecycle_unauthorized() -> axum::response::Response {
    (StatusCode::FORBIDDEN, axum::Json(serde_json::json!({
        "error": "admin role required for daemon lifecycle"
    }))).into_response()
}

fn lifecycle_unavailable() -> axum::response::Response {
    (StatusCode::SERVICE_UNAVAILABLE, axum::Json(serde_json::json!({
        "error": "ChannelManager not wired in this build"
    }))).into_response()
}

pub async fn start_account_daemon(
    AuthUser(caller):  AuthUser,
    Extension(mgr):    Extension<ChannelManagerExt>,
    Path(id):          Path<String>,
) -> axum::response::Response {
    if caller.role != Role::Admin { return lifecycle_unauthorized(); }
    let mut guard = mgr.0.write().await;
    match guard.start_account(&id).await {
        Ok(()) => (StatusCode::OK, axum::Json(LifecycleResp {
            ok: true, action: "start", account_id: id.clone(),
            message: format!("daemon started for {id}"),
        })).into_response(),
        Err(msg) => (StatusCode::CONFLICT, axum::Json(LifecycleResp {
            ok: false, action: "start", account_id: id, message: msg,
        })).into_response(),
    }
}

pub async fn stop_account_daemon(
    AuthUser(caller):  AuthUser,
    Extension(mgr):    Extension<ChannelManagerExt>,
    Path(id):          Path<String>,
) -> axum::response::Response {
    if caller.role != Role::Admin { return lifecycle_unauthorized(); }
    let mut guard = mgr.0.write().await;
    match guard.stop_account(&id).await {
        Ok(true)  => (StatusCode::OK, axum::Json(LifecycleResp {
            ok: true, action: "stop", account_id: id.clone(),
            message: format!("daemon stopped for {id}"),
        })).into_response(),
        Ok(false) => (StatusCode::OK, axum::Json(LifecycleResp {
            ok: true, action: "stop", account_id: id.clone(),
            message: format!("account {id} was already stopped"),
        })).into_response(),
        Err(msg)  => (StatusCode::CONFLICT, axum::Json(LifecycleResp {
            ok: false, action: "stop", account_id: id, message: msg,
        })).into_response(),
    }
}

pub async fn restart_account_daemon(
    AuthUser(caller):  AuthUser,
    Extension(mgr):    Extension<ChannelManagerExt>,
    Path(id):          Path<String>,
) -> axum::response::Response {
    if caller.role != Role::Admin { return lifecycle_unauthorized(); }
    let mut guard = mgr.0.write().await;
    match guard.restart_account(&id).await {
        Ok(()) => (StatusCode::OK, axum::Json(LifecycleResp {
            ok: true, action: "restart", account_id: id.clone(),
            message: format!("daemon restarted for {id}"),
        })).into_response(),
        Err(msg) => (StatusCode::CONFLICT, axum::Json(LifecycleResp {
            ok: false, action: "restart", account_id: id, message: msg,
        })).into_response(),
    }
}

// Suppress dead-code warning on lifecycle_unavailable until the gateway
// builder gates extension presence on it (a future cleanup; right now
// the router shadows the api_routes binding instead).
#[allow(dead_code)] fn _ensure_used() { let _ = lifecycle_unavailable; }

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_short_string_all_dots() {
        assert_eq!(redact("abc"), "•••");
    }

    #[test]
    fn redact_keeps_head_and_tail() {
        let got = redact("1234567890ABCD");
        assert!(got.starts_with("1234"));
        assert!(got.ends_with("CD"));
        assert!(got.contains("•"));
    }

    #[test]
    fn encode_config_roundtrips_signal() {
        let v = serde_json::json!({
            "phone_number": "+1234567890",
            "cli_binary":   "signal-cli",
            "data_dir":     "/tmp"
        });
        let s = encode_config(ChannelKind::Signal, &v).unwrap();
        assert!(s.contains("+1234567890"));
    }

    #[test]
    fn encode_config_rejects_mismatched_shape() {
        let v = serde_json::json!({ "bot_token": "x" }); // missing required fields
        assert!(encode_config(ChannelKind::Signal, &v).is_err());
    }
}
