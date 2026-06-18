// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/guardian_inspect.rs
//! `guardian_inspect` — the MIRA-Guardian's read-only window into the running
//! instance: the latest health snapshot, active subsystem degradations, and a
//! bounded tail of the application log.
//!
//! Strictly read-only and diagnostic — no network, no mutation. It is
//! `System`-visibility (hidden from the normal user tool palette since logs can
//! contain sensitive detail); the Guardian agent reaches it via its explicit
//! `allowed_tools` allowlist, which bypasses the palette filter. See
//! `design-docs/guardian-agent.md` (P1).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::health::degradation::DegradationTracker;
use crate::health::store::HealthStore;
use crate::tools::{Tool, ToolArgs, ToolResult, ToolVisibility, Tier};
use crate::MiraError;

const DEFAULT_LOG_LINES: usize = 40;
const MAX_LOG_LINES: usize = 200;
/// Cap on bytes read from the tail of the log file, so a huge log never blows
/// the read. We decode this window lossily and keep the last N whole lines.
const LOG_TAIL_BYTES: u64 = 256 * 1024;

pub struct GuardianInspectTool {
    health:       Option<Arc<HealthStore>>,
    degradations: Option<Arc<DegradationTracker>>,
    log_path:     Option<PathBuf>,
}

impl GuardianInspectTool {
    pub fn new(
        health:       Option<Arc<HealthStore>>,
        degradations: Option<Arc<DegradationTracker>>,
        log_path:     Option<PathBuf>,
    ) -> Self {
        Self { health, degradations, log_path }
    }
}

#[async_trait]
impl Tool for GuardianInspectTool {
    fn name(&self) -> &str { "guardian_inspect" }

    fn description(&self) -> &str {
        "Read MIRA's current operational state: the latest health snapshot (detector \
         levels + messages), active subsystem degradations, and a tail of the application \
         log. Read-only. Pass `what` = \"health\" | \"degradations\" | \"logs\" | \"all\" \
         (default \"all\"), and optionally `log_lines` (default 40, max 200)."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "what": {
                    "type": "string",
                    "enum": ["health", "degradations", "logs", "all"],
                    "description": "Which section(s) to return. Default \"all\"."
                },
                "log_lines": {
                    "type": "integer",
                    "description": "How many trailing log lines to include (default 40, max 200)."
                }
            }
        })
    }

    fn visibility(&self) -> ToolVisibility { ToolVisibility::system("guardian") }
    fn tier(&self) -> Tier { Tier::Filesystem }
    fn enabled(&self) -> bool { self.health.is_some() || self.log_path.is_some() }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let what = args.get("what").and_then(|v| v.as_str()).unwrap_or("all").to_ascii_lowercase();
        let log_lines = args.get("log_lines")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).min(MAX_LOG_LINES))
            .unwrap_or(DEFAULT_LOG_LINES);

        let mut out = String::new();
        let want = |s: &str| what == "all" || what == s;

        if want("health") {
            out.push_str(&self.render_health());
            out.push('\n');
        }
        if want("degradations") {
            out.push_str(&self.render_degradations());
            out.push('\n');
        }
        if want("logs") {
            out.push_str(&self.render_logs(log_lines));
            out.push('\n');
        }
        if out.trim().is_empty() {
            out = "No inspection sections selected.".to_string();
        }
        Ok(ToolResult::success(out.trim_end().to_string()))
    }
}

impl GuardianInspectTool {
    fn render_health(&self) -> String {
        let Some(store) = self.health.as_ref() else {
            return "## Health\n(health store unavailable in this build)".to_string();
        };
        match store.latest() {
            Ok(Some(snap)) => {
                let mut s = format!(
                    "## Health\nworst_level: {:?}\ntriggered: {}\nchecked_at(unix): {}\n",
                    snap.worst_level(), snap.triggered_count(), snap.taken_at,
                );
                // List non-green detectors first (the actionable ones); cap the
                // dump so a noisy snapshot can't blow the context window.
                let mut shown = 0;
                for r in snap.reports.iter().filter(|r| !matches!(r.level, crate::health::HealthLevel::Green)) {
                    s.push_str(&format!("- [{:?}] {}: {}\n", r.level, r.name, r.message.trim()));
                    shown += 1;
                    if shown >= 40 { s.push_str("- … (truncated)\n"); break; }
                }
                if shown == 0 {
                    s.push_str("- all detectors green\n");
                }
                s
            }
            Ok(None) => "## Health\n(no snapshot recorded yet — the audit may not have run)".to_string(),
            Err(e)   => format!("## Health\n(error reading snapshot: {e})"),
        }
    }

    fn render_degradations(&self) -> String {
        let Some(tracker) = self.degradations.as_ref() else {
            return "## Degradations\n(degradation tracker unavailable)".to_string();
        };
        let active = tracker.active();
        if active.is_empty() {
            return "## Degradations\n- none active".to_string();
        }
        let mut s = String::from("## Degradations\n");
        for d in &active {
            s.push_str(&format!(
                "- {} ({}): {} → {} — {} [{}, x{}]\n",
                d.subsystem, d.label, d.from, d.to, d.reason.trim(),
                if d.persistent { "persistent" } else { "transient" }, d.count,
            ));
        }
        s
    }

    fn render_logs(&self, lines: usize) -> String {
        let Some(path) = self.log_path.as_ref() else {
            return "## Logs\n(log path not configured for this instance)".to_string();
        };
        match tail_lines(path, lines) {
            Ok(text) if !text.trim().is_empty() => format!("## Logs (last {lines})\n{text}"),
            Ok(_)  => "## Logs\n(log file empty)".to_string(),
            Err(e) => format!("## Logs\n(could not read {}: {e})", path.display()),
        }
    }
}

/// Read the last `n` whole lines of a file without loading all of it: seek to
/// the final [`LOG_TAIL_BYTES`] window, decode lossily, drop the first partial
/// line, and keep the last `n`.
fn tail_lines(path: &std::path::Path, n: usize) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(LOG_TAIL_BYTES);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    // If we started mid-file, the first line is likely partial — drop it.
    let mut iter = text.lines().peekable();
    if start > 0 { iter.next(); }
    let all: Vec<&str> = iter.collect();
    let tail = if all.len() > n { &all[all.len() - n..] } else { &all[..] };
    Ok(tail.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_gracefully_when_stores_absent() {
        let t = GuardianInspectTool::new(None, None, None);
        let r = t.execute(json!({"what": "all"})).await.unwrap();
        assert!(r.success);
        assert!(r.output.contains("Health"));
        assert!(!t.enabled());
    }

    #[test]
    fn tail_lines_returns_last_n() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.log");
        let body: String = (0..100).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&p, body).unwrap();
        let out = tail_lines(&p, 5).unwrap();
        let got: Vec<&str> = out.lines().collect();
        assert_eq!(got, vec!["line 95", "line 96", "line 97", "line 98", "line 99"]);
    }
}
