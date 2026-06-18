// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/heartbeats/watchdog.rs
//! Slice W1 watchdog — tail the live `mira.log`, fire `watchdog.alert`
//! events for lines at or above the configured severity.
//!
//! Design notes:
//!
//! - **Stateless trait, stateful instance.** The `HeartbeatTask` trait
//!   gives us no per-call state hook, so the `Watchdog` struct holds
//!   its file offset + dedup map + per-minute counter behind a single
//!   `Mutex`. Cheap (one short critical section per fire); avoids the
//!   complexity of an actor.
//!
//! - **Crash-safe offsets.** Offsets are persisted to
//!   `<data_dir>/watchdog_state.json` after every successful scan. A
//!   fresh boot reads the file and resumes; a missing file starts at
//!   the current end of the log (we don't replay history on first boot
//!   — operators don't want yesterday's incident dump on Monday
//!   morning).
//!
//! - **Log rotation.** When `current_offset > file_size` we assume the
//!   file was rotated/truncated and reset to 0. The next scan reads
//!   the new contents from the start. This is the same trick `tail
//!   -F` uses; not perfect (a rotated-then-grown file at exactly the
//!   old offset slips through), but good enough for the WARN/ERROR
//!   cadence we care about.
//!
//! - **Spam guards (minimal-W4).** Two layers:
//!     1. Per-fingerprint dedup with TTL. Same `(severity, module,
//!        first_80_chars(message))` triple within `dedup_ttl_secs` is
//!        silently dropped.
//!     2. Global rate limit per minute. Hard cap; additional alerts
//!        in the same minute are dropped at debug-log level.
//!   PII redaction runs *before* fingerprinting so the dedup key is
//!   based on the same redacted text the recipient sees.
//!
//! - **Notifications via the event bus.** The heartbeat doesn't send
//!   ChannelMessages directly; it emits `watchdog.alert`. A
//!   system-seeded `event_subscription` routes those events to a
//!   ChannelMessage when `notify_user_id` is set in config. This
//!   keeps the heartbeat dispatcher-free and reuses every existing
//!   delivery guard (per-channel rate limit, etc.).
//!
//! - **Self-immunity.** Lines emitted by the watchdog itself
//!   (`mira::automations::heartbeats::watchdog`) are ignored at the
//!   parse layer to prevent feedback loops.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::config::WatchdogConfig;
use crate::events::{Event, EventBus};
use crate::MiraError;

use super::super::store::{AutomationsStore, NewWatchdogIncident};

use super::{HeartbeatContext, HeartbeatOutcome, HeartbeatTask};

/// Stable name used in `Action::Internal { task: "watchdog", … }` and
/// in the seeded system schedule.
pub const WATCHDOG_TASK_NAME: &str = "watchdog";

/// Single-line PII redactors. Conservative on purpose — false negatives
/// (PII slipping through) are worse than false positives. Patterns are
/// compiled once at struct construction. W4 expanded the set; the
/// originals (api_key, bearer_token, home_path) are unchanged.
struct Redactors {
    api_key:           Regex,
    bearer_token:      Regex,
    home_path:         Regex,
    /// `AKIA…` access keys plus the longer `ASIA…` / `AGPA…` /
    /// `AIDA…` family. Always 20 chars.
    aws_access_key:    Regex,
    /// JWT shape: 3 base64url-ish segments separated by dots, each
    /// at least 8 chars. Long enough to avoid colliding with other
    /// dotted identifiers like file paths.
    jwt_token:         Regex,
    /// `proto://user:PASS@host` — the `:PASS@` part is the
    /// sensitive bit. Captures the password via group 1; the rest
    /// is preserved so the host context is still useful for
    /// debugging.
    url_password:      Regex,
    /// Bare email — masks the local part, keeps the domain. Useful
    /// for "who hit this?" without leaking individuals.
    email:             Regex,
    /// E.164 phone numbers (Signal stores them this way). Catches
    /// `+1234567890` style; ignores 5-digit short codes.
    phone_e164:        Regex,
}

impl Redactors {
    fn new() -> Self {
        Self {
            // Catches OpenAI-shape (sk-...), Anthropic-shape (sk-ant-...),
            // and most provider keys that share the prefix idiom. 20-char
            // lower bound avoids matching short identifiers.
            api_key:        Regex::new(r"sk-[A-Za-z0-9_\-]{20,}").unwrap(),
            // `Authorization: Bearer …` and bare `Bearer xyz` patterns.
            bearer_token:   Regex::new(r"(?i)Bearer\s+[A-Za-z0-9_\-\.]{20,}").unwrap(),
            // `/home/<username>/...` → `~/...` so user homedirs don't
            // leak in error messages. Doesn't touch /home alone.
            home_path:      Regex::new(r"/home/[A-Za-z0-9_\-]+/").unwrap(),
            aws_access_key: Regex::new(r"\b(?:AKIA|ASIA|AGPA|AIDA)[A-Z0-9]{16}\b").unwrap(),
            jwt_token:      Regex::new(r"\beyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\b").unwrap(),
            url_password:   Regex::new(r"(?P<scheme>[a-zA-Z][a-zA-Z0-9+.\-]*://[^:/\s@]+):[^@/\s]+@").unwrap(),
            email:          Regex::new(r"\b[A-Za-z0-9._%+\-]+@([A-Za-z0-9.\-]+\.[A-Za-z]{2,})\b").unwrap(),
            phone_e164:     Regex::new(r"\+\d{8,15}\b").unwrap(),
        }
    }

    fn redact(&self, line: &str) -> String {
        let s = self.api_key.replace_all(line, "sk-***REDACTED***");
        let s = self.bearer_token.replace_all(&s, "Bearer ***REDACTED***");
        let s = self.home_path.replace_all(&s, "~/");
        let s = self.aws_access_key.replace_all(&s, "AKIA***REDACTED***");
        let s = self.jwt_token.replace_all(&s, "eyJ***JWT-REDACTED***");
        // Keep the scheme + user + host so an admin still has a
        // meaningful context line; only the password segment is
        // gone.
        let s = self.url_password.replace_all(&s, "${scheme}:***REDACTED***@");
        // Keep the domain; mask the local part.
        let s = self.email.replace_all(&s, "***@${1}");
        let s = self.phone_e164.replace_all(&s, "+***REDACTED***");
        s.into_owned()
    }
}

/// Persisted per-source position. Each source kind owns one field in
/// the file. New fields are `#[serde(default)]` so an existing W1
/// state file (just `log_offset` + `last_scanned_at`) deserialises
/// cleanly on first boot of W2 — the new positions get initialised
/// on the first scan.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct OnDiskState {
    /// Byte offset within `log_file` last scanned through. Reset to 0
    /// when the file appears to have rotated (size < offset).
    log_offset: u64,
    /// Wall clock at last successful scan. Stored for forensics; the
    /// scan algorithm doesn't depend on it.
    last_scanned_at: i64,

    /// Largest `started_at` (unix seconds) we've seen in
    /// `automations.db.automation_runs` for an `outcome='failure'` row.
    /// 0 = not yet initialised; first scan with 0 is treated as
    /// "now" so an upgrade doesn't replay every historical failure
    /// in one boot.
    #[serde(default)]
    automation_runs_last_seen_at: i64,

    /// Largest `ts_ms` (unix millis) we've seen in
    /// `agent_audit.db.agent_audit` for an alert-worthy event_kind.
    /// 0 = not yet initialised (same "treat as now" semantics).
    #[serde(default)]
    agent_audit_last_seen_ms: i64,
}

/// In-memory state — never persisted. Resets on restart, which is
/// fine: a recurring error will simply emit one more alert after
/// boot (acts as an implicit "still broken" reminder).
#[derive(Default)]
struct LiveState {
    /// Maps fingerprint → (last_fired_at, count_within_ttl). Pruned
    /// during each scan so the map stays bounded.
    fingerprints: HashMap<String, (i64, u32)>,
    /// (minute_epoch, count) for the current minute. Reset when the
    /// minute changes. `count` is the number of alerts fired this
    /// minute across all fingerprints.
    rate_window: (i64, u32),
    /// W4 storm-pause state, keyed by source_id (`log:mira.log` etc.).
    /// `events` is a sliding window of recent alert timestamps from
    /// that source; `paused_until` is 0 when the source is healthy
    /// or a unix-seconds expiry while paused. `paused_announced` is
    /// the flag for "we already emitted the one-shot 'storm
    /// detected' notification" so re-pause-while-paused doesn't
    /// re-spam.
    storm_state: HashMap<String, StormState>,
}

#[derive(Default)]
struct StormState {
    events:            std::collections::VecDeque<i64>,
    paused_until:      i64,
    paused_announced:  bool,
}

pub struct Watchdog {
    cfg:        WatchdogConfig,
    state_path: PathBuf,
    log_path:   PathBuf,
    /// W2 source dependencies — paths to the SQLite files the DB
    /// scanners need to open. Held as paths (not connections) so
    /// the scanner can open + close per fire and gracefully handle
    /// the file going away mid-run.
    automations_db: PathBuf,
    agent_audit_db: PathBuf,
    redactors:  Redactors,
    inner:      Mutex<WatchdogInner>,
    /// Compiled once at construction so we don't re-parse user
    /// regexes per line.
    ignore_regexes: Vec<Regex>,
    /// W3 — when wired, every emitted alert is first persisted as a
    /// `watchdog_incidents` row so the auto-routed ChannelMessage
    /// template can include a stable `incident_id` link to the
    /// analyze endpoint. None in tests / minimal builds; emit still
    /// works without persistence (template just shows no link).
    incidents: Option<std::sync::Arc<AutomationsStore>>,
}

struct WatchdogInner {
    on_disk: OnDiskState,
    live:    LiveState,
}

impl Watchdog {
    pub fn new(cfg: WatchdogConfig, data_dir: PathBuf, default_log_path: PathBuf) -> Self {
        let state_path = data_dir.join("watchdog_state.json");
        let log_path = cfg.log_file.as_deref()
            .map(crate::config::expand_path)
            .unwrap_or(default_log_path);
        let automations_db = data_dir.join("automations.db");
        let agent_audit_db = data_dir.join("agent_audit.db");

        // Detect "fresh state" by whether the state file actually
        // loaded. The earlier `log_offset == 0` check confused
        // "never ran" with "ran and read zero new bytes" — fine in
        // practice today, but it bit the test suite and would have
        // bitten any prod instance whose log happened to be empty
        // at the moment of first-boot.
        let (mut on_disk, fresh) = match load_state(&state_path) {
            Ok(s) => (s, false),
            Err(e) => {
                debug!("watchdog: state file unreadable / absent, starting fresh: {e}");
                (OnDiskState::default(), true)
            }
        };

        // First-boot initialisation. A fresh deploy hasn't seen any
        // historical events yet; replaying the entire history on
        // first run would be a flood. Pin every cursor to "now" so
        // we only emit alerts for things that happen after the
        // watchdog starts watching. For the log file, "now" means
        // the current end-of-file (so subsequent scans pick up
        // newly-appended lines only). Tests that need to scan
        // pre-existing content seed a state file before the first
        // run — see `seed_state_for_test`.
        let now_secs = chrono::Utc::now().timestamp();
        let now_millis = now_secs * 1000;
        if fresh {
            if let Ok(meta) = std::fs::metadata(&log_path) {
                on_disk.log_offset = meta.len();
            }
            on_disk.automation_runs_last_seen_at = now_secs;
            on_disk.agent_audit_last_seen_ms = now_millis;
        }

        // Compile ignore patterns once. Bad regexes are logged and
        // skipped — a single typo shouldn't disable the whole watchdog.
        let ignore_regexes = cfg.ignore_patterns.iter()
            .filter_map(|p| match Regex::new(p) {
                Ok(r) => Some(r),
                Err(e) => { warn!("watchdog: ignore pattern {p:?} invalid: {e} — skipping"); None }
            })
            .collect();

        Self {
            cfg,
            state_path,
            log_path,
            automations_db,
            agent_audit_db,
            redactors: Redactors::new(),
            inner: Mutex::new(WatchdogInner { on_disk, live: LiveState::default() }),
            ignore_regexes,
            incidents: None,
        }
    }

    /// Wire an AutomationsStore so each emitted alert is persisted as
    /// a `watchdog_incidents` row before the bus event fires. The
    /// resulting incident id is included in the event payload so the
    /// auto-routed ChannelMessage template can render a markdown link
    /// to `/incidents/<id>` for the analyze flow.
    pub fn with_incident_store(mut self, store: std::sync::Arc<AutomationsStore>) -> Self {
        self.incidents = Some(store);
        self
    }
}

#[async_trait]
impl HeartbeatTask for Watchdog {
    fn name(&self) -> &'static str { WATCHDOG_TASK_NAME }

    async fn run(
        &self,
        ctx:   &HeartbeatContext,
        _args: &serde_json::Value,
    ) -> Result<HeartbeatOutcome, MiraError> {
        let now = chrono::Utc::now().timestamp();
        let mut total_lines_scanned = 0usize;

        // Snapshot the per-source positions so we can scan without
        // holding the lock. Updated via the new_* locals below before
        // we re-acquire the lock to commit them.
        let (log_off_in, autoruns_in, audit_in) = {
            let g = self.inner.lock().expect("watchdog state");
            (g.on_disk.log_offset, g.on_disk.automation_runs_last_seen_at, g.on_disk.agent_audit_last_seen_ms)
        };

        // ── Source 1: live log file ─────────────────────────────────
        let (log_alerts, new_log_offset) = match scan_log(&self.log_path, log_off_in) {
            Ok(TailOutcome { new_offset, lines }) => {
                total_lines_scanned += lines.len();
                let alerts = lines.iter()
                    .filter_map(|l| parse_log_line_to_alert(l, &self.cfg.severity_threshold))
                    .filter(|a| !self.is_ignored(&a.message))
                    // Self-immunity.
                    .filter(|a| !a.module.contains("automations::heartbeats::watchdog"))
                    .collect::<Vec<_>>();
                (alerts, new_offset)
            }
            Err(e) => {
                debug!("watchdog: log read failed ({}): {e}", self.log_path.display());
                (Vec::new(), log_off_in)
            }
        };

        // ── Source 2: automation_runs failures ───────────────────────
        let (autorun_alerts, new_autoruns_seen) = scan_automation_runs(&self.automations_db, autoruns_in)
            .unwrap_or_else(|e| {
                debug!("watchdog: automation_runs scan failed: {e}");
                (Vec::new(), autoruns_in)
            });

        // ── Source 3: agent_audit alert-worthy events ────────────────
        let (audit_alerts, new_audit_seen) = scan_agent_audit(&self.agent_audit_db, audit_in)
            .unwrap_or_else(|e| {
                debug!("watchdog: agent_audit scan failed: {e}");
                (Vec::new(), audit_in)
            });

        let parsed: Vec<Alert> = log_alerts.into_iter()
            .chain(autorun_alerts)
            .chain(audit_alerts)
            .collect();

        let bus = ctx.event_bus.as_ref();

        let mut alerts_emitted = 0u32;
        let mut alerts_deduped = 0u32;
        let mut alerts_rate_limited = 0u32;
        let mut alerts_storm_dropped = 0u32;
        // Per-source pause-trip notifications. Built up during the
        // emission loop so we emit them OUTSIDE the lock (Bus emit
        // is sync but we keep critical sections tight). Each entry
        // becomes a `watchdog.alert` event.
        let mut storm_notifications: Vec<Alert> = Vec::new();

        // Single critical section: dedup + rate-limit decisions
        // followed by emission per matched line.
        {
            let mut guard = self.inner.lock().expect("watchdog state");
            // Prune dedup map of expired fingerprints.
            let ttl = self.cfg.dedup_ttl_secs as i64;
            guard.live.fingerprints.retain(|_, (last, _)| now - *last < ttl);

            // Auto-resume any source whose cooldown has elapsed. Done
            // here so a source that's been quiet during cooldown
            // immediately processes its first post-cooldown alert.
            for state in guard.live.storm_state.values_mut() {
                if state.paused_until > 0 && now >= state.paused_until {
                    state.paused_until = 0;
                    state.paused_announced = false;
                    state.events.clear();
                }
            }

            for a in parsed {
                let redacted = self.redactors.redact(&a.message);
                let fp = fingerprint(&a.severity, &a.module, &redacted);

                // ── Storm pause check ─────────────────────────────
                if self.cfg.storm_threshold > 0 {
                    let window = self.cfg.storm_window_secs as i64;
                    let cooldown = self.cfg.storm_cooldown_secs as i64;
                    let entry = guard.live.storm_state.entry(a.source_id.clone()).or_default();

                    // Drop already-paused sources outright.
                    if entry.paused_until > now {
                        alerts_storm_dropped += 1;
                        // First time dropping while paused, queue the
                        // one-shot announcement.
                        if !entry.paused_announced {
                            entry.paused_announced = true;
                            storm_notifications.push(Alert {
                                severity:  "WARN".into(),
                                source_id: "watchdog:storm".into(),
                                module:    format!("watchdog:storm:{}", a.source_id),
                                message:   format!(
                                    "Source {} paused for {}s due to alert storm \
                                     ({}+ events in last {}s). Suppressing further alerts \
                                     from this source until {}.",
                                    a.source_id, cooldown,
                                    self.cfg.storm_threshold, window,
                                    chrono::DateTime::<chrono::Utc>::from_timestamp(entry.paused_until, 0)
                                        .map(|d| d.to_rfc3339())
                                        .unwrap_or_else(|| entry.paused_until.to_string()),
                                ),
                            });
                        }
                        continue;
                    }

                    // Update sliding window. Drop timestamps that have
                    // aged past the window edge.
                    while entry.events.front().map(|t| now - *t > window).unwrap_or(false) {
                        entry.events.pop_front();
                    }
                    entry.events.push_back(now);
                    if entry.events.len() as u32 >= self.cfg.storm_threshold {
                        // Trip the storm.
                        entry.paused_until = now + cooldown;
                        entry.paused_announced = true;
                        entry.events.clear();
                        warn!(
                            "watchdog: source {} hit storm threshold ({} events / {}s) — pausing for {}s",
                            a.source_id, self.cfg.storm_threshold, window, cooldown,
                        );
                        storm_notifications.push(Alert {
                            severity:  "WARN".into(),
                            source_id: "watchdog:storm".into(),
                            module:    format!("watchdog:storm:{}", a.source_id),
                            message:   format!(
                                "Alert storm from {} — {}+ events in {}s. Pausing this \
                                 source for {}s; check upstream and clear noisy patterns.",
                                a.source_id, self.cfg.storm_threshold, window, cooldown,
                            ),
                        });
                        // Drop this triggering alert too — the storm
                        // notification is the meaningful thing for
                        // this fire.
                        alerts_storm_dropped += 1;
                        continue;
                    }
                }

                // Dedup
                if let Some((last, count)) = guard.live.fingerprints.get_mut(&fp) {
                    *count += 1;
                    if now - *last < ttl {
                        alerts_deduped += 1;
                        continue;
                    }
                    *last = now;
                } else {
                    guard.live.fingerprints.insert(fp.clone(), (now, 1));
                }

                // Rate limit
                if self.cfg.rate_limit_per_min > 0 {
                    let minute = now / 60;
                    if guard.live.rate_window.0 != minute {
                        guard.live.rate_window = (minute, 0);
                    }
                    if guard.live.rate_window.1 >= self.cfg.rate_limit_per_min {
                        alerts_rate_limited += 1;
                        debug!("watchdog: rate-limit hit ({} alerts/min cap)", self.cfg.rate_limit_per_min);
                        continue;
                    }
                    guard.live.rate_window.1 += 1;
                }

                let count = guard.live.fingerprints.get(&fp).map(|(_, c)| *c).unwrap_or(1);

                // W3 — persist incident first so the bus payload can
                // reference a stable id. The recipient is the
                // configured notify_user_id; absent recipient = we
                // skip the persist (no one would see the link anyway)
                // and just emit. Persist failures are logged but
                // don't suppress the alert — degraded mode is still
                // a notification, just without the analyze link.
                let incident_id: Option<String> = match (&self.incidents, self.cfg.notify_user_id.as_deref()) {
                    (Some(store), Some(uid)) => match store.create_watchdog_incident(NewWatchdogIncident {
                        user_id:      uid.to_string(),
                        fingerprint:  fp.clone(),
                        severity:     a.severity.clone(),
                        source:       a.source_id.clone(),
                        module:       a.module.clone(),
                        message:      redacted.clone(),
                        payload_json: serde_json::json!({
                            "first_seen_at": now,
                            "recent_count":  count,
                        }).to_string(),
                    }) {
                        Ok(id)  => Some(id),
                        Err(e)  => {
                            warn!("watchdog: persist incident failed (alert still emitted): {e}");
                            None
                        }
                    },
                    _ => None,
                };

                if let Some(bus) = bus {
                    emit_alert(bus, &a, &redacted, &fp, now, count, incident_id.as_deref());
                    alerts_emitted += 1;
                }
            }

            guard.on_disk.log_offset = new_log_offset;
            guard.on_disk.automation_runs_last_seen_at = new_autoruns_seen;
            guard.on_disk.agent_audit_last_seen_ms = new_audit_seen;
            guard.on_disk.last_scanned_at = now;
            // Persist offset best-effort. A failed write means the
            // next boot will re-scan the same range — harmless given
            // the dedup TTL.
            if let Err(e) = save_state(&self.state_path, &guard.on_disk) {
                debug!("watchdog: state file save failed (non-fatal): {e}");
            }
        }

        // Emit storm-pause notifications outside the dedup/rate-limit
        // gates — they're meta-alerts about the watchdog itself, not
        // user-facing errors, and should always reach the operator
        // exactly once per pause event. Persisted as incidents too so
        // the analyze flow can be used on them ("why is this source
        // storming?" is a fair question for the LLM).
        let mut storm_emitted = 0u32;
        if let Some(bus) = bus {
            for sa in storm_notifications {
                let storm_fp = fingerprint(&sa.severity, &sa.module, &sa.message);
                let incident_id: Option<String> = match (&self.incidents, self.cfg.notify_user_id.as_deref()) {
                    (Some(store), Some(uid)) => store.create_watchdog_incident(NewWatchdogIncident {
                        user_id:      uid.to_string(),
                        fingerprint:  storm_fp.clone(),
                        severity:     sa.severity.clone(),
                        source:       sa.source_id.clone(),
                        module:       sa.module.clone(),
                        message:      sa.message.clone(),
                        payload_json: serde_json::json!({"first_seen_at": now, "recent_count": 1u32}).to_string(),
                    }).ok(),
                    _ => None,
                };
                emit_alert(bus, &sa, &sa.message, &storm_fp, now, 1, incident_id.as_deref());
                storm_emitted += 1;
            }
        }

        let summary = format!(
            "watchdog: scanned {} line(s); emitted={} deduped={} rate_limited={} storm_dropped={} storm_pauses={}",
            total_lines_scanned, alerts_emitted, alerts_deduped, alerts_rate_limited, alerts_storm_dropped, storm_emitted,
        );
        Ok(HeartbeatOutcome { summary })
    }
}

impl Watchdog {
    fn is_ignored(&self, message: &str) -> bool {
        self.ignore_regexes.iter().any(|r| r.is_match(message))
    }
}

// ── File tail ────────────────────────────────────────────────────────────────

struct TailOutcome {
    new_offset: u64,
    lines:      Vec<String>,
}

fn scan_log(path: &std::path::Path, since_offset: u64) -> Result<TailOutcome, std::io::Error> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    // Rotation / truncation detection. If the file is shorter than
    // our last offset, treat it as a fresh file and start from 0.
    let start = if since_offset > len { 0 } else { since_offset };
    file.seek(SeekFrom::Start(start))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    let lines: Vec<String> = buf.lines().map(|l| l.to_string()).collect();
    Ok(TailOutcome { new_offset: len, lines })
}

// ── DB scanner: automation_runs failures ─────────────────────────────────────

/// Pull failed automation_runs rows newer than `since_started_at`.
/// Returns the alerts plus the new high-water mark to persist. Bounded
/// by `MAX_DB_ROWS_PER_SCAN` so a backlog doesn't flood the channel
/// in one tick — leftovers come through on subsequent fires.
fn scan_automation_runs(
    db_path:           &std::path::Path,
    since_started_at:  i64,
) -> Result<(Vec<Alert>, i64), rusqlite::Error> {
    use rusqlite::{Connection, OpenFlags};
    if !db_path.exists() {
        return Ok((Vec::new(), since_started_at));
    }
    // Read-only open — we never write the automations DB from here,
    // so SHARED_CACHE doesn't matter and read-only is the right
    // posture (one fewer way for the watchdog to corrupt anything).
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(
        "SELECT source_kind, source_id, started_at, error
           FROM automation_runs
          WHERE outcome = 'failure' AND started_at > ?1
          ORDER BY started_at ASC
          LIMIT ?2",
    )?;
    let rows: Vec<(String, String, i64, Option<String>)> = stmt.query_map(
        rusqlite::params![since_started_at, MAX_DB_ROWS_PER_SCAN as i64],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?.collect::<rusqlite::Result<Vec<_>>>()?;

    let mut max_seen = since_started_at;
    let alerts: Vec<Alert> = rows.into_iter()
        .map(|(kind, id, started_at, error)| {
            if started_at > max_seen { max_seen = started_at; }
            Alert {
                severity:  "ERROR".into(),
                source_id: "db:automation_runs".into(),
                // Group by (source_kind, source_id-prefix) so repeated
                // failures of the same schedule/webhook collapse via
                // the fingerprinter.
                module:    format!("automation_runs/{kind}/{}", short_id(&id)),
                message:   error.unwrap_or_else(|| "(no error message recorded)".into()),
            }
        })
        .collect();
    Ok((alerts, max_seen))
}

// ── DB scanner: agent_audit alert-worthy events ──────────────────────────────

/// Alert-worthy event_kinds. `interrupted` is intentionally excluded
/// — interrupts are usually user-driven and routine. `policy_decision`
/// and `status_change` need JSON parsing to filter the alert-worthy
/// subset (granted=false, to=failed); handled inside the scanner.
const AUDIT_HARD_ALERT_KINDS: &[&str] = &[
    "spawn_denied",
    "agent_budget_exceeded",
    "session_budget_exceeded",
];

/// Cap on rows pulled per scan from a SQLite source. Backlogs come
/// through on subsequent fires.
const MAX_DB_ROWS_PER_SCAN: usize = 100;

fn scan_agent_audit(
    db_path:        &std::path::Path,
    since_ts_ms:    i64,
) -> Result<(Vec<Alert>, i64), rusqlite::Error> {
    use rusqlite::{Connection, OpenFlags};
    if !db_path.exists() {
        return Ok((Vec::new(), since_ts_ms));
    }
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(
        "SELECT ts_ms, agent_id, event_kind, event_json
           FROM agent_audit
          WHERE ts_ms > ?1
          ORDER BY ts_ms ASC
          LIMIT ?2",
    )?;
    let rows: Vec<(i64, String, String, String)> = stmt.query_map(
        rusqlite::params![since_ts_ms, MAX_DB_ROWS_PER_SCAN as i64],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?.collect::<rusqlite::Result<Vec<_>>>()?;

    let mut max_seen = since_ts_ms;
    let mut alerts = Vec::new();
    for (ts_ms, agent_id, event_kind, event_json) in rows {
        if ts_ms > max_seen { max_seen = ts_ms; }
        let Some(message) = audit_alert_message(&event_kind, &event_json) else { continue; };
        alerts.push(Alert {
            severity:  "ERROR".into(),
            source_id: "db:agent_audit".into(),
            module:    format!("agent_audit/{event_kind}/{}", short_id(&agent_id)),
            message,
        });
    }
    Ok((alerts, max_seen))
}

/// Decide whether an audit row is alert-worthy and, if so, render
/// the human-readable message body. Returns None for routine kinds
/// (spawn_requested, spawn_approved, status_change to non-failed,
/// policy_decision granted=true, interrupted).
fn audit_alert_message(event_kind: &str, event_json: &str) -> Option<String> {
    if AUDIT_HARD_ALERT_KINDS.contains(&event_kind) {
        // Hard kinds: the kind itself is the alert; pull a useful
        // detail field if present, otherwise just say it happened.
        let v: serde_json::Value = serde_json::from_str(event_json).ok()?;
        let detail = match event_kind {
            "spawn_denied" => v.get("reason").and_then(|x| x.as_str())
                .map(|s| format!("spawn denied: {s}")),
            "agent_budget_exceeded" => Some(format!(
                "agent budget exceeded ({}/${} cap)",
                v.get("spent_usd").and_then(|x| x.as_f64()).unwrap_or(0.0),
                v.get("cap_usd").and_then(|x| x.as_f64()).unwrap_or(0.0),
            )),
            "session_budget_exceeded" => Some(format!(
                "session budget exceeded ({}/${} cap)",
                v.get("session_spent_usd").and_then(|x| x.as_f64()).unwrap_or(0.0),
                v.get("session_cap_usd").and_then(|x| x.as_f64()).unwrap_or(0.0),
            )),
            _ => None,
        };
        return detail.or_else(|| Some(format!("{event_kind} (no detail)")));
    }
    let v: serde_json::Value = serde_json::from_str(event_json).ok()?;
    match event_kind {
        // Status transitions: alert only when the new state is `failed`.
        "status_change" => {
            let to = v.get("to").and_then(|x| x.as_str())?;
            if to == "failed" {
                let from = v.get("from").and_then(|x| x.as_str()).unwrap_or("?");
                Some(format!("agent status changed {from} → {to}"))
            } else {
                None
            }
        }
        // Policy decisions: alert only when denied.
        "policy_decision" => {
            let granted = v.get("granted").and_then(|x| x.as_bool())?;
            if granted { return None; }
            let rule = v.get("rule").and_then(|x| x.as_str()).unwrap_or("?");
            let detail = v.get("detail").and_then(|x| x.as_str()).unwrap_or("(no detail)");
            Some(format!("policy denied [{rule}]: {detail}"))
        }
        _ => None,
    }
}

/// Truncate an id (UUID/long string) for the alert module label so
/// the displayed bucket isn't dominated by a 36-char id.
fn short_id(s: &str) -> String {
    s.chars().take(8).collect()
}

// ── Alert shape ──────────────────────────────────────────────────────────────

/// Source-agnostic alert candidate. The log scanner, the
/// automation_runs scanner, and the agent_audit scanner all produce
/// these. `module` is whatever logical bucket the source uses to
/// group related events — Rust module path for the log, schedule/
/// webhook id for automation_runs, agent_id for agent_audit. The
/// dedup fingerprint hashes (severity, module, redacted_message), so
/// two errors with different `source_id` but the same module + msg
/// shape collapse together — usually what you want.
#[derive(Debug)]
struct Alert {
    severity:  String,
    /// Where this alert came from. Surfaced verbatim in the alert
    /// payload so subscribers can route differently per source.
    /// Examples: `log:mira.log`, `db:automation_runs`,
    /// `db:agent_audit`.
    source_id: String,
    module:    String,
    message:   String,
}

/// Parse one tracing-formatted line. Format we expect (matches the
/// default `tracing-subscriber` formatter MIRA uses):
///
/// ```text
/// 2026-05-09T12:34:56.789012Z  WARN mira::tts::backend::openai: send failed for …
/// ```
///
/// Returns None if the line doesn't match, isn't at the threshold, or
/// is malformed. Lenient — a partial line at end-of-file just gets
/// dropped and picked up next scan.
fn parse_log_line_to_alert(line: &str, threshold: &str) -> Option<Alert> {
    let trimmed = line.trim_end();
    if trimmed.is_empty() { return None; }

    // Find "  WARN " or "  ERROR " (one or more spaces).
    let (severity, after_severity) = if let Some(idx) = trimmed.find(" ERROR ") {
        ("ERROR", &trimmed[idx + 7..])
    } else if let Some(idx) = trimmed.find(" WARN ") {
        ("WARN", &trimmed[idx + 6..])
    } else {
        return None;
    };

    if !meets_threshold(severity, threshold) { return None; }

    // After "WARN " comes "module::path: message". Match on `": "`
    // (colon followed by space) so the `::` separators inside Rust
    // module paths don't trip the find — the first single colon in
    // `mira::foo::bar: msg` is the one we want, not the one inside
    // `::`. The trailing-space requirement is the discriminator.
    let colon = after_severity.find(": ")?;
    let module = after_severity[..colon].trim().to_string();
    let message = after_severity[colon + 2..].trim().to_string();
    if module.is_empty() || message.is_empty() { return None; }

    Some(Alert {
        severity:  severity.to_string(),
        source_id: "log:mira.log".into(),
        module,
        message,
    })
}

fn meets_threshold(severity: &str, threshold: &str) -> bool {
    // ERROR ≥ WARN. Anything else is rejected at parse_line, so this
    // is just the WARN-vs-ERROR comparison.
    match (severity, threshold.to_ascii_uppercase().as_str()) {
        ("ERROR", _)            => true,
        ("WARN",  "WARN")       => true,
        _                       => false,
    }
}

// ── Fingerprinting ──────────────────────────────────────────────────────────

fn fingerprint(severity: &str, module: &str, message: &str) -> String {
    use sha2::{Sha256, Digest};
    // First 80 chars of message for the body of the print — enough to
    // distinguish "send failed for X" from "send failed for Y" while
    // collapsing the same error against different inputs only when
    // the prefix matches. Hash for storage; full triple is in the
    // alert payload so the user sees the actual line.
    let head: String = message.chars().take(80).collect();
    let mut h = Sha256::new();
    h.update(severity.as_bytes());
    h.update(b"\0");
    h.update(module.as_bytes());
    h.update(b"\0");
    h.update(head.as_bytes());
    let digest = h.finalize();
    hex::encode(&digest[..8]) // 64 bits is plenty of collision resistance for in-memory dedup
}

// ── Emission ─────────────────────────────────────────────────────────────────

fn emit_alert(
    bus:          &EventBus,
    alert:        &Alert,
    redacted:     &str,
    fingerprint:  &str,
    now:          i64,
    recent_count: u32,
    incident_id:  Option<&str>,
) {
    // Pre-render the emoji so the auto-routed ChannelMessage template
    // (`{{payload.severity_emoji}} …`) doesn't need conditional logic
    // — the template engine resolves dotted paths only.
    let severity_emoji = match alert.severity.as_str() {
        "ERROR" => "🚨",
        "WARN"  => "⚠️",
        _       => "ℹ️",
    };
    // Pre-render the analyze-link line so the same template covers
    // both "incident persisted" and "no recipient configured" cases
    // without needing template conditionals. When there's no incident,
    // the line is empty and renders as an empty paragraph.
    let analyze_link = match incident_id {
        Some(id) => format!("[🔍 Analyze with LLM](/incidents/{id})"),
        None     => String::new(),
    };
    let payload = serde_json::json!({
        "severity":       alert.severity,
        "severity_emoji": severity_emoji,
        "source":         alert.source_id,
        "module":         alert.module,
        "message":        redacted,
        "fingerprint":    fingerprint,
        "first_seen_at":  now,
        "recent_count":   recent_count,
        "incident_id":    incident_id.unwrap_or(""),
        "analyze_link":   analyze_link,
    });
    bus.emit(Event::new(crate::events::names::WATCHDOG_ALERT, None, payload));
}

// ── State file I/O ──────────────────────────────────────────────────────────

fn load_state(path: &std::path::Path) -> Result<OnDiskState, std::io::Error> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(std::io::Error::other)
}

fn save_state(path: &std::path::Path, state: &OnDiskState) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    std::fs::write(path, bytes)
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Pre-seed a default state file so the watchdog treats the next
/// `run()` as "not fresh" — it'll scan from offset 0 instead of
/// pinning to current EOF. Used by tests that need to scan
/// pre-existing log content.
#[cfg(test)]
fn seed_state_for_test(data_dir: &std::path::Path) {
    save_state(&data_dir.join("watchdog_state.json"), &OnDiskState::default())
        .expect("seed state");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn parse_warn_line() {
        let line = "2026-05-09T12:34:56.789012Z  WARN mira::tts::backend::openai: send failed for http://x";
        let p = parse_log_line_to_alert(line, "WARN").unwrap();
        assert_eq!(p.severity, "WARN");
        assert_eq!(p.module, "mira::tts::backend::openai");
        assert_eq!(p.message, "send failed for http://x");
    }

    #[test]
    fn parse_error_line_passes_warn_threshold() {
        let line = "2026-05-09T12:34:56.789012Z ERROR mira::foo: boom";
        let p = parse_log_line_to_alert(line, "WARN").unwrap();
        assert_eq!(p.severity, "ERROR");
    }

    #[test]
    fn parse_info_line_skipped() {
        let line = "2026-05-09T12:34:56.789012Z  INFO mira::foo: just chatter";
        assert!(parse_log_line_to_alert(line, "WARN").is_none());
    }

    #[test]
    fn parse_warn_skipped_at_error_threshold() {
        let line = "2026-05-09T12:34:56.789012Z  WARN mira::foo: lukewarm";
        assert!(parse_log_line_to_alert(line, "ERROR").is_none());
    }

    #[test]
    fn redactor_masks_api_key() {
        let r = Redactors::new();
        let s = r.redact("api key sk-ant-api03-abcdefghijklmnopqrstuvwxyz123456");
        assert!(s.contains("sk-***REDACTED***"), "got: {s}");
        assert!(!s.contains("abcdefghij"), "key body should be masked: {s}");
    }

    #[test]
    fn redactor_masks_bearer() {
        let r = Redactors::new();
        let s = r.redact("Authorization: Bearer abcdefghij1234567890ZZZZZZ");
        assert!(s.contains("Bearer ***REDACTED***"), "got: {s}");
    }

    #[test]
    fn redactor_collapses_home_path() {
        let r = Redactors::new();
        let s = r.redact("error reading /home/user/.mira/data/foo");
        assert_eq!(s, "error reading ~/.mira/data/foo");
    }

    #[test]
    fn fingerprint_collapses_message_tail() {
        // Two messages that differ inside the first 80 chars produce
        // different fingerprints — sanity check that the fingerprint
        // isn't ONLY using severity + target.
        let a = fingerprint("WARN", "mira::tts", &format!("send failed for url-A {}", "a".repeat(80)));
        let b = fingerprint("WARN", "mira::tts", &format!("send failed for url-A {}", "b".repeat(80)));
        assert_ne!(a, b, "messages that differ in the first 80 chars must produce different fingerprints");

        // Two messages with an identical 85-char prefix → same
        // fingerprint regardless of suffix. This is the "collapse"
        // property: storms of similar errors don't blow up the dedup
        // map with unique fingerprints.
        let prefix = "x".repeat(85);
        let fp_short = fingerprint("WARN", "mira::tts", &prefix);
        let fp_long  = fingerprint("WARN", "mira::tts", &format!("{prefix}DIFFERENT-SUFFIX"));
        assert_eq!(fp_short, fp_long,
            "identical first-80 prefix should collapse to the same fingerprint");
    }

    #[test]
    fn meets_threshold_matrix() {
        assert!(meets_threshold("ERROR", "WARN"));
        assert!(meets_threshold("ERROR", "ERROR"));
        assert!(meets_threshold("WARN",  "WARN"));
        assert!(!meets_threshold("WARN", "ERROR"));
    }

    #[tokio::test]
    async fn watchdog_reads_log_and_emits_event() {
        use crate::events::EventBus;
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("test.log");
        std::fs::write(
            &log_path,
            "2026-05-09T12:00:00.000000Z  WARN mira::test::a: bad thing happened\n\
             2026-05-09T12:00:01.000000Z  INFO mira::test::a: ignored chatter\n\
             2026-05-09T12:00:02.000000Z ERROR mira::test::a: real bad thing\n",
        ).unwrap();
        // The watchdog pins to EOF on first boot in production —
        // sensible to avoid replaying historical log content. Tests
        // that pre-write content and then run() rely on the
        // not-fresh path; seed a state file to mark "already ran".
        seed_state_for_test(dir.path());

        let cfg = WatchdogConfig {
            enabled: true,
            severity_threshold: "WARN".into(),
            dedup_ttl_secs: 1, // tight TTL so we don't dedup across this single-shot test
            rate_limit_per_min: 100,
            log_file: Some(log_path.to_string_lossy().into()),
            ..WatchdogConfig::default()
        };
        let wd = Watchdog::new(cfg, dir.path().to_path_buf(), log_path.clone());

        let bus = Arc::new(EventBus::new());
        let mut rx = bus.subscribe();
        let ctx = HeartbeatContext {
            data_dir:  dir.path().to_path_buf(),
            event_bus: Some(Arc::clone(&bus)),
        };
        let outcome = wd.run(&ctx, &serde_json::Value::Null).await.unwrap();
        assert!(outcome.summary.contains("emitted=2"), "expected 2 alerts, got: {}", outcome.summary);

        // Drain bus events.
        let mut alerts = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if ev.name == crate::events::names::WATCHDOG_ALERT {
                alerts.push(ev);
            }
        }
        assert_eq!(alerts.len(), 2, "expected 2 alert events on bus");
        assert!(alerts.iter().any(|e| e.payload["severity"] == "WARN"));
        assert!(alerts.iter().any(|e| e.payload["severity"] == "ERROR"));

        // State file should have been written.
        let state_path = dir.path().join("watchdog_state.json");
        assert!(state_path.exists(), "state file not persisted");
        let state: OnDiskState = serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        assert!(state.log_offset > 0, "expected non-zero offset after scan");
    }

    #[tokio::test]
    async fn watchdog_dedups_within_ttl() {
        use crate::events::EventBus;
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("test.log");
        // Two identical WARN lines — same fingerprint — should emit
        // one event, dedup the other.
        std::fs::write(
            &log_path,
            "2026-05-09T12:00:00.000000Z  WARN mira::test::a: same thing\n\
             2026-05-09T12:00:01.000000Z  WARN mira::test::a: same thing\n",
        ).unwrap();
        seed_state_for_test(dir.path());

        let cfg = WatchdogConfig {
            enabled: true,
            severity_threshold: "WARN".into(),
            dedup_ttl_secs: 600,
            rate_limit_per_min: 100,
            log_file: Some(log_path.to_string_lossy().into()),
            ..WatchdogConfig::default()
        };
        let wd = Watchdog::new(cfg, dir.path().to_path_buf(), log_path.clone());
        let bus = Arc::new(EventBus::new());
        let ctx = HeartbeatContext {
            data_dir:  dir.path().to_path_buf(),
            event_bus: Some(Arc::clone(&bus)),
        };
        let outcome = wd.run(&ctx, &serde_json::Value::Null).await.unwrap();
        assert!(outcome.summary.contains("emitted=1"), "expected 1 alert (deduped), got: {}", outcome.summary);
        assert!(outcome.summary.contains("deduped=1"));
    }

    #[tokio::test]
    async fn watchdog_skips_self_log_lines() {
        use crate::events::EventBus;
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("test.log");
        std::fs::write(
            &log_path,
            "2026-05-09T12:00:00.000000Z  WARN mira::automations::heartbeats::watchdog: self-induced\n\
             2026-05-09T12:00:01.000000Z  WARN mira::test::other: external\n",
        ).unwrap();
        seed_state_for_test(dir.path());

        let cfg = WatchdogConfig {
            enabled: true,
            severity_threshold: "WARN".into(),
            log_file: Some(log_path.to_string_lossy().into()),
            ..WatchdogConfig::default()
        };
        let wd = Watchdog::new(cfg, dir.path().to_path_buf(), log_path.clone());
        let bus = Arc::new(EventBus::new());
        let ctx = HeartbeatContext {
            data_dir:  dir.path().to_path_buf(),
            event_bus: Some(Arc::clone(&bus)),
        };
        let outcome = wd.run(&ctx, &serde_json::Value::Null).await.unwrap();
        assert!(outcome.summary.contains("emitted=1"), "self-line should be filtered: {}", outcome.summary);
    }

    #[tokio::test]
    async fn watchdog_handles_missing_log_file() {
        use crate::events::EventBus;
        let dir = tempfile::tempdir().unwrap();
        let cfg = WatchdogConfig {
            enabled: true,
            log_file: Some(dir.path().join("nope.log").to_string_lossy().into()),
            ..WatchdogConfig::default()
        };
        let wd = Watchdog::new(cfg.clone(), dir.path().to_path_buf(), dir.path().join("nope.log"));
        let bus = Arc::new(EventBus::new());
        let ctx = HeartbeatContext {
            data_dir:  dir.path().to_path_buf(),
            event_bus: Some(Arc::clone(&bus)),
        };
        // Missing log → graceful skip, not an error.
        let outcome = wd.run(&ctx, &serde_json::Value::Null).await.unwrap();
        // No "log read skipped" surfaces in the summary anymore (we
        // collect across all 3 sources and the missing log is just
        // an empty alerts list); the summary should still be a clean
        // success with 0 emitted.
        assert!(outcome.summary.contains("emitted=0"), "got: {}", outcome.summary);
    }

    // ── W2 unit tests ───────────────────────────────────────────────

    fn open_test_automations_db(path: &std::path::Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE automation_runs (
                id TEXT PRIMARY KEY,
                source_kind TEXT NOT NULL,
                source_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                started_at INTEGER NOT NULL,
                finished_at INTEGER,
                outcome TEXT NOT NULL,
                output_snippet TEXT,
                error TEXT,
                context TEXT
            );",
        ).unwrap();
        conn
    }

    #[tokio::test]
    async fn scan_automation_runs_returns_only_failures_after_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("automations.db");
        let conn = open_test_automations_db(&db_path);
        // 3 rows: 1 success (excluded), 1 failure before cursor (excluded),
        // 1 failure after cursor (included).
        conn.execute(
            "INSERT INTO automation_runs VALUES
              ('r1','schedule','sched-A','sys',100,100,'success','ok',NULL,NULL),
              ('r2','event',   'sub-B',  'sys',200,200,'failure',NULL,'old fail',NULL),
              ('r3','event',   'sub-C',  'sys',400,400,'failure',NULL,'new fail',NULL)",
            [],
        ).unwrap();
        let (alerts, max_seen) = scan_automation_runs(&db_path, 300).unwrap();
        assert_eq!(alerts.len(), 1, "only the post-cursor failure should surface");
        assert_eq!(alerts[0].source_id, "db:automation_runs");
        assert!(alerts[0].module.contains("event/sub-C") || alerts[0].module.contains("event/sub-C  ".trim()));
        assert_eq!(alerts[0].message, "new fail");
        assert_eq!(max_seen, 400);
    }

    #[tokio::test]
    async fn scan_automation_runs_handles_missing_db() {
        let dir = tempfile::tempdir().unwrap();
        // No DB file — should return clean empty + cursor unchanged.
        let (alerts, max_seen) = scan_automation_runs(
            &dir.path().join("nope.db"), 42,
        ).unwrap();
        assert!(alerts.is_empty());
        assert_eq!(max_seen, 42);
    }

    fn open_test_audit_db(path: &std::path::Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE agent_audit (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts_ms INTEGER NOT NULL,
                agent_id TEXT NOT NULL,
                event_kind TEXT NOT NULL,
                event_json TEXT NOT NULL,
                prev_hmac TEXT,
                hmac TEXT
            );",
        ).unwrap();
        conn
    }

    #[tokio::test]
    async fn scan_agent_audit_picks_alert_worthy_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("agent_audit.db");
        let conn = open_test_audit_db(&db_path);
        conn.execute(
            "INSERT INTO agent_audit (ts_ms, agent_id, event_kind, event_json) VALUES
              (100, 'a1', 'spawn_requested',   '{}'),
              (200, 'a2', 'spawn_denied',      '{\"reason\":\"depth_exceeded\"}'),
              (300, 'a3', 'agent_budget_exceeded', '{\"spent_usd\":1.5,\"cap_usd\":1.0}'),
              (400, 'a4', 'status_change',     '{\"from\":\"running\",\"to\":\"completed\"}'),
              (500, 'a5', 'status_change',     '{\"from\":\"running\",\"to\":\"failed\"}'),
              (600, 'a6', 'policy_decision',   '{\"granted\":true,\"rule\":\"x\",\"detail\":\"ok\"}'),
              (700, 'a7', 'policy_decision',   '{\"granted\":false,\"rule\":\"budget\",\"detail\":\"too_much\"}'),
              (800, 'a8', 'interrupted',       '{\"reason\":\"user\"}')",
            [],
        ).unwrap();
        let (alerts, max_seen) = scan_agent_audit(&db_path, 0).unwrap();
        // Expected: spawn_denied(200), agent_budget_exceeded(300),
        // status_change→failed(500), policy_decision granted=false(700).
        // Skipped: spawn_requested, status→completed, policy granted, interrupted.
        let kinds: Vec<&str> = alerts.iter()
            .map(|a| a.module.split('/').nth(1).unwrap_or(""))
            .collect();
        assert_eq!(kinds, vec![
            "spawn_denied",
            "agent_budget_exceeded",
            "status_change",
            "policy_decision",
        ], "got alerts: {alerts:#?}");
        assert_eq!(max_seen, 800, "high-water mark must advance past every row, not just the alerting ones");
        // Spot-check one rendered message.
        assert!(alerts.iter().any(|a| a.message.contains("spawn denied: depth_exceeded")));
        assert!(alerts.iter().any(|a| a.message.contains("policy denied [budget]: too_much")));
    }

    // ── W4 unit tests ───────────────────────────────────────────────

    #[test]
    fn redactor_masks_aws_access_key() {
        let r = Redactors::new();
        let s = r.redact("creds: AKIAIOSFODNN7EXAMPLE leaked");
        assert!(s.contains("AKIA***REDACTED***"), "got: {s}");
        assert!(!s.contains("AKIAIOSFODNN7EXAMPLE"), "raw key should be gone: {s}");
    }

    #[test]
    fn redactor_masks_jwt_token() {
        let r = Redactors::new();
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ4eHgifQ.abcdefghij1234567890";
        let s = r.redact(&format!("Authorization: {jwt}"));
        assert!(s.contains("eyJ***JWT-REDACTED***"), "got: {s}");
        assert!(!s.contains("abcdefghij"));
    }

    #[test]
    fn redactor_masks_url_password_keeping_scheme_and_host() {
        let r = Redactors::new();
        let s = r.redact("connecting postgres://alice:hunter2@db.example/myapp");
        assert!(s.contains("postgres://alice:***REDACTED***@db.example/myapp"), "got: {s}");
    }

    #[test]
    fn redactor_masks_email_local_part_only() {
        let r = Redactors::new();
        let s = r.redact("alert sent to ops@example.com about issue");
        assert!(s.contains("***@example.com"), "got: {s}");
        assert!(!s.contains("ops@"), "local part should be gone: {s}");
    }

    #[test]
    fn redactor_masks_phone_e164() {
        let r = Redactors::new();
        let s = r.redact("user phone +14155552671 hit rate limit");
        assert!(s.contains("+***REDACTED***"), "got: {s}");
        assert!(!s.contains("4155552671"));
    }

    #[tokio::test]
    async fn watchdog_storm_pause_trips_after_threshold_and_drops_subsequent() {
        use crate::events::EventBus;
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("test.log");
        // Generate 5 distinct WARN lines (each unique → 5 fingerprints).
        let mut content = String::new();
        for i in 0..5 {
            content.push_str(&format!(
                "2026-05-09T12:00:0{i}.000000Z  WARN mira::test::ev{i}: unique error number {i}\n"
            ));
        }
        std::fs::write(&log_path, content).unwrap();
        seed_state_for_test(dir.path());
        let cfg = WatchdogConfig {
            enabled: true,
            severity_threshold: "WARN".into(),
            dedup_ttl_secs: 1,
            rate_limit_per_min: 1000,
            storm_threshold: 3,    // trip on the 3rd alert
            storm_window_secs: 60,
            storm_cooldown_secs: 120,
            log_file: Some(log_path.to_string_lossy().into()),
            ..WatchdogConfig::default()
        };
        let wd = Watchdog::new(cfg, dir.path().to_path_buf(), log_path.clone());
        let bus = Arc::new(EventBus::new());
        let mut rx = bus.subscribe();
        let ctx = HeartbeatContext {
            data_dir:  dir.path().to_path_buf(),
            event_bus: Some(Arc::clone(&bus)),
        };
        let outcome = wd.run(&ctx, &serde_json::Value::Null).await.unwrap();
        // Expect: 2 alerts pass through normal emit (counts under
        // `emitted`); the 3rd trips the storm and is dropped; 4th
        // and 5th drop because the source is paused. 1 storm
        // notification fires in their place (counts under
        // `storm_pauses`, not `emitted`).
        assert!(outcome.summary.contains("emitted=2"), "2 normal alerts expected, got: {}", outcome.summary);
        assert!(outcome.summary.contains("storm_pauses=1"), "exactly 1 storm notification, got: {}", outcome.summary);
        assert!(outcome.summary.contains("storm_dropped=3"),
            "trigger + 2 subsequent dropped, got: {}", outcome.summary);

        // Walk the bus events — exactly one event should have
        // module starting with `watchdog:storm:`.
        let mut storm_events = 0;
        while let Ok(ev) = rx.try_recv() {
            if ev.payload["module"].as_str().unwrap_or("").starts_with("watchdog:storm:") {
                storm_events += 1;
            }
        }
        assert_eq!(storm_events, 1, "exactly one storm notification expected");
    }

    #[tokio::test]
    async fn watchdog_first_boot_does_not_replay_history() {
        // Open the DB with historical failures predating the
        // watchdog. On Watchdog::new with a missing state file, the
        // cursor must be initialised to "now" so the first scan
        // doesn't surface every row in history.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("automations.db");
        let conn = open_test_automations_db(&db_path);
        // 5 historical failures, all in the past.
        for i in 0..5 {
            conn.execute(
                "INSERT INTO automation_runs VALUES (?, 'schedule', 'sched-A', 'sys', ?, ?, 'failure', NULL, ?, NULL)",
                rusqlite::params![format!("r{i}"), 100 + i, 100 + i, format!("hist-{i}")],
            ).unwrap();
        }

        let cfg = WatchdogConfig {
            enabled: true,
            // Disable log scanning by pointing at a missing file.
            log_file: Some(dir.path().join("nope.log").to_string_lossy().into()),
            ..WatchdogConfig::default()
        };
        let wd = Watchdog::new(cfg, dir.path().to_path_buf(), dir.path().join("nope.log"));
        let bus = Arc::new(crate::events::EventBus::new());
        let ctx = HeartbeatContext {
            data_dir:  dir.path().to_path_buf(),
            event_bus: Some(Arc::clone(&bus)),
        };
        let outcome = wd.run(&ctx, &serde_json::Value::Null).await.unwrap();
        assert!(outcome.summary.contains("emitted=0"), "first boot must not replay history: {}", outcome.summary);
    }
}
