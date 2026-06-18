// SPDX-License-Identifier: AGPL-3.0-or-later

// src/agent/run_logs.rs
//! 0.113.0 — per-agent stdout/stderr/progress tee.
//!
//! While a subagent runs, the adapter tees its stdout + stderr lines
//! and emitted Progress events to files inside the task's artifact
//! dir (`<output_dir>/logs/`). The Agents detail page reads these
//! for "live terminal" + "rewatch completed task" views.
//!
//! Capped at MAX_LOG_BYTES per file to keep disk usage bounded for a
//! runaway-output agent. Truncation is silent in the file — only the
//! first N MB land. The capping prevents a single rogue task from
//! filling the artifact volume; the user can still inspect what got
//! captured.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use std::fs::{File, OpenOptions};
use std::io::Write;

const MAX_LOG_BYTES: usize = 5 * 1024 * 1024;  // 5 MB per file

pub struct AgentLogTees {
    inner: Mutex<TeeState>,
}

struct TeeState {
    stdout:        Option<File>,
    stderr:        Option<File>,
    progress:      Option<File>,
    stdout_bytes:  usize,
    stderr_bytes:  usize,
    progress_bytes: usize,
}

impl AgentLogTees {
    /// Construct a tees handle for the given output_dir. None when
    /// the artifact-dir context isn't set (legacy spawn path, tests).
    /// Failures to open are silent — agent run continues, just no
    /// log files for the detail page to read.
    pub fn open_for(output_dir: Option<&Path>) -> Self {
        let state = match output_dir {
            Some(dir) => {
                let logs_dir = dir.join("logs");
                let _ = std::fs::create_dir_all(&logs_dir);
                TeeState {
                    stdout:   OpenOptions::new().append(true).create(true).open(logs_dir.join("stdout.log")).ok(),
                    stderr:   OpenOptions::new().append(true).create(true).open(logs_dir.join("stderr.log")).ok(),
                    progress: OpenOptions::new().append(true).create(true).open(logs_dir.join("progress.jsonl")).ok(),
                    stdout_bytes: 0, stderr_bytes: 0, progress_bytes: 0,
                }
            }
            None => TeeState {
                stdout: None, stderr: None, progress: None,
                stdout_bytes: 0, stderr_bytes: 0, progress_bytes: 0,
            },
        };
        Self { inner: Mutex::new(state) }
    }

    pub fn write_stdout(&self, line: &str) {
        if let Ok(mut g) = self.inner.lock() {
            if g.stdout_bytes >= MAX_LOG_BYTES { return; }
            if let Some(f) = g.stdout.as_mut() {
                let _ = writeln!(f, "{line}");
                g.stdout_bytes += line.len() + 1;
            }
        }
    }

    pub fn write_stderr(&self, line: &str) {
        if let Ok(mut g) = self.inner.lock() {
            if g.stderr_bytes >= MAX_LOG_BYTES { return; }
            if let Some(f) = g.stderr.as_mut() {
                let _ = writeln!(f, "{line}");
                g.stderr_bytes += line.len() + 1;
            }
        }
    }

    /// One JSONL row per Progress event. Schema kept stable so the
    /// activity endpoint can deserialize directly.
    pub fn write_progress(&self, summary: &str, percent_done: Option<f32>, llm_spend_usd: f64) {
        if let Ok(mut g) = self.inner.lock() {
            if g.progress_bytes >= MAX_LOG_BYTES { return; }
            if let Some(f) = g.progress.as_mut() {
                let row = serde_json::json!({
                    "ts_ms":         chrono::Utc::now().timestamp_millis(),
                    "summary":       summary,
                    "percent_done":  percent_done,
                    "llm_spend_usd": llm_spend_usd,
                });
                if let Ok(line) = serde_json::to_string(&row) {
                    let _ = writeln!(f, "{line}");
                    g.progress_bytes += line.len() + 1;
                }
            }
        }
    }
}

/// Pull `output_dir` from a `WorkerAssignment.context`. Centralised so
/// both adapters and the supervisor agree on the key name.
pub fn output_dir_from_assignment(
    ctx: &Option<serde_json::Value>,
) -> Option<PathBuf> {
    ctx.as_ref()
        .and_then(|c| c.get("output_dir"))
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_three_files_in_logs_dir() {
        let dir = tempfile::tempdir().unwrap();
        let tees = AgentLogTees::open_for(Some(dir.path()));
        tees.write_stdout("hello stdout");
        tees.write_stderr("hello stderr");
        tees.write_progress("step 1", Some(0.25), 0.0012);
        let logs = dir.path().join("logs");
        assert!(logs.join("stdout.log").exists());
        assert!(logs.join("stderr.log").exists());
        assert!(logs.join("progress.jsonl").exists());
        let s = std::fs::read_to_string(logs.join("stdout.log")).unwrap();
        assert!(s.contains("hello stdout"));
    }

    #[test]
    fn caps_at_max_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let tees = AgentLogTees::open_for(Some(dir.path()));
        let big = "x".repeat(MAX_LOG_BYTES + 1024);
        tees.write_stdout(&big);
        tees.write_stdout("after cap");  // should be dropped
        let s = std::fs::read_to_string(dir.path().join("logs/stdout.log")).unwrap();
        assert!(!s.contains("after cap"));
    }

    #[test]
    fn none_output_dir_is_safe() {
        let tees = AgentLogTees::open_for(None);
        // All writes are no-ops; just checking we don't panic.
        tees.write_stdout("x");
        tees.write_progress("y", None, 0.0);
    }
}
