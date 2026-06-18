// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/health_dashboard.rs
//! 0.107.0 — admin dashboard for the system_audit subsystem.
//!
//! Read endpoints (latest snapshot, history, incidents, ip-bans, config)
//! plus three writes: per-signal policy upserts, IP-ban lifts, and
//! force-fire of the audit. All admin-only — health data exposes
//! detector payloads (file paths, queue depths, IP addresses) that
//! aren't appropriate for non-admin visibility.
//!
//! There's already a `health` handler module — that's the basic
//! liveness probe at `/health`. This module owns the richer
//! `/api/health/*` admin surface.

use std::sync::Arc;

use axum::{
    extract::{Json, Path, Query},
    http::StatusCode,
    response::IntoResponse,
    Extension,
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::auth::{AuthUser, Role};
use crate::automations::AutomationsStore;
use crate::health::store::HealthStore;
use crate::health::ActionPolicy;

fn err(status: StatusCode, msg: &str) -> axum::response::Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

fn admin_only(caller: &AuthUser) -> Option<axum::response::Response> {
    if caller.0.role != Role::Admin {
        return Some(err(StatusCode::FORBIDDEN, "admin only"));
    }
    None
}

// ── GET /api/health/snapshot ────────────────────────────────────────────────

pub async fn get_snapshot(
    caller: AuthUser,
    Extension(store): Extension<Arc<HealthStore>>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    match store.latest() {
        Ok(Some(s)) => (StatusCode::OK, Json(s)).into_response(),
        Ok(None)    => err(StatusCode::NOT_FOUND, "no snapshot recorded yet"),
        Err(e)      => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    }
}

// ── GET /api/health/degradations ────────────────────────────────────────────
//
// Live (not snapshot-based) view of subsystems currently on a degraded
// fallback path — TTS → Piper, embedding server → internal, etc. Drives the
// timely health banner; the hourly `subsystem.degraded` detector is the
// snapshot-integrated counterpart.
pub async fn list_degradations(
    caller: AuthUser,
    Extension(tracker): Extension<Arc<crate::health::degradation::DegradationTracker>>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    (StatusCode::OK, Json(tracker.active())).into_response()
}

// ── GET /api/health/history?hours=24 ────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct HistoryQuery {
    /// How far back to look. Default 24h, hard cap 30 days (matches
    /// the snapshot retention window).
    #[serde(default)]
    pub hours: Option<u64>,
}

pub async fn get_history(
    caller: AuthUser,
    Extension(store): Extension<Arc<HealthStore>>,
    Query(q): Query<HistoryQuery>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    let hours = q.hours.unwrap_or(24).min(30 * 24);
    let since = chrono::Utc::now().timestamp() - (hours as i64) * 3600;
    match store.list_summaries_since(since) {
        Ok(rows) => (StatusCode::OK, Json(rows)).into_response(),
        Err(e)   => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    }
}

// ── GET /api/health/incidents?limit=50 ──────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct IncidentsQuery {
    /// Hard cap 500. Default 50.
    #[serde(default)]
    pub limit: Option<usize>,
}

pub async fn list_incidents(
    caller: AuthUser,
    Extension(automations): Extension<Arc<AutomationsStore>>,
    Query(q): Query<IncidentsQuery>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    let limit = q.limit.unwrap_or(50).min(500);
    // Show incidents for the calling admin. The seeded watchdog
    // notification subscription targets one admin user; in practice
    // there's only one, so this matches the routed alerts.
    match automations.list_watchdog_incidents(&caller.0.id, limit) {
        Ok(rows) => {
            // Filter to system_health source only — log/db-derived
            // watchdog incidents have their own UI surface in the
            // future. Keeping the dashboard scoped avoids noise.
            let filtered: Vec<_> = rows.into_iter()
                .filter(|i| i.source == "system_health")
                .collect();
            (StatusCode::OK, Json(filtered)).into_response()
        }
        Err(e)   => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    }
}

// ── GET /api/health/config ──────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct DetectorConfigEntry {
    pub detector_name: String,
    pub policy:        String,
    pub note:          Option<String>,
    pub updated_at:    Option<i64>,
    pub updated_by:    Option<String>,
    /// True when this entry comes from the database; false when it's
    /// a default (NotifyOnly) implied by the detector existing without
    /// an override row.
    pub overridden:    bool,
    /// 0.109.0 — when in the future, the detector is currently muted
    /// and will revert to `policy` at this unix timestamp. None when
    /// no snooze is active.
    pub snooze_until:  Option<i64>,
}

pub async fn list_config(
    caller: AuthUser,
    Extension(store): Extension<Arc<HealthStore>>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    let overrides = store.list_signal_configs().unwrap_or_default();
    let by_name: std::collections::HashMap<String, _> =
        overrides.into_iter().map(|r| (r.detector_name.clone(), r)).collect();

    let detectors = crate::health::detectors::default_registry();
    let entries: Vec<DetectorConfigEntry> = detectors.iter().map(|d| {
        let name = d.name();
        match by_name.get(name) {
            Some(row) => DetectorConfigEntry {
                detector_name: name.to_string(),
                policy:        row.policy.clone(),
                note:          row.note.clone(),
                updated_at:    Some(row.updated_at),
                updated_by:    Some(row.updated_by.clone()),
                overridden:    true,
                snooze_until:  row.snooze_until,
            },
            None => DetectorConfigEntry {
                detector_name: name.to_string(),
                policy:        "notify_only".into(),
                note:          None,
                updated_at:    None,
                updated_by:    None,
                overridden:    false,
                snooze_until:  None,
            },
        }
    }).collect();
    (StatusCode::OK, Json(entries)).into_response()
}

// ── PUT /api/health/config ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct UpsertConfigBody {
    pub detector_name: String,
    /// `"disabled"` | `"notify_only"` | `"auto_cleanup"` (matches the
    /// `ActionPolicy` discriminator). `"notify_only"` is special-cased
    /// when no snooze is set: it deletes any override row, returning
    /// to default behaviour. With a snooze, the override is kept so
    /// the snooze field has a row to live on.
    pub policy:        String,
    #[serde(default)]
    pub note:          Option<String>,
    /// 0.109.0 — optional snooze in seconds from now. `Some(0)` clears
    /// any existing snooze; `Some(N)` snoozes for N seconds. `None`
    /// preserves whatever snooze state is already on the row.
    #[serde(default)]
    pub snooze_secs:   Option<i64>,
}

pub async fn upsert_config(
    caller: AuthUser,
    Extension(store): Extension<Arc<HealthStore>>,
    Json(body): Json<UpsertConfigBody>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    let policy = match body.policy.as_str() {
        "disabled"     => ActionPolicy::Disabled,
        "notify_only"  => ActionPolicy::NotifyOnly,
        "auto_cleanup" => ActionPolicy::AutoCleanup,
        other          => return err(
            StatusCode::BAD_REQUEST, &format!("unknown policy: {other}"),
        ),
    };
    // Validate detector_name exists — typo-protection for the API.
    let known: std::collections::HashSet<&'static str> =
        crate::health::detectors::default_registry().iter().map(|d| d.name()).collect();
    if !known.contains(body.detector_name.as_str()) {
        return err(
            StatusCode::BAD_REQUEST,
            &format!("unknown detector: {}", body.detector_name),
        );
    }
    // Resolve the snooze: Some(N>0) → unix-time N seconds from now;
    // Some(0) → clear; None with policy=notify_only → no snooze info,
    // so we delete the row entirely.
    let snooze_until: Option<i64> = match body.snooze_secs {
        Some(n) if n > 0 => Some(chrono::Utc::now().timestamp() + n),
        _                => None,
    };

    // notify_only without a snooze = revert to default: clear the row.
    if matches!(policy, ActionPolicy::NotifyOnly) && snooze_until.is_none() {
        if let Err(e) = store.clear_signal_config(&body.detector_name) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}"));
        }
        return (StatusCode::OK, Json(serde_json::json!({"reset": true}))).into_response();
    }
    if let Err(e) = store.upsert_signal_config(
        &body.detector_name, policy, body.note.as_deref(), &caller.0.id, snooze_until,
    ) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}"));
    }
    (StatusCode::OK, Json(serde_json::json!({"saved": true, "snooze_until": snooze_until}))).into_response()
}

// ── POST /api/health/run-now ────────────────────────────────────────────────

/// Force the next system_audit fire to happen immediately. Implemented
/// by setting `next_run_at` on the seeded `heartbeat.system_audit`
/// schedule to now; the existing dispatcher tick picks it up. Cleaner
/// than calling SystemAudit::run directly because it goes through the
/// same code path as scheduled fires (run_history row included).
pub async fn run_now(
    caller: AuthUser,
    Extension(automations): Extension<Arc<AutomationsStore>>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    match automations.force_schedule_next_run("heartbeat.system_audit") {
        Ok(true)  => (StatusCode::ACCEPTED, Json(serde_json::json!({"queued": true}))).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "heartbeat.system_audit not seeded"),
        Err(e)    => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    }
}

// ── GET /api/health/ip-bans ─────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct IpBanRow {
    pub ip:           String,
    pub banned_until: i64,
    pub reason:       Option<String>,
}

pub async fn list_ip_bans(
    caller: AuthUser,
    Extension(auth_db): Extension<Arc<crate::auth::AuthDb>>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    match auth_db.list_active_bans() {
        Ok(rows) => {
            let out: Vec<IpBanRow> = rows.into_iter()
                .map(|(ip, until, reason)| IpBanRow { ip, banned_until: until, reason })
                .collect();
            (StatusCode::OK, Json(out)).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    }
}

// ── POST /api/health/ip-bans/{ip}/lift ──────────────────────────────────────

pub async fn lift_ip_ban(
    caller: AuthUser,
    Extension(auth_db): Extension<Arc<crate::auth::AuthDb>>,
    Path(ip): Path<String>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    match auth_db.unban_ip(&ip) {
        Ok(true)  => (StatusCode::OK, Json(serde_json::json!({"lifted": true}))).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "no active ban for that IP"),
        Err(e)    => {
            warn!("lift_ip_ban({ip}) failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}"))
        }
    }
}

// ── GET /metrics — Prometheus exposition ────────────────────────────────────

/// Plain-text Prometheus format. Admin-gated: detector values may
/// expose paths/identifiers that aren't appropriate for a public scrape
/// endpoint. Operators who want public scraping should plumb through a
/// dedicated reverse-proxy with its own auth.
///
/// Exposed series:
///   - `mira_health_detector_value{name="...", level="..."}` — last value per detector
///   - `mira_health_audit_duration_ms` — last audit's wall-clock duration
///   - `mira_health_detectors_triggered` — count of non-green in last audit
///   - `mira_health_audit_total` — counter (snapshot rows in window)
pub async fn prometheus_metrics(
    caller: AuthUser,
    Extension(store): Extension<Arc<HealthStore>>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    let snap = match store.latest() {
        Ok(Some(s)) => s,
        Ok(None)    => return (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
            "# no snapshot recorded yet\n".to_string(),
        ).into_response(),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    };

    let mut out = String::new();
    out.push_str("# HELP mira_health_detector_value Most recent reading from each health detector\n");
    out.push_str("# TYPE mira_health_detector_value gauge\n");
    let level_int = |l: crate::health::HealthLevel| -> i64 {
        match l { crate::health::HealthLevel::Green => 0, crate::health::HealthLevel::Yellow => 1, crate::health::HealthLevel::Red => 2 }
    };
    for r in &snap.reports {
        let v = r.value.unwrap_or(level_int(r.level) as f64);
        // Prometheus label values must escape `\` and `"`. Detector
        // names are dotted ASCII so they don't need escaping in
        // practice, but be defensive.
        let safe_name = r.name.replace('\\', "\\\\").replace('"', "\\\"");
        out.push_str(&format!(
            "mira_health_detector_value{{name=\"{safe_name}\",level=\"{}\"}} {v}\n",
            r.level.as_str(),
        ));
    }
    out.push_str("# HELP mira_health_audit_duration_ms Wall-clock duration of the most recent audit\n");
    out.push_str("# TYPE mira_health_audit_duration_ms gauge\n");
    out.push_str(&format!("mira_health_audit_duration_ms {}\n", snap.duration_ms));
    out.push_str("# HELP mira_health_detectors_triggered Count of non-green detectors in the most recent audit\n");
    out.push_str("# TYPE mira_health_detectors_triggered gauge\n");
    out.push_str(&format!("mira_health_detectors_triggered {}\n", snap.triggered_count()));

    // 24h counter using snapshot summaries — Prometheus consumers
    // would normally graph these as a rate().
    if let Ok(summaries) = store.list_summaries_since(chrono::Utc::now().timestamp() - 24 * 3600) {
        out.push_str("# HELP mira_health_audits_total Audits run in the last 24h\n");
        out.push_str("# TYPE mira_health_audits_total counter\n");
        out.push_str(&format!("mira_health_audits_total {}\n", summaries.len()));
        let red = summaries.iter().filter(|s| s.worst_level == "red").count();
        let yel = summaries.iter().filter(|s| s.worst_level == "yellow").count();
        out.push_str("# HELP mira_health_audit_red_total Audits whose worst level was red, last 24h\n");
        out.push_str("# TYPE mira_health_audit_red_total counter\n");
        out.push_str(&format!("mira_health_audit_red_total {}\n", red));
        out.push_str("# HELP mira_health_audit_yellow_total Audits whose worst level was yellow, last 24h\n");
        out.push_str("# TYPE mira_health_audit_yellow_total counter\n");
        out.push_str(&format!("mira_health_audit_yellow_total {}\n", yel));
    }

    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        out,
    ).into_response()
}

// ── Custom detectors CRUD ───────────────────────────────────────────────────

pub async fn list_custom_detectors(
    caller: AuthUser,
    Extension(store): Extension<Arc<HealthStore>>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    match store.list_custom_detectors() {
        Ok(rows) => (StatusCode::OK, Json(rows)).into_response(),
        Err(e)   => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    }
}

#[derive(Debug, Deserialize)]
pub struct UpsertCustomDetectorBody {
    pub name:        String,
    #[serde(default)]
    pub description: Option<String>,
    pub target_db:   String,
    pub sql:         String,
    #[serde(default)]
    pub yellow_at:   Option<f64>,
    #[serde(default)]
    pub red_at:      Option<f64>,
    #[serde(default = "default_above")]
    pub direction:   String,
    #[serde(default = "default_true")]
    pub enabled:     bool,
}
fn default_above() -> String { "above".into() }
fn default_true() -> bool { true }

pub async fn upsert_custom_detector(
    caller: AuthUser,
    Extension(store): Extension<Arc<HealthStore>>,
    Json(body): Json<UpsertCustomDetectorBody>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    if body.name.is_empty() {
        return err(StatusCode::BAD_REQUEST, "name required");
    }
    if !["above", "below"].contains(&body.direction.as_str()) {
        return err(StatusCode::BAD_REQUEST, "direction must be 'above' or 'below'");
    }
    let trimmed = body.sql.trim_start().to_ascii_lowercase();
    if !trimmed.starts_with("select") && !trimmed.starts_with("with") {
        return err(StatusCode::BAD_REQUEST, "sql must start with SELECT or WITH (read-only)");
    }
    let row = crate::health::store::CustomDetectorRow {
        name:        body.name,
        description: body.description,
        target_db:   body.target_db,
        sql:         body.sql,
        yellow_at:   body.yellow_at,
        red_at:      body.red_at,
        direction:   body.direction,
        enabled:     body.enabled,
        created_at:  0, // upsert keeps the existing timestamp
        updated_at:  0,
        updated_by:  caller.0.id.clone(),
    };
    match store.upsert_custom_detector(&row) {
        Ok(())  => (StatusCode::OK, Json(serde_json::json!({"saved": true}))).into_response(),
        Err(e)  => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    }
}

pub async fn delete_custom_detector(
    caller: AuthUser,
    Extension(store): Extension<Arc<HealthStore>>,
    Path(name): Path<String>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    match store.delete_custom_detector(&name) {
        Ok(true)  => (StatusCode::OK, Json(serde_json::json!({"deleted": true}))).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "no such custom detector"),
        Err(e)    => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    }
}

/// Dry-run a custom detector's SQL against the target DB without
/// persisting it. Returns the value the row would have produced.
#[derive(Debug, Deserialize)]
pub struct TestCustomDetectorBody {
    pub target_db: String,
    pub sql:       String,
}

pub async fn test_custom_detector(
    caller: AuthUser,
    axum::extract::Extension(_): axum::extract::Extension<Arc<HealthStore>>,
    Json(body): Json<TestCustomDetectorBody>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    let row = crate::health::store::CustomDetectorRow {
        name:        "_test".into(),
        description: None,
        target_db:   body.target_db,
        sql:         body.sql,
        yellow_at:   None, red_at: None, direction: "above".into(),
        enabled: true, created_at: 0, updated_at: 0, updated_by: caller.0.id.clone(),
    };
    // Resolve the data_dir the same way the rest of the system does
    // (expand `~/.mira/data` from the user's HOME). Custom detectors
    // running against alternate-config installs would need this
    // plumbed via an Extension; that's a slice-6 nicety.
    let data_dir = std::path::PathBuf::from(
        crate::config::expand_path("~/.mira/data"),
    );
    let report = crate::health::collector::run_custom_detector(&row, &data_dir);
    (StatusCode::OK, Json(serde_json::json!({
        "level":   report.level.as_str(),
        "value":   report.value,
        "message": report.message,
        "payload": report.payload,
    }))).into_response()
}

// ── Webhooks CRUD ───────────────────────────────────────────────────────────

pub async fn list_webhooks(
    caller: AuthUser,
    Extension(store): Extension<Arc<HealthStore>>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    match store.list_webhooks() {
        Ok(rows) => {
            // Strip secrets from the response — they're write-only via
            // the upsert path.
            let safe: Vec<serde_json::Value> = rows.iter().map(|w| serde_json::json!({
                "id": w.id, "url": w.url,
                "has_secret":   w.secret.as_deref().filter(|s| !s.is_empty()).is_some(),
                "levels_csv":   w.levels_csv,
                "enabled":      w.enabled,
                "description":  w.description,
                "created_at":   w.created_at, "updated_at": w.updated_at,
                "updated_by":   w.updated_by,
                "last_fire_at": w.last_fire_at,
                "last_status":  w.last_status,
                "last_error":   w.last_error,
            })).collect();
            (StatusCode::OK, Json(safe)).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    }
}

#[derive(Debug, Deserialize)]
pub struct UpsertWebhookBody {
    #[serde(default)]
    pub id:          Option<String>,
    pub url:         String,
    #[serde(default)]
    pub secret:      Option<String>,
    #[serde(default)]
    pub levels_csv:  Option<String>,
    #[serde(default = "default_true")]
    pub enabled:     bool,
    #[serde(default)]
    pub description: Option<String>,
}

pub async fn upsert_webhook(
    caller: AuthUser,
    Extension(store): Extension<Arc<HealthStore>>,
    Json(body): Json<UpsertWebhookBody>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    if !body.url.starts_with("http://") && !body.url.starts_with("https://") {
        return err(StatusCode::BAD_REQUEST, "url must be http(s)");
    }
    let row = crate::health::store::WebhookRow {
        id:          body.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        url:         body.url,
        secret:      body.secret,
        levels_csv:  body.levels_csv,
        enabled:     body.enabled,
        description: body.description,
        created_at:  0, updated_at: 0, updated_by: caller.0.id.clone(),
        last_fire_at: None, last_status: None, last_error: None,
    };
    let id = row.id.clone();
    match store.upsert_webhook(&row) {
        Ok(())  => (StatusCode::OK, Json(serde_json::json!({"saved": true, "id": id}))).into_response(),
        Err(e)  => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    }
}

pub async fn delete_webhook(
    caller: AuthUser,
    Extension(store): Extension<Arc<HealthStore>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    match store.delete_webhook(&id) {
        Ok(true)  => (StatusCode::OK, Json(serde_json::json!({"deleted": true}))).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "no such webhook"),
        Err(e)    => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    }
}

// ── Threshold overrides CRUD ────────────────────────────────────────────────

pub async fn list_thresholds(
    caller: AuthUser,
    Extension(store): Extension<Arc<HealthStore>>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    match store.list_thresholds() {
        Ok(rows) => (StatusCode::OK, Json(rows)).into_response(),
        Err(e)   => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    }
}

#[derive(Debug, Deserialize)]
pub struct UpsertThresholdBody {
    pub detector_name: String,
    #[serde(default)]
    pub yellow_at:     Option<f64>,
    #[serde(default)]
    pub red_at:        Option<f64>,
    #[serde(default = "default_above")]
    pub direction:     String,
}

// ── 0.111.0 — Task artifacts ────────────────────────────────────────────────

pub async fn list_artifacts(
    caller: AuthUser,
    arts:   Option<Extension<Arc<crate::task_artifacts::TaskArtifactsStore>>>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    let Some(Extension(store)) = arts else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "task artifacts store not wired");
    };
    let entries = store.list();
    // Strip the local FS path from the response — useful but only
    // meaningful on the host. Add a `name` field instead.
    let safe: Vec<serde_json::Value> = entries.iter().map(|e| serde_json::json!({
        "name":         e.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        "skill":        e.skill,
        "size_bytes":   e.size_bytes,
        "manifest":     e.manifest,
        "absolute_path": e.path.display().to_string(),
    })).collect();
    (StatusCode::OK, Json(safe)).into_response()
}

pub async fn delete_artifact(
    caller: AuthUser,
    arts:   Option<Extension<Arc<crate::task_artifacts::TaskArtifactsStore>>>,
    Path(name): Path<String>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    let Some(Extension(store)) = arts else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "task artifacts store not wired");
    };
    // Find the dir by name (across all skill subdirs).
    let target = store.list().into_iter()
        .find(|e| e.path.file_name().map(|n| n.to_string_lossy() == name).unwrap_or(false))
        .map(|e| e.path);
    let Some(dir) = target else {
        return err(StatusCode::NOT_FOUND, "no artifact with that name");
    };
    match store.delete(&dir) {
        Ok(())  => (StatusCode::OK, Json(serde_json::json!({"deleted": true}))).into_response(),
        Err(e)  => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("delete: {e}")),
    }
}

/// Cap on serving a single artifact file into memory (Phase A4). Task outputs
/// are normally small; this just stops a pathological multi-GB file OOMing us.
const MAX_ARTIFACT_SERVE_BYTES: u64 = 50 * 1024 * 1024;

/// `GET /api/admin/tasks/{task_id}/files` — list a task's output/log files for
/// the artifact browser. Admin-only.
pub async fn list_task_files(
    caller: AuthUser,
    arts:   Option<Extension<Arc<crate::task_artifacts::TaskArtifactsStore>>>,
    Path(task_id): Path<String>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    let Some(Extension(store)) = arts else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "task artifacts store not wired");
    };
    match store.list_files(&task_id) {
        Some(files) => (StatusCode::OK, Json(serde_json::json!({ "task_id": task_id, "files": files }))).into_response(),
        None => err(StatusCode::NOT_FOUND, "no task with that id"),
    }
}

#[derive(serde::Deserialize)]
pub struct FileQuery {
    /// Relative path within the task dir, e.g. `output/report.md`.
    pub path: String,
    /// When true, serve as an attachment (download) rather than inline.
    #[serde(default)]
    pub download: bool,
}

/// `GET /api/admin/tasks/{task_id}/file?path=<rel>[&download=1]` — serve one file
/// from a task's dir (path-traversal-safe). Admin-only.
pub async fn get_task_file(
    caller: AuthUser,
    arts:   Option<Extension<Arc<crate::task_artifacts::TaskArtifactsStore>>>,
    Path(task_id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<FileQuery>,
) -> axum::response::Response {
    use axum::http::header;
    if let Some(resp) = admin_only(&caller) { return resp; }
    let Some(Extension(store)) = arts else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "task artifacts store not wired");
    };
    let Some(path) = store.resolve_file(&task_id, &q.path) else {
        return err(StatusCode::NOT_FOUND, "no such file in this task");
    };
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > MAX_ARTIFACT_SERVE_BYTES {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "file too large to serve — fetch it from disk");
    }
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("read file: {e}")),
    };
    let mime = mime_guess::from_path(&path).first_or_octet_stream();
    let fname = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
    let disposition = if q.download {
        format!("attachment; filename=\"{}\"", fname.replace('"', ""))
    } else {
        "inline".to_string()
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime.essence_str().to_string()),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        bytes,
    )
        .into_response()
}

pub async fn migrate_artifacts(
    caller: AuthUser,
    arts:   Option<Extension<Arc<crate::task_artifacts::TaskArtifactsStore>>>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    let Some(Extension(store)) = arts else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "task artifacts store not wired");
    };
    let home = match dirs::home_dir() {
        Some(h) => h,
        None    => return err(StatusCode::INTERNAL_SERVER_ERROR, "could not resolve $HOME"),
    };
    match crate::task_artifacts::migrate_existing(&home, store.root()) {
        Ok(moves) => {
            let summary: Vec<serde_json::Value> = moves.iter().map(|(from, to)| serde_json::json!({
                "from": from.display().to_string(),
                "to":   to.display().to_string(),
            })).collect();
            (StatusCode::OK, Json(serde_json::json!({
                "moved": moves.len(),
                "details": summary,
            }))).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("migrate: {e}")),
    }
}

pub async fn upsert_threshold(
    caller: AuthUser,
    Extension(store): Extension<Arc<HealthStore>>,
    Json(body): Json<UpsertThresholdBody>,
) -> axum::response::Response {
    if let Some(resp) = admin_only(&caller) { return resp; }
    if !["above", "below"].contains(&body.direction.as_str()) {
        return err(StatusCode::BAD_REQUEST, "direction must be 'above' or 'below'");
    }
    if body.yellow_at.is_none() && body.red_at.is_none() {
        // Both null = clear the override.
        match store.clear_threshold(&body.detector_name) {
            Ok(_)  => return (StatusCode::OK, Json(serde_json::json!({"cleared": true}))).into_response(),
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
        }
    }
    match store.upsert_threshold(
        &body.detector_name, body.yellow_at, body.red_at, &body.direction, &caller.0.id,
    ) {
        Ok(())  => (StatusCode::OK, Json(serde_json::json!({"saved": true}))).into_response(),
        Err(e)  => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    }
}
