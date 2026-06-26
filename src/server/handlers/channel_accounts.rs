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
use crate::web::LiveConfig;
use crate::MiraError;
use tracing::{info, warn};

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

/// Restore secret fields the client sent back in redacted form. GET masks
/// secrets with the '•' bullet; when an update carries a value still containing
/// '•', the user didn't re-type it, so we substitute the real value from the
/// stored config. Generic over the flat channel-config objects (top-level string
/// fields), so it covers bot_token / access_token / signing_secret / app_secret /
/// inbound_secret / outbound_secret / secret_token without enumerating them. No
/// legitimate secret contains '•'.
fn restore_redacted_config(incoming: &mut serde_json::Value, existing_json: &str) {
    let Ok(existing) = serde_json::from_str::<serde_json::Value>(existing_json) else { return };
    let (Some(obj), Some(exist)) = (incoming.as_object_mut(), existing.as_object()) else { return };
    for (k, v) in obj.iter_mut() {
        if let Some(s) = v.as_str() {
            if s.contains('•') {
                if let Some(orig) = exist.get(k) {
                    *v = orig.clone();
                }
            }
        }
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

// Turn on the global `channels.<kind>.enabled` gate for a channel type when an
// account of that type is added. The per-account row is the primary control;
// this global flag is a kill switch that defaults off, so without flipping it an
// added account is gated off and receives nothing. No-op if already on.
async fn ensure_channel_type_enabled(live: &LiveConfig, kind: ChannelKind) {
    let cur = live.get().await;
    let already = match kind {
        ChannelKind::Signal   => cur.channels.signal.enabled,
        ChannelKind::Telegram => cur.channels.telegram.enabled,
        ChannelKind::Discord  => cur.channels.discord.enabled,
        ChannelKind::Matrix   => cur.channels.matrix.enabled,
        ChannelKind::WhatsApp => cur.channels.whatsapp.enabled,
        ChannelKind::Slack    => cur.channels.slack.enabled,
        ChannelKind::External => cur.channels.external.enabled,
    };
    if already {
        return;
    }
    let mut next = (*cur).clone();
    match kind {
        ChannelKind::Signal   => next.channels.signal.enabled = true,
        ChannelKind::Telegram => next.channels.telegram.enabled = true,
        ChannelKind::Discord  => next.channels.discord.enabled = true,
        ChannelKind::Matrix   => next.channels.matrix.enabled = true,
        ChannelKind::WhatsApp => next.channels.whatsapp.enabled = true,
        ChannelKind::Slack    => next.channels.slack.enabled = true,
        ChannelKind::External => next.channels.external.enabled = true,
    }
    match live.update(next).await {
        Ok(())  => info!("Auto-enabled '{}' channel (an account was added)", kind.as_str()),
        Err(e)  => warn!("Could not auto-enable '{}' channel: {e}", kind.as_str()),
    }
}

// ── POST /api/channel-accounts ───────────────────────────────────────────────

pub async fn create_account(
    AuthUser(caller): AuthUser,
    Extension(store): Extension<Arc<ChannelAccountStore>>,
    Extension(auth):  Extension<Arc<LocalAuthService>>,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
    // Optional: present only when the gateway wired the ChannelManager (absent
    // in tests / minimal builds). Used to auto-start the new account's
    // poller/daemon so a freshly added bot works without a service restart.
    mgr: Option<Extension<ChannelManagerExt>>,
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
            // Adding an enabled account is intent to use that channel — flip the
            // global `channels.<kind>.enabled` gate on so messages aren't
            // silently dropped. Without this, a freshly-added Telegram bot
            // (per-account enabled=true) still never receives anything because
            // the global kill switch defaults off. Best-effort: a config write
            // failure shouldn't fail the account creation. Takes effect live
            // (the inbound handlers read the gate per-message).
            if acct.enabled {
                ensure_channel_type_enabled(&live_cfg, kind).await;
                // Spawn the live poller/daemon now, so a freshly added bot
                // starts receiving messages immediately — no service restart
                // and no separate "Start" click. Best-effort: a failure here
                // does NOT fail the create (the account is saved; a restart or
                // the per-account Start endpoint still picks it up). This is the
                // fix for "added a Telegram bot under a second account but it
                // never receives anything" — create used to only persist the
                // row, and the poller wasn't started until the next start_all.
                if let Some(Extension(cm)) = &mgr {
                    let id = acct.id.clone();
                    if let Err(e) = cm.0.write().await.start_account(&id).await {
                        warn!("channel account {id}: saved but poller didn't auto-start ({e}); \
                               a service restart or the per-account Start action will pick it up");
                        // Signal on a fresh box: the daemon can't start because
                        // signal-cli (and, off Linux-x86_64, a JRE) isn't
                        // installed yet. Auto-install the MIRA-managed runtime
                        // in the background — a ~100 MB download — then start
                        // the account once it's ready. The user doesn't have to
                        // install Java or signal-cli by hand. Idempotent and
                        // best-effort; gated so we don't download when signal-cli
                        // is already available (the start failed for another
                        // reason, e.g. the number isn't registered).
                        if kind == ChannelKind::Signal
                            && !crate::install::deps::signal_cli_present("signal-cli")
                        {
                            let cm2 = Arc::clone(&cm.0);
                            tokio::spawn(async move {
                                info!("signal account {id}: installing managed signal-cli + JRE in background…");
                                match tokio::task::spawn_blocking(
                                    || crate::install::deps::ensure_signal_runtime(false)
                                        .map_err(|e| e.to_string())
                                ).await {
                                    Ok(Ok(summary)) => {
                                        info!("signal account {id}: runtime ready ({summary}); starting daemon");
                                        if let Err(e) = cm2.write().await.start_account(&id).await {
                                            warn!("signal account {id}: runtime installed but daemon start failed \
                                                   ({e}); check the number is registered, then use Start");
                                        }
                                    }
                                    Ok(Err(e)) => warn!("signal account {id}: managed runtime install failed: {e}"),
                                    Err(e)     => warn!("signal account {id}: runtime install task panicked: {e}"),
                                }
                            });
                        }
                    } else {
                        info!("channel account {id}: poller/daemon started on create");
                    }
                }
            }
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
    // Optional (absent in tests/minimal builds) — reconcile the live
    // poller/daemon with the updated row so token/mode edits and (re)enables
    // take effect immediately, without a service restart.
    mgr: Option<Extension<ChannelManagerExt>>,
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
    let config_json = if let Some(mut cfg) = req.config {
        // Restore redacted secrets: GET redacts secret fields to a masked form
        // containing the '•' bullet. If the UI sends that masked value back
        // (e.g. the user only changed `mode` and never re-typed the token), keep
        // the real stored value instead of persisting the mask — otherwise
        // editing any field destroys the secret. Mirrors the `***` sentinel
        // restore on PUT /api/config. No real secret contains '•'.
        restore_redacted_config(&mut cfg, &existing.config_json);
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
        Ok(acct) => {
            // Reconcile the live poller/daemon: restart it on the new config
            // when enabled (picks up a changed token/mode, or starts it on a
            // re-enable), stop it when disabled. Best-effort — a failure just
            // means the change applies on the next restart.
            if let Some(Extension(cm)) = &mgr {
                let mut guard = cm.0.write().await;
                let res: Result<(), String> = if acct.enabled {
                    guard.restart_account(&id).await
                } else {
                    guard.stop_account(&id).await.map(|_| ())
                };
                if let Err(e) = res {
                    warn!("channel account {id}: updated but live reconcile failed ({e}); \
                           a service restart will apply it");
                }
            }
            match ChannelAccountResponse::from_row(acct) {
                Ok(r)  => axum::Json(r).into_response(),
                Err(e) => err_resp(e),
            }
        }
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
    fn restore_redacted_keeps_real_secret_when_mask_sent_back() {
        // Simulate: GET redacted the token; the UI sends it back unchanged while
        // only flipping `mode`. The real token must survive.
        let real = "8941234567:AAHrealtokenrealtokenrealtoken12345";
        let existing = serde_json::json!({ "bot_token": real, "mode": "webhook" }).to_string();
        let mut incoming = serde_json::json!({ "bot_token": redact(real), "mode": "polling" });
        restore_redacted_config(&mut incoming, &existing);
        assert_eq!(incoming["bot_token"], real, "real token must be restored");
        assert_eq!(incoming["mode"], "polling", "non-secret edit preserved");
    }

    #[test]
    fn restore_redacted_accepts_newly_typed_secret() {
        // A freshly-typed token (no bullet) must pass through unchanged.
        let existing = serde_json::json!({ "bot_token": "oldoldoldold:OLD" }).to_string();
        let mut incoming = serde_json::json!({ "bot_token": "9999999999:AAHbrandnewtokenbrandnewtoken99999" });
        restore_redacted_config(&mut incoming, &existing);
        assert_eq!(incoming["bot_token"], "9999999999:AAHbrandnewtokenbrandnewtoken99999");
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
