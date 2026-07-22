// SPDX-License-Identifier: AGPL-3.0-or-later

// src/gateway/channel_manager.rs
//! Per-account lifecycle manager for channel daemons.
//!
//! The `ChannelManager` replaces the legacy single-instance startup in
//! `GatewayBuilder`. At gateway start it iterates every enabled
//! [`ChannelAccount`] and fans out:
//!
//! - **Signal** — starts a dedicated `signal-cli` daemon per account on an
//! auto-allocated REST port, then spawns a [`SignalSseListener`] keyed to
//! that account's owner so inbound messages land under the right user.
//! - **Telegram** — builds an `AccountCtx` lookup table used by the
//! shared `/webhook/telegram/{account_id}` handler. No daemon is
//! launched — Telegram delivers via the shared webhook.
//!
//! ## Design notes
//!
//! - **Port allocator**: scans upward from [`BASE_SIGNAL_PORT`] using
//! `TcpListener::bind` to probe for a free port. Only called when the
//! stored config has no `rest_port` yet; on first launch we rewrite the
//! account's `config_json` so the port survives restarts.
//! - **Non-fatal startup**: a failed daemon logs and the rest keep going.
//! A single misconfigured Signal account should never take down Telegram.
//! - **`user_id` stamping**: both listeners receive `owner_user_id` at
//! construction and pass it to
//! [`HistoryStore::find_or_create_external_conversation`] — inbound
//! threads are scoped to (owner, channel, sender), matching the 
//! visibility filter.

use std::collections::HashMap;
use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;

use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::agent::AgentCore;
use crate::auth::LocalAuthService;
use crate::channel_accounts::{
    ChannelAccount, ChannelAccountStore, ChannelKind, SignalAccountConfig, UpdateChannelAccount,
};
use crate::history::HistoryStore;
use crate::providers::signal_cli::daemon::SignalCliDaemon;
use crate::providers::signal_cli::sse_listener::SignalSseListener;
use crate::stt::SttService;
use crate::tts::TtsService;

// ── Port allocation ──────────────────────────────────────────────────────────

const BASE_SIGNAL_PORT: u16 = 18080;
const MAX_PORT_SCAN:    u16 = 200;

// Find a free localhost TCP port starting at `base`. Returns `None` after
// `MAX_PORT_SCAN` consecutive failures — a clear signal the operator is
// running something unusual and should configure ports manually.
fn allocate_free_port(base: u16, skip: &[u16]) -> Option<u16> {
    let mut port = base;
    for _ in 0..MAX_PORT_SCAN {
        if !skip.contains(&port) {
            if StdTcpListener::bind(("127.0.0.1", port)).is_ok() {
                return Some(port);
            }
        }
        port = port.checked_add(1)?;
    }
    None
}

// ── Public types ─────────────────────────────────────────────────────────────

// Per-account context consumed by the shared Telegram webhook handler.
// Cheap to clone — a thin wrapper around owned `String`s.
#[derive(Clone, Debug)]
pub struct TelegramAccountCtx {
    pub account_id:    String,
    pub owner_user_id: String,
    pub bot_token:     String,
    pub secret_token:  Option<String>,
    // R1+R2 routing mode. `Personal` runs every inbound as
    // `owner_user_id` (the v1 single-user model); `Shared`/`GuestOk`
    // resolve the sender via the identity store. Defaults to `Personal`
    // for existing rows via the DB migration.
    pub routing_mode:  crate::channel_accounts::RoutingMode,
}

// Runtime state for a live Signal account: the daemon itself and the SSE
// listener task handle. Both are kept alive in `ChannelManager::signal`.
pub struct SignalRuntime {
    #[allow(dead_code)]
    pub account_id: String,
    pub daemon:     SignalCliDaemon,
    pub listener:   JoinHandle<()>,
}

// Runtime state for a polling-mode Telegram account. The handle is held
// here so stop_account / restart_account can abort the long-poll task.
// Webhook-mode accounts don't have a runtime — they live entirely in
// the `telegram` HashMap and are dispatched by the shared HTTP handler.
pub struct TelegramRuntime {
    pub account_id: String,
    pub poller:     JoinHandle<()>,
}

// Runtime state for a live Discord account. The WebSocket gateway
// connection is a long-lived task held here so `stop_account` /
// `restart_account` can request a clean shutdown via the Notify and
// then abort the join handle on drop.
pub struct DiscordRuntime {
    pub account_id: String,
    pub gateway:    JoinHandle<()>,
    // Set when the manager asks the gateway loop to close cleanly
    // (sends a `Close` frame and returns). The loop also drops on
    // `gateway.abort()` if the Notify path is racing a hang.
    pub shutdown:   Arc<tokio::sync::Notify>,
}

// Runtime state for a live Matrix account — the `/sync` long-poll task.
// Same shape as `DiscordRuntime`: hold the join handle for abort-on-drop
// and a Notify so stop_account can end the poll loop promptly.
pub struct MatrixRuntime {
    pub account_id: String,
    pub sync_task:  JoinHandle<()>,
    pub shutdown:   Arc<tokio::sync::Notify>,
}

// Bundle of long-lived dependencies the per-account starter needs.
// Stashed on `ChannelManager` during `start_all` so the lifecycle
// methods (`start_account` / `stop_account` / `restart_account`)
// invoked from HTTP handlers don't have to drag the same 7 args
// across the wire — they just call `start_account(account_id)`.
#[derive(Clone)]
pub struct AccountDeps {
    pub store:       Arc<ChannelAccountStore>,
    pub agent_core:  Arc<AgentCore>,
    pub history:     Option<Arc<HistoryStore>>,
    pub auth:        Option<Arc<LocalAuthService>>,
    pub stt:         Option<SttService>,
    pub tts:         Option<TtsService>,
    // Threaded into the Telegram polling daemon's `TelegramState` so the
    // MIRA-wide `channels.telegram.enabled` kill switch is honoured by
    // poll-mode bots, not just webhook-mode bots.
    pub live_config: Option<Arc<crate::web::LiveConfig>>,
    // threaded into the Telegram polling daemon's
    // `TelegramState` so poll-mode bots honour the per-user MCP
    // filter the same way webhook-mode bots do.
    pub mcp_servers: Option<Arc<crate::mcp::McpServerRegistry>>,
    // R1+R2 — channel-identity lookup (Shared/GuestOk bots) + pending
    // one-time link codes. Both `None` falls every channel back to
    // `Personal` semantics (matches pre-R1+R2 behaviour).
    pub identity:    Option<Arc<crate::channel_identity::IdentityStore>>,
    pub link_codes:  Option<Arc<crate::channel_identity::LinkCodeStore>>,
}

// Owns per-account runtimes and shares a lookup table with the Telegram
// webhook handler.
pub struct ChannelManager {
    pub signal:   Vec<SignalRuntime>,
    pub telegram: HashMap<String, TelegramAccountCtx>,
    // Polling-mode Telegram tasks. Held so stop_account can abort the
    // long-poll loop and re-spawn it on start/restart. Webhook-mode
    // accounts are absent here — their delivery is handled at the
    // HTTP layer with no per-account task.
    pub telegram_polling: Vec<TelegramRuntime>,
    // Live Discord gateway tasks — one per enabled account. Held so
    // stop_account / restart_account can request a clean WS close and
    // then abort. Outbound dispatch (D3+) will read the bot_token via
    // a parallel `discord_ctx` lookup map.
    pub discord: Vec<DiscordRuntime>,
    // Live Matrix `/sync` long-poll tasks — one per enabled account.
    pub matrix: Vec<MatrixRuntime>,
    // WhatsApp account contexts keyed by account id. Like Telegram's
    // webhook mode there is no per-account task — Meta POSTs inbound to
    // the shared `/webhook/whatsapp/{account_id}` handler, which looks
    // the ctx up here. Cloned into the router's WhatsAppState at startup.
    pub whatsapp: HashMap<String, crate::whatsapp::WhatsAppAccountCtx>,
    // Slack account contexts keyed by account id — same webhook-driven
    // model as WhatsApp. Read by the shared `/webhook/slack/{id}` handler.
    pub slack: HashMap<String, crate::slack::SlackAccountCtx>,
    // External (CPP) account contexts keyed by account id — webhook-driven
    // like Slack/WhatsApp. Read by the `/webhook/external/{id}` handler.
    pub external: HashMap<String, crate::external::ExternalAccountCtx>,
    // Captured during `start_all` so per-account lifecycle methods
    // invoked later (from HTTP handlers, on user start/stop/restart
    // clicks) can re-spawn a daemon without the gateway having to
    // re-thread every dependency through the call site. None until
    // the first `start_all` runs.
    pub deps:     Option<AccountDeps>,
}

impl ChannelManager {
    pub fn new() -> Self {
        Self {
            signal: Vec::new(),
            telegram: HashMap::new(),
            telegram_polling: Vec::new(),
            discord: Vec::new(),
            matrix: Vec::new(),
            whatsapp: HashMap::new(),
            slack: HashMap::new(),
            external: HashMap::new(),
            deps: None,
        }
    }

    // 0.108.0 — snapshot of (account_id, alive) per Signal account
    // for the `channel.signal.daemon_alive` health detector. Takes
    // `&mut self` because `is_running()` performs a `try_wait` that
    // requires mutable access to the child handle. Caller is expected
    // to hold a write lock on the manager only briefly.
    pub fn signal_account_aliveness(&mut self) -> Vec<(String, bool)> {
        self.signal.iter_mut()
            .map(|rt| (rt.account_id.clone(), rt.daemon.is_running()))
            .collect()
    }

    // Every known channel account id (across all channels/users), as recorded
    // in the account store. This is the authoritative set of valid
    // `restart_account` / `restart_bridge` targets. Empty when the store deps
    // haven't been wired yet (pre-`start_all`) or the lookup fails — callers
    // treat empty as "can't validate" and fail open. Used by the Guardian's
    // propose/inspect tools so the model targets a real account instead of
    // guessing a label like "signal" or "guardian".
    pub fn known_account_ids(&self) -> Vec<String> {
        self.deps.as_ref()
            .and_then(|d| d.store.list_all().ok())
            .map(|rows| rows.into_iter().map(|a| a.id).collect())
            .unwrap_or_default()
    }

    // Fan out all enabled accounts. Errors on individual accounts are
    // logged and swallowed — one misconfigured row shouldn't block the
    // others.
    pub async fn start_all(
        &mut self,
        store:       Arc<ChannelAccountStore>,
        agent_core:  Arc<AgentCore>,
        history:     Option<Arc<HistoryStore>>,
        auth:        Option<Arc<LocalAuthService>>,
        stt:         Option<SttService>,
        tts:         Option<TtsService>,
        live_config: Option<Arc<crate::web::LiveConfig>>,
        mcp_servers: Option<Arc<crate::mcp::McpServerRegistry>>,
        identity:    Option<Arc<crate::channel_identity::IdentityStore>>,
        link_codes:  Option<Arc<crate::channel_identity::LinkCodeStore>>,
    ) {
        // Stash the deps so per-account lifecycle endpoints
        // (`start_account` / `stop_account` / `restart_account`) can
        // re-invoke the starter later without the gateway having to
        // re-thread every dependency through the HTTP layer.
        self.deps = Some(AccountDeps {
            store:       Arc::clone(&store),
            agent_core:  Arc::clone(&agent_core),
            history:     history.clone(),
            auth:        auth.clone(),
            stt:         stt.clone(),
            tts:         tts.clone(),
            live_config: live_config.clone(),
            mcp_servers: mcp_servers.clone(),
            identity:    identity.clone(),
            link_codes:  link_codes.clone(),
        });

        let accounts = match store.list_enabled() {
            Ok(a)  => a,
            Err(e) => { warn!("channel_accounts list_enabled failed: {}", e); return; }
        };

        for acct in accounts {
            match acct.channel {
                ChannelKind::Signal => {
                    if let Err(e) = self
                        .start_signal_account(&acct, &store, &agent_core, &history, &auth, &stt, &tts)
                        .await
                    {
                        warn!(
                            "Signal account '{}' (user={}) failed to start: {}",
                            acct.account_label, acct.user_id, e
                        );
                    }
                }
                ChannelKind::Telegram => {
                    if let Err(e) = self.register_telegram_account(&acct) {
                        warn!(
                            "Telegram account '{}' (user={}) misconfigured: {}",
                            acct.account_label, acct.user_id, e
                        );
                    }
                }
                ChannelKind::Discord => {
                    if let Err(e) = self.register_discord_account(&acct) {
                        warn!(
                            "Discord account '{}' (user={}) misconfigured: {}",
                            acct.account_label, acct.user_id, e
                        );
                    }
                }
                ChannelKind::Matrix => {
                    if let Err(e) = self.register_matrix_account(&acct) {
                        warn!(
                            "Matrix account '{}' (user={}) misconfigured: {}",
                            acct.account_label, acct.user_id, e
                        );
                    }
                }
                ChannelKind::WhatsApp => {
                    if let Err(e) = self.register_whatsapp_account(&acct) {
                        warn!(
                            "WhatsApp account '{}' (user={}) misconfigured: {}",
                            acct.account_label, acct.user_id, e
                        );
                    }
                }
                ChannelKind::Slack => {
                    if let Err(e) = self.register_slack_account(&acct) {
                        warn!(
                            "Slack account '{}' (user={}) misconfigured: {}",
                            acct.account_label, acct.user_id, e
                        );
                    }
                }
                ChannelKind::External => {
                    if let Err(e) = self.register_external_account(&acct) {
                        warn!(
                            "External account '{}' (user={}) misconfigured: {}",
                            acct.account_label, acct.user_id, e
                        );
                    }
                }
            }
        }

        info!(
            "ChannelManager started — signal={} telegram={} discord={} matrix={} whatsapp={} slack={} external={}",
            self.signal.len(), self.telegram.len(), self.discord.len(),
            self.matrix.len(), self.whatsapp.len(), self.slack.len(), self.external.len(),
        );
    }

    // Stop one Signal account's daemon + listener. Idempotent — no-op
    // when the account isn't currently running. Returns `Ok(true)` if
    // a daemon was actually stopped, `Ok(false)` when the account was
    // already idle.
    pub async fn stop_account(&mut self, account_id: &str) -> Result<bool, String> {
        // Try Signal first.
        if let Some(idx) = self.signal.iter().position(|rt| rt.account_id == account_id) {
            let SignalRuntime { account_id, mut daemon, listener } = self.signal.remove(idx);
            info!("Stopping Signal daemon for account {}", account_id);
            daemon.stop().await;
            listener.abort();
            return Ok(true);
        }
        // Then Telegram polling.
        if let Some(idx) = self.telegram_polling.iter().position(|rt| rt.account_id == account_id) {
            let TelegramRuntime { account_id, poller } = self.telegram_polling.remove(idx);
            info!("Stopping Telegram poller for account {}", account_id);
            poller.abort();
            // Also drop the webhook-shared ctx entry so the bot isn't
            // reachable for outbound either until restart.
            self.telegram.remove(&account_id);
            return Ok(true);
        }
        // Telegram webhook-mode (no poller task — just the outbound ctx). Drop
        // it so a `restart_account` can re-register a changed token, and so
        // `start_account`'s already-running guard doesn't block a fresh start.
        if self.telegram.remove(account_id).is_some() {
            info!("Removed Telegram webhook ctx for account {}", account_id);
            return Ok(true);
        }
        // Then Discord. Notify the gateway loop to send a clean WS close,
        // give it a brief moment, then abort the task if it's still alive
        // (covers a hang during close write).
        if let Some(idx) = self.discord.iter().position(|rt| rt.account_id == account_id) {
            let DiscordRuntime { account_id, gateway, shutdown } = self.discord.remove(idx);
            info!("Stopping Discord gateway for account {}", account_id);
            shutdown.notify_waiters();
            // 2 s is more than enough for one WS Close round-trip; the
            // task aborts on drop regardless.
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                gateway,
            ).await;
            return Ok(true);
        }
        // Then Matrix — same clean-shutdown-then-abort dance.
        if let Some(idx) = self.matrix.iter().position(|rt| rt.account_id == account_id) {
            let MatrixRuntime { account_id, sync_task, shutdown } = self.matrix.remove(idx);
            info!("Stopping Matrix sync loop for account {}", account_id);
            shutdown.notify_waiters();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                sync_task,
            ).await;
            return Ok(true);
        }
        Ok(false)
    }

    // Start one Signal account by id. Looks up the row in the store,
    // resolves the bundled deps captured during `start_all`, and runs
    // the same per-account starter as the boot path. Returns Err with
    // a user-readable message when the account is already running, the
    // id is unknown, the channel isn't `signal`, or the daemon spawn
    // fails.
    pub async fn start_account(&mut self, account_id: &str) -> Result<(), String> {
        if self.signal.iter().any(|rt| rt.account_id == account_id)
            || self.discord.iter().any(|rt| rt.account_id == account_id)
            || self.matrix.iter().any(|rt| rt.account_id == account_id)
        {
            return Err(format!("account {account_id} already running"));
        }
        let deps = self.deps.clone()
            .ok_or_else(|| "ChannelManager.start_all has not run yet".to_string())?;
        let acct = deps.store.get(account_id)
            .map_err(|e| format!("account lookup failed: {e}"))?
            .ok_or_else(|| format!("account {account_id} not found"))?;
        if !acct.enabled {
            return Err(format!("account {account_id} is disabled — enable it first"));
        }
        match acct.channel {
            ChannelKind::Signal => self.start_signal_account(
                &acct, &deps.store, &deps.agent_core,
                &deps.history, &deps.auth, &deps.stt, &deps.tts,
            ).await,
            ChannelKind::Discord => self.register_discord_account(&acct),
            ChannelKind::Matrix  => self.register_matrix_account(&acct),
            ChannelKind::Telegram => {
                // Telegram is NOT purely webhook-driven: the per-account mode
                // defaults to "polling", and a polling account needs its
                // getUpdates poller spawned just like at boot. (`stop_account`
                // already aborts that poller + drops the ctx.) Reuse the same
                // registrar `start_all` uses — it spawns the poller for polling
                // mode and registers the outbound ctx for webhook mode. Without
                // this, a Telegram account added after boot (e.g. a second
                // user's bot) is saved + enabled but never polled until the
                // next full restart.
                if self.telegram_polling.iter().any(|rt| rt.account_id == account_id)
                    || self.telegram.contains_key(account_id)
                {
                    return Err(format!("account {account_id} already running"));
                }
                self.register_telegram_account(&acct)
            }
            ChannelKind::WhatsApp => Err(format!(
                "account {account_id} is channel=whatsapp; whatsapp lifecycle is webhook-driven",
            )),
            ChannelKind::Slack => Err(format!(
                "account {account_id} is channel=slack; slack lifecycle is webhook-driven",
            )),
            ChannelKind::External => Err(format!(
                "account {account_id} is channel=external; external lifecycle is webhook-driven",
            )),
        }
    }

    // Stop then start one Signal account. Convenience wrapper for the
    // "Restart" UI button. The brief gap between stop and start is
    // not coordinated against incoming SSE events — callers that need
    // to drain in-flight messages first should `stop_account()`,
    // wait, then `start_account()` themselves.
    pub async fn restart_account(&mut self, account_id: &str) -> Result<(), String> {
        let _ = self.stop_account(account_id).await?;
        self.start_account(account_id).await
    }

    // Stop all signal daemons + telegram pollers. Called from
    // `Gateway::run_until_shutdown`.
    pub async fn shutdown(&mut self) {
        for rt in self.signal.drain(..) {
            let SignalRuntime { account_id, mut daemon, listener } = rt;
            info!("Stopping Signal daemon for account {}", account_id);
            daemon.stop().await;
            listener.abort();
        }
        for rt in self.telegram_polling.drain(..) {
            let TelegramRuntime { account_id, poller } = rt;
            info!("Stopping Telegram poller for account {}", account_id);
            poller.abort();
        }
    }

    // ── Internal helpers ─────────────────────────────────────────────────

    async fn start_signal_account(
        &mut self,
        acct:       &ChannelAccount,
        store:      &Arc<ChannelAccountStore>,
        agent_core: &Arc<AgentCore>,
        history:    &Option<Arc<HistoryStore>>,
        auth:       &Option<Arc<LocalAuthService>>,
        stt:        &Option<SttService>,
        tts:        &Option<TtsService>,
    ) -> Result<(), String> {
        let mut cfg = acct.signal_config().map_err(|e| e.to_string())?;

        // Resolve port: reuse the saved one if present, else allocate and
        // persist so subsequent restarts stay stable.
        let port = match cfg.rest_port {
            Some(p) => p,
            None => {
                let used: Vec<u16> = self.signal.iter()
                    .filter_map(|rt| Some(rt.daemon.port))
                    .collect();
                let p = allocate_free_port(BASE_SIGNAL_PORT, &used)
                    .ok_or_else(|| "no free TCP port in scan range".to_string())?;
                cfg.rest_port = Some(p);
                let json = serde_json::to_string(&cfg)
                    .map_err(|e| format!("reserialise signal cfg: {}", e))?;
                store.update(&acct.id, UpdateChannelAccount {
                    config_json: Some(json),
                    ..Default::default()
                }).map_err(|e| format!("persist allocated port: {}", e))?;
                info!("Allocated Signal REST port {} for account {}", p, acct.account_label);
                p
            }
        };

        // Each account gets its own data dir so signal-cli keystores don't
        // stomp on each other when a host serves multiple numbers.
        let data_dir = per_account_data_dir(&cfg, &acct.id);

        // Auto-migrate from a legacy base data_dir on first boot under
        // the per-account scheme. Without this, an existing install that
        // had its number registered against the shared
        // ~/.local/share/signal-cli/data/ would suddenly hit
        // "User <number> is not registered" the moment per-account data
        // dirs landed (signal-cli reads only its --config tree).
        // Idempotent + best-effort: a copy failure is logged but does
        // NOT abort startup — the daemon will then surface the
        // not-registered error itself, which is the right diagnostic.
        if let Err(e) = migrate_signal_data_if_needed(
            &crate::config::expand_path(&cfg.data_dir),
            std::path::Path::new(&data_dir),
            &cfg.phone_number,
        ) {
            warn!(
                "Signal data migration for {} skipped/failed: {e}. \
                 The daemon may report the number as unregistered.",
                acct.account_label,
            );
        }

        // Prefer a MIRA-managed signal-cli + JRE under ~/.mira/deps (auto-
        // installed when the channel is enabled) over a system install, so a
        // fresh box needs no manual Java/signal-cli setup. Falls back to the
        // configured `cli_binary` + system Java when no managed install exists.
        let (binary, java_home) = crate::install::deps::resolve_signal_cli(&cfg.cli_binary);
        let mut daemon = SignalCliDaemon::new(
            binary,
            cfg.phone_number.clone(),
            port,
            data_dir.clone(),
        ).with_java_home(java_home);
        daemon.start(15).await.map_err(|e| format!("daemon start: {}", e))?;
        info!(
            "Signal daemon up on :{} for {} (user={})",
            port, cfg.phone_number, acct.user_id
        );

        let listener = SignalSseListener::new(
            port,
            cfg.phone_number.clone(),
            data_dir,
            Arc::clone(agent_core),
            history.clone(),
            acct.user_id.clone(),
            auth.clone(),
            stt.clone(),
            tts.clone(),
        );
        let handle = tokio::spawn(listener.run());

        self.signal.push(SignalRuntime {
            account_id: acct.id.clone(),
            daemon,
            listener: handle,
        });
        Ok(())
    }

    fn register_telegram_account(&mut self, acct: &ChannelAccount) -> Result<(), String> {
        let cfg = acct.telegram_config().map_err(|e| e.to_string())?;
        let mode = cfg.mode.clone();
        let poll_timeout = cfg.poll_timeout_secs;
        let ctx = TelegramAccountCtx {
            account_id:    acct.id.clone(),
            owner_user_id: acct.user_id.clone(),
            bot_token:     cfg.bot_token,
            secret_token:  cfg.secret_token,
            routing_mode:  acct.routing_mode,
        };
        // Always register the ctx in the shared map — even polling-mode
        // accounts need their bot_token reachable for outbound delivery
        // from the companion + automations dispatchers, and the shape
        // matches the webhook handler's lookup so a mode flip doesn't
        // re-wire anything else.
        self.telegram.insert(acct.id.clone(), ctx.clone());

        match mode.as_str() {
            "polling" => {
                // Build the same TelegramState shape the webhook handler
                // uses, scoped to just this account. The poller calls
                // `process_message_for_account` which doesn't touch the
                // `accounts` map (it already has the ctx in hand).
                let Some(deps) = self.deps.clone() else {
                    return Err("polling mode requires AccountDeps (start_all must run first)".into());
                };
                let state = crate::server::handlers::telegram::TelegramState {
                    agent_core:  Arc::clone(&deps.agent_core),
                    http_client: crate::server::handlers::telegram::telegram_http_client(),
                    history:     deps.history.clone(),
                    accounts:    Arc::new(HashMap::new()),
                    tts:         deps.tts.clone(),
                    stt:         deps.stt.clone(),
                    auth:        deps.auth.clone(),
                    live_config: deps.live_config.clone(),
                    mcp_servers: deps.mcp_servers.clone(),
                    identity:    deps.identity.clone(),
                    link_codes:  deps.link_codes.clone(),
                };
                let poller = crate::server::handlers::telegram::spawn_telegram_poller(
                    state, ctx, poll_timeout,
                );
                self.telegram_polling.push(TelegramRuntime {
                    account_id: acct.id.clone(),
                    poller,
                });
                info!(
                    "Telegram account '{}' polling (user={} id={} timeout={}s)",
                    acct.account_label, acct.user_id, acct.id, poll_timeout,
                );
            }
            _ => {
                // Webhook mode — nothing per-account to spawn; the shared
                // /webhook/telegram/{account_id} handler picks the ctx
                // up from `self.telegram` at request time.
                info!(
                    "Telegram account '{}' registered (webhook) for user {} (id={})",
                    acct.account_label, acct.user_id, acct.id,
                );
            }
        }
        Ok(())
    }

    fn register_discord_account(&mut self, acct: &ChannelAccount) -> Result<(), String> {
        let cfg = acct.discord_config().map_err(|e| e.to_string())?;
        if cfg.bot_token.trim().is_empty() {
            return Err("bot_token is empty".into());
        }
        let Some(deps) = self.deps.clone() else {
            return Err("Discord requires AccountDeps (start_all must run first)".into());
        };

        let mention_only = cfg.mention_only;
        let ctx = crate::discord::DiscordAccountCtx {
            account_id:     acct.id.clone(),
            owner_user_id:  acct.user_id.clone(),
            bot_token:      cfg.bot_token,
            application_id: cfg.application_id,
            mention_only,
            routing_mode:   acct.routing_mode,
        };
        let dispatcher_deps = crate::discord::DiscordDispatcherDeps {
            agent_core:  Arc::clone(&deps.agent_core),
            history:     deps.history.clone(),
            auth:        deps.auth.clone(),
            live_config: deps.live_config.clone(),
            mcp_servers: deps.mcp_servers.clone(),
            // Per-account reqwest::Client — light enough to make one;
            // shares the rustls store with the rest of the binary.
            http_client: crate::server::handlers::telegram::telegram_http_client(),
            identity:    deps.identity.clone(),
            link_codes:  deps.link_codes.clone(),
        };
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let gateway  = crate::discord::spawn_gateway(
            ctx, dispatcher_deps, Arc::clone(&shutdown),
        );

        self.discord.push(DiscordRuntime {
            account_id: acct.id.clone(),
            gateway,
            shutdown,
        });
        info!(
            "Discord account '{}' connected (user={} id={} mention_only={})",
            acct.account_label, acct.user_id, acct.id, mention_only,
        );
        Ok(())
    }

    fn register_matrix_account(&mut self, acct: &ChannelAccount) -> Result<(), String> {
        let cfg = acct.matrix_config().map_err(|e| e.to_string())?;
        if cfg.homeserver.trim().is_empty() || cfg.access_token.trim().is_empty() {
            return Err("homeserver and access_token are required".into());
        }
        let Some(deps) = self.deps.clone() else {
            return Err("Matrix requires AccountDeps (start_all must run first)".into());
        };

        let mention_only = cfg.mention_only;
        let loop_cfg = crate::matrix::MatrixLoopConfig {
            account_id:    acct.id.clone(),
            owner_user_id: acct.user_id.clone(),
            homeserver:    cfg.homeserver,
            access_token:  cfg.access_token,
            mention_only,
            routing_mode:  acct.routing_mode,
        };
        let dispatcher_deps = crate::matrix::MatrixDispatcherDeps {
            agent_core:  Arc::clone(&deps.agent_core),
            history:     deps.history.clone(),
            auth:        deps.auth.clone(),
            live_config: deps.live_config.clone(),
            mcp_servers: deps.mcp_servers.clone(),
            http_client: crate::server::handlers::telegram::telegram_http_client(),
            identity:    deps.identity.clone(),
            link_codes:  deps.link_codes.clone(),
        };
        let shutdown  = Arc::new(tokio::sync::Notify::new());
        let sync_task = crate::matrix::spawn_sync_loop(
            loop_cfg, dispatcher_deps, Arc::clone(&shutdown),
        );

        self.matrix.push(MatrixRuntime {
            account_id: acct.id.clone(),
            sync_task,
            shutdown,
        });
        info!(
            "Matrix account '{}' started (user={} id={} mention_only={})",
            acct.account_label, acct.user_id, acct.id, mention_only,
        );
        Ok(())
    }

    fn register_whatsapp_account(&mut self, acct: &ChannelAccount) -> Result<(), String> {
        let cfg = acct.whatsapp_config().map_err(|e| e.to_string())?;
        if cfg.phone_number_id.trim().is_empty()
            || cfg.access_token.trim().is_empty()
            || cfg.verify_token.trim().is_empty()
        {
            return Err("phone_number_id, access_token and verify_token are required".into());
        }
        if cfg.app_secret.as_deref().map(str::trim).unwrap_or("").is_empty() {
            warn!(
                "WhatsApp account '{}' has no app_secret — inbound webhook \
                 signature verification is DISABLED (anyone who learns the \
                 URL can post). Set an app secret in Channels to enable it.",
                acct.account_label,
            );
        }
        // Webhook-driven: no per-account task, just register the ctx in the
        // shared map the /webhook/whatsapp/{id} handler reads at request time.
        let ctx = crate::whatsapp::WhatsAppAccountCtx {
            account_id:      acct.id.clone(),
            owner_user_id:   acct.user_id.clone(),
            phone_number_id: cfg.phone_number_id,
            access_token:    cfg.access_token,
            app_secret:      cfg.app_secret,
            verify_token:    cfg.verify_token,
            mention_only:    cfg.mention_only,
            routing_mode:    acct.routing_mode,
        };
        self.whatsapp.insert(acct.id.clone(), ctx);
        info!(
            "WhatsApp account '{}' registered (user={} id={})",
            acct.account_label, acct.user_id, acct.id,
        );
        Ok(())
    }

    fn register_slack_account(&mut self, acct: &ChannelAccount) -> Result<(), String> {
        let cfg = acct.slack_config().map_err(|e| e.to_string())?;
        if cfg.bot_token.trim().is_empty() || cfg.signing_secret.trim().is_empty() {
            return Err("bot_token and signing_secret are required".into());
        }
        // Webhook-driven: no per-account task, just register the ctx in the
        // shared map the /webhook/slack/{id} handler reads at request time.
        let ctx = crate::slack::SlackAccountCtx {
            account_id:     acct.id.clone(),
            owner_user_id:  acct.user_id.clone(),
            bot_token:      cfg.bot_token,
            signing_secret: cfg.signing_secret,
            mention_only:   cfg.mention_only,
            routing_mode:   acct.routing_mode,
        };
        self.slack.insert(acct.id.clone(), ctx);
        info!(
            "Slack account '{}' registered (user={} id={})",
            acct.account_label, acct.user_id, acct.id,
        );
        Ok(())
    }

    fn register_external_account(&mut self, acct: &ChannelAccount) -> Result<(), String> {
        if acct.external_config().map(|c| c.send_url.starts_with("http://")).unwrap_or(false) {
            warn!(
                "External account '{}' send_url is plain http — outbound CPP \
                 calls are unencrypted. Use https in production.",
                acct.account_label,
            );
        }
        let ctx = crate::external::ExternalAccountCtx::from_account(acct)?;
        self.external.insert(acct.id.clone(), ctx);
        info!(
            "External account '{}' registered (user={} id={} kind={})",
            acct.account_label, acct.user_id, acct.id,
            acct.external_config().map(|c| c.provider_kind).unwrap_or_default(),
        );
        Ok(())
    }
}

impl Default for ChannelManager {
    fn default() -> Self { Self::new() }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

// Derive a per-account signal-cli data directory by suffixing the account
// id. Reusing the shared default would corrupt the keystore once two
// daemons race.
fn per_account_data_dir(cfg: &SignalAccountConfig, account_id: &str) -> String {
    let base = crate::config::expand_path(&cfg.data_dir);
    base.join(account_id).to_string_lossy().into_owned()
}

// One-shot migration: if the per-account `<dest>/data/` directory has
// no registered accounts but the legacy `<base>/data/` does have the
// caller's `phone_number`, copy `accounts.json` plus the matching
// account's path/path.d folders across.
// // Returns:
// `Ok(true)`  — migration ran (something was copied).
// `Ok(false)` — no migration needed (per-account already populated,
// or base had nothing to offer).
// `Err(_)`    — IO/parse failure mid-copy. Caller logs but doesn't
// abort: the daemon's own error message is the
// better diagnostic when the source data is bad.
fn migrate_signal_data_if_needed(
    base:         &std::path::Path,
    dest:         &std::path::Path,
    phone_number: &str,
) -> std::io::Result<bool> {
    use std::fs;

    let dest_data = dest.join("data");
    let dest_accounts = dest_data.join("accounts.json");
    let base_data = base.join("data");
    let base_accounts = base_data.join("accounts.json");

    // If the per-account dir already lists this number, nothing to do.
    if dest_accounts.exists() && account_listed(&dest_accounts, phone_number) {
        return Ok(false);
    }
    // No source to migrate from?
    if !base_accounts.exists() {
        return Ok(false);
    }
    // Avoid migrating from the per-account dir to itself when, for
    // some reason, base resolves to the same place. Defensive — the
    // caller normally splits them, but symlinks could fool us.
    if base_data == dest_data {
        return Ok(false);
    }

    // Find the entry in base/accounts.json that matches our number.
    let raw = fs::read_to_string(&base_accounts)?;
    let parsed: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let entry = parsed.get("accounts")
        .and_then(|a| a.as_array())
        .and_then(|arr| arr.iter().find(|a| {
            a.get("number").and_then(|n| n.as_str()) == Some(phone_number)
        }));
    let Some(entry) = entry else { return Ok(false) };

    // Pull the `path` token (e.g. "380407"). signal-cli stores the
    // account's keystore as `<base>/data/<path>` (a file) plus
    // `<base>/data/<path>.d/` (a directory).
    let Some(path_token) = entry.get("path").and_then(|p| p.as_str()) else {
        return Ok(false);
    };

    fs::create_dir_all(&dest_data)?;

    // Copy accounts.json (only the entry for our number, so a
    // multi-number base install doesn't leak siblings into the
    // per-account dir).
    let trimmed_accounts = serde_json::json!({
        "accounts": [entry],
        "version":  parsed.get("version").cloned().unwrap_or(serde_json::json!(2)),
    });
    fs::write(
        &dest_accounts,
        serde_json::to_vec_pretty(&trimmed_accounts)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
    )?;

    // Copy <path> + <path>.d.
    let path_file = base_data.join(path_token);
    if path_file.exists() {
        fs::copy(&path_file, dest_data.join(path_token))?;
    }
    let path_dir = base_data.join(format!("{path_token}.d"));
    if path_dir.exists() {
        copy_dir_recursive(&path_dir, &dest_data.join(format!("{path_token}.d")))?;
    }

    info!(
        "Signal: migrated registration for {phone_number} from {} → {} ({path_token})",
        base.display(), dest.display(),
    );
    Ok(true)
}

// Read accounts.json and return true if any entry's `number` matches.
// Returns false on any parse error — the caller treats "can't tell"
// the same as "not listed", which falls through to running migration
// (safe: migration is idempotent and only copies on a clean source).
fn account_listed(accounts_json: &std::path::Path, phone_number: &str) -> bool {
    let Ok(raw) = std::fs::read_to_string(accounts_json) else { return false };
    let Ok(v): Result<serde_json::Value, _> = serde_json::from_str(&raw) else { return false };
    let arr = v.get("accounts").and_then(|a| a.as_array());
    arr.map_or(false, |arr| arr.iter().any(|a| {
        a.get("number").and_then(|n| n.as_str()) == Some(phone_number)
    }))
}

fn copy_dir_recursive(src: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    use std::fs;
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let typ   = entry.file_type()?;
        let from  = entry.path();
        let to    = dest.join(entry.file_name());
        if typ.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if typ.is_file() {
            fs::copy(&from, &to)?;
        }
        // symlinks intentionally skipped — signal-cli's keystore doesn't
        // use them, and following them blindly into a compromised base
        // dir is the kind of thing you regret in security review.
    }
    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_allocator_returns_some_in_normal_env() {
        // In CI/dev the base port is almost never in use. We don't care
        // *which* port comes back — only that the scan terminates with a
        // reasonable answer.
        let p = allocate_free_port(BASE_SIGNAL_PORT, &[]);
        assert!(p.is_some());
        let p = p.unwrap();
        assert!(p >= BASE_SIGNAL_PORT);
    }

    #[test]
    fn port_allocator_skips_in_use_port() {
        // Bind a real socket on an OS-chosen free port, then ask the
        // allocator to start scanning from it. It must return a *different*
        // port since ours is held.
        let held = StdTcpListener::bind(("127.0.0.1", 0)).unwrap();
        let busy = held.local_addr().unwrap().port();
        let found = allocate_free_port(busy, &[]).unwrap();
        assert_ne!(found, busy);
        drop(held);
    }

    #[test]
    fn port_allocator_honours_skip_list() {
        let held = StdTcpListener::bind(("127.0.0.1", 0)).unwrap();
        let busy = held.local_addr().unwrap().port();
        drop(held); // free it — skip list must still keep us off it
        let found = allocate_free_port(busy, &[busy]).unwrap();
        assert_ne!(found, busy);
    }

    // ── migrate_signal_data_if_needed ──

    // The bug we shipped in production: per-account dir is empty,
    // base dir holds the registered number, daemon won't start.
    // Migration must copy and the second call must report no-op.
    #[test]
    fn migrate_copies_registered_number_to_per_account_dir() {
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        let dest = tmp.path().join("acct-uuid");
        let base_data = base.join("data");
        fs::create_dir_all(&base_data).unwrap();

        // Seed the legacy base install with one registered number.
        fs::write(base_data.join("accounts.json"), r#"{
            "accounts": [{"path":"380407","number":"+61450644506","environment":"LIVE","uuid":"abc"}],
            "version": 2
        }"#).unwrap();
        fs::write(base_data.join("380407"), b"keystore-bytes").unwrap();
        fs::create_dir_all(base_data.join("380407.d")).unwrap();
        fs::write(base_data.join("380407.d/identity.bin"), b"identity").unwrap();

        // First migrate: copies, returns true.
        assert!(migrate_signal_data_if_needed(&base, &dest, "+61450644506").unwrap());
        let migrated = fs::read_to_string(dest.join("data/accounts.json")).unwrap();
        assert!(migrated.contains("+61450644506"));
        assert!(dest.join("data/380407").exists());
        assert!(dest.join("data/380407.d/identity.bin").exists());

        // Second call is a no-op (idempotent).
        assert!(!migrate_signal_data_if_needed(&base, &dest, "+61450644506").unwrap());
    }

    // Migration with a multi-number base install must NOT leak the
    // other numbers' entries into the per-account dir.
    #[test]
    fn migrate_only_copies_matching_number() {
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        let dest = tmp.path().join("dest");
        let base_data = base.join("data");
        fs::create_dir_all(&base_data).unwrap();
        fs::write(base_data.join("accounts.json"), r#"{
            "accounts": [
                {"path":"a1","number":"+1","environment":"LIVE","uuid":"u1"},
                {"path":"a2","number":"+2","environment":"LIVE","uuid":"u2"}
            ],
            "version": 2
        }"#).unwrap();
        fs::write(base_data.join("a1"), b"x").unwrap();
        fs::write(base_data.join("a2"), b"y").unwrap();

        migrate_signal_data_if_needed(&base, &dest, "+1").unwrap();
        let dest_json = fs::read_to_string(dest.join("data/accounts.json")).unwrap();
        assert!(dest_json.contains("+1"));
        assert!(!dest_json.contains("+2"), "must not leak sibling numbers");
        assert!(dest.join("data/a1").exists());
        assert!(!dest.join("data/a2").exists(), "must not copy sibling keystores");
    }

    // Already-populated per-account dir must NOT be overwritten — that
    // would clobber a previously-registered standalone account.
    #[test]
    fn migrate_skips_when_per_account_already_listed() {
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        let dest = tmp.path().join("dest");
        fs::create_dir_all(base.join("data")).unwrap();
        fs::create_dir_all(dest.join("data")).unwrap();

        // Both dirs claim the same number; per-account wins.
        fs::write(base.join("data/accounts.json"), r#"{
            "accounts": [{"path":"base","number":"+1","environment":"LIVE","uuid":"u1"}],
            "version": 2
        }"#).unwrap();
        fs::write(base.join("data/base"), b"BASE").unwrap();
        fs::write(dest.join("data/accounts.json"), r#"{
            "accounts": [{"path":"local","number":"+1","environment":"LIVE","uuid":"u2"}],
            "version": 2
        }"#).unwrap();
        fs::write(dest.join("data/local"), b"LOCAL").unwrap();

        assert!(!migrate_signal_data_if_needed(&base, &dest, "+1").unwrap());
        // Per-account keystore untouched.
        assert_eq!(fs::read(dest.join("data/local")).unwrap(), b"LOCAL");
        assert!(!dest.join("data/base").exists());
    }

    // Number not present in base → no-op, no error.
    #[test]
    fn migrate_no_op_when_number_not_in_base() {
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        let dest = tmp.path().join("dest");
        fs::create_dir_all(base.join("data")).unwrap();
        fs::write(base.join("data/accounts.json"), r#"{"accounts":[],"version":2}"#).unwrap();
        assert!(!migrate_signal_data_if_needed(&base, &dest, "+1").unwrap());
        assert!(!dest.exists() || !dest.join("data/accounts.json").exists());
    }
}
