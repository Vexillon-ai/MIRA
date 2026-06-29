// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/router.rs
//! Build the axum [`Router`] with the full middleware stack.

use std::sync::Arc;

use tracing::{info, warn};
use axum::routing::{delete, get, patch, post, put};
use axum::{Extension, Router};

use crate::agent::AgentCore;
use crate::artifacts::ArtifactStore;
use crate::auth::LocalAuthService;
use crate::automations::{AutomationsStore, Worker as AutomationsWorker};
use crate::calendar::CalendarStore;
use crate::channel_accounts::ChannelAccountStore;
use crate::config::MiraConfig;
use crate::events::EventBus;
use crate::history::HistoryStore;
use crate::notifications::NotificationBus;
use crate::onboarding::ProfilePreambleCache;
use crate::gateway::channel_manager::TelegramAccountCtx;
use crate::security::{
    AuthLayer, HmacLayer, RateLimitLayer, RequestLogLayer, SecurityConfig,
    build_cors_layer,
};
use crate::tools::audit::ToolAuditStore;
use crate::stt::SttService;
use crate::tts::TtsService;
use crate::voice::ChannelRegistry;
use crate::server::handlers::{
    admin::{restart_handler, consolidator_run_now},
    admin_audit::list_tool_audit,
    admin_history::{admin_conversations_stats, list_grouped_conversations},
    artifacts::get_artifact,
    auth::{login_handler, logout_handler, me_handler, refresh_handler,
        oidc_authorize_handler, oidc_callback_handler, oidc_providers_handler},
    automations::{
        approve_schedule, create_schedule, delete_schedule, get_schedule, list_automations,
        list_runs, list_schedules, next_fires, pause_schedule, reject_schedule,
        resume_schedule, run_now, snooze_schedule, update_schedule,
    },
    calendar::{
        create_event as calendar_create_event,
        caldav_connect as calendar_caldav_connect,
        caldav_disconnect as calendar_caldav_disconnect,
        delete_event as calendar_delete_event,
        get_event as calendar_get_event,
        list_events as calendar_list_events,
        oauth_callback as calendar_oauth_callback,
        oauth_disconnect as calendar_oauth_disconnect,
        oauth_start as calendar_oauth_start,
        oauth_status as calendar_oauth_status,
        trigger_sync as calendar_trigger_sync,
        update_event as calendar_update_event,
    },
    channel_accounts::{
        account_health, create_account, delete_account, get_account, list_accounts, update_account,
    },
    channels::list_channels,
    chat::chat_handler,
    config_api::{
        delete_agent_avatar, get_agent_appearance, get_config, put_config,
        set_agent_avatar, upload_agent_avatar, validate_config,
    },
    conversations::{
        conversations_stats, create_conversation, delete_conversation, delete_message,
        get_conversation, get_messages, list_conversations, update_conversation,
    },
    groups::{
        add_member as add_group_member, create_group, delete_group,
        get_group, get_group_capabilities, get_my_capabilities,
        get_user_capabilities, list_groups, list_members as list_group_members,
        list_my_groups, remove_member as remove_group_member,
        set_group_capabilities, set_user_capabilities, update_group,
    },
    health_handler, signal_handler, telegram_handler, SignalState, TelegramState,
    logs::{get_log_level, logs_stream, set_log_level},
    memory::{create_memory, delete_memory, get_memory, list_memory, search_memory, supersede_memory},
    notifications::notifications_stream,
    onboarding::{
        finalize as onboarding_finalize, get_state as onboarding_state, post_complete_chat,
        reset_onboarding, restart_group, start_onboarding, DataDir,
    },
    providers::{openrouter_models, provider_catalog, providers_health, providers_models},
    sessions::{evict_session, list_sessions},
    status::status_handler,
    stt::{status as stt_status, transcribe as stt_transcribe},
    tools::{list_tools, run_tool},
    tts::{download_voice as tts_download_voice, speak as tts_speak, speak_stream as tts_speak_stream, status as tts_status, voices as tts_voices},
    users::{
        change_password, create_user, delete_avatar, delete_user, get_user, list_users,
        reset_password, update_user_full, upload_avatar, AvatarDir,
    },
    triggers::{
        approve_sub, create_sub, delete_sub, get_sub, list_event_names, list_subs,
        pause_sub, reject_sub, resume_sub, test_emit, update_sub,
    },
    webhooks::{
        approve_webhook, create_webhook, delete_webhook, get_webhook, ingest_webhook,
        list_payloads, list_webhooks, pause_webhook, reject_webhook, resume_webhook,
        rotate_secret, rotate_token, test_replay, update_webhook, webhook_url,
    },
};
use tower_http::services::ServeDir;
use crate::web::{static_files::spa_router, LiveConfig};

// ── Build router ──────────────────────────────────────────────────────────────

pub fn build_router(
    agent_core:        Arc<AgentCore>,
    security:          &SecurityConfig,
    config:            &MiraConfig,
    auth_service:      Option<Arc<LocalAuthService>>,
    history:           Option<Arc<HistoryStore>>,
    live_config:       Option<Arc<LiveConfig>>,
    notification_bus:  Arc<NotificationBus>,
    telegram_accounts: Arc<std::collections::HashMap<String, TelegramAccountCtx>>,
    whatsapp_accounts: Arc<std::collections::HashMap<String, crate::whatsapp::WhatsAppAccountCtx>>,
    slack_accounts:    Arc<std::collections::HashMap<String, crate::slack::SlackAccountCtx>>,
    external_accounts: Arc<std::collections::HashMap<String, crate::external::ExternalAccountCtx>>,
    channel_accounts:  Option<Arc<ChannelAccountStore>>,
    // R1+R2 — channel-identity link table + one-time link-code store.
    // Both live in auth.db. Optional so tests + minimal builds compile;
    // the channel-links handlers return 500 if the extensions are absent.
    identity_store:    Option<Arc<crate::channel_identity::IdentityStore>>,
    link_code_store:   Option<Arc<crate::channel_identity::LinkCodeStore>>,
    // Live ChannelManager wrapped in RwLock so per-account lifecycle
    // endpoints (`/api/channel-accounts/{id}/{start,stop,restart}`)
    // can take a brief write lock to spawn / kill daemons. Optional —
    // tests + minimal deployments can omit; the lifecycle endpoints
    // return 503 when absent.
    channel_manager:   Option<Arc<tokio::sync::RwLock<crate::gateway::channel_manager::ChannelManager>>>,
    tool_audit:        Option<Arc<ToolAuditStore>>,
    calendar_store:    Option<Arc<CalendarStore>>,
    automations_store: Option<Arc<AutomationsStore>>,
    automations_worker: Option<Arc<AutomationsWorker>>,
    event_bus:         Arc<EventBus>,
    restart_notify:    Arc<tokio::sync::Notify>,
    agent_registry:    Arc<crate::agent::AgentRegistry>,
    supervisor:        Arc<crate::agent::Supervisor>,
    // D3 — admin-defined policy rules. Optional so tests + minimal
    // deployments can omit; production wiring opens it in the gateway.
    admin_policy_rules: Option<Arc<crate::policy::AdminRulesStore>>,
    // encrypted skill secrets. Optional; the open-failure
    // path is logged at boot. Handlers under /api/admin/skills/.../secrets
    // return 503 when None.
    secrets_store: Option<Arc<crate::skills::SecretsStore>>,
    // 0.107.0 — system-health dashboard backing store. Optional so
    // tests + minimal builds can omit; the dashboard handlers 503 when
    // absent (currently just panic via Extension lookup, but the
    // routes are admin-gated so non-admins won't ever hit them).
    health_store: Option<Arc<crate::health::store::HealthStore>>,
    // 0.111.0 — task artifacts store (per-task output dirs).
    task_artifacts: Option<Arc<crate::task_artifacts::TaskArtifactsStore>>,
    // 0.142.0 (Q1.2) — Web Push service. `None` keeps the new
    // /api/notifications/push/* endpoints 503; the SSE stream stays
    // unaffected.
    web_push: Option<Arc<crate::notifications::web_push::WebPushService>>,
    // 0.150.0 (Q1.7) — landing-page waitlist store. `None` keeps the
    // /api/waitlist/* endpoints 503.
    waitlist: Option<Arc<crate::waitlist::WaitlistStore>>,
    // Q2 #7 MCP host registry. Required to back
    // GET /api/mcp/status; the agent already sees the registered
    // adapters via the shared ToolRegistry.
    mcp_servers: Arc<crate::mcp::McpServerRegistry>,
    // per-user MCP store. CRUD endpoints write here;
    // changes take effect on the next gateway restart.
    mcp_store: Option<Arc<crate::mcp::McpServerStore>>,
    // Admin-managed catalog of recommended MCP servers (Q2 #7 follow-up).
    mcp_catalog: Option<Arc<crate::mcp::McpCatalogStore>>,
    // E1+E3 chunk 1 — per-user email account store. CRUD only in
    // chunk 1; the IMAP poller arrives in chunk 2 and starts
    // consuming the rows.
    email_store: Option<Arc<crate::email::EmailAccountStore>>,
    // E1+E3 chunk 2 — per-account IMAP poller registry. Required to
    // back GET /api/email/status; an empty registry when no
    // accounts are configured.
    email_pollers: Arc<crate::email::EmailPollerRegistry>,
    // E1+E3 chunk 5 — quarantine queue + audit log stores. Required
    // by the /quarantine and /audit endpoints + the approve/reject
    // re-dispatch path.
    email_quarantine: Option<Arc<crate::email::EmailQuarantineStore>>,
    email_audit:      Option<Arc<crate::email::EmailAuditStore>>,
    // E5 — system email mailer.
    system_mailer:    Option<Arc<crate::email::SystemMailer>>,
    // K3 (Q2 #10) — Chatterbox server supervisor, present only when
    // tts.chatterbox.supervise is on (and same-host management is viable).
    chatterbox_supervisor: Option<Arc<crate::tts::chatterbox::ChatterboxSupervisor>>,
    // Live subsystem-fallback tracker — powers GET /api/health/degradations.
    degradations: Option<Arc<crate::health::degradation::DegradationTracker>>,
    // MIRA-Guardian pending action proposals — powers /api/guardian/actions (P4).
    guardian_actions: Option<Arc<crate::agent::guardian_actions::GuardianActionStore>>,
    // HMAC-chained agent audit log — the guardian approve/decline handlers
    // record tamper-evident decision events (P4).
    audit_store:      Option<Arc<crate::agent::AuditStore>>,
) -> Router {
    // Held aside so the AuthLayer (built after `auth_service` is moved
    // into the live-config wiring below) can still see it for JWT
    // validation. Each `Option<Arc<_>>` clone is one refcount bump.
    let auth_service_for_layer = auth_service.clone();

    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            config.server.request_timeout_secs as u64,
        ))
        .build()
        .expect("Failed to build HTTP client");

    // TTS façade. Cheap to construct: backends defer their first
    // Piper download / subprocess spawn until first /api/tts/speak. Built
    // here (rather than further down) so channel-state structs can clone it.
    // Attach the subsystem-fallback tracker so the HTTP /api/tts + /api/stt
    // paths (this router's services) also record + notify on a degraded
    // fallback — the same tracker the channel-side services use.
    let tts_service: TtsService = match &degradations {
        Some(dt) => TtsService::from_config(config).with_degradations(Arc::clone(dt)),
        None     => TtsService::from_config(config),
    };

    // STT façade. Same lazy-load posture: the whisper.cpp model
    // file is fetched on first /api/stt/transcribe, not at startup.
    let stt_service: SttService = match &degradations {
        Some(dt) => SttService::from_config(config).with_degradations(Arc::clone(dt)),
        None     => SttService::from_config(config),
    };

    // 0.106.0 — IpBanLayer sits between RateLimit and Auth. Cheap
    // in-memory cache (refreshed every 30s from auth.db.auth_ip_bans);
    // skipped entirely when the auth service isn't wired (tests /
    // minimal deploys). Computed up-front because `auth_service` is
    // consumed later when wiring the live-config watcher.
    // 0.107.0 — also clone an Arc<AuthDb> for the dashboard handlers
    // (failed-login + ip-ban list/lift endpoints).
    let auth_db_arc: Option<Arc<crate::auth::AuthDb>> =
        auth_service.as_ref().map(|svc| svc.db_arc());
    let ip_ban_layer = auth_db_arc.as_ref().map(|db| {
        crate::security::IpBanLayer::new(
            crate::security::IpBanCache::new(Arc::clone(db)),
        )
    });

    let tg_state = TelegramState {
        agent_core:  Arc::clone(&agent_core),
        http_client: http_client.clone(),
        history:     history.as_ref().map(Arc::clone),
        accounts:    telegram_accounts,
        tts:         Some(tts_service.clone()),
        stt:         Some(stt_service.clone()),
        auth:        auth_service.as_ref().map(Arc::clone),
        live_config: live_config.as_ref().map(Arc::clone),
        mcp_servers: Some(Arc::clone(&mcp_servers)),
        identity:    identity_store.as_ref().map(Arc::clone),
        link_codes:  link_code_store.as_ref().map(Arc::clone),
    };

    let wa_state = crate::whatsapp::WhatsAppState {
        accounts: whatsapp_accounts,
        deps: crate::whatsapp::WhatsAppDispatcherDeps {
            agent_core:  Arc::clone(&agent_core),
            history:     history.as_ref().map(Arc::clone),
            auth:        auth_service.as_ref().map(Arc::clone),
            live_config: live_config.as_ref().map(Arc::clone),
            mcp_servers: Some(Arc::clone(&mcp_servers)),
            http_client: http_client.clone(),
            identity:    identity_store.as_ref().map(Arc::clone),
            link_codes:  link_code_store.as_ref().map(Arc::clone),
        },
    };

    let sl_state = crate::slack::SlackState {
        accounts: slack_accounts,
        deps: crate::slack::SlackDispatcherDeps {
            agent_core:  Arc::clone(&agent_core),
            history:     history.as_ref().map(Arc::clone),
            auth:        auth_service.as_ref().map(Arc::clone),
            live_config: live_config.as_ref().map(Arc::clone),
            mcp_servers: Some(Arc::clone(&mcp_servers)),
            http_client: http_client.clone(),
            identity:    identity_store.as_ref().map(Arc::clone),
            link_codes:  link_code_store.as_ref().map(Arc::clone),
        },
    };

    let ext_state = crate::external::ExternalState {
        accounts: Arc::clone(&external_accounts),
        deps: crate::external::ExternalDispatcherDeps {
            agent_core:  Arc::clone(&agent_core),
            history:     history.as_ref().map(Arc::clone),
            auth:        auth_service.as_ref().map(Arc::clone),
            live_config: live_config.as_ref().map(Arc::clone),
            mcp_servers: Some(Arc::clone(&mcp_servers)),
            http_client: http_client.clone(),
            identity:    identity_store.as_ref().map(Arc::clone),
            link_codes:  link_code_store.as_ref().map(Arc::clone),
            channel_store: channel_accounts.as_ref().map(Arc::clone),
            tts:         Some(tts_service.clone()),
            stt:         Some(stt_service.clone()),
        },
    };

    let signal_port   = config.channels.signal.rest_port;
    let signal_number = config.channels.signal.phone_number.clone()
        .filter(|s| !s.is_empty());
    let sig_state = SignalState {
        agent_core:    Arc::clone(&agent_core),
        signal_port,
        signal_number,
        history:       history.as_ref().map(Arc::clone),
        auth:          auth_service.as_ref().map(Arc::clone),
        mcp_servers:   Some(Arc::clone(&mcp_servers)),
    };

    // /webhook/signal is for deployments that forward Signal events
    // over HTTP POST (e.g. a Signal -> webhook bridge sitting somewhere
    // else on the network). The default MIRA topology talks to a local
    // signal-cli daemon via JSON-RPC, so the route is typically unused.
    // Mount it only when an HMAC key is set — running an unsigned
    // inbound webhook would let anyone who can reach the port inject
    // fake Signal messages.
    let signal_hmac_layer: Option<HmacLayer> = security.signal_hmac_key
        .as_deref()
        .map(|k| HmacLayer::new(Some(k)));
    if signal_hmac_layer.is_some() {
        info!("Signal: /webhook/signal mounted (HMAC-signed)");
    } else {
        info!(
            "Signal: /webhook/signal route disabled — set channels.signal.hmac_key \
             to enable HTTP webhook ingest (signal-cli daemon flow is unaffected)"
        );
    }

    // Avatar store — filesystem root where user uploads live, and a static
    // mount so `<img src="/avatars/...">` resolves without bearer auth.
    let avatar_dir = Arc::new(config.data_dir_path().join("avatars"));
    let _ = std::fs::create_dir_all(avatar_dir.as_path());
    let avatar_store = AvatarDir(Arc::clone(&avatar_dir));

    // Artifact store — content-addressed binaries dropped by tools (e.g.
    // images written to /tmp/output/ by code_run). Public-readable like
    // avatars; the SHA-256 in the URL is the capability.
    let artifact_store: Option<Arc<ArtifactStore>> =
        match ArtifactStore::new(config.data_dir_path()) {
            Ok(s)  => Some(Arc::new(s)),
            Err(e) => {
                tracing::warn!(
                    "/api/artifacts disabled — cannot init store at {:?}/artifacts: {e}",
                    config.data_dir_path(),
                );
                None
            }
        };

    // Root data dir — used by onboarding/reset to locate per-user profile.md.
    let data_dir = DataDir(Arc::new(config.data_dir_path()));

    // Named-agent definitions (Phase B) — reusable agent profiles managed via
    // /api/agents/definitions. Optional: a failure to open just disables the
    // feature surface rather than blocking the server.
    let agent_defs: Option<Arc<crate::agent::AgentDefinitionStore>> =
        match crate::agent::AgentDefinitionStore::open(
            &config.data_dir_path().join("agent_definitions.db"),
        ) {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                tracing::warn!("named-agent store unavailable: {e}");
                None
            }
        };

    // Workflows (Phase C) — saved orchestrations + run history, managed via
    // /api/workflows. Same optional-open pattern as the named-agent store.
    let workflow_store: Option<Arc<crate::agent::WorkflowStore>> =
        match crate::agent::WorkflowStore::open(
            &config.data_dir_path().join("workflows.db"),
        ) {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                tracing::warn!("workflow store unavailable: {e}");
                None
            }
        };
    // Orchestrator for the UI "Run" endpoint. Built here from the shared
    // supervisor + registry + event bus (same instances the gateway uses), so
    // runs started via the API are identical to those started by the
    // `run_workflow` tool. Writes to the same workflows.db via its own handle.
    let workflow_orchestrator: Option<Arc<crate::agent::Orchestrator>> =
        workflow_store.as_ref().map(|store| {
            Arc::new(
                crate::agent::Orchestrator::new(
                    Arc::clone(&supervisor),
                    Arc::clone(&agent_registry),
                    Arc::clone(store),
                    config.agent.default_task_budget_usd,
                    config.agent.max_task_budget_usd,
                ).with_event_bus(Arc::clone(&event_bus))
            )
        });

    // Skill per-user preferences (slice A5). Shared between the API handlers
    // (read for /api/skills, write for PUT preferences) and the SkillTools
    // already registered in the agent's tool registry — both reach the same
    // SQLite file via WAL for consistent reads across connections.
    let skill_prefs: Option<Arc<crate::skills::SkillPrefsStore>> =
        match crate::skills::SkillPrefsStore::open(
            &config.data_dir_path().join("skill_prefs.db"),
        ) {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                warn!("Skill prefs unavailable (per-user enable/disable disabled at API layer): {e}");
                None
            }
        };

    // Cache of per-user profile preambles layered onto the chat system prompt.
    let preamble_cache: Arc<ProfilePreambleCache> = Arc::new(ProfilePreambleCache::new());

    // Channel registry — built-ins plus a descriptor per configured CPP
    // (External) provider, so the per-channel voice settings UI offers each
    // `external:<kind>` channel. `supports_voice` comes from the provider's
    // own config flag (the provider decides whether it can play audio).
    let channel_registry: Arc<ChannelRegistry> = Arc::new(ChannelRegistry::builtin());
    {
        use std::collections::HashSet;
        let mut seen: HashSet<String> = HashSet::new();
        for ctx in external_accounts.values() {
            let id = ctx.channel_str(); // "external:<provider_kind>"
            if seen.insert(id.clone()) {
                channel_registry.register(crate::voice::ChannelDescriptor {
                    id,
                    display_name: format!("External — {}", ctx.provider_kind),
                    supports_voice: ctx.supports_voice,
                });
            }
        }
    }

    let mut router = Router::new()
        .route("/health", get(health_handler).with_state(Arc::clone(&agent_core)))
        .nest_service("/avatars", ServeDir::new(avatar_dir.as_path()))
        .route(
            "/webhook/telegram/{account_id}",
            post(telegram_handler).with_state(tg_state),
        )
        .route(
            "/webhook/whatsapp/{account_id}",
            get(crate::whatsapp::whatsapp_verify)
                .post(crate::whatsapp::whatsapp_inbound)
                .with_state(wa_state),
        )
        .route(
            "/webhook/slack/{account_id}",
            post(crate::slack::slack_inbound).with_state(sl_state),
        )
        .route(
            "/webhook/external/{account_id}",
            post(crate::external::external_inbound).with_state(ext_state),
        );

    if let Some(layer) = signal_hmac_layer {
        router = router.route(
            "/webhook/signal",
            post(signal_handler)
                .layer(layer)
                .with_state(sig_state),
        );
    } else {
        // Keep `sig_state` alive until end-of-scope so the type checker
        // doesn't complain about an unused variable — it has the same
        // shape no matter what, and the cost is one Arc refcount.
        drop(sig_state);
    }

    // Public webhook ingest (/). Lives outside `api_routes`
    // so the AuthLayer doesn't intercept signed third-party POSTs; the
    // token + per-webhook HMAC secret IS the authentication. Wired only
    // when both the automations store and worker are present, since the
    // handler dispatches actions through the worker's dispatcher.
    if let (Some(s), Some(w)) = (automations_store.as_ref(), automations_worker.as_ref()) {
        let ingest = Router::new()
            .route("/webhook/incoming/{token}", post(ingest_webhook))
            .layer(Extension(Arc::clone(s)))
            .layer(Extension(Arc::clone(w)));
        router = router.merge(ingest);
    }

    // /api/artifacts/{id} — public (capability URL), wired only when the
    // store initialised cleanly. Lives outside `api_routes` so AuthLayer's
    // bearer-token requirement doesn't reject browser <img> requests.
    if let Some(ref store) = artifact_store {
        router = router.route(
            "/api/artifacts/{id}",
            get(get_artifact).with_state(Arc::clone(store)),
        );
    }

    // Q1.7 — public waitlist signup. Outside the auth-required block
    // so the landing page can POST without a Bearer token. The
    // admin-only read/export/delete endpoints sit inside the
    // authenticated routes below.
    {
        let waitlist_public = Router::new()
            .route(
                "/api/waitlist/signup",
                post(crate::server::handlers::waitlist::signup),
            )
            .layer(Extension(waitlist.clone()));
        router = router.merge(waitlist_public);
    }

    if let (Some(auth), Some(hist), Some(live_cfg)) =
        (auth_service, history, live_config)
    {
        // Watch live-config for TTS-relevant changes and rebuild the backend
        // map in-place. Without this, the user can update an
        // openai_compat URL / API key from the Settings UI and the running
        // service still uses the old map (which may have no openai_compat
        // backend at all → fallback to piper, sounding like "the internal
        // voice keeps winning").
        {
            let tts = tts_service.clone();
            let stt = stt_service.clone();
            let mut rx = live_cfg.subscribe();
            tokio::spawn(async move {
                while rx.changed().await.is_ok() {
                    let cfg = rx.borrow().clone();
                    tts.reload(&cfg);
                    stt.reload(&cfg);
                }
            });
        }

        // Public auth routes. OIDC SSO (Q2 #11) is built from the initial
        // config (a config change to providers needs a restart) and layered
        // alongside the auth service — the callback handler needs both.
        let oidc_service = Arc::new(
            crate::auth::oidc::OidcService::new(&config.auth.oidc, config.server.port),
        );
        // LDAP/AD auth (Q2 #11) — built from initial config (provider change
        // needs a restart); inert unless enabled. Needed by login_handler.
        let ldap_service = Arc::new(crate::auth::ldap::LdapService::new(&config.auth.ldap));
        let auth_routes = Router::new()
            .route("/api/auth/login",   post(login_handler))
            .route("/api/auth/logout",  post(logout_handler))
            .route("/api/auth/refresh", post(refresh_handler))
            .route("/api/auth/me",      get(me_handler))
            .route("/api/auth/oidc/providers",  get(oidc_providers_handler))
            .route("/api/auth/oidc/authorize",  get(oidc_authorize_handler))
            .route("/api/auth/oidc/callback",   get(oidc_callback_handler))
            // Q2 #11 — self-service onboarding (public).
            .route("/api/auth/signup",         post(crate::server::handlers::signup::signup))
            .route("/api/auth/signup/config",  get(crate::server::handlers::signup::signup_config))
            .route("/api/auth/invite",         get(crate::server::handlers::signup::invite_info))
            // 0.282.0 — QR device pairing (mobile onboarding). /start +
            // /status are authed (AuthUser extractor); /claim is public
            // (allow-listed in security::public_routes) — the phone has no
            // token yet and exchanges the single-use secret for one.
            .route("/api/auth/pairing/start",        post(crate::server::handlers::auth::pairing_start_handler))
            .route("/api/auth/pairing/claim",        post(crate::server::handlers::auth::pairing_claim_handler))
            .route("/api/auth/pairing/{id}/status",  get(crate::server::handlers::auth::pairing_status_handler))
            .layer(Extension(oidc_service))
            .layer(Extension(ldap_service))
            .layer(Extension(Arc::clone(&auth)));
        // The signup handlers need the live config (open-signup policy).
        let auth_routes = auth_routes.layer(Extension(Arc::clone(&live_cfg)));

        // Authenticated API routes.
        let api_routes = Router::new()
            // Chat
            .route("/api/chat", post(chat_handler))
            // Conversations
            .route("/api/conversations",              get(list_conversations).post(create_conversation))
            .route("/api/conversations/stats",        get(conversations_stats))
            .route("/api/conversations/{id}",         get(get_conversation).patch(update_conversation).delete(delete_conversation))
            .route("/api/conversations/{id}/messages", get(get_messages))
            .route("/api/messages/{id}",              delete(delete_message))
            // Config (admin only)
            .route("/api/config",          get(get_config).put(put_config))
            .route("/api/config/validate", post(validate_config))
            // Agent avatar: upload (multipart) + preset/clear + delete.
            // Read lives at /api/agent/appearance so non-admin users can
            // render the assistant avatar in chat without seeing the full
            // config.
            .route(
                "/api/config/agent-avatar",
                post(upload_agent_avatar).put(set_agent_avatar).delete(delete_agent_avatar),
            )
            .route("/api/agent/appearance", get(get_agent_appearance))
            // Users (admin only)
            .route("/api/users",               get(list_users).post(create_user))
            .route("/api/users/{id}",          get(get_user).put(update_user_full).delete(delete_user))
            .route("/api/users/{id}/password",        post(change_password))
            .route("/api/users/{id}/reset-password",  post(reset_password))
            .route("/api/users/{id}/avatar",   post(upload_avatar).delete(delete_avatar))
            // Q2 #11 — self-service onboarding admin surface.
            .route("/api/invites",
                   get(crate::server::handlers::signup::list_invites)
                   .post(crate::server::handlers::signup::create_invite))
            .route("/api/invites/{id}",
                   delete(crate::server::handlers::signup::revoke_invite))
            .route("/api/admin/users/pending",
                   get(crate::server::handlers::signup::list_pending))
            .route("/api/users/{id}/approve",
                   post(crate::server::handlers::signup::approve_user))
            .route("/api/users/{id}/revoke-sessions",
                   post(crate::server::handlers::signup::revoke_user_sessions))
            // Groups (admin only for writes; /api/me/groups for self-service reads)
            .route("/api/groups",              get(list_groups).post(create_group))
            .route("/api/groups/{id}",         get(get_group).put(update_group).delete(delete_group))
            .route("/api/groups/{id}/members", get(list_group_members).post(add_group_member))
            .route("/api/groups/{group_id}/members/{user_id}", delete(remove_group_member))
            .route("/api/me/groups",           get(list_my_groups))
            // Capability RBAC — per-group/per-user allowlists + budget caps
            // (admin writes); a user reads their own effective profile.
            .route("/api/groups/{id}/capabilities", get(get_group_capabilities).put(set_group_capabilities))
            .route("/api/users/{id}/capabilities",  get(get_user_capabilities).put(set_user_capabilities))
            .route("/api/me/capabilities",          get(get_my_capabilities))
            // Memory — reads/writes gated by visibility + scope rules.
            // Non-admins cannot delete or mutate directly; they POST /supersede
            // to append a newer memory that replaces an old one.
            .route("/api/memory",        get(list_memory).post(create_memory))
            .route("/api/memory/search", post(search_memory))
            .route("/api/memory/{id}",   get(get_memory).delete(delete_memory))
            .route("/api/memory/{id}/supersede", post(supersede_memory))
            // Wiki (Slice E) — per-user markdown knowledge base. Page CRUD
            // applies immediately under the user's identity; the review
            // queue is where extractor + agent-tool writes land for the
            // user to approve or reject.
            .route("/api/wiki/pages",
                   get(crate::server::handlers::wiki::list_pages))
            .route("/api/wiki/page",
                   get(crate::server::handlers::wiki::get_page)
                   .put(crate::server::handlers::wiki::put_page)
                   .delete(crate::server::handlers::wiki::delete_page))
            .route("/api/wiki/page/append-section",
                   post(crate::server::handlers::wiki::append_section))
            .route("/api/wiki/log",
                   post(crate::server::handlers::wiki::add_log_entry))
            .route("/api/wiki/nav",
                   get(crate::server::handlers::wiki::get_nav))
            .route("/api/wiki/ops/pending",
                   get(crate::server::handlers::wiki::list_pending_ops))
            .route("/api/wiki/ops",
                   get(crate::server::handlers::wiki::list_recent_ops))
            .route("/api/wiki/ops/{op_id}/approve",
                   post(crate::server::handlers::wiki::approve_op))
            .route("/api/wiki/ops/{op_id}/reject",
                   post(crate::server::handlers::wiki::reject_op))
            .route("/api/wiki/ops/approve-all",
                   post(crate::server::handlers::wiki::approve_all_ops))
            .route("/api/wiki/ops/reject-all",
                   post(crate::server::handlers::wiki::reject_all_ops))
            // Git + import/export (Slice G) — per-user wiki only.
            .route("/api/wiki/git/status",
                   get(crate::server::handlers::wiki::git_status))
            .route("/api/wiki/git/commit",
                   post(crate::server::handlers::wiki::git_commit))
            .route("/api/wiki/git/remote",
                   post(crate::server::handlers::wiki::git_set_remote))
            .route("/api/wiki/git/push",
                   post(crate::server::handlers::wiki::git_push))
            .route("/api/wiki/git/pull",
                   post(crate::server::handlers::wiki::git_pull))
            .route("/api/wiki/export",
                   get(crate::server::handlers::wiki::export_tarball))
            .route("/api/wiki/import",
                   post(crate::server::handlers::wiki::import_tarball))
            // Slice H — save a chat thread as a wiki page.
            .route("/api/wiki/save-thread",
                   post(crate::server::handlers::wiki::save_thread))
            // System wiki (Slice F) — admin-only. Edits to persona.md
            // hot-reload the runtime system prompt, no restart needed.
            .route("/api/admin/wiki/pages",
                   get(crate::server::handlers::wiki::admin_list_pages))
            .route("/api/admin/wiki/page",
                   get(crate::server::handlers::wiki::admin_get_page)
                   .put(crate::server::handlers::wiki::admin_put_page)
                   .delete(crate::server::handlers::wiki::admin_delete_page))
            .route("/api/admin/wiki/page/append-section",
                   post(crate::server::handlers::wiki::admin_append_section))
            .route("/api/admin/wiki/nav",
                   get(crate::server::handlers::wiki::admin_get_nav))
            .route("/api/admin/wiki/ops",
                   get(crate::server::handlers::wiki::admin_list_recent_ops))
            .route("/api/admin/wiki/reload-prompt",
                   post(crate::server::handlers::wiki::admin_reload_prompt))
            // Companion mode — group-based family bridge.
            // Admin endpoints configure which groups relay companion
            // signals + per-member flags; user endpoints let each
            // member control their own opt-in / channel pref / mute.
            .route("/api/admin/companion/groups",
                   get(crate::server::handlers::companion::admin_list_groups))
            .route("/api/admin/companion/groups/{group_id}",
                   get(crate::server::handlers::companion::admin_get_group)
                   .delete(crate::server::handlers::companion::admin_delete_group))
            .route("/api/admin/companion/groups/{group_id}/policy",
                   put(crate::server::handlers::companion::admin_put_policy))
            .route("/api/admin/companion/groups/{group_id}/members/{user_id}",
                   put(crate::server::handlers::companion::admin_put_member)
                   .delete(crate::server::handlers::companion::admin_delete_member))
            .route("/api/me/companion/groups",
                   get(crate::server::handlers::companion::list_my_memberships))
            .route("/api/me/companion/groups/{group_id}",
                   patch(crate::server::handlers::companion::update_my_membership))
            // Self-serve enable for the setup wizard: turn check-ins on for the
            // caller (optional safety contact + cadence cap + daily briefing) in
            // one shot — the HTTP equivalent of the chat `companion_enable` flow.
            .route("/api/me/companion/enable",
                   post(crate::server::handlers::companion::enable_companion))
            // Presence settings (rhythm + personality) for the caller — read +
            // partial-update. The Presence page binds to these; enabling stays
            // on /enable (safety-contact gate + persona seeding).
            .route("/api/me/companion",
                   get(crate::server::handlers::companion::get_my_companion)
                   .put(crate::server::handlers::companion::update_my_companion))
            // On-demand check-in trigger — fire a companion check-in to the
            // caller right now (bypassing scheduler policy) and return the
            // delivery outcome. Makes proactive delivery testable. (The
            // briefing equivalent already exists at /api/me/briefing/send-now.)
            .route("/api/companion/checkin/test",
                   post(crate::server::handlers::companion::test_checkin))
            // Sessions
            .route("/api/sessions",     get(list_sessions))
            .route("/api/sessions/{id}", delete(evict_session))
            // Tools
            .route("/api/tools",        get(list_tools))
            .route("/api/tools/run",    post(run_tool))
            // Providers
            .route("/api/providers/health", get(providers_health))
            .route("/api/providers/models", get(providers_models))
            .route("/api/providers/openrouter/models", get(openrouter_models))
            .route("/api/providers/{slug}/catalog", get(provider_catalog))
            // Status
            .route("/api/status", get(status_handler))
            // Host hardware probe (K2 / Q2 #10) — GPU + CUDA/Vulkan detection
            // and the local-TTS recommendation.
            .route("/api/system/hardware",
                   get(crate::server::handlers::system::hardware_info))
            // Chatterbox AMD Vulkan TTS server (K3 / Q2 #10).
            .route("/api/system/chatterbox/status",
                   get(crate::server::handlers::system::chatterbox_status))
            .route("/api/system/chatterbox/install",
                   post(crate::server::handlers::system::chatterbox_install))
            // MCP host (Q2 #7) — per-user server CRUD and
            // runtime status snapshot. Status is scoped to the caller's
            // rows. The configured server list moved from
            // config.mcp.servers to the mcp_servers DB table in;
            // these endpoints are the source of truth.
            .route("/api/mcp/status",
                   get(crate::server::handlers::mcp::status))
            .route("/api/mcp/servers",
                   get(crate::server::handlers::mcp::list_servers)
                       .post(crate::server::handlers::mcp::create_server))
            .route("/api/mcp/servers/{id}",
                   axum::routing::put(crate::server::handlers::mcp::update_server)
                       .delete(crate::server::handlers::mcp::delete_server))
            // Install a managed MCP runtime (Node/uv) after the user consents to
            // the dependency prompt, then reconnect servers.
            .route("/api/mcp/runtime/install",
                   post(crate::server::handlers::mcp::install_runtime))
            // Recommended-server catalog: any user reads the enabled list to
            // pre-fill the add form; admins manage the entries.
            .route("/api/mcp/catalog",
                   get(crate::server::handlers::mcp::catalog_list))
            .route("/api/admin/mcp/catalog",
                   get(crate::server::handlers::mcp::catalog_admin_list)
                       .post(crate::server::handlers::mcp::catalog_create))
            .route("/api/admin/mcp/catalog/{id}",
                   axum::routing::put(crate::server::handlers::mcp::catalog_update)
                       .delete(crate::server::handlers::mcp::catalog_delete))
            // Email channel (Q2 #8, slice E1+E3) — per-user account CRUD.
            // The IMAP poller (chunk 2) and security pipeline (chunk 3)
            // are still landing; until then, accounts can be created
            // here but nothing's reading them.
            .route("/api/email/accounts",
                   get(crate::server::handlers::email::list_accounts)
                       .post(crate::server::handlers::email::create_account))
            .route("/api/email/accounts/{id}",
                   axum::routing::put(crate::server::handlers::email::update_account)
                       .delete(crate::server::handlers::email::delete_account))
            .route("/api/email/status",
                   get(crate::server::handlers::email::status))
            .route("/api/email/quarantine",
                   get(crate::server::handlers::email::list_quarantine))
            .route("/api/email/quarantine/{id}/approve",
                   post(crate::server::handlers::email::approve_quarantine))
            .route("/api/email/quarantine/{id}/reject",
                   post(crate::server::handlers::email::reject_quarantine))
            .route("/api/email/audit",
                   get(crate::server::handlers::email::list_audit))
            // E4 — OAuth flow start. JWT-gated; per-account.
            .route("/api/email/accounts/{id}/oauth/{provider}/start",
                   post(crate::server::handlers::email::oauth_start))
            // E4 — provider callback. Listed in security's
            // public_routes whitelist so the provider's redirect
            // reaches it without a Bearer token; the state token
            // (CSRF) is what binds the call back to the user.
            .route("/api/email/oauth/callback",
                   get(crate::server::handlers::email::oauth_callback))
            // E5 — admin "send a test" against the global
            // system_email config. Useful both as a smoke check
            // after editing the config and as something the
            // Settings UI can wire to a button.
            .route("/api/admin/email/system/test",
                   post(crate::server::handlers::email::system_email_test))
            // E6 — webhook ingest from hosted-mail providers.
            // Public route (per-account secret in the path is what
            // authenticates); whitelisted in SecurityConfig.public_routes
            // under /webhook/email/*. Provider POSTs JSON or
            // form-data depending on which one configured.
            .route("/webhook/email/{id}/{secret}",
                   post(crate::server::handlers::email::webhook_inbound))
            // Agents (B7 — multi-agent runtime view + control)
            .route("/api/agents",
                   get(crate::server::handlers::agents::list_agents))
            // Fleet-wide live SSE (A3 mission-control). Before `/{id}` so
            // "stream" isn't parsed as an agent UUID.
            .route("/api/agents/stream",
                   get(crate::server::handlers::agents::agents_stream))
            // Named-agent definitions (Phase B). Before `/{id}` routes so
            // "definitions" isn't parsed as an agent UUID.
            .route("/api/agents/definitions",
                   get(crate::server::handlers::agent_defs::list_definitions)
                   .post(crate::server::handlers::agent_defs::create_definition))
            .route("/api/agents/definitions/{id}",
                   get(crate::server::handlers::agent_defs::get_definition)
                   .put(crate::server::handlers::agent_defs::update_definition)
                   .delete(crate::server::handlers::agent_defs::delete_definition))
            // Workflows (Phase C) — `runs` routes are registered before `{id}`
            // so the run-history paths can't be shadowed by the def lookup.
            .route("/api/workflows",
                   get(crate::server::handlers::workflows::list_workflows)
                   .post(crate::server::handlers::workflows::create_workflow))
            .route("/api/workflows/{id}/run",
                   post(crate::server::handlers::workflows::run_workflow))
            .route("/api/workflows/runs",
                   get(crate::server::handlers::workflows::list_runs))
            .route("/api/workflows/runs/{id}",
                   get(crate::server::handlers::workflows::get_run))
            .route("/api/workflows/runs/{id}/approve",
                   post(crate::server::handlers::workflows::approve_run))
            .route("/api/workflows/{id}",
                   get(crate::server::handlers::workflows::get_workflow)
                   .put(crate::server::handlers::workflows::update_workflow)
                   .delete(crate::server::handlers::workflows::delete_workflow))
            // Audit log (B9). Path comes BEFORE `/{id}` routes so axum
            // doesn't try to parse "audit" as an agent UUID.
            .route("/api/agents/audit",
                   get(crate::server::handlers::agents::list_audit))
            .route("/api/agents/{id}/interrupt",
                   post(crate::server::handlers::agents::interrupt_agent))
            .route("/api/agents/{id}/pause",
                   post(crate::server::handlers::agents::pause_agent))
            .route("/api/agents/{id}/resume",
                   post(crate::server::handlers::agents::resume_agent))
            // 0.113.0 — agent detail view: live structured events + raw stdout.
            .route("/api/agents/{id}/activity",
                   get(crate::server::handlers::agents::agent_activity))
            .route("/api/agents/{id}/stdout",
                   get(crate::server::handlers::agents::agent_stdout))
            .route("/api/agents/{id}/activity/stream",
                   get(crate::server::handlers::agents::agent_activity_stream))
            .route("/api/agents/{id}/stdout/stream",
                   get(crate::server::handlers::agents::agent_stdout_stream))
            // Policy — admin-defined rules (D3). All endpoints
            // are admin-only via the AdminUser extractor.
            .route("/api/policy/rules",
                   get(crate::server::handlers::policy::list_rules)
                   .post(crate::server::handlers::policy::create_rule))
            .route("/api/policy/rules/{id}",
                   get(crate::server::handlers::policy::get_rule)
                   .put(crate::server::handlers::policy::update_rule)
                   .delete(crate::server::handlers::policy::delete_rule))
            // Skills (A4 list + A5 per-user toggle + A6 admin install + A7 trust-store)
            .route("/api/skills", get(crate::server::handlers::skills::list_skills))
            .route("/api/skills/preview",
                   post(crate::server::handlers::skills::preview_skill))
            .route("/api/skills/install",
                   post(crate::server::handlers::skills::install_skill))
            .route("/api/skills/trust-store",
                   get(crate::server::handlers::skills::list_trust_store)
                   .post(crate::server::handlers::skills::add_trust_entry))
            .route("/api/skills/trust-store/{fingerprint}",
                   delete(crate::server::handlers::skills::remove_trust_entry))
            .route("/api/skills/{id}",
                   delete(crate::server::handlers::skills::uninstall_skill))
            .route("/api/skills/{id}/preferences",
                   put(crate::server::handlers::skills::set_skill_enabled))
            // Plugin packages: preview/verify, install, list, uninstall.
            // Admin-gated in-handler; reuses the skills trust store + MCP host.
            .route("/api/admin/packages/preview",
                   post(crate::server::handlers::packages::preview_package))
            .route("/api/admin/packages",
                   get(crate::server::handlers::packages::list_installed))
            .route("/api/admin/packages/install",
                   post(crate::server::handlers::packages::install_package))
            .route("/api/admin/packages/{id}",
                   delete(crate::server::handlers::packages::uninstall_package))
            .route("/api/admin/packages/{id}/disable",
                   post(crate::server::handlers::packages::disable_package))
            .route("/api/admin/packages/{id}/enable",
                   post(crate::server::handlers::packages::enable_package))
            // cpp_provider install wizard: begin → step → cancel, with
            // a resumable session GET. Guided + verified channel-bridge install.
            .route("/api/admin/packages/cpp/install",
                   post(crate::server::handlers::packages::cpp_install_begin))
            .route("/api/admin/packages/cpp/update",
                   post(crate::server::handlers::packages::cpp_update_begin))
            .route("/api/admin/packages/cpp/{id}/session",
                   get(crate::server::handlers::packages::cpp_session))
            .route("/api/admin/packages/cpp/{id}/step",
                   post(crate::server::handlers::packages::cpp_step))
            .route("/api/admin/packages/cpp/{id}/cancel",
                   post(crate::server::handlers::packages::cpp_cancel))
            // Skill secrets (slice 4) — admin-only env-var management.
            // Values never round-trip through the API; only metadata is
            // returned. Scope query param: `?scope=system` (default) or
            // `?scope=user:<id>`.
            .route("/api/admin/skills/{id}/secrets",
                   get(crate::server::handlers::skills::list_skill_secrets))
            .route("/api/admin/skills/{id}/secrets/{key}",
                   put(crate::server::handlers::skills::set_skill_secret)
                   .delete(crate::server::handlers::skills::delete_skill_secret))
            // "Test connection" probe — runs the skill's upstream check
            // (e.g. `claude --print ping` for com.mira.claudecode) with the
            // configured env. Returns ok/error + latency_ms.
            .route("/api/admin/skills/{id}/probe",
                   post(crate::server::handlers::skills::probe_skill))
            // One-click install of a coding-agent skill's CLI (Claude Code /
            // OpenCode) via the managed Node's npm — consent-driven, no restart.
            .route("/api/admin/skills/{id}/install-cli",
                   post(crate::server::handlers::skills::install_skill_cli))
            // Re-extract bundled skills onto disk. Body shape (optional):
            // {"force": true, "id": "com.mira.claudecode"}
            .route("/api/admin/skills/refresh-bundled",
                   post(crate::server::handlers::skills::refresh_bundled_skills))
            // LLM aliases — admin-only routing of skill-tier model choice.
            // GET returns the current `agent.llm_aliases` map; PUT
            // replaces it and persists to mira_config.json.
            .route("/api/admin/llm-aliases",
                   get(crate::server::handlers::skills::list_llm_aliases)
                   .put(crate::server::handlers::skills::set_llm_aliases))
            // Channel accounts (per-user Signal / Telegram)
            .route("/api/channel-accounts",        get(list_accounts).post(create_account))
            .route("/api/channel-accounts/health", get(account_health))
            .route("/api/channel-accounts/{id}",   get(get_account).put(update_account).delete(delete_account))
            // Per-account daemon lifecycle (Signal only; Telegram returns 422).
            // Admin click on Start/Stop/Restart in the UI hits these.
            .route("/api/channel-accounts/{id}/start",
                   post(crate::server::handlers::channel_accounts::start_account_daemon))
            .route("/api/channel-accounts/{id}/stop",
                   post(crate::server::handlers::channel_accounts::stop_account_daemon))
            .route("/api/channel-accounts/{id}/restart",
                   post(crate::server::handlers::channel_accounts::restart_account_daemon))
            // R1+R2 — per-user channel identity links (drives shared-bot
            // routing). Self-service from Settings → My Channels.
            .route("/api/me/channel-links",
                   get(crate::server::handlers::channel_links::list_my_links))
            .route("/api/me/channel-links/{id}",
                   axum::routing::delete(crate::server::handlers::channel_links::delete_my_link))
            .route("/api/me/channel-links/codes",
                   post(crate::server::handlers::channel_links::issue_link_code))
            // Watchdog incidents (W3 — analyze-with-LLM opt-in).
            .route("/api/watchdog/incidents",
                   get(crate::server::handlers::watchdog::list_incidents))
            .route("/api/watchdog/incidents/{id}",
                   get(crate::server::handlers::watchdog::get_incident))
            .route("/api/watchdog/incidents/{id}/analyze",
                   post(crate::server::handlers::watchdog::analyze_incident))
            // MIRA-Guardian action approval (P4a-2). Admin-only; the store
            // Extension is layered below only when present.
            .route("/api/wsl/host-url-check",
                   get(crate::server::handlers::wsl::host_url_check))
            .route("/api/wsl/fix-host-urls",
                   post(crate::server::handlers::wsl::fix_host_urls))
            .route("/api/guardian/status",
                   get(crate::server::handlers::guardian::status))
            .route("/api/guardian/actions",
                   get(crate::server::handlers::guardian::list_actions))
            .route("/api/guardian/actions/{id}/approve",
                   post(crate::server::handlers::guardian::approve_action))
            .route("/api/guardian/actions/{id}/decline",
                   post(crate::server::handlers::guardian::decline_action))
            .route("/api/guardian/provision/status",
                   get(crate::server::handlers::guardian::provision_status))
            .route("/api/guardian/provision",
                   post(crate::server::handlers::guardian::provision))
            // 0.107.0 — system-health admin dashboard.
            .route("/api/health/degradations",
                   get(crate::server::handlers::health_dashboard::list_degradations))
            .route("/api/health/snapshot",
                   get(crate::server::handlers::health_dashboard::get_snapshot))
            .route("/api/health/history",
                   get(crate::server::handlers::health_dashboard::get_history))
            .route("/api/health/incidents",
                   get(crate::server::handlers::health_dashboard::list_incidents))
            .route("/api/health/config",
                   get(crate::server::handlers::health_dashboard::list_config)
                   .put(crate::server::handlers::health_dashboard::upsert_config))
            .route("/api/health/run-now",
                   post(crate::server::handlers::health_dashboard::run_now))
            .route("/api/health/ip-bans",
                   get(crate::server::handlers::health_dashboard::list_ip_bans))
            .route("/api/health/ip-bans/{ip}/lift",
                   post(crate::server::handlers::health_dashboard::lift_ip_ban))
            // 0.110.0 — slice 5 surfaces.
            .route("/metrics",
                   get(crate::server::handlers::health_dashboard::prometheus_metrics))
            .route("/api/health/custom-detectors",
                   get(crate::server::handlers::health_dashboard::list_custom_detectors)
                   .put(crate::server::handlers::health_dashboard::upsert_custom_detector))
            .route("/api/health/custom-detectors/{name}",
                   axum::routing::delete(crate::server::handlers::health_dashboard::delete_custom_detector))
            .route("/api/health/custom-detectors/test",
                   post(crate::server::handlers::health_dashboard::test_custom_detector))
            .route("/api/health/webhooks",
                   get(crate::server::handlers::health_dashboard::list_webhooks)
                   .put(crate::server::handlers::health_dashboard::upsert_webhook))
            .route("/api/health/webhooks/{id}",
                   axum::routing::delete(crate::server::handlers::health_dashboard::delete_webhook))
            .route("/api/health/thresholds",
                   get(crate::server::handlers::health_dashboard::list_thresholds)
                   .put(crate::server::handlers::health_dashboard::upsert_threshold))
            // 0.111.0 — task artifacts dashboard.
            .route("/api/health/artifacts",
                   get(crate::server::handlers::health_dashboard::list_artifacts))
            .route("/api/health/artifacts/{name}",
                   axum::routing::delete(crate::server::handlers::health_dashboard::delete_artifact))
            .route("/api/health/artifacts/migrate",
                   post(crate::server::handlers::health_dashboard::migrate_artifacts))
            // A4 — browse + open a task's artifact files (not just the list).
            .route("/api/admin/tasks/{task_id}/files",
                   get(crate::server::handlers::health_dashboard::list_task_files))
            .route("/api/admin/tasks/{task_id}/file",
                   get(crate::server::handlers::health_dashboard::get_task_file))
            // Admin: cross-user history view (sidebar/dropdown stay per-user).
            .route("/api/admin/conversations/grouped", get(list_grouped_conversations))
            .route("/api/admin/conversations/stats",   get(admin_conversations_stats))
            // Admin: server restart (required after channel-account edits)
            .route("/api/admin/restart", post(restart_handler))
            // Admin: trigger the sleep-like memory consolidator on-demand
            // (runs Phases C, A, D for every user — same order as nightly).
            .route("/api/admin/consolidator/run-now", post(consolidator_run_now))
            // Q1.7 — admin waitlist read/export/delete.
            .route("/api/admin/waitlist",
                   get(crate::server::handlers::waitlist::list))
            .route("/api/admin/waitlist/export",
                   get(crate::server::handlers::waitlist::export_csv))
            .route("/api/admin/waitlist/{id}",
                   axum::routing::delete(crate::server::handlers::waitlist::delete_entry))
            // Q1.6 — Daily Briefing (user-scoped). Read/update the
            // briefing config + fire on demand for testing.
            .route("/api/me/briefing",
                   axum::routing::get(crate::server::handlers::briefing::get_briefing)
                       .patch(crate::server::handlers::briefing::patch_briefing))
            .route("/api/me/briefing/send-now",
                   post(crate::server::handlers::briefing::send_briefing_now))
            // Q1.5 — backup + restore. Both admin-only. Backup
            // streams a tar.gz of the data dir + config; restore
            // stages an uploaded tarball and triggers a restart so
            // the startup hook swaps it in.
            .route("/api/admin/backup",
                   get(crate::server::handlers::backup::download_backup)
                  .post(crate::server::handlers::backup::download_backup_encrypted))
            .route("/api/admin/restore",
                   post(crate::server::handlers::backup::upload_restore))
            // Listing + on-demand snapshot to the scheduled-backup dir
            // + restore from one of those local files (no upload needed).
            .route("/api/admin/backups",
                   get(crate::server::handlers::backup::list_scheduled_backups))
            .route("/api/admin/backups/run-now",
                   post(crate::server::handlers::backup::run_scheduled_backup_now))
            .route("/api/admin/backups/{name}/restore",
                   post(crate::server::handlers::backup::restore_from_scheduled))
            // admin-only update-check. Returns disabled marker
            // when `server.update_check.enabled = false` (the default),
            // otherwise probes the configured Releases API and reports
            // newer_available + release_url.
            .route("/api/admin/update-check",
                   get(crate::server::handlers::update_check::update_check))
            // admin-only one-click in-place upgrade: download + verify
            // + atomic swap + supervisor restart (runs `mira upgrade --binary`).
            .route("/api/admin/upgrade",
                   post(crate::server::handlers::update_check::upgrade))
            // Admin: tool-call audit log (one row per ToolRegistry::execute)
            .route("/api/admin/tool_audit", get(list_tool_audit))
            // Admin: managed native deps (ONNX Runtime, future: signal-cli, JRE).
            // Drives the Settings page's "ONNX Runtime not installed" install dialog.
            .route("/api/admin/deps",
                   get(crate::server::handlers::deps::list_deps))
            .route("/api/admin/deps/{name}/install",
                   post(crate::server::handlers::deps::install_dep))
            // Admin: list embedding models for the requested provider. Backs
            // the Settings page's embedding-model combobox.
            .route("/api/admin/embedding-models",
                   get(crate::server::handlers::providers::list_embedding_models))
            // Calendar — native event CRUD, sync trigger, OAuth flows.
            .route("/api/calendar/events",      get(calendar_list_events).post(calendar_create_event))
            .route("/api/calendar/events/{id}", get(calendar_get_event).put(calendar_update_event).delete(calendar_delete_event))
            .route("/api/calendar/sync",        post(calendar_trigger_sync))
            .route("/api/calendar/oauth/start",      post(calendar_oauth_start))
            .route("/api/calendar/oauth/callback",   get(calendar_oauth_callback))
            .route("/api/calendar/oauth/status",     get(calendar_oauth_status))
            .route("/api/calendar/oauth/disconnect", post(calendar_oauth_disconnect))
            .route("/api/calendar/caldav",            post(calendar_caldav_connect))
            .route("/api/calendar/caldav/disconnect", post(calendar_caldav_disconnect))
            // Onboarding lifecycle (state / start / restart-group). Chat-turn
            // prompt+tool wiring happens on /api/chat itself.
            .route("/api/onboarding/state",              get(onboarding_state))
            .route("/api/onboarding/start",              post(start_onboarding))
            .route("/api/onboarding/restart-group",      post(restart_group))
            .route("/api/onboarding/reset",              post(reset_onboarding))
            .route("/api/onboarding/finalize",           post(onboarding_finalize))
            .route("/api/onboarding/post-complete-chat", post(post_complete_chat))
            // TTS — full-buffer + sentence-streamed synthesis,
            // voice listing, backend status. Voice-download progress arrives
            // in 
            .route("/api/tts/speak",        post(tts_speak))
            .route("/api/tts/speak/stream", post(tts_speak_stream))
            .route("/api/tts/voices",       get(tts_voices))
            .route("/api/tts/voices/download", post(tts_download_voice))
            .route("/api/tts/status",       get(tts_status))
            // STT — multipart-upload transcription + status probe.
            // Channel-side ingest (Signal/Telegram voice notes) calls
            // SttService directly from the webhook, bypassing AuthLayer.
            .route("/api/stt/transcribe",   post(stt_transcribe))
            .route("/api/stt/status",       get(stt_status))
            // Channels — registry of channel descriptors used by the per-
            // channel voice prefs UI. Plugins can register new descriptors
            // before the router is built; the endpoint reflects whatever's
            // in the registry at request time.
            .route("/api/channels", get(list_channels))
            // Automations — schedules CRUD, lifecycle, cron preview,
            // unified list, runs audit. Webhooks + event-subs land in 
            .route("/api/schedules",                get(list_schedules).post(create_schedule))
            .route("/api/schedules/{id}",           get(get_schedule).put(update_schedule).delete(delete_schedule))
            .route("/api/schedules/{id}/next-fires", get(next_fires))
            .route("/api/schedules/{id}/run-now",    post(run_now))
            .route("/api/schedules/{id}/pause",      post(pause_schedule))
            .route("/api/schedules/{id}/resume",     post(resume_schedule))
            .route("/api/schedules/{id}/snooze",     post(snooze_schedule))
            .route("/api/schedules/{id}/approve",    post(approve_schedule))
            .route("/api/schedules/{id}/reject",     post(reject_schedule))
            .route("/api/automations",      get(list_automations))
            .route("/api/automations/runs", get(list_runs))
            // Webhooks (/). The public ingest path
            // `/webhook/incoming/{token}` is wired separately (no Bearer
            // auth); these are owner-scoped CRUD + lifecycle endpoints.
            .route("/api/webhooks",           get(list_webhooks).post(create_webhook))
            .route("/api/webhooks/{id}",      get(get_webhook).put(update_webhook).delete(delete_webhook))
            .route("/api/webhooks/{id}/pause",         post(pause_webhook))
            .route("/api/webhooks/{id}/resume",        post(resume_webhook))
            .route("/api/webhooks/{id}/rotate-token",  post(rotate_token))
            .route("/api/webhooks/{id}/rotate-secret", post(rotate_secret))
            .route("/api/webhooks/{id}/approve",       post(approve_webhook))
            .route("/api/webhooks/{id}/reject",        post(reject_webhook))
            .route("/api/webhooks/{id}/payloads",      get(list_payloads))
            .route("/api/webhooks/{id}/test",          post(test_replay))
            .route("/api/webhooks/{id}/url",           get(webhook_url))
            // Event subscriptions ("triggers") + the catalog of emitter
            // names + an admin-only synthetic emit endpoint for testing.
            .route("/api/event-subscriptions",            get(list_subs).post(create_sub))
            .route("/api/event-subscriptions/{id}",       get(get_sub).put(update_sub).delete(delete_sub))
            .route("/api/event-subscriptions/{id}/pause", post(pause_sub))
            .route("/api/event-subscriptions/{id}/resume", post(resume_sub))
            .route("/api/event-subscriptions/{id}/approve", post(approve_sub))
            .route("/api/event-subscriptions/{id}/reject",  post(reject_sub))
            .route("/api/events/names", get(list_event_names))
            .route("/api/events/test",  post(test_emit))
            // Notifications SSE
            .route("/api/notifications/stream", get(notifications_stream))
            // Web Push (Q1.2) — VAPID-based browser/phone push.
            .route("/api/notifications/push/public-key",
                   get(crate::server::handlers::notifications::push_public_key))
            .route("/api/notifications/push/subscribe",
                   post(crate::server::handlers::notifications::push_subscribe))
            .route("/api/notifications/push/subscriptions",
                   get(crate::server::handlers::notifications::push_list_subscriptions))
            .route("/api/notifications/push/subscriptions/{id}",
                   delete(crate::server::handlers::notifications::push_unsubscribe))
            .route("/api/notifications/push/test",
                   post(crate::server::handlers::notifications::push_test))
            // Logs SSE
            .route("/api/logs/stream", get(logs_stream))
            // Runtime log level toggle (admin only). Live for the process
            // lifetime — restart restores `config.logging.level`.
            .route("/api/logs/level", get(get_log_level).put(set_log_level))
            // Inject extensions.
            .layer(Extension(Arc::clone(&agent_core)))
            .layer(Extension(Arc::clone(&auth)))
            .layer(Extension(Arc::clone(&hist)))
            .layer(Extension(Arc::clone(&live_cfg)))
            .layer(Extension(Arc::clone(&notification_bus)))
            .layer(Extension(Arc::clone(&mcp_servers)))
            .layer({
                // Only add the store layer when wired (tests can omit
                // it). The CRUD handlers expect a populated extension;
                // an empty Arc keeps them returning 503-style errors
                // without crashing the layer build.
                let store = mcp_store.clone().unwrap_or_else(|| Arc::new(
                    crate::mcp::McpServerStore::open(std::path::Path::new(":memory:"))
                        .expect("in-memory mcp_servers store for tests")
                ));
                Extension(store)
            })
            .layer({
                // MCP catalog store — in-memory fallback (re-seeds the
                // defaults) keeps tests + minimal builds compiling.
                let catalog = mcp_catalog.clone().unwrap_or_else(|| Arc::new(
                    crate::mcp::McpCatalogStore::open(std::path::Path::new(":memory:"))
                        .expect("in-memory mcp_catalog store for tests")
                ));
                Extension(catalog)
            })
            .layer({
                // Same pattern for the email store — an in-memory
                // fallback keeps tests + minimal builds compiling.
                let store = email_store.clone().unwrap_or_else(|| Arc::new(
                    crate::email::EmailAccountStore::open(std::path::Path::new(":memory:"))
                        .expect("in-memory email_accounts store for tests")
                ));
                Extension(store)
            })
            .layer(Extension(Arc::clone(&email_pollers)))
            .layer({
                let store = email_quarantine.clone().unwrap_or_else(|| Arc::new(
                    crate::email::EmailQuarantineStore::open(std::path::Path::new(":memory:"))
                        .expect("in-memory quarantine store for tests")
                ));
                Extension(store)
            })
            .layer({
                let store = email_audit.clone().unwrap_or_else(|| Arc::new(
                    crate::email::EmailAuditStore::open(std::path::Path::new(":memory:"))
                        .expect("in-memory audit store for tests")
                ));
                Extension(store)
            })
            // E4 — in-process OAuth state token store. Single
            // instance per gateway; pending flows expire after 10
            // minutes so a closed browser tab doesn't leak entries.
            .layer(Extension(Arc::new(crate::email::OAuthStateStore::new())))
            // E5 — system mailer. Empty fallback is a no-op mailer
            // whose `send` always errors with "not enabled"; the
            // handler returns the error message verbatim so tests +
            // unconfigured installs get a clean response, not a 500.
            .layer({
                let m = system_mailer.clone().unwrap_or_else(|| Arc::new(
                    crate::email::SystemMailer::new(
                        Arc::clone(&live_cfg),
                        Arc::new(crate::email::ReplyLoopCache::new()),
                    )
                ));
                Extension(m)
            })
            // K3 — Chatterbox supervisor (Option; None when not supervising).
            .layer(Extension(chatterbox_supervisor.clone()))
            .layer(Extension(web_push.clone()))
            .layer(Extension(waitlist.clone()))
            .layer(Extension(Arc::clone(&restart_notify)))
            .layer(Extension(avatar_store.clone()))
            .layer(Extension(data_dir.clone()))
            .layer(Extension(crate::server::handlers::skills::SkillPrefsExt(skill_prefs.clone())))
            .layer(Extension(Arc::clone(&agent_registry)))
            .layer(Extension(Arc::clone(&supervisor)))
            .layer(Extension(Arc::clone(&preamble_cache)))
            .layer(Extension(tts_service.clone()))
            .layer(Extension(stt_service.clone()))
            .layer(Extension(Arc::clone(&channel_registry)))
            .layer(Extension(Arc::clone(&event_bus)))
            .layer(Extension(crate::server::handlers::skills::SecretsStoreExt(secrets_store.clone())));

        // 0.107.0 — health-dashboard extensions. Only layered when the
        // backing stores are present; admin-gated handlers will get
        // an Extension-extraction error (→ 500) if a non-admin somehow
        // routes here without the layer, but the admin gate fires first.
        let api_routes = if let Some(ref hs) = health_store {
            api_routes.layer(Extension(Arc::clone(hs)))
        } else {
            api_routes
        };
        let api_routes = if let Some(ref dt) = degradations {
            api_routes.layer(Extension(Arc::clone(dt)))
        } else {
            api_routes
        };
        let api_routes = if let Some(ref s) = guardian_actions {
            api_routes.layer(Extension(Arc::clone(s)))
        } else {
            api_routes
        };
        let api_routes = if let Some(ref s) = audit_store {
            api_routes.layer(Extension(Arc::clone(s)))
        } else {
            api_routes
        };
        let api_routes = if let Some(ref db) = auth_db_arc {
            api_routes.layer(Extension(Arc::clone(db)))
        } else {
            api_routes
        };
        let api_routes = if let Some(ref arts) = task_artifacts {
            api_routes.layer(Extension(Arc::clone(arts)))
        } else {
            api_routes
        };

        // Channel-accounts extension only layered when the store is present.
        let api_routes = if let Some(ref store) = channel_accounts {
            api_routes.layer(Extension(Arc::clone(store)))
        } else {
            api_routes
        };

        // R1+R2 — IdentityStore + LinkCodeStore. Both Options so tests
        // + minimal builds compile (the handlers return 500 if the
        // extension is missing). Real instances are opened by the
        // gateway against auth.db and passed in here.
        let api_routes = if let Some(ref s) = identity_store {
            api_routes.layer(Extension(Arc::clone(s)))
        } else { api_routes };
        let api_routes = if let Some(ref s) = link_code_store {
            api_routes.layer(Extension(Arc::clone(s)))
        } else { api_routes };

        // ChannelManager handle for per-account daemon lifecycle endpoints.
        // Wrapped in a typed newtype so the Extension lookup is unambiguous
        // (Arc<RwLock<ChannelManager>> alone might collide with future
        // RwLock<T> extensions). Absent in tests + minimal builds.
        let api_routes = if let Some(ref mgr) = channel_manager {
            api_routes.layer(Extension(
                crate::server::handlers::channel_accounts::ChannelManagerExt(Arc::clone(mgr))
            ))
        } else {
            api_routes
        };

        // Tool-audit extension only layered when the store is present; the
        // admin endpoint returns 500 if called without it.
        let api_routes = if let Some(ref store) = tool_audit {
            api_routes.layer(Extension(Arc::clone(store)))
        } else {
            api_routes
        };

        // Admin policy rules (D3) — layered when the store is present.
        // /api/policy/rules handlers reach for this; without it they
        // 500 (rather than silently allowing every rule to look empty).
        let api_routes = if let Some(ref store) = admin_policy_rules {
            api_routes.layer(Extension(Arc::clone(store)))
        } else {
            api_routes
        };

        // Named-agent definitions (Phase B) — layered when the store opened.
        let api_routes = if let Some(ref store) = agent_defs {
            api_routes.layer(Extension(Arc::clone(store)))
        } else {
            api_routes
        };
        let api_routes = if let Some(ref store) = workflow_store {
            api_routes.layer(Extension(Arc::clone(store)))
        } else {
            api_routes
        };
        let api_routes = if let Some(ref orch) = workflow_orchestrator {
            api_routes.layer(Extension(Arc::clone(orch)))
        } else {
            api_routes
        };

        // Calendar extensions — store + config Arc needed by OAuth / sync
        // handlers. We always layer the config Arc so read-only endpoints
        // that need provider config keep working even without a store.
        let api_routes = api_routes
            .layer(Extension(Arc::new(config.clone())));
        let api_routes = if let Some(ref store) = calendar_store {
            api_routes.layer(Extension(Arc::clone(store)))
        } else {
            api_routes
        };

        // Automations. Both store and worker are optional —
        // when the subsystem fails to start, the routes are still wired
        // but every handler will 500 because the Extensions are absent.
        let api_routes = if let Some(ref s) = automations_store {
            api_routes.layer(Extension(Arc::clone(s)))
        } else { api_routes };
        let api_routes = if let Some(ref w) = automations_worker {
            api_routes.layer(Extension(Arc::clone(w)))
        } else { api_routes };

        let spa_route = spa_router();

        router = router
            .merge(auth_routes)
            .merge(api_routes)
            .merge(spa_route);
    }

    let mut auth_layer = AuthLayer::new(
        security.auth_token.clone(),
        security.public_routes.to_vec(),
    );
    if let Some(svc) = auth_service_for_layer {
        auth_layer = auth_layer.with_auth_service(svc);
    }
    let mut layered = router.layer(auth_layer);
    if let Some(layer) = ip_ban_layer {
        layered = layered.layer(layer);
    }
    layered
        .layer(RateLimitLayer::new(security.rate_limit_rpm, vec![]))
        .layer(build_cors_layer(&security.cors_origins))
        .layer(RequestLogLayer)
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn test_router() -> Router {
        use async_trait::async_trait;
        use crate::types::{ChatMessage, GenerationOptions, GenerationResponse, TokenUsage, ProviderId};
        use crate::providers::ModelProvider;
        use crate::memory::MemorySystem;
        use crate::tools::ToolRegistry;
        use crate::session::SessionStore;
        use crate::config::MiraConfig;
        use tempfile::TempDir;

        struct AlwaysHealthy;
        #[async_trait]
        impl ModelProvider for AlwaysHealthy {
            fn name(&self) -> &str { "test" }
            async fn generate(&self, _: &[ChatMessage], _: &GenerationOptions)
                -> Result<GenerationResponse, crate::MiraError>
            {
                Ok(GenerationResponse {
                    content: "test".to_string(),
                    tool_calls: None,
                reasoning: None,
                    usage: TokenUsage::default(),
                    provider_id: ProviderId::Local("test".to_string()),
                    model_name: "test".to_string(),
                    fallback: None,
            })
            }
            async fn health_check(&self) -> bool { true }
        }

        let dir = TempDir::new().unwrap();
        let mut cfg = MiraConfig::default();
        cfg.agent.tool_mode = "disabled".to_string();
        cfg.memory.embedding.provider = "lmstudio".to_string();
        cfg.data_dir = dir.path().to_string_lossy().to_string();

        let core = Arc::new(AgentCore::new(
            Arc::new(cfg.clone()),
            Arc::new(AlwaysHealthy) as Arc<dyn ModelProvider>,
            Arc::new(MemorySystem::new_keyword_only(dir.path().join("mem.db")).unwrap()),
            Arc::new(ToolRegistry::new()),
            Arc::new(SessionStore::new()),
        ));

        let security = SecurityConfig::default();
        let bus = Arc::new(NotificationBus::new());
        let accounts = Arc::new(std::collections::HashMap::new());
        let event_bus = Arc::new(EventBus::new());
        let restart = Arc::new(tokio::sync::Notify::new());
        let agent_registry = Arc::new(crate::agent::AgentRegistry::new());
        let supervisor     = Arc::new(crate::agent::Supervisor::new(agent_registry.clone()));
        // Empty MCP registry — tests don't spawn child processes.
        let mcp_servers = Arc::new(crate::mcp::McpServerRegistry::empty());
        build_router(
            core, &security, &cfg, None, None, None, bus, accounts,
            Arc::new(std::collections::HashMap::new()), // whatsapp_accounts
            Arc::new(std::collections::HashMap::new()), // slack_accounts
            Arc::new(std::collections::HashMap::new()), // external_accounts
            None, // channel_accounts store
            None, // identity_store (R1+R2 — link-tier surface absent in tests)
            None, // link_code_store (R1+R2 — link-tier surface absent in tests)
            None, // channel_manager (lifecycle endpoints disabled in test)
            None, None, None, None, event_bus, restart,
            agent_registry, supervisor,
            None, // admin_policy_rules — D3
            None, // secrets_store — slice 4
            None, // health_store — 0.107.0
            None, // task_artifacts — 0.111.0
            None, // web_push — 0.142.0 (Q1.2)
            None, // waitlist — 0.150.0 (Q1.7)
            mcp_servers, // Q2 #7 empty registry in tests
            None,        // mcp_store —, omitted in tests
            None,        // mcp_catalog — omitted in tests (in-memory fallback seeds it)
            None,        // email_store — Q2 #8 chunk 1, omitted in tests
            Arc::new(crate::email::EmailPollerRegistry::empty()), // chunk 2
            None, // email_quarantine — chunk 5, omitted in tests
            None, // email_audit      — chunk 5, omitted in tests
            None, // system_mailer    — E5, omitted in tests
            None, // chatterbox_supervisor — K3, omitted in tests
            None, // degradations — omitted in tests
            None, // guardian_actions — P4a-2, omitted in tests
            None, // audit_store — P4 HMAC audit, omitted in tests
        )
    }

    #[tokio::test]
    async fn health_route_returns_200() {
        let app = test_router().await;
        let req = Request::builder().uri("/health").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn signal_route_is_unmounted_when_hmac_key_absent() {
        // Since 0.130.2 the /webhook/signal route is only mounted when
        // channels.signal.hmac_key is configured — running an unsigned
        // inbound webhook would let anyone POST fake messages. The
        // default test config has no key, so the route 404s. With a
        // key the route mounts; signature validation would then return
        // 401 on mismatch and the handler would return 400 on bad
        // payload — exercised in security::hmac tests.
        let app = test_router().await;
        let req = Request::builder()
            .method("POST")
            .uri("/webhook/signal")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 404);
    }
}
