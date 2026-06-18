// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/mod.rs
//! Central Server for MIRA.
//!
//! [`MiraServer`] is a thin wrapper around an axum [`Router`] + a TCP listener.
//! All wiring (AgentCore, security, channels) is assembled by the `Gateway`
//! and injected here — the server does not read env vars or create providers.
//!
//! # Usage
//!
//! ```rust,ignore
//! let server = MiraServer::new(agent_core, security_config, &config, None, None, None);
//! server.run_until_shutdown(shutdown_signal).await?;
//! ```

pub mod handlers;
pub mod router;

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::info;

use crate::agent::AgentCore;
use crate::auth::LocalAuthService;
use crate::automations::{AutomationsStore, Worker as AutomationsWorker};
use crate::calendar::CalendarStore;
use crate::channel_accounts::ChannelAccountStore;
use crate::config::MiraConfig;
use crate::events::EventBus;
use crate::gateway::channel_manager::TelegramAccountCtx;
use crate::history::HistoryStore;
use crate::notifications::NotificationBus;
use crate::security::SecurityConfig;
use crate::tools::audit::ToolAuditStore;
use crate::web::LiveConfig;
use crate::MiraError;

// ─────────────────────────────────────────────────────────────────────────────

// The MIRA Central Server — owns the TCP socket and the axum router.
pub struct MiraServer {
    router:         axum::Router,
    bind_addr:      SocketAddr,
    // Notified when an admin hits `POST /api/admin/restart`. The Gateway
    // races this against ctrl_c to trigger graceful shutdown.
    pub restart_notify: Arc<tokio::sync::Notify>,
}

impl MiraServer {
    // Construct the server from fully-wired components.
    //     // `bind_addr` comes from the Gateway: `127.0.0.1:{port}` when nginx is
    // enabled (internal-only), `0.0.0.0:{port}` when running directly.
    pub fn new(
        agent_core:        Arc<AgentCore>,
        security:          SecurityConfig,
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
        identity_store:    Option<Arc<crate::channel_identity::IdentityStore>>,
        link_code_store:   Option<Arc<crate::channel_identity::LinkCodeStore>>,
        channel_manager:   Option<Arc<tokio::sync::RwLock<crate::gateway::channel_manager::ChannelManager>>>,
        tool_audit:        Option<Arc<ToolAuditStore>>,
        calendar_store:    Option<Arc<CalendarStore>>,
        automations_store:  Option<Arc<AutomationsStore>>,
        automations_worker: Option<Arc<AutomationsWorker>>,
        event_bus:          Arc<EventBus>,
        agent_registry:     Arc<crate::agent::AgentRegistry>,
        supervisor:         Arc<crate::agent::Supervisor>,
        admin_policy_rules: Option<Arc<crate::policy::AdminRulesStore>>,
        secrets_store:      Option<Arc<crate::skills::SecretsStore>>,
        // 0.107.0 — system-health dashboard backing store.
        health_store:       Option<Arc<crate::health::store::HealthStore>>,
        // 0.111.0 — task artifacts dir manager.
        task_artifacts:     Option<Arc<crate::task_artifacts::TaskArtifactsStore>>,
        // 0.142.0 (Q1.2) — Web Push (VAPID) service. `None` keeps the
        // push endpoints 503 and the bus forwarder unspawned.
        web_push:           Option<Arc<crate::notifications::web_push::WebPushService>>,
        // 0.150.0 (Q1.7) — landing-page waitlist store. `None` keeps
        // /api/waitlist/* endpoints 503.
        waitlist:           Option<Arc<crate::waitlist::WaitlistStore>>,
        // Q2 #7 MCP host registry.
        mcp_servers:        Arc<crate::mcp::McpServerRegistry>,
        // Q2 #7 per-user MCP server store. None disables
        // CRUD endpoints; registry can still hold connected clients.
        mcp_store:          Option<Arc<crate::mcp::McpServerStore>>,
        // Admin-managed MCP server catalog (recommended servers).
        mcp_catalog:        Option<Arc<crate::mcp::McpCatalogStore>>,
        // Q2 #8 E1+E3 chunk 1 — per-user email account store.
        email_store:        Option<Arc<crate::email::EmailAccountStore>>,
        // Q2 #8 E1+E3 chunk 2 — per-account IMAP poller registry.
        email_pollers:      Arc<crate::email::EmailPollerRegistry>,
        // Q2 #8 E1+E3 chunk 5 — quarantine + audit stores.
        email_quarantine:   Option<Arc<crate::email::EmailQuarantineStore>>,
        email_audit:        Option<Arc<crate::email::EmailAuditStore>>,
        // Q2 #8 E5 — system email mailer.
        system_mailer:      Option<Arc<crate::email::SystemMailer>>,
        // Q2 #10 K3 — Chatterbox server supervisor (None when not supervising).
        chatterbox_supervisor: Option<Arc<crate::tts::chatterbox::ChatterboxSupervisor>>,
        // Live subsystem-fallback tracker (powers /api/health/degradations).
        degradations:          Option<Arc<crate::health::degradation::DegradationTracker>>,
        // MIRA-Guardian pending action proposals (powers /api/guardian/actions).
        guardian_actions:      Option<Arc<crate::agent::guardian_actions::GuardianActionStore>>,
        // HMAC-chained agent audit log — guardian decision events (P4).
        audit_store:           Option<Arc<crate::agent::AuditStore>>,
        // Externally-supplied restart notifier — passed in (rather than
        // created here) so callers that need to wire a restart trigger into
        // pre-server components (e.g. the backup agent tools registered
        // before the router is built) can share the same Arc.
        restart_notify:        Arc<tokio::sync::Notify>,
    ) -> Self {
        let bind_addr = if config.proxy.enabled {
            format!("127.0.0.1:{}", config.server.port)
        } else {
            format!("{}:{}", config.server.host, config.server.port)
        };
        let bind_addr: SocketAddr = bind_addr
            .parse()
            .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], config.server.port)));

        let router = router::build_router(
            agent_core,
            &security,
            config,
            auth_service,
            history,
            live_config,
            notification_bus,
            telegram_accounts,
            whatsapp_accounts,
            slack_accounts,
            external_accounts,
            channel_accounts,
            identity_store,
            link_code_store,
            channel_manager,
            tool_audit,
            calendar_store,
            automations_store,
            automations_worker,
            event_bus,
            Arc::clone(&restart_notify),
            agent_registry,
            supervisor,
            admin_policy_rules,
            secrets_store,
            health_store,
            task_artifacts,
            web_push,
            waitlist,
            mcp_servers,
            mcp_store,
            mcp_catalog,
            email_store,
            email_pollers,
            email_quarantine,
            email_audit,
            system_mailer,
            chatterbox_supervisor,
            degradations,
            guardian_actions,
            audit_store,
        );

        Self { router, bind_addr, restart_notify }
    }

    // Bind and serve until the given `shutdown_signal` future resolves.
    pub async fn run_until_shutdown<F>(self, shutdown_signal: F) -> Result<(), MiraError>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind(self.bind_addr).await
            .map_err(|e| MiraError::ServerError(format!(
                "Failed to bind {}: {}", self.bind_addr, e
            )))?;

        info!("MIRA server listening on http://{}", self.bind_addr);

        axum::serve(listener, self.router)
            .with_graceful_shutdown(shutdown_signal)
            .await
            .map_err(|e| MiraError::ServerError(e.to_string()))
    }
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::config::MiraConfig;
    use crate::security::SecurityConfig;

    #[test]
    fn default_server_config_has_sensible_values() {
        let cfg = MiraConfig::default();
        assert!(cfg.server.port > 0);
        assert!(!cfg.server.host.is_empty());
    }

    #[test]
    fn security_config_default_has_empty_auth_token() {
        let sc = SecurityConfig::default();
        assert!(sc.auth_token.is_none());
        assert!(!sc.public_routes.is_empty());
    }
}
