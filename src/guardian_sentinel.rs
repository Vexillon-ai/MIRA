// SPDX-License-Identifier: AGPL-3.0-or-later

// src/guardian_sentinel.rs
//! **MIRA-Guardian liveness sentinel** — the out-of-process watchdog
//! (`mira guardian-watch`).
//!
//! The co-resident Guardian watch loop runs *inside* the MIRA process, so the
//! one failure it cannot catch is MIRA itself going down — it shares MIRA's
//! fate. This sentinel is a **separate, supervised process** whose only job is
//! to confirm MIRA is alive and, if it isn't, raise a **direct** alarm to the
//! household — without routing through the (down) MIRA.
//!
//! It probes MIRA's unauthenticated `/health` endpoint on an interval. **Any**
//! HTTP response — even a 5xx (provider-down but process-up) — counts as alive;
//! only a transport error (connection refused / timeout) counts as down. After
//! `down_after_failures` consecutive misses it declares MIRA down and delivers a
//! web-push alarm, opened **cold** from the shared data dir (`web_push_vapid.key`
//! + `web_push.db`) so it works with MIRA fully offline. On recovery it clears
//! the alarm. Observe-and-alarm only in this increment — it never restarts or
//! mutates anything (see design-docs/guardian-separate-process.md).
//!
//! The failure threshold is what makes a normal restart *not* alarm: MIRA comes
//! back within one probe window, so the miss count never reaches the threshold.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, error, info, warn};

use crate::config::MiraConfig;
use crate::MiraError;

/// What the sentinel state machine decides after one observation. Pure — the
/// I/O (push, audit, log) happens in the loop based on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentinelAction {
    /// Nothing changed worth acting on.
    Nothing,
    /// MIRA just crossed from up → down (miss count reached the threshold).
    RaiseAlarm,
    /// MIRA just came back after an alarm was raised.
    Recover,
}

/// Rolling liveness state. `observe` is a pure transition so it's unit-tested
/// without any network or clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SentinelState {
    consecutive_failures: u32,
    in_alarm:             bool,
}

impl SentinelState {
    /// Fold one probe result into the state and report the edge action.
    /// - alive → reset the miss counter; if we were alarming, `Recover` once.
    /// - down  → bump the miss counter; when it first reaches `down_after`
    ///   (and we're not already alarming), `RaiseAlarm` once. No repeat alarms
    ///   while it stays down.
    pub fn observe(&mut self, alive: bool, down_after: u32) -> SentinelAction {
        let down_after = down_after.max(1);
        if alive {
            self.consecutive_failures = 0;
            if self.in_alarm {
                self.in_alarm = false;
                return SentinelAction::Recover;
            }
            SentinelAction::Nothing
        } else {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            if self.consecutive_failures >= down_after && !self.in_alarm {
                self.in_alarm = true;
                return SentinelAction::RaiseAlarm;
            }
            SentinelAction::Nothing
        }
    }

    /// Consecutive failed probes so far (for logging).
    pub fn misses(&self) -> u32 { self.consecutive_failures }
}

/// The liveness URL to probe: the explicit `guardian.process.probe_url` if set,
/// else `http://127.0.0.1:<server.port>/health` (the unauthenticated readiness
/// route). Loopback, not `server.host`, since the sentinel is co-located and a
/// `0.0.0.0` bind isn't a dialable address.
pub fn probe_url(config: &MiraConfig) -> String {
    config.guardian.process.probe_url.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("http://127.0.0.1:{}/health", config.server.port))
}

/// One liveness probe. ANY HTTP response (incl. 5xx) means the process is up;
/// only a transport error (connection refused / timeout / DNS) means down.
async fn probe(client: &reqwest::Client, url: &str) -> bool {
    client.get(url).send().await.is_ok()
}

// ── Surface-through-MIRA relay (2c) ──────────────────────────────────────────

/// Path to the shared secret the sentinel presents to MIRA's relay endpoint.
/// Minted by MIRA at boot (0600), read by the sentinel.
pub fn relay_token_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("guardian_relay.token")
}

/// Load the relay token, minting a fresh 256-bit random one (0600) if absent.
/// MIRA calls this at boot so the file exists for the sentinel to read; the
/// pattern mirrors the VAPID keypair mint. `None` only if the write fails.
pub fn load_or_create_relay_token(data_dir: &Path) -> Option<String> {
    let path = relay_token_path(data_dir);
    if let Some(existing) = read_relay_token(data_dir) {
        return Some(existing);
    }
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    let token = hex::encode(buf);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, &token) {
        warn!("guardian relay token: write {} failed: {e}", path.display());
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Some(token)
}

/// Read the relay token if present (the sentinel side — never mints).
pub fn read_relay_token(data_dir: &Path) -> Option<String> {
    std::fs::read_to_string(relay_token_path(data_dir))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Deliver a Guardian message: **prefer routing THROUGH MIRA** (its own voice +
/// full channel reach — Signal/Telegram/email/web/push) when MIRA is reachable;
/// **fall back to a direct web-push** when it isn't (the MIRA-down safety net).
/// Best-effort throughout; a down-alarm naturally takes the push path since MIRA
/// is unreachable, while a recovery notice / future while-up triage goes through
/// MIRA.
async fn deliver(
    config:  &MiraConfig,
    client:  &reqwest::Client,
    webpush: Option<&crate::notifications::web_push::WebPushService>,
    user_id: Option<&str>,
    message: &str,
) {
    let uid = user_id.map(str::trim).filter(|s| !s.is_empty());
    if relay_through_mira(config, client, uid, message).await {
        info!("guardian-watch: delivered through MIRA (full channel reach)");
        return;
    }
    push_notice(webpush, uid, message).await;
}

/// POST the message to MIRA's loopback relay endpoint with the shared-secret
/// bearer token. `true` on a 2xx (MIRA accepted + will deliver it in its voice);
/// `false` when MIRA is unreachable, the token is missing, or the call fails —
/// the caller then falls back to a direct push.
async fn relay_through_mira(
    config: &MiraConfig, client: &reqwest::Client, user_id: Option<&str>, message: &str,
) -> bool {
    let Some(token) = read_relay_token(&config.data_dir_path()) else { return false };
    let url = format!("http://127.0.0.1:{}/internal/guardian/relay", config.server.port);
    let body = serde_json::json!({ "message": message, "user_id": user_id });
    match client.post(&url).bearer_auth(token).json(&body).send().await {
        Ok(r) if r.status().is_success() => true,
        Ok(r)  => { warn!("guardian-watch: MIRA relay returned {} — using direct push", r.status()); false }
        Err(e) => { debug!("guardian-watch: MIRA relay unreachable ({e}) — using direct push"); false }
    }
}

/// Deliver the direct out-of-process alarm/notice via web push (cold from the
/// shared data dir). Best-effort: a missing push target or store just logs.
async fn push_notice(
    webpush:  Option<&crate::notifications::web_push::WebPushService>,
    user_id:  Option<&str>,
    message:  &str,
) {
    let Some(uid) = user_id.map(str::trim).filter(|s| !s.is_empty()) else {
        warn!("guardian-watch: no guardian.process.notify_user_id set — logged only, no push sent");
        return;
    };
    let Some(wp) = webpush else {
        warn!("guardian-watch: web-push unavailable (no keys/subscriptions in data dir) — logged only");
        return;
    };
    let notif = crate::notifications::Notification {
        kind:            crate::notifications::NotificationKind::GuardianAlert,
        conversation_id: None,
        channel:         None,
        user_id:         Some(uid.to_string()),
        message:         Some(message.to_string()),
        category:        None,
    };
    match wp.send_to_user(uid, &notif.to_envelope()).await {
        Ok(n)  => info!("guardian-watch: alarm delivered to {n} device(s) for '{uid}'"),
        Err(e) => error!("guardian-watch: web-push delivery failed for '{uid}': {e}"),
    }
}

/// The base alarm message on MIRA going down. [`down_message`] appends the
/// last-known health summary when one is available.
const DOWN_MSG: &str =
    "MIRA appears to be down — the assistant isn't responding. Someone may need to check the server.";
/// The reassurance message on recovery.
const RECOVER_MSG: &str = "MIRA is back up — the assistant is responding again.";

/// Humanize an age in seconds for alarm text: `2h` / `45m` / `30s`.
fn humanize_age(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 3600 { format!("{}h", s / 3600) }
    else if s >= 60 { format!("{}m", s / 60) }
    else { format!("{s}s") }
}

/// A one-line summary of MIRA's last-known health for the down-alarm, or `None`
/// when there's no snapshot. Pure — unit-tested. When non-green, lists up to 4
/// triggered detectors (sorted, `+N more` beyond that).
fn health_summary(snap: Option<&crate::health::HealthSnapshot>, now: i64) -> Option<String> {
    let snap = snap?;
    let ago = humanize_age(now - snap.taken_at);
    if matches!(snap.worst_level(), crate::health::HealthLevel::Green) {
        return Some(format!("Its last health check {ago} ago was all-green."));
    }
    let mut det: Vec<&str> = snap.reports.iter()
        .filter(|r| !matches!(r.level, crate::health::HealthLevel::Green))
        .map(|r| r.name.as_str())
        .collect();
    det.sort_unstable();
    let shown = det.len().min(4);
    let more  = det.len() - shown;
    let list  = det[..shown].join(", ");
    let tail  = if more > 0 { format!(" (+{more} more)") } else { String::new() };
    Some(format!("Its last health check {ago} ago showed {:?}: {list}{tail}.", snap.worst_level()))
}

/// The MIRA-down alarm text, optionally enriched with the last-known health.
fn down_message(health: Option<&str>) -> String {
    match health {
        Some(h) => format!("{DOWN_MSG} {h}"),
        None    => DOWN_MSG.to_string(),
    }
}

/// Hard bound on the standalone triage tool loop (a few generate↔tool rounds). A
/// down-alarm was already detected over several probe intervals, so a short
/// reasoning delay is fine — but never let a slow/unreachable model or a stuck
/// tool stall the alarm: on timeout we fall back to the deterministic message.
const LLM_TRIAGE_TIMEOUT_SECS: u64 = 45;

/// Bound on building the optional `recall_history` tool (opening the history DB +
/// loading the embedding model). Kept short so a slow/absent embedding stack
/// never eats the triage budget — we just proceed without recall.
const RECALL_INIT_TIMEOUT_SECS: u64 = 12;

/// Best-effort construction of the read-only `recall_history` tool for the
/// sentinel: opens the history DB + a semantic `MemorySystem` (default
/// "internal" embedding — local, standalone) from the shared data dir. `None`
/// when either won't build (e.g. embeddings disabled/unavailable), so the caller
/// simply omits recall from the turn.
async fn build_recall_tool(config: &MiraConfig) -> Option<crate::tools::recall::RecallHistoryTool> {
    let data_dir = config.data_dir_path();
    let history  = crate::history::HistoryStore::open(&data_dir.join("history.db")).ok()?;
    let memory   = crate::memory::MemorySystem::new_from_embedding_config(
        data_dir.join("memory.db"), &config.memory,
    ).await.ok()?;
    Some(crate::tools::recall::RecallHistoryTool::new(Arc::new(history), Arc::new(memory)))
}

/// The user-turn prompt for standalone triage. Pure — unit-tested. The last-known
/// health is pre-fed so even a non-tool-calling model has context; the model MAY
/// also call `guardian_inspect` for the full snapshot + logs and `mira_help` for
/// how MIRA works.
fn triage_prompt(health: &str, misses: u32, url: &str) -> String {
    let health = if health.trim().is_empty() {
        "unknown (no recent health snapshot on disk)"
    } else {
        health.trim()
    };
    format!(
        "You are watching over MIRA — the household's assistant — and it has just become \
         unreachable: {misses} consecutive failed health checks at {url}. Its last known health \
         was: {health} You may call `guardian_inspect` to read the full latest health snapshot, \
         active degradations, and recent logs, and `mira_help` to check how MIRA works — use them \
         only if they'd sharpen your assessment. Then, in one or two calm, clear sentences, tell \
         the household what appears to be wrong and the single most useful next step to get MIRA \
         back. Do not speculate beyond the evidence. Begin with \"MIRA-Guardian:\"."
    )
}

/// The user-turn prompt for a while-MIRA-is-up health triage (2d, `owns_watch`).
/// Pure — the model is told a health problem was flagged (deterministic summary
/// pre-fed) and may pull full detail via the tools.
fn health_watch_prompt(health: &str) -> String {
    let health = if health.trim().is_empty() {
        "a non-green health state (details unavailable)"
    } else {
        health.trim()
    };
    format!(
        "MIRA — the household's assistant — is running, but its health check just flagged a \
         problem: {health} You may call `guardian_inspect` for the full snapshot + active \
         degradations + recent logs, `mira_help` for how MIRA works, and `recall_history` for \
         relevant context. Then, in one or two calm, clear sentences, tell the household what's \
         wrong and the single most useful next step. Do not speculate beyond the evidence. Begin \
         with \"MIRA-Guardian:\"."
    )
}

/// Fingerprint of a snapshot's triggered (non-green) detectors — sorted names,
/// joined. Empty when all-green. Dedups while-up health triage so an unchanged
/// non-green state doesn't re-alert. Mirrors the co-resident loop's fingerprint.
fn health_fingerprint(snap: &crate::health::HealthSnapshot) -> String {
    let mut t: Vec<&str> = snap.reports.iter()
        .filter(|r| !matches!(r.level, crate::health::HealthLevel::Green))
        .map(|r| r.name.as_str())
        .collect();
    t.sort_unstable();
    t.join(",")
}

/// Increment 2b(i) — run a REAL Guardian triage turn **in the sentinel process**,
/// independent of the (down) MIRA. Builds the Guardian's own local provider from
/// config (fail-closed local-only per the tier's `model_check`) and drives MIRA's
/// actual tool loop with a minimal read-only registry, so the model can actively
/// pull the full health snapshot + logs (`guardian_inspect`) and consult the docs
/// (`mira_help`) — reusing the battle-tested loop, no `AgentCore` needed. The
/// last-known health is also pre-fed, so a model that doesn't tool-call still has
/// context (the loop then degrades to a single generation). Returns `None`
/// (→ caller falls back to the deterministic alarm) when no local model is
/// configured/allowed, the provider won't build, or the call errors/times out —
/// so this never weakens the guarantee that a down-alarm still fires.
async fn llm_triage(
    config:       &MiraConfig,
    health_store: Option<&Arc<crate::health::store::HealthStore>>,
    task:         &str,
) -> Option<String> {
    use crate::agent::guardian::{self, GuardianTier};
    // Fail-closed: the triage tier must resolve to a LOCAL model, or we don't
    // reach for it at all (never egress what we're guarding).
    let chk = guardian::model_check_for(config, GuardianTier::Triage);
    if !chk.allowed {
        warn!("guardian-watch: standalone triage skipped (fail-closed) — {}", chk.reason);
        return None;
    }
    let (prov, model) = guardian::tier_model(config, GuardianTier::Triage);
    let provider = match crate::agent::named_agent::build_provider_for_alias(config, &prov, model.as_deref()) {
        Ok(p)  => p,
        Err(e) => { warn!("guardian-watch: triage provider build failed: {e}"); return None; }
    };

    // A minimal, read-only tool registry — the Guardian's Ring-0 diagnostic
    // tools, opened out-of-process from the shared data dir. (recall_history is
    // deferred: it needs the async embedding stack. DegradationTracker is empty
    // out-of-process — the live state is in the down MIRA — so pass None.)
    let mut registry = crate::tools::ToolRegistry::new();
    registry.register(crate::tools::guardian_inspect::GuardianInspectTool::new(
        health_store.cloned(), None, Some(config.log_file_path()),
    ));
    registry.register(crate::tools::mira_help::MiraHelpTool);
    let mut allowed = vec![ "guardian_inspect".to_string(), "mira_help".to_string() ];
    // recall_history (2b(i)-3): best-effort — needs the history DB + a semantic
    // embedding provider. Bounded init so a slow/absent embedding stack can't
    // stall the alarm; if it won't build we just don't offer recall.
    match tokio::time::timeout(std::time::Duration::from_secs(RECALL_INIT_TIMEOUT_SECS), build_recall_tool(config)).await {
        Ok(Some(recall)) => { registry.register(recall); allowed.push("recall_history".to_string()); }
        Ok(None) => warn!("guardian-watch: recall_history unavailable (history/embedding init failed) — proceeding without it"),
        Err(_)   => warn!("guardian-watch: recall_history init timed out — proceeding without it"),
    }
    let tools = Arc::new(registry);

    let mut messages = vec![
        crate::types::ChatMessage::system(guardian::definition().system_prompt),
        crate::types::ChatMessage::user(task.to_string()),
    ];
    let opts = crate::types::GenerationOptions { max_tokens: Some(400), ..Default::default() };
    // recall_history scopes results per-user via a trusted injected `_user_id`;
    // supply the configured notify user so recall (when present) is usable.
    let mut inject = serde_json::Map::new();
    if let Some(uid) = config.guardian.process.notify_user_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        inject.insert("_user_id".to_string(), serde_json::Value::String(uid.to_string()));
    }

    // The loop streams events; we only want the returned final text, so drain the
    // channel in the background (an absent receiver would drop events).
    let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::agent::stream::StreamEvent>(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let fut = crate::agent::tool_loop::run_tool_loop_with_context(
        &provider, &tools, &mut messages, &opts,
        &crate::agent::tool_loop::ToolMode::Auto, 4, &tx,
        Some(allowed.as_slice()), &inject, crate::agent::tool_loop::ToolEventCtx::NONE, None, None,
    );
    let out = match tokio::time::timeout(std::time::Duration::from_secs(LLM_TRIAGE_TIMEOUT_SECS), fut).await {
        Ok(Ok((text, _usage))) if !text.trim().is_empty() => Some(text.trim().to_string()),
        Ok(Ok(_))  => None,
        Ok(Err(e)) => { warn!("guardian-watch: standalone triage tool-loop failed: {e}"); None }
        Err(_)     => { warn!("guardian-watch: standalone triage timed out after {LLM_TRIAGE_TIMEOUT_SECS}s"); None }
    };
    drop(tx);          // close the channel so the drain task finishes
    let _ = drain.await;
    out
}

/// Run the sentinel loop. Blocks (a long-running process) until the task is
/// cancelled / the process exits. A no-op (returns immediately) when
/// `guardian.process.enabled` is false.
pub async fn run(config: Arc<MiraConfig>) -> Result<(), MiraError> {
    let pc = &config.guardian.process;
    if !pc.enabled {
        // Park instead of exiting. This process is meant to run under a
        // supervisor with `Restart=always`, so an immediate `exit(0)` would spin
        // the unit in a tight restart loop. Idle quietly instead; the operator
        // enables `guardian.process.enabled` and restarts this unit to begin
        // watching. (A manual `mira guardian-watch` here just idles — Ctrl-C to
        // stop.)
        warn!("guardian-watch: guardian.process.enabled is false — idling (enable it and restart this unit to begin watching)");
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    }
    let data_dir   = config.data_dir_path();
    let url        = probe_url(&config);
    let interval   = Duration::from_secs(pc.probe_interval_secs.max(5));
    let down_after = pc.down_after_failures.max(1);
    let notify_uid = pc.notify_user_id.clone();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| MiraError::ConfigError(format!("guardian-watch: http client: {e}")))?;

    // Cold web-push service, opened from the shared data dir — no running MIRA
    // needed. Best-effort: if the keys/DB aren't there yet, alarms log only.
    let webpush = crate::notifications::web_push::WebPushService::open(
        &data_dir,
        &crate::notifications::web_push::service_path(&data_dir),
        None,
    ).map_err(|e| warn!("guardian-watch: web-push unavailable: {e}")).ok();

    // Read-only handle on MIRA's health snapshots, so a MIRA-down alarm can
    // carry the last-known health state ("… last check 2h ago showed Red: …").
    // Best-effort: absent/locked DB just means a plainer alarm. (2a — the
    // out-of-process health-read plumbing the future triage turn builds on.)
    let health = crate::health::store::HealthStore::open(&data_dir.join("health.db"))
        .map_err(|e| warn!("guardian-watch: health store unavailable (alarms won't include last-known health): {e}"))
        .ok()
        .map(Arc::new); // Arc so the standalone triage tool loop can share it.

    info!(
        "guardian-watch: probing {url} every {}s; MIRA declared down after {down_after} consecutive misses; \
         push target = {}",
        interval.as_secs(),
        notify_uid.as_deref().filter(|s| !s.is_empty()).unwrap_or("(none — logs only)"),
    );

    let mut state  = SentinelState::default();
    // Dedup for the while-up health watch (2d) — the triggered-detector
    // fingerprint of the last health state we triaged.
    let mut last_health_fp: Option<String> = None;
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let alive = probe(&client, &url).await;
        match state.observe(alive, down_after) {
            SentinelAction::RaiseAlarm => {
                // Enrich the alarm with MIRA's last-known health, if we can read it.
                let now  = chrono::Utc::now().timestamp();
                let hsum = health.as_ref()
                    .and_then(|h| h.latest().ok().flatten())
                    .and_then(|snap| health_summary(Some(&snap), now));
                // 2b(i): try a REAL standalone Guardian triage (own local model,
                // out-of-process). Fall back to the deterministic alarm if no
                // local model is configured/reachable — the alarm always fires.
                let msg = match llm_triage(&config, health.as_ref(),
                    &triage_prompt(hsum.as_deref().unwrap_or(""), state.misses(), &url)).await
                {
                    Some(t) => { info!("guardian-watch: delivering standalone-triaged alarm"); t }
                    None    => down_message(hsum.as_deref()),
                };
                error!("guardian-watch: MIRA DOWN ({} failed probes of {url}) — {msg}", state.misses());
                // Prefer through-MIRA; a truly-down MIRA fails the relay → direct push.
                deliver(&config, &client, webpush.as_ref(), notify_uid.as_deref(), &msg).await;
                // Audit the down-alarm. Safe to write the shared HMAC chain here:
                // MIRA is confirmed down, so it isn't concurrently writing.
                audit_down(&data_dir, &url, state.misses(), hsum.as_deref());
            }
            SentinelAction::Recover => {
                info!("guardian-watch: MIRA recovered — responding again at {url}");
                // MIRA is back up → this goes THROUGH MIRA (its voice, all channels).
                deliver(&config, &client, webpush.as_ref(), notify_uid.as_deref(), RECOVER_MSG).await;
                // No audit write on recovery: MIRA is back up and writing the
                // chain itself; a concurrent sentinel write could interleave.
            }
            SentinelAction::Nothing => {
                if !alive {
                    warn!("guardian-watch: probe miss {}/{down_after} for {url}", state.misses());
                }
            }
        }

        // 2d — when the sentinel OWNS the watch and MIRA is UP, it also triages
        // non-green health snapshots (surfacing THROUGH MIRA), taking over the
        // co-resident loop's job (which stands down under the same flag). Deduped
        // by triggered-detector fingerprint so an unchanged non-green state
        // doesn't re-alert; an all-green snapshot clears the dedup.
        if alive && config.guardian.process.owns_watch {
            if let Some(snap) = health.as_ref().and_then(|h| h.latest().ok().flatten()) {
                let fp = health_fingerprint(&snap);
                if fp.is_empty() {
                    last_health_fp = None;
                } else if last_health_fp.as_deref() != Some(fp.as_str()) {
                    last_health_fp = Some(fp.clone());
                    let now = chrono::Utc::now().timestamp();
                    let summary = health_summary(Some(&snap), now).unwrap_or_default();
                    info!("guardian-watch: non-green health while MIRA up — triaging [{fp}]");
                    let msg = llm_triage(&config, health.as_ref(), &health_watch_prompt(&summary)).await
                        .unwrap_or_else(|| format!("MIRA-Guardian: MIRA flagged a health issue. {summary}"));
                    deliver(&config, &client, webpush.as_ref(), notify_uid.as_deref(), &msg).await;
                }
            }
        }
    }
}

/// Append a tamper-evident audit record for a down-alarm. Best-effort + isolated
/// so an audit failure never stops the watch. Only called when MIRA is down
/// (single-writer, so the HMAC chain stays ordered).
fn audit_down(data_dir: &Path, url: &str, misses: u32, health: Option<&str>) {
    let Ok(store) = crate::agent::audit::AuditStore::open(&data_dir.join("agent_audit.db")) else {
        return;
    };
    let mut detail = format!("{misses} consecutive failed probes of {url}");
    if let Some(h) = health {
        detail.push_str(" — ");
        detail.push_str(h);
    }
    let _ = store.record(
        crate::agent::audit::guardian_agent_id(),
        None,
        crate::agent::audit::AuditEvent::GuardianAction {
            action_id:   format!("sentinel-down-{url}"),
            action_kind: "sentinel_liveness".to_string(),
            decision:    "mira_down".to_string(),
            detail:      Some(detail),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machine_alarms_once_then_recovers_once() {
        let mut s = SentinelState::default();
        // Below threshold — no alarm.
        assert_eq!(s.observe(false, 3), SentinelAction::Nothing);
        assert_eq!(s.observe(false, 3), SentinelAction::Nothing);
        // Third miss crosses the threshold → alarm exactly once.
        assert_eq!(s.observe(false, 3), SentinelAction::RaiseAlarm);
        // Still down → no repeat alarms.
        assert_eq!(s.observe(false, 3), SentinelAction::Nothing);
        assert_eq!(s.observe(false, 3), SentinelAction::Nothing);
        // Back up → recover exactly once.
        assert_eq!(s.observe(true, 3), SentinelAction::Recover);
        assert_eq!(s.observe(true, 3), SentinelAction::Nothing);
    }

    #[test]
    fn brief_blip_under_threshold_never_alarms() {
        // A normal restart: two misses then recovery, threshold 3 → no alarm,
        // no recovery edge (we never entered the alarm state).
        let mut s = SentinelState::default();
        assert_eq!(s.observe(false, 3), SentinelAction::Nothing);
        assert_eq!(s.observe(false, 3), SentinelAction::Nothing);
        assert_eq!(s.observe(true, 3),  SentinelAction::Nothing);
        assert_eq!(s.misses(), 0);
    }

    #[test]
    fn threshold_of_one_alarms_immediately() {
        let mut s = SentinelState::default();
        assert_eq!(s.observe(false, 1), SentinelAction::RaiseAlarm);
        assert_eq!(s.observe(true, 1),  SentinelAction::Recover);
    }

    fn rep(name: &str, level: crate::health::HealthLevel) -> crate::health::DetectorReport {
        crate::health::DetectorReport {
            name: name.to_string(), level, message: String::new(), value: None,
            payload: serde_json::Value::Null, auto_action_eligible: false, analytics: None,
        }
    }

    #[test]
    fn health_summary_lists_triggered_detectors_worst_first() {
        use crate::health::{HealthSnapshot, HealthLevel, DetectorReport};
        let snap = HealthSnapshot {
            taken_at: 1000, duration_ms: 0,
            reports: vec![
                DetectorReport::green("proc.ok", "fine"),
                rep("db.integrity", HealthLevel::Red),
                rep("disk.free", HealthLevel::Yellow),
            ],
        };
        let s = health_summary(Some(&snap), 1000 + 7200).unwrap(); // 2h later
        assert!(s.contains("2h ago"), "{s}");
        assert!(s.contains("Red"), "worst level shown: {s}");
        assert!(s.contains("db.integrity") && s.contains("disk.free"));
        assert!(!s.contains("proc.ok"), "green detectors omitted: {s}");
    }

    #[test]
    fn health_summary_all_green_and_none() {
        use crate::health::{HealthSnapshot, DetectorReport};
        let snap = HealthSnapshot {
            taken_at: 500, duration_ms: 0,
            reports: vec![DetectorReport::green("a", "")],
        };
        let s = health_summary(Some(&snap), 500 + 90).unwrap(); // 90s → "1m"
        assert!(s.contains("all-green"), "{s}");
        assert!(s.contains("1m ago"), "{s}");
        assert_eq!(health_summary(None, 0), None);
    }

    #[test]
    fn humanize_age_units() {
        assert_eq!(humanize_age(30), "30s");
        assert_eq!(humanize_age(90), "1m");
        assert_eq!(humanize_age(7200), "2h");
        assert_eq!(humanize_age(-5), "0s");
    }

    #[test]
    fn down_message_enriches_when_health_present() {
        assert!(down_message(None).starts_with("MIRA appears to be down"));
        assert_eq!(down_message(None), DOWN_MSG);
        let m = down_message(Some("Its last health check 2h ago showed Red: db.integrity."));
        assert!(m.contains("MIRA appears to be down") && m.contains("Red: db.integrity"));
    }

    #[test]
    fn triage_prompt_includes_context_and_handles_empty() {
        let p = triage_prompt(
            "Its last health check 2h ago showed Red: db.integrity.", 3,
            "http://127.0.0.1:8087/health",
        );
        assert!(p.contains("MIRA-Guardian"), "{p}");
        assert!(p.contains("3 consecutive"));
        assert!(p.contains("http://127.0.0.1:8087/health"));
        assert!(p.contains("db.integrity"));
        // The model is offered the read-only diagnostic tools.
        assert!(p.contains("guardian_inspect") && p.contains("mira_help"));
        // Empty health → a placeholder, never a dangling "was: ".
        let p2 = triage_prompt("   ", 1, "http://x/health");
        assert!(p2.contains("no recent health snapshot"), "{p2}");
    }

    #[test]
    fn health_fingerprint_sorts_and_ignores_green() {
        use crate::health::{HealthSnapshot, HealthLevel, DetectorReport};
        let snap = HealthSnapshot { taken_at: 0, duration_ms: 0, reports: vec![
            DetectorReport::green("z.ok", ""),
            rep("disk.free", HealthLevel::Yellow),
            rep("db.integrity", HealthLevel::Red),
        ]};
        assert_eq!(health_fingerprint(&snap), "db.integrity,disk.free"); // sorted, green omitted
        let green = HealthSnapshot { taken_at: 0, duration_ms: 0, reports: vec![DetectorReport::green("a", "")] };
        assert_eq!(health_fingerprint(&green), ""); // all-green → empty (clears dedup)
    }

    #[test]
    fn health_watch_prompt_mentions_tools_and_handles_empty() {
        let p = health_watch_prompt("Its last check showed Red: db.integrity.");
        assert!(p.contains("MIRA-Guardian") && p.contains("running") && p.contains("db.integrity"), "{p}");
        assert!(p.contains("guardian_inspect"));
        assert!(health_watch_prompt("  ").contains("details unavailable"));
    }

    #[test]
    fn probe_url_defaults_to_loopback_health() {
        let mut c = MiraConfig::default();
        c.server.port = 8087;
        assert_eq!(probe_url(&c), "http://127.0.0.1:8087/health");
        // Explicit override wins; blank is ignored.
        c.guardian.process.probe_url = Some("   ".into());
        assert_eq!(probe_url(&c), "http://127.0.0.1:8087/health");
        c.guardian.process.probe_url = Some("http://mira.local:9000/healthz".into());
        assert_eq!(probe_url(&c), "http://mira.local:9000/healthz");
    }
}
