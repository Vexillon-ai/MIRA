// SPDX-License-Identifier: AGPL-3.0-or-later

// src/agent/guardian.rs

//! **MIRA-Guardian** — the built-in, code-defined system watchdog agent.
//!
//! Unlike user [`AgentDefinition`]s (rows in `agent_definitions`, full CRUD),
//! the Guardian's identity — name, system prompt, tool allowlist — lives **in
//! the binary**. The named-agent resolver returns this definition for the
//! reserved handle [`RESERVED_NAME`] *before* consulting the DB, and
//! `definitions::validate_name` rejects that handle, so the Guardian is
//! **non-deletable** and its **tooling is immutable** by construction (you would
//! have to replace the binary). The only operator-controllable knob is
//! [`GuardianMode`] (`guardian.mode` in config); the identity is fixed.
//!
//! [`fingerprint`] is a SHA-256 over the canonical definition, logged + audited
//! at boot so any drift from the shipped spec is visible. (It attests
//! "definition unchanged", not "binary authentic" — that is the separate
//! release-signing layer.) See `design-docs/guardian-agent.md`.

use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::agent::definitions::AgentDefinition;
use crate::config::MiraConfig;

/// Reserved invocation handle. Cannot be created/updated as a user agent.
pub const RESERVED_NAME: &str = "mira-guardian";
/// Stable id for the built-in definition (never a real DB row).
pub const RESERVED_ID: &str = "builtin:mira-guardian";
/// The dedicated `agent.llm_aliases` key the Guardian's model binds to. Users
/// (and P2 provisioning) point this at a local provider/model.
pub const GUARDIAN_ALIAS: &str = "guardian";

/// Ring-0 (read-only) tool allowlist for the Guardian. Diagnostic only — no
/// network, filesystem-write, or action tools. This is the **immutable
/// identity** (the fingerprint covers it); `active` mode grants the Ring-1
/// propose tool on top (see `active_tools`) without changing the identity.
const RING0_TOOLS: &[&str] = &["guardian_inspect", "mira_help", "recall_history"];

/// The Ring-1 *propose* tool granted only in `active` mode. The Guardian can
/// PROPOSE a bounded action with it; it never executes (approval is out-of-band,
/// server-side). Not part of the fingerprinted identity.
const RING1_PROPOSE_TOOL: &str = "guardian_propose_action";

/// Tools available to a Guardian turn given the operating mode: Ring-0 always,
/// plus the propose tool in `active`.
pub fn tools_for_mode(mode: GuardianMode) -> Vec<String> {
    let mut t: Vec<String> = RING0_TOOLS.iter().map(|s| s.to_string()).collect();
    if mode == GuardianMode::Active {
        t.push(RING1_PROPOSE_TOOL.to_string());
    }
    t
}

/// Operator-controlled authority. Parsed from `guardian.mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardianMode {
    /// Fully disabled — the agent does not resolve at all. Default.
    Off,
    /// Observe + alert only; no actions (Ring 0).
    Monitor,
    /// Monitor plus gated/isolation remediation actions (Rings 1 + isolation).
    Active,
}

impl GuardianMode {
    pub fn from_config(config: &MiraConfig) -> Self {
        match config.guardian.mode.trim().to_ascii_lowercase().as_str() {
            "monitor" => GuardianMode::Monitor,
            "active"  => GuardianMode::Active,
            _          => GuardianMode::Off, // unknown / "off" → safe default
        }
    }
}

/// Resolve the Guardian's authority for this instance.
pub fn mode(config: &MiraConfig) -> GuardianMode {
    GuardianMode::from_config(config)
}

/// The Guardian's persona. Read-only/monitor framing for v0; the cardinal rule
/// (detectors decide *if*, the model decides *how*) is baked in so a small model
/// can't suppress a real signal or invent a fake one.
const SYSTEM_PROMPT: &str = "\
You are MIRA-Guardian, the built-in watchdog for this MIRA instance. You always \
identify yourself as \"MIRA-Guardian\" — never as the user's normal assistant. \
Your job is to watch MIRA's health, audit trail, and logs, explain what is \
happening in plain language, and recommend concrete fixes.\n\n\
Cardinal rule: the deterministic health detectors decide WHETHER something is \
wrong. You never override them — you do not invent problems they did not report, \
and you do not declare things healthy when a detector is Yellow/Red. Your value \
is interpretation: correlate signals into a likely root cause, and say clearly \
what you would do about it.\n\n\
Use `guardian_inspect` to read the current health snapshot, active degradations, \
and recent logs; use `mira_help` for how MIRA works; use `recall_history` for \
prior context. Be concise and operational. When you are unsure, say so. You are \
read-only in this mode: describe and recommend, do not claim to have changed \
anything.";

/// The built-in Guardian definition. Constructed fresh each call (cheap); never
/// persisted. `enabled` is always true — gating is via [`GuardianMode`], not the
/// definition flag.
pub fn definition() -> AgentDefinition {
    AgentDefinition {
        id:            RESERVED_ID.to_string(),
        name:          RESERVED_NAME.to_string(),
        description:   "Built-in system watchdog: monitors MIRA's health, audit, and logs; \
                        explains issues and recommends fixes. Identity is immutable."
                          .to_string(),
        system_prompt: SYSTEM_PROMPT.to_string(),
        allowed_tools: RING0_TOOLS.iter().map(|s| s.to_string()).collect(),
        // Pinned to the dedicated `guardian` llm-alias (P2). If that alias isn't
        // configured, the resolver falls back to the primary provider — and the
        // fail-closed `model_check` (§5) refuses to run if whatever resolves
        // isn't a local provider. The alias name is fixed; its target is config.
        model_alias:   Some(GUARDIAN_ALIAS.to_string()),
        budget_usd:    None,
        enabled:       true,
        created_at:    0,
        updated_at:    0,
    }
}

/// SHA-256 (hex) over the canonical Guardian definition — name, system prompt,
/// sorted tool allowlist. Stable across runs of the same binary; changes iff the
/// shipped definition changes. Logged + written to the audit chain at boot.
///
/// Scope: detects drift / config-level alteration and proves the running
/// definition equals the shipped spec. It does NOT defend against a recompiled
/// binary (that is release signing's job) — a tampered binary can forge this.
pub fn fingerprint() -> String {
    let def = definition();
    let mut tools = def.allowed_tools.clone();
    tools.sort();
    let canonical = format!(
        "name={}\nprompt={}\ntools={}",
        def.name,
        def.system_prompt,
        tools.join(","),
    );
    let mut h = Sha256::new();
    h.update(canonical.as_bytes());
    let digest = h.finalize();
    hex::encode(digest)
}

// ── Local-only enforcement (§5) ───────────────────────────────────────────────

/// Where the Guardian's resolved model lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelLocality {
    /// Local provider bound to loopback (127.0.0.1 / localhost / ::1) — ideal.
    LoopbackLocal,
    /// Local provider (ollama/lmstudio) but a non-loopback (LAN) address — the
    /// operator's deliberate choice; allowed, but warned (data leaves the box).
    LanLocal,
    /// A cloud/remote provider — REFUSED (would egress conversation data).
    Cloud,
}

/// Verdict of the fail-closed local-model check.
#[derive(Debug, Clone)]
pub struct GuardianModelCheck {
    pub provider: String,
    pub url:      Option<String>,
    pub locality: ModelLocality,
    /// Whether the Guardian is permitted to run with this model.
    pub allowed:  bool,
    pub reason:   String,
}

/// Providers that serve a local model and expose a `url` we can classify.
fn is_local_provider(name: &str) -> bool {
    matches!(name, "ollama" | "lmstudio")
}

/// True when a base URL points at the local host.
fn url_is_loopback(url: &str) -> bool {
    // Pull the host between scheme:// and the next / or :.
    let after = url.split("://").nth(1).unwrap_or(url);
    let host = after.split('/').next().unwrap_or(after);
    let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    let host = host.trim_start_matches('[').trim_end_matches(']'); // [::1]
    matches!(host, "127.0.0.1" | "localhost" | "::1") || host.starts_with("127.")
}

/// Resolve which provider the Guardian's model binds to (its `guardian` alias if
/// configured, else the primary provider) and its URL if local.
fn resolved_provider(config: &MiraConfig) -> (String, Option<String>) {
    let provider = config.agent.llm_aliases.get(GUARDIAN_ALIAS)
        .map(|a| a.provider.clone())
        .unwrap_or_else(|| config.primary_provider.clone());
    let url = match provider.as_str() {
        "ollama"   => Some(config.providers.ollama.url.clone()),
        "lmstudio" => Some(config.providers.lmstudio.url.clone()),
        _          => None,
    };
    (provider, url)
}

/// Fail-closed local-model check (§5). The Guardian must use a *local* model so
/// no conversation/log/health data ever egresses — even though the co-resident
/// main agent may legitimately use a cloud provider. Cloud → refused; loopback →
/// ideal; LAN-local → allowed with a warning.
pub fn model_check(config: &MiraConfig) -> GuardianModelCheck {
    let (provider, url) = resolved_provider(config);
    if !is_local_provider(&provider) {
        return GuardianModelCheck {
            locality: ModelLocality::Cloud, allowed: false,
            reason: format!(
                "Guardian model resolves to non-local provider '{provider}' — refusing \
                 (would egress data). Point the '{GUARDIAN_ALIAS}' llm-alias at a local \
                 provider (ollama/lmstudio)."),
            provider, url,
        };
    }
    let u = url.clone().unwrap_or_default();
    if url_is_loopback(&u) {
        GuardianModelCheck {
            locality: ModelLocality::LoopbackLocal, allowed: true,
            reason: format!("local provider '{provider}' on loopback"), provider, url,
        }
    } else {
        GuardianModelCheck {
            locality: ModelLocality::LanLocal, allowed: true,
            reason: format!(
                "local provider '{provider}' on non-loopback address ({u}) — allowed, but \
                 data leaves this host to reach it"),
            provider, url,
        }
    }
}

/// Assert the built-in allowlist carries no network-capable tool by name. The
/// allowlist is a code const (Ring-0), so this is a guard against future edits.
pub fn allowlist_has_no_network_tool() -> bool {
    const NETWORKISH: &[&str] = &[
        "web_fetch", "web_search", "url_preview", "image_generate", "video_generate",
        "calendar_create_event", "calendar_list_events", "calendar_update_event",
        "calendar_delete_event",
    ];
    !RING0_TOOLS.iter().any(|t| NETWORKISH.contains(t))
}

// ── Proactive watch loop (P3) ─────────────────────────────────────────────────

/// Spawn the proactive watch loop: every `guardian.watch_interval_secs`, if the
/// latest health snapshot is non-green **and** its triggered-detector set changed
/// since the last alert, run a Guardian turn and deliver the alert via the
/// `NotificationBus` (web/push) and the `watchdog.alert` event rail (any
/// configured channel + run history). Self-contained background task; a no-op on
/// each tick while `mode == off`, so flipping the mode live takes effect.
///
/// "Detectors decide *if*, the LLM decides *how*" — the Guardian turn only runs
/// when the deterministic detectors already flagged a non-green state.
/// Per-kind cooldown (seconds) between autonomous executions under isolation —
/// part of the bounded blast radius (no thrash). 1 hour.
const AUTO_COOLDOWN_SECS: i64 = 3600;

/// Whether a proposed action kind is eligible for autonomous (no-approval)
/// execution under isolation (§4.5) — the clearly-safe, comms-restoring subset.
/// Requeue/trim are NOT autonomy-eligible; they wait for approval.
pub fn is_autonomy_eligible(kind: crate::agent::guardian_actions::GuardianActionKind) -> bool {
    use crate::agent::guardian_actions::GuardianActionKind::*;
    matches!(kind, RestartBridge | RerunAudit)
}

/// Process-global telemetry for the proactive watch loop, surfaced read-only to
/// the operator via `GET /api/guardian/status`. There's exactly one watch loop
/// per process, so a singleton avoids threading a new Extension through the
/// whole router/builder. In-memory: reset on restart.
#[derive(Clone, Default, serde::Serialize)]
pub struct WatchStatus {
    /// Configured tick interval (seconds).
    pub interval_secs:        u64,
    /// Unix secs of the most recent completed tick — proves the loop is alive.
    pub last_run_at:          Option<i64>,
    /// Unix secs of the most recent alert the loop raised.
    pub last_alert_at:        Option<i64>,
    /// First ~200 chars of that alert.
    pub last_alert_summary:   Option<String>,
    /// How many detectors were non-green when that alert fired.
    pub last_alert_detectors: usize,
    /// Alerts raised since this process started.
    pub alerts_total:         u64,
}

static WATCH_STATUS: std::sync::OnceLock<tokio::sync::RwLock<WatchStatus>> =
    std::sync::OnceLock::new();

/// Shared watch-loop telemetry (lazily created). The loop writes it each tick;
/// the `/api/guardian/status` handler reads it.
pub fn watch_status() -> &'static tokio::sync::RwLock<WatchStatus> {
    WATCH_STATUS.get_or_init(|| tokio::sync::RwLock::new(WatchStatus::default()))
}

pub fn spawn_watch_loop(
    agent:            std::sync::Arc<crate::agent::core::AgentCore>,
    health:           std::sync::Arc<crate::health::store::HealthStore>,
    notifications:    std::sync::Arc<crate::notifications::NotificationBus>,
    event_bus:        Option<std::sync::Arc<crate::events::EventBus>>,
    config:           std::sync::Arc<MiraConfig>,
    notify_user_id:   Option<String>,
    // P4c — to detect this turn's proposals + record isolation-autonomy decisions.
    guardian_actions: Option<std::sync::Arc<crate::agent::guardian_actions::GuardianActionStore>>,
    audit:            Option<std::sync::Arc<crate::agent::audit::AuditStore>>,
    // P4c-2 — execution deps for real autonomous action (when isolation_dry_run=false).
    automations:      Option<std::sync::Arc<crate::automations::AutomationsStore>>,
    channel_manager:  Option<std::sync::Arc<tokio::sync::RwLock<crate::gateway::channel_manager::ChannelManager>>>,
) -> tokio::task::JoinHandle<()> {
    let interval_secs = config.guardian.watch_interval_secs.max(60);
    info!("MIRA-Guardian: proactive watch loop every {interval_secs}s");
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        ticker.tick().await; // skip the immediate first tick
        let mut last_fingerprint: Option<String> = None;
        // P4c-2 — per-kind cooldown for autonomous actions (kind → last-exec unix)
        // + pending reconciliations (action_id, message) to deliver once a
        // channel is back. In-memory: a restart re-derives state from health.
        let mut auto_cooldown: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        let mut reconcile_queue: Vec<(String, String)> = Vec::new();
        loop {
            ticker.tick().await;
            {
                // Liveness — record every completed tick so the operator's
                // status panel can show "watch loop ran Ns ago".
                let mut ws = watch_status().write().await;
                ws.last_run_at   = Some(chrono::Utc::now().timestamp());
                ws.interval_secs = interval_secs;
            }
            let gmode = mode(&config);
            // P4c-2 — flush pending reconciliations first (independent of health):
            // deliver "I acted while you were unreachable" as soon as a channel returns.
            if !reconcile_queue.is_empty() {
                if let (Some(recipient), Some(disp)) =
                    (notify_user_id.as_deref(), agent.companion_dispatcher())
                {
                    let mut still = Vec::new();
                    for (aid, msg) in reconcile_queue.drain(..) {
                        match disp.deliver_to_user(recipient, &msg).await {
                            crate::companion::dispatcher::DeliveryOutcome::Delivered(ch) => {
                                info!("MIRA-Guardian reconciled isolation action [id={aid}] via '{ch}'");
                                if let Some(au) = audit.as_ref() {
                                    let _ = au.record(crate::agent::audit::guardian_agent_id(),
                                        crate::agent::audit::AuditEvent::GuardianAction {
                                            action_id: aid.clone(), action_kind: "reconcile".into(),
                                            decision: "reconciled".into(), detail: Some(msg.clone()) });
                                }
                            }
                            _ => still.push((aid, msg)), // still unreachable — keep for next tick
                        }
                    }
                    reconcile_queue = still;
                }
            }
            if gmode == GuardianMode::Off { last_fingerprint = None; continue; }
            let snap = match health.latest() { Ok(Some(s)) => s, _ => continue };
            if matches!(snap.worst_level(), crate::health::HealthLevel::Green) {
                last_fingerprint = None; // Green clears dedup → a re-trigger re-alerts
                continue;
            }
            let mut triggered: Vec<String> = snap.reports.iter()
                .filter(|r| !matches!(r.level, crate::health::HealthLevel::Green))
                .map(|r| r.name.clone())
                .collect();
            triggered.sort();
            let fp = triggered.join(",");
            if last_fingerprint.as_deref() == Some(fp.as_str()) { continue; } // dedup unchanged

            let mut task = format!(
                "A health audit just ran and MIRA's worst level is {:?} ({} detector(s) triggered: {}). \
                 Call guardian_inspect (what=\"all\"), then write a 2-3 sentence operator alert: what is \
                 wrong, the most likely root cause, and the single most useful next action. Be specific \
                 and concise; begin with 'MIRA-Guardian:'.",
                snap.worst_level(), triggered.len(), triggered.join(", "),
            );
            if gmode == GuardianMode::Active {
                task.push_str(
                    " If exactly ONE bounded fix is clearly warranted (rerun_audit / restart_bridge / \
                     requeue_automation / trim_logs), you MAY propose it with guardian_propose_action — \
                     it is recorded as PENDING for operator approval and does NOT run now. Otherwise just \
                     alert.");
            }
            let uid = notify_user_id.clone().unwrap_or_else(|| "system".to_string());
            let turn_start = chrono::Utc::now().timestamp(); // to find THIS turn's proposals
            match agent.run_guardian_turn(&uid, &task).await {
                Ok(text) if !text.trim().is_empty() => {
                    let text = text.trim().to_string();
                    info!("MIRA-Guardian alert ({} triggered): {}", triggered.len(), text);
                    {
                        // Telemetry for the operator status panel.
                        let mut ws = watch_status().write().await;
                        ws.last_alert_at        = Some(chrono::Utc::now().timestamp());
                        ws.last_alert_detectors = triggered.len();
                        ws.last_alert_summary   = Some(text.chars().take(200).collect());
                        ws.alerts_total        += 1;
                    }
                    notifications.send(crate::notifications::Notification {
                        kind:            crate::notifications::NotificationKind::GuardianAlert,
                        conversation_id: None,
                        channel:         Some("web".to_string()),
                        user_id:         notify_user_id.clone(),
                        message:         Some(text.clone()),
                    });
                    if let Some(ref bus) = event_bus {
                        bus.emit(crate::events::Event::new(
                            crate::events::names::WATCHDOG_ALERT,
                            notify_user_id.clone(),
                            serde_json::json!({
                                "severity":       format!("{:?}", snap.worst_level()),
                                "severity_emoji": "⚠️",
                                "module":         "mira-guardian",
                                "message":        text,
                                "fingerprint":    fp,
                                "recent_count":   1,
                                "analyze_link":   "",
                            }),
                        ));
                    }
                    // P3b — push to the operator's last-used *messaging* channel
                    // (web is covered by the NotificationBus above). Capture the
                    // outcome for P4c isolation detection.
                    use crate::companion::dispatcher::DeliveryOutcome;
                    let outcome = if let (Some(recipient), Some(disp)) =
                        (notify_user_id.as_deref(), agent.companion_dispatcher())
                    {
                        let o = disp.deliver_to_user(recipient, &text).await;
                        match &o {
                            DeliveryOutcome::Delivered(ch) =>
                                info!("MIRA-Guardian alert also delivered to channel '{ch}'"),
                            DeliveryOutcome::NoChannel => {}
                            DeliveryOutcome::Failed(ch, e) =>
                                warn!("MIRA-Guardian alert delivery to '{ch}' failed: {e}"),
                        }
                        Some(o)
                    } else { None };

                    // P4c-1 — isolation autonomy DETECTION (dry-run only this slice).
                    // Active mode + a *failed* channel delivery = isolation (§4.5).
                    // For each autonomy-eligible proposal made THIS turn, log + HMAC-
                    // record what the Guardian WOULD do. Real execution is P4c-2.
                    if gmode == GuardianMode::Active
                        && matches!(outcome, Some(DeliveryOutcome::Failed(..)))
                    {
                        if let (Some(store), Some(au)) = (guardian_actions.as_ref(), audit.as_ref()) {
                            let ch = match &outcome {
                                Some(DeliveryOutcome::Failed(c, _)) => c.clone(),
                                _ => String::new(),
                            };
                            let pend = store.list(
                                Some(crate::agent::guardian_actions::GuardianActionStatus::Pending), 20,
                            ).unwrap_or_default();
                            use crate::agent::audit::{guardian_agent_id, AuditEvent};
                            use crate::agent::guardian_actions::{execute_action, GuardianActionStatus};
                            for a in pend.into_iter()
                                .filter(|a| a.created_at >= turn_start && is_autonomy_eligible(a.kind))
                            {
                                let kind_s = a.kind.as_str().to_string();
                                if config.guardian.isolation_dry_run {
                                    // P4c-1 — observe-only: record what we WOULD do.
                                    let detail = format!(
                                        "ISOLATED (channel '{ch}' down): would autonomously {} {} — {} [dry_run=true]",
                                        kind_s, a.target.as_deref().unwrap_or(""), a.reason);
                                    warn!("MIRA-Guardian isolation autonomy (dry-run): {detail}");
                                    let _ = au.record(guardian_agent_id(), AuditEvent::GuardianAction {
                                        action_id: a.id.clone(), action_kind: kind_s,
                                        decision: "autonomous_dry_run".into(), detail: Some(detail) });
                                    continue;
                                }
                                // ── P4c-2 — REAL autonomous execution ──────────────
                                let now = chrono::Utc::now().timestamp();
                                // Blast-radius: per-kind cooldown.
                                if auto_cooldown.get(&kind_s).is_some_and(|&t| now - t < AUTO_COOLDOWN_SECS) {
                                    warn!("MIRA-Guardian: autonomy cooldown active for '{kind_s}' — skip [id={}]", a.id);
                                    continue;
                                }
                                // Grace window — a web decision during grace wins.
                                info!("MIRA-Guardian: ISOLATED — {}s grace before autonomous '{kind_s}' [id={}]",
                                      config.guardian.isolation_grace_secs, a.id);
                                let _ = au.record(guardian_agent_id(), AuditEvent::GuardianAction {
                                    action_id: a.id.clone(), action_kind: kind_s.clone(),
                                    decision: "autonomous_grace".into(),
                                    detail: Some(format!("{}s grace; channel '{ch}' down", config.guardian.isolation_grace_secs)) });
                                tokio::time::sleep(std::time::Duration::from_secs(config.guardian.isolation_grace_secs)).await;
                                let still_pending = store.get(&a.id).ok().flatten()
                                    .map(|x| x.status == GuardianActionStatus::Pending).unwrap_or(false);
                                if !still_pending {
                                    info!("MIRA-Guardian: '{}' decided during grace — autonomy skipped [id={}]", kind_s, a.id);
                                    continue;
                                }
                                let res = execute_action(a.kind, a.target.as_deref(),
                                    automations.as_ref(), channel_manager.as_ref()).await;
                                auto_cooldown.insert(kind_s.clone(), now);
                                match res {
                                    Ok(msg) => {
                                        let _ = store.decide(&a.id, GuardianActionStatus::Executed,
                                            &format!("AUTONOMOUS (isolated): {msg}"));
                                        let _ = au.record(guardian_agent_id(), AuditEvent::GuardianAction {
                                            action_id: a.id.clone(), action_kind: kind_s.clone(),
                                            decision: "auto_executed".into(), detail: Some(msg.clone()) });
                                        warn!("MIRA-Guardian AUTONOMOUS execution [id={}]: {msg}", a.id);
                                        reconcile_queue.push((a.id.clone(), format!(
                                            "MIRA-Guardian: I couldn't reach you (channel '{ch}' was down), so I \
                                             autonomously ran '{kind_s}' — {}. Result: {msg}.", a.reason)));
                                    }
                                    Err(e) => {
                                        let _ = store.decide(&a.id, GuardianActionStatus::Failed,
                                            &format!("AUTONOMOUS failed: {e}"));
                                        let _ = au.record(guardian_agent_id(), AuditEvent::GuardianAction {
                                            action_id: a.id.clone(), action_kind: kind_s.clone(),
                                            decision: "auto_failed".into(), detail: Some(e.clone()) });
                                        warn!("MIRA-Guardian AUTONOMOUS execution FAILED [id={}]: {e}", a.id);
                                        reconcile_queue.push((a.id.clone(), format!(
                                            "MIRA-Guardian: while you were unreachable I tried to autonomously run \
                                             '{kind_s}' but it FAILED: {e}. Please check.")));
                                    }
                                }
                                break; // blast-radius: at most one autonomous action per incident
                            }
                        }
                    }
                    last_fingerprint = Some(fp);
                }
                Ok(_)  => {} // empty output — retry next tick (don't set dedup)
                Err(e) => warn!("MIRA-Guardian watch turn failed: {e}"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_is_stable_and_reserved() {
        let d = definition();
        assert_eq!(d.name, RESERVED_NAME);
        assert_eq!(d.id, RESERVED_ID);
        assert!(d.enabled);
        // Ring-0 only: no action/network tools leak into the built-in allowlist.
        for forbidden in ["shell", "filesystem_write", "settings_set", "backup_restore",
                          "web_fetch", "web_search", "code_run"] {
            assert!(!d.allowed_tools.iter().any(|t| t == forbidden),
                "Guardian must not ship with action/network tool {forbidden}");
        }
    }

    #[test]
    fn fingerprint_is_deterministic() {
        assert_eq!(fingerprint(), fingerprint());
        assert_eq!(fingerprint().len(), 64); // sha256 hex
    }

    #[test]
    fn mode_parses_and_defaults_off() {
        let mut c = MiraConfig::default();
        assert_eq!(mode(&c), GuardianMode::Off); // default
        c.guardian.mode = "monitor".into();
        assert_eq!(mode(&c), GuardianMode::Monitor);
        c.guardian.mode = "ACTIVE".into();
        assert_eq!(mode(&c), GuardianMode::Active);
        c.guardian.mode = "nonsense".into();
        assert_eq!(mode(&c), GuardianMode::Off); // unknown → safe default
    }

    #[test]
    fn loopback_detection() {
        for ok in ["http://127.0.0.1:11434", "http://localhost:1234/v1", "http://[::1]:1234"] {
            assert!(url_is_loopback(ok), "{ok} should be loopback");
        }
        for no in ["http://192.168.70.243:1234/v1", "https://api.openai.com", "http://10.0.0.5:11434"] {
            assert!(!url_is_loopback(no), "{no} should NOT be loopback");
        }
    }

    #[test]
    fn model_check_refuses_cloud_allows_local() {
        let mut c = MiraConfig::default();
        // Default primary provider with no guardian alias: classify whatever it is.
        // Force a cloud provider → refused.
        c.primary_provider = "openrouter".into();
        let r = c.agent.llm_aliases.remove("guardian"); let _ = r;
        let chk = model_check(&c);
        assert!(!chk.allowed && chk.locality == ModelLocality::Cloud);

        // Local loopback ollama → allowed (loopback).
        c.primary_provider = "ollama".into();
        c.providers.ollama.url = "http://127.0.0.1:11434".into();
        let chk = model_check(&c);
        assert!(chk.allowed && chk.locality == ModelLocality::LoopbackLocal);

        // LAN lmstudio → allowed but flagged LAN.
        c.agent.llm_aliases.insert("guardian".into(),
            crate::config::LlmAlias { provider: "lmstudio".into(), model: None });
        c.providers.lmstudio.url = "http://192.168.70.243:1234/v1".into();
        let chk = model_check(&c);
        assert!(chk.allowed && chk.locality == ModelLocality::LanLocal);
    }

    #[test]
    fn no_network_tool_in_allowlist() {
        assert!(allowlist_has_no_network_tool());
    }

    #[test]
    fn propose_tool_only_in_active_mode() {
        // Ring-0 (identity) never includes the action/propose tool.
        assert!(!definition().allowed_tools.iter().any(|t| t == RING1_PROPOSE_TOOL));
        // Monitor/off turns get Ring-0 only; active adds the propose tool.
        assert!(!tools_for_mode(GuardianMode::Monitor).iter().any(|t| t == RING1_PROPOSE_TOOL));
        assert!(tools_for_mode(GuardianMode::Active).iter().any(|t| t == RING1_PROPOSE_TOOL));
        // Fingerprint (identity) is mode-independent — adding the propose tool
        // in active mode must NOT change it.
        let fp = fingerprint();
        assert_eq!(fp, fingerprint());
    }
}
