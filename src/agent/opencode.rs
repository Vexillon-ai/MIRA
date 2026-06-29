// SPDX-License-Identifier: AGPL-3.0-or-later

//! OpenCode adapter (slice C3).
//!
//! Wraps the `opencode` CLI (sst/opencode) as a
//! [`crate::agent::supervisor::WorkerTask`]. Same shape as the C2
//! Claude Code adapter, but adapted to OpenCode's distinct output
//! format and exit-code conventions.
//!
//! Spawn shape:
//!
//!   opencode run --format json [--model PROVIDER/MODEL] [--agent A] \
//!                [--dir D] [--dangerously-skip-permissions] \
//!                [--thinking] [--print-logs] <prompt>
//!
//! NDJSON parsing rules:
//!
//!   - `step_start` events → silent (no Progress emitted; they're
//!     too chatty and carry no user-facing info).
//!   - `text` events → Progress with the text content (truncated to
//!     400 chars for the UI; full text reaches stdout via OpenCode's
//!     own session log).
//!   - `tool_use` events → Progress tagged with the tool name plus
//!     OpenCode's own `state.title` if present (e.g. "[tool_use]
//!     bash: List files in /tmp"). Far more useful than the bare
//!     tool name alone.
//!   - `step_finish` events → Progress with the *running* cost so the
//!     supervisor's session-budget math sees spend as it happens
//!     (Claude Code only reports total cost at the end; OpenCode
//!     reports per-step). Reason field surfaces stop / tool-calls /
//!     length so debugging is possible from the audit log.
//!   - `error` events → terminal Failed. OpenCode emits these instead
//!     of crashing — exit code is unreliable as a failure signal.
//!
//! Terminal logic differs from C2:
//!
//!   - OpenCode has *no single "result" event*. Each agent step ends
//!     with `step_finish`; multi-step runs emit several. The terminal
//!     state is "stdout EOF + child exited cleanly + we saw at least
//!     one text block + no error event seen."
//!   - Process exit is generally 0 even on errors (the JSON `error`
//!     event is canonical). We still surface non-zero exit + stderr
//!     tail when exit code AND no terminal events both fail us — that
//!     handles the "spawned the wrong binary" / argv error case where
//!     OpenCode dumps usage to stderr and exits 1 without any JSON.
//!
//! Cost accounting:
//!
//!   - We sum `part.cost` across every `step_finish` event and forward
//!     each step's cost as `llm_spend_usd` on its Progress event. The
//!     manager's running-spend tracking already handles cumulative
//!     deltas correctly (it does `delta = current - last_seen`), so
//!     reporting per-step lets the per-agent budget kill trigger as
//!     spend accumulates rather than only at the end.
//!   - When the model has no cost (local provider, free tier),
//!     OpenCode reports `cost: 0` and the budget never fires. Same
//!     behaviour as Claude Code on the same setup.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{debug, warn};

use crate::agent::protocol::Event;
use crate::agent::supervisor::{
    WorkerAssignment, WorkerComplete, WorkerContext, WorkerFailure, WorkerTask,
};

/// Soft cap on captured-stream bytes — same rationale as C2.
const MAX_STREAM_BYTES: usize = 4 * 1024 * 1024;

/// Truncate Progress summaries past this so the agents UI stays
/// readable on large text blocks.
const PROGRESS_TRUNCATE_CHARS: usize = 400;

#[derive(Debug, Clone)]
pub struct OpenCodeConfig {
    /// Path to the `opencode` binary. Default: `"opencode"` (resolves via PATH).
    pub binary: PathBuf,

    /// Working directory. None = inherit. OpenCode also accepts `--dir`
    /// which we set when this is provided so the agent's pwd matches
    /// what the user intended (the two can diverge under bash if
    /// CWD-aware shell init runs at spawn).
    pub cwd: Option<PathBuf>,

    /// `--model PROVIDER/MODEL`. None = let OpenCode use its configured default.
    pub model: Option<String>,

    /// `--agent <name>` — picks one of the agents declared in the
    /// user's OpenCode config. None = default agent.
    pub agent: Option<String>,

    /// Pass `--dangerously-skip-permissions`. Required for unattended
    /// runs where there's no human to click "approve" prompts.
    pub skip_permissions: bool,

    /// Pass `--thinking` to surface model thinking blocks in the JSON
    /// stream. Off by default — usually too verbose for the UI.
    pub show_thinking: bool,

    /// Pass `--print-logs` so OpenCode's internal logs reach stderr
    /// (useful for debugging adapter issues; off by default).
    pub print_logs: bool,
}

impl Default for OpenCodeConfig {
    fn default() -> Self {
        Self {
            binary:           "opencode".into(),
            cwd:              None,
            model:            None,
            agent:            None,
            skip_permissions: false,
            show_thinking:    false,
            print_logs:       false,
        }
    }
}

impl OpenCodeConfig {
    pub fn new() -> Self { Self::default() }

    pub fn with_binary(mut self, path: impl Into<PathBuf>)   -> Self { self.binary = path.into(); self }
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>)       -> Self { self.cwd = Some(cwd.into()); self }
    pub fn with_model(mut self, model: impl Into<String>)    -> Self { self.model = Some(model.into()); self }
    pub fn with_agent(mut self, agent: impl Into<String>)    -> Self { self.agent = Some(agent.into()); self }
    pub fn with_skip_permissions(mut self, skip: bool)       -> Self { self.skip_permissions = skip; self }
    pub fn with_thinking(mut self, show: bool)               -> Self { self.show_thinking = show; self }
    pub fn with_print_logs(mut self, on: bool)               -> Self { self.print_logs = on; self }
}

/// `WorkerTask` implementation that runs OpenCode as a subagent.
pub struct OpenCodeAdapter {
    config:               OpenCodeConfig,
    secrets:              Option<Arc<crate::skills::SecretsStore>>,
    skill_id_for_secrets: String,
}

impl OpenCodeAdapter {
    pub fn new(config: OpenCodeConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            secrets: None,
            skill_id_for_secrets: "com.mira.opencode".to_string(),
        })
    }

    /// Plug in a [`SecretsStore`] for env-var injection. See
    /// `ClaudeCodeAdapter::with_secrets` for semantics.
    pub fn with_secrets(
        mut self: Arc<Self>,
        store: Arc<crate::skills::SecretsStore>,
    ) -> Arc<Self> {
        let inner = Arc::get_mut(&mut self)
            .expect("OpenCodeAdapter::with_secrets called on aliased Arc");
        inner.secrets = Some(store);
        self
    }

    pub fn with_skill_id(
        mut self: Arc<Self>,
        skill_id: impl Into<String>,
    ) -> Arc<Self> {
        let inner = Arc::get_mut(&mut self)
            .expect("OpenCodeAdapter::with_skill_id called on aliased Arc");
        inner.skill_id_for_secrets = skill_id.into();
        self
    }
}

// ─── NDJSON event shapes ───────────────────────────────────────────────
//
// Same `allow(dead_code)` strategy as C2: keep extra fields typed so
// they're available the moment a consumer wants them.

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum OcEvent {
    StepStart   (StepStartEvent),
    StepFinish  (StepFinishEvent),
    Text        (TextEvent),
    ToolUse     (ToolUseEvent),
    Error       (ErrorEvent),
    /// Future event types we haven't seen — ignored at the adapter layer.
    #[serde(other)]
    Other,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub(crate) struct StepStartEvent {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub timestamp:  Option<i64>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub(crate) struct StepFinishEvent {
    #[serde(default)]
    pub timestamp:  Option<i64>,
    #[serde(default)]
    pub session_id: Option<String>,
    pub part:       StepFinishPart,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub(crate) struct StepFinishPart {
    /// "stop", "tool-calls", "length", etc. — useful for forensics.
    #[serde(default)]
    pub reason: Option<String>,
    /// Cost for *this step*, in USD. Sum across step_finish events to get
    /// the total run cost.
    #[serde(default)]
    pub cost:   f64,
    #[serde(default)]
    pub tokens: Option<TokenStats>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub(crate) struct TokenStats {
    #[serde(default)]
    pub total:     u64,
    #[serde(default)]
    pub input:     u64,
    #[serde(default)]
    pub output:    u64,
    #[serde(default)]
    pub reasoning: u64,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub(crate) struct TextEvent {
    pub part: TextPart,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub(crate) struct TextPart {
    pub text: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub(crate) struct ToolUseEvent {
    pub part: ToolUsePart,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub(crate) struct ToolUsePart {
    pub tool: String,
    /// May be missing when the tool call is still in flight; the title
    /// field within state is the human-readable label.
    #[serde(default)]
    pub state: Option<ToolUseState>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub(crate) struct ToolUseState {
    #[serde(default)]
    pub title:  Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub(crate) struct ErrorEvent {
    pub error: ErrorPayload,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub(crate) struct ErrorPayload {
    #[serde(default)]
    pub name: Option<String>,
    pub data: serde_json::Value,
}

/// What one parsed line resolves to. The adapter accumulates these as
/// it reads stdout, then decides Complete/Failed at EOF.
#[derive(Debug, PartialEq)]
pub(crate) enum LineOutcome {
    /// Forward as `Event::Progress`. `cost_usd` is the spend reported
    /// by THIS event (per-step for `step_finish`, 0 for everything else).
    Progress { summary: String, cost_usd: f64 },
    /// A `text` event — same as Progress but the parser also stashes
    /// the text as a candidate `result_summary` (last text wins).
    Text     { text: String },
    /// `step_finish` — like Progress, but separately surfaced so the
    /// caller can sum costs across all steps.
    StepFinish { reason: String, cost_usd: f64 },
    /// Terminal `error` event from OpenCode.
    Failed   { error: String },
    /// Boring / unrecognised line — adapter logs at debug and moves on.
    Skip,
}

/// Pure: parse one NDJSON line.
pub(crate) fn parse_line(line: &str) -> LineOutcome {
    let trimmed = line.trim();
    if trimmed.is_empty() { return LineOutcome::Skip; }

    let event: OcEvent = match serde_json::from_str(trimmed) {
        Ok(e)  => e,
        Err(e) => {
            debug!("opencode: skipping unparseable line ({e}): {trimmed}");
            return LineOutcome::Skip;
        }
    };

    match event {
        OcEvent::StepStart(_) => LineOutcome::Skip,
        OcEvent::Text(t) => LineOutcome::Text { text: t.part.text },
        OcEvent::ToolUse(tu) => {
            let title = tu.part.state.as_ref()
                .and_then(|s| s.title.as_deref())
                .filter(|t| !t.is_empty());
            let summary = match title {
                Some(t) => format!("[tool_use] {}: {}",
                    tu.part.tool,
                    truncate(t, PROGRESS_TRUNCATE_CHARS - 32),
                ),
                None    => format!("[tool_use] {}", tu.part.tool),
            };
            LineOutcome::Progress { summary, cost_usd: 0.0 }
        }
        OcEvent::StepFinish(sf) => {
            let reason = sf.part.reason.unwrap_or_else(|| "unknown".into());
            LineOutcome::StepFinish { reason, cost_usd: sf.part.cost }
        }
        OcEvent::Error(e) => {
            // The error payload's shape is open-ended (varies by failure
            // type). Try a few well-known fields, fall back to JSON.
            let detail = e.error.data.get("message")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| e.error.data.to_string());
            let kind = e.error.name.as_deref().unwrap_or("Error");
            LineOutcome::Failed { error: format!("{kind}: {detail}") }
        }
        OcEvent::Other => LineOutcome::Skip,
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars { return s.to_string(); }
    let head: String = s.chars().take(max_chars).collect();
    format!("{head}…")
}

#[async_trait]
impl WorkerTask for OpenCodeAdapter {
    async fn run(
        &self,
        assignment: WorkerAssignment,
        ctx:        WorkerContext,
    ) -> Result<WorkerComplete, WorkerFailure> {
        // Resolve the CLI fresh at spawn (PATH + common install dirs + MIRA's
        // managed npm install under ~/.mira/deps) so a CLI installed *after*
        // boot via the one-click skill install works without a restart — but
        // ONLY when the binary is still the bare default. An explicitly
        // configured path (the builder's boot-time resolution, or a test's fake
        // binary) always wins. The helper augments the child PATH for the CLI's
        // own node subprocess and routes Windows `.cmd` shims through `cmd /C`.
        let binary = if self.config.binary == std::path::Path::new("opencode") {
            crate::install::deps::resolve_external_cli("opencode")
                .unwrap_or_else(|| self.config.binary.clone())
        } else {
            self.config.binary.clone()
        };
        let mut cmd = crate::install::deps::external_cli_command(&binary);
        cmd.arg("run")
            .arg("--format").arg("json")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // 0.111.0 — per-task artifact dir overrides the static cwd.
        // The brief tells the agent its dir is `$MIRA_TASK_OUTPUT_DIR`;
        // setting cwd + --dir + env makes that the natural default.
        let task_output_dir: Option<std::path::PathBuf> = assignment.context.as_ref()
            .and_then(|c| c.get("output_dir"))
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from);
        let active_cwd: Option<&std::path::Path> = task_output_dir.as_deref()
            .or(self.config.cwd.as_deref());
        if let Some(cwd) = active_cwd {
            cmd.current_dir(cwd);
            cmd.arg("--dir").arg(cwd);
            cmd.env("MIRA_TASK_OUTPUT_DIR", cwd);
        }

        // Skill secrets — env vars from the vault, scoped to this
        // worker's user (or system-wide when unset). `OPENCODE_MODEL`
        // is a *synthetic* vault key: opencode doesn't read a model
        // from env (only `--model` flag or its own config file), so
        // we pop it here and forward it as `--model VAL`. Vault wins
        // over `OpenCodeConfig.model`. Everything else passes through
        // unchanged so per-provider keys (`OPENROUTER_API_KEY`, etc.)
        // reach the subprocess.
        let (env_for_subprocess, model_from_vault): (
            std::collections::HashMap<String, String>,
            Option<String>,
        ) = if let Some(store) = &self.secrets {
            let mut env = store.env_vars_for(
                assignment.user_id.as_deref(),
                &self.skill_id_for_secrets,
            );
            let model = env.remove("OPENCODE_MODEL").filter(|s| !s.trim().is_empty());
            (env, model)
        } else {
            (std::collections::HashMap::new(), None)
        };
        let chosen_model = model_from_vault.or_else(|| self.config.model.clone());
        if let Some(m) = &chosen_model { cmd.arg("--model").arg(m); }
        if let Some(agent) = &self.config.agent { cmd.arg("--agent").arg(agent); }
        if self.config.skip_permissions {
            cmd.arg("--dangerously-skip-permissions");
        }
        if self.config.show_thinking { cmd.arg("--thinking"); }
        if self.config.print_logs    { cmd.arg("--print-logs"); }

        let injected_env_keys: Vec<String> = env_for_subprocess.keys().cloned().collect();
        for (k, v) in env_for_subprocess {
            cmd.env(k, v);
        }

        // Final positional: the prompt. OpenCode `run` takes `[message..]`
        // as a variadic positional; we pass one shell-quoted argument.
        let prompt = build_prompt(&assignment);
        cmd.arg(&prompt);

        debug!(
            "opencode: spawning {:?} cwd={:?} model={:?} env_keys={:?}",
            self.config.binary, self.config.cwd, chosen_model,
            injected_env_keys,
        );

        let mut child = match cmd.spawn() {
            Ok(c)  => c,
            Err(e) => return Err(WorkerFailure {
                error: format!("spawn {:?}: {e}", self.config.binary),
                partial_artifacts: vec![], fault: None,
            }),
        };

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // 0.113.0 — tee each line to the artifact dir's logs/ files
        // for the Agents detail page.
        let tees = std::sync::Arc::new(
            super::run_logs::AgentLogTees::open_for(
                super::run_logs::output_dir_from_assignment(&assignment.context).as_deref(),
            ),
        );

        // Drain stderr so OpenCode's logs (when --print-logs is on, or
        // when it's complaining about argv) don't deadlock the stdout
        // reader by filling the OS pipe buffer.
        let tees_for_stderr = std::sync::Arc::clone(&tees);
        let stderr_task = tokio::spawn(async move {
            let mut buf = String::new();
            let mut reader = BufReader::new(stderr).lines();
            let mut total = 0usize;
            let mut hit_cap = false;
            while let Ok(Some(line)) = reader.next_line().await {
                tees_for_stderr.write_stderr(&line);
                if total + line.len() > MAX_STREAM_BYTES {
                    if !hit_cap {
                        warn!("opencode stderr exceeded {MAX_STREAM_BYTES}B — truncating");
                        hit_cap = true;
                    }
                    continue;
                }
                total += line.len();
                buf.push_str(&line);
                buf.push('\n');
            }
            buf
        });

        // Stream-state. last_text is the running candidate for
        // result_summary; total_cost accumulates across step_finish events.
        let mut reader     = BufReader::new(stdout).lines();
        let mut last_text  = String::new();
        let mut total_cost = 0.0_f64;
        let mut got_terminal_failure: Option<String> = None;
        let mut total_bytes = 0usize;
        let mut over_cap = false;

        while let Ok(Some(line)) = reader.next_line().await {
            tees.write_stdout(&line);
            if total_bytes + line.len() > MAX_STREAM_BYTES {
                if !over_cap {
                    over_cap = true;
                    warn!("opencode stdout exceeded {MAX_STREAM_BYTES}B — dropping further lines");
                }
                continue;
            }
            total_bytes += line.len();

            match parse_line(&line) {
                LineOutcome::Skip => {}
                LineOutcome::Progress { summary, cost_usd } => {
                    tees.write_progress(&summary, None, total_cost + cost_usd);
                    let _ = ctx.sender_clone().send_event(Event::Progress {
                        step_summary: summary,
                        percent_done: None,
                        llm_spend_usd: total_cost + cost_usd,
                    });
                }
                LineOutcome::Text { text } => {
                    let display = truncate(&text, PROGRESS_TRUNCATE_CHARS);
                    last_text = text;
                    tees.write_progress(&display, None, total_cost);
                    let _ = ctx.sender_clone().send_event(Event::Progress {
                        step_summary: display,
                        percent_done: None,
                        llm_spend_usd: total_cost,
                    });
                }
                LineOutcome::StepFinish { reason, cost_usd } => {
                    total_cost += cost_usd;
                    let summary = format!(
                        "[step_finish] reason={reason} step_cost=${cost_usd:.4} total=${total_cost:.4}",
                    );
                    tees.write_progress(&summary, None, total_cost);
                    let _ = ctx.sender_clone().send_event(Event::Progress {
                        step_summary: summary,
                        percent_done: None,
                        llm_spend_usd: total_cost,
                    });
                }
                LineOutcome::Failed { error } => {
                    got_terminal_failure = Some(error);
                    break;
                }
            }
        }

        // Wait for the child even after seeing a terminal event so the
        // process exits cleanly (no zombies). The supervisor's deadline
        // / interrupt path bounds us if OpenCode hangs.
        let exit_status = child.wait().await;
        let stderr_buf  = stderr_task.await.unwrap_or_default();

        if let Some(err) = got_terminal_failure {
            return Err(WorkerFailure {
                error: format!("opencode: {err}"),
                partial_artifacts: vec![], fault: None,
            });
        }

        // OpenCode exits 0 even on most failures, so a non-zero exit
        // combined with no terminal events means something pathological
        // (bad argv, missing config, crash). Surface stderr tail in
        // that case so the user has a clue.
        let exited_clean = matches!(&exit_status, Ok(s) if s.success());
        if !exited_clean && last_text.is_empty() {
            let tail = tail_chars(&stderr_buf, 1024);
            let exit_str = match &exit_status {
                Ok(s)  => s.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
                Err(e) => format!("wait error: {e}"),
            };
            return Err(WorkerFailure {
                error: format!("opencode exited {exit_str} with no result (stderr tail: {tail})"),
                partial_artifacts: vec![], fault: None,
            });
        }

        // Clean exit + at least one text block = success. Empty text +
        // clean exit is unusual but we treat it as completed-without-output.
        let summary = if last_text.is_empty() {
            "(opencode exited cleanly with no text output)".into()
        } else {
            last_text
        };
        Ok(WorkerComplete {
            result_summary: summary,
            artifacts: vec![],
        })
    }
}

/// Pure: build the prompt argv from a [`WorkerAssignment`]. `context`
/// (when present) is appended as a fenced JSON block so the model can
/// read structured fields without us imposing a schema. Identical
/// shape to the C2 Claude Code helper — kept as a sibling rather than
/// shared so each adapter stays self-contained and easy to tweak.
fn build_prompt(asn: &WorkerAssignment) -> String {
    match &asn.context {
        None => asn.task.clone(),
        Some(ctx) => format!(
            "{task}\n\nContext:\n```json\n{ctx}\n```",
            task = asn.task,
            ctx  = serde_json::to_string_pretty(ctx).unwrap_or_else(|_| ctx.to_string()),
        ),
    }
}

fn tail_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n { return s.to_string(); }
    let skip = s.chars().count() - n;
    s.chars().skip(skip).collect()
}

// These tests exec fake `#!/bin/bash` scripts as stand-in binaries and
// chmod them, so the module is Unix-only; gate it so non-Unix targets
// (Windows) still compile `cargo test`.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::timeout;

    use crate::agent::instance::{Agent, AgentRegistry};
    use crate::agent::supervisor::{Supervisor, WorkerOutcome};

    // ── parse_line unit tests against shapes captured from a real binary ──

    #[test]
    fn parses_text_event_as_text_outcome() {
        // From a real `opencode run --format json "what is 2+2"`.
        let line = r#"{"type":"text","timestamp":1,"sessionID":"ses_x","part":{"id":"prt_x","messageID":"msg_x","sessionID":"ses_x","type":"text","text":"4","time":{"start":0,"end":1}}}"#;
        match parse_line(line) {
            LineOutcome::Text { text } => assert_eq!(text, "4"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn parses_tool_use_with_title_as_human_friendly_progress() {
        let line = r#"{"type":"tool_use","timestamp":1,"sessionID":"ses_x","part":{"type":"tool","tool":"bash","callID":"toolu_x","state":{"status":"completed","input":{"command":"ls /tmp"},"output":"x","title":"List files in /tmp directory","time":{"start":0,"end":1}},"id":"prt_x","sessionID":"ses_x","messageID":"msg_x"}}"#;
        match parse_line(line) {
            LineOutcome::Progress { summary, cost_usd } => {
                assert_eq!(cost_usd, 0.0);
                assert!(summary.contains("[tool_use] bash"), "got: {summary}");
                assert!(summary.contains("List files in /tmp"), "got: {summary}");
            }
            other => panic!("expected Progress, got {other:?}"),
        }
    }

    #[test]
    fn parses_tool_use_without_title_falls_back_to_bare_name() {
        let line = r#"{"type":"tool_use","timestamp":1,"sessionID":"ses_x","part":{"type":"tool","tool":"read","callID":"x","state":{"status":"running"}}}"#;
        match parse_line(line) {
            LineOutcome::Progress { summary, .. } => {
                assert_eq!(summary, "[tool_use] read");
            }
            other => panic!("expected Progress, got {other:?}"),
        }
    }

    #[test]
    fn parses_step_finish_with_cost_and_reason() {
        let line = r#"{"type":"step_finish","timestamp":1,"sessionID":"ses_x","part":{"id":"prt_x","reason":"tool-calls","snapshot":"abc","messageID":"msg_x","sessionID":"ses_x","type":"step-finish","tokens":{"total":100,"input":50,"output":50,"reasoning":0,"cache":{"write":0,"read":0}},"cost":0.025}}"#;
        match parse_line(line) {
            LineOutcome::StepFinish { reason, cost_usd } => {
                assert_eq!(reason, "tool-calls");
                assert!((cost_usd - 0.025).abs() < 1e-9);
            }
            other => panic!("expected StepFinish, got {other:?}"),
        }
    }

    #[test]
    fn parses_error_event_as_failed_with_message() {
        // Captured from `opencode run --format json --model no/such "hi"`.
        let line = r#"{"type":"error","timestamp":1,"sessionID":"ses_x","error":{"name":"UnknownError","data":{"message":"Model not found: no/such-model. Did you mean: opencode?"}}}"#;
        match parse_line(line) {
            LineOutcome::Failed { error } => {
                assert!(error.contains("UnknownError"), "got: {error}");
                assert!(error.contains("Model not found"), "got: {error}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn parses_error_event_falls_back_to_json_when_no_message_field() {
        let line = r#"{"type":"error","error":{"name":"WeirdError","data":{"foo":"bar"}}}"#;
        match parse_line(line) {
            LineOutcome::Failed { error } => {
                assert!(error.contains("WeirdError"), "got: {error}");
                // Falls back to JSON dump when message field is missing.
                assert!(error.contains("foo"), "got: {error}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn skips_step_start() {
        let line = r#"{"type":"step_start","timestamp":1,"sessionID":"ses_x","part":{"id":"prt_x","messageID":"msg_x","sessionID":"ses_x","snapshot":"abc","type":"step-start"}}"#;
        assert_eq!(parse_line(line), LineOutcome::Skip);
    }

    #[test]
    fn skips_unparseable_lines() {
        assert_eq!(parse_line(""), LineOutcome::Skip);
        assert_eq!(parse_line("not json"), LineOutcome::Skip);
        assert_eq!(parse_line("   \t  "), LineOutcome::Skip);
    }

    #[test]
    fn skips_unknown_event_types() {
        let line = r#"{"type":"future_event","payload":42}"#;
        assert_eq!(parse_line(line), LineOutcome::Skip);
    }

    // ── build_prompt tests (sibling helper to C2's; deliberately
    // duplicated so each adapter is self-contained) ──

    #[test]
    fn build_prompt_returns_task_alone_when_no_context() {
        let asn = WorkerAssignment {
            task: "hi".into(),
            context: None,
            budget_usd: 1.0,
            deadline_ms: None,
            ..Default::default()
        };
        assert_eq!(build_prompt(&asn), "hi");
    }

    #[test]
    fn build_prompt_appends_context_block() {
        let asn = WorkerAssignment {
            task: "do x".into(),
            context: Some(serde_json::json!({"k": "v"})),
            budget_usd: 1.0,
            deadline_ms: None,
            ..Default::default()
        };
        let p = build_prompt(&asn);
        assert!(p.starts_with("do x\n\nContext:\n```json\n"));
        assert!(p.contains(r#""k": "v""#));
    }

    // ── End-to-end with a fake binary ──

    fn fixture() -> (Arc<Supervisor>, crate::agent::instance::AgentId, u8) {
        let reg = Arc::new(AgentRegistry::new());
        let root = reg.register(Agent::new_root());
        let (root_id, depth) = { let r = root.read().unwrap(); (r.id, r.depth) };
        let sup = Arc::new(Supervisor::new(reg));
        (sup, root_id, depth)
    }

    /// Make an adapter whose binary is a tempfile shebang script that
    /// prints `payload` (and exits with `exit_code`). Lets us drive the
    /// full subprocess + parser path without a real opencode binary.
    fn fake_opencode(payload: &str, exit_code: i32) -> Arc<OpenCodeAdapter> {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(
            format!("mira-fake-opencode-{}.sh", uuid::Uuid::new_v4())
        );
        let mut f = std::fs::File::create(&path).expect("create fake oc");
        writeln!(f, "#!/bin/bash").unwrap();
        writeln!(f, "cat <<'PAYLOAD_EOF'").unwrap();
        f.write_all(payload.as_bytes()).unwrap();
        writeln!(f, "\nPAYLOAD_EOF").unwrap();
        writeln!(f, "exit {exit_code}").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        OpenCodeAdapter::new(OpenCodeConfig::new().with_binary(path))
    }

    #[tokio::test]
    async fn end_to_end_success_completes_with_last_text() {
        // Two-step run: tool call, then a text reply with the result.
        let payload = r#"{"type":"step_start","timestamp":1,"sessionID":"s","part":{"id":"p","messageID":"m","sessionID":"s","snapshot":"x","type":"step-start"}}
{"type":"tool_use","timestamp":2,"sessionID":"s","part":{"type":"tool","tool":"bash","callID":"t","state":{"status":"completed","input":{"command":"ls"},"output":"a\nb","title":"List files","time":{"start":0,"end":1}},"id":"p2","sessionID":"s","messageID":"m"}}
{"type":"step_finish","timestamp":3,"sessionID":"s","part":{"id":"p3","reason":"tool-calls","snapshot":"x","messageID":"m","sessionID":"s","type":"step-finish","tokens":{"total":1,"input":1,"output":0,"reasoning":0,"cache":{"write":0,"read":0}},"cost":0.01}}
{"type":"step_start","timestamp":4,"sessionID":"s","part":{"id":"p4","messageID":"m2","sessionID":"s","snapshot":"x","type":"step-start"}}
{"type":"text","timestamp":5,"sessionID":"s","part":{"id":"p5","messageID":"m2","sessionID":"s","type":"text","text":"two files: a, b","time":{"start":0,"end":1}}}
{"type":"step_finish","timestamp":6,"sessionID":"s","part":{"id":"p6","reason":"stop","snapshot":"x","messageID":"m2","sessionID":"s","type":"step-finish","tokens":{"total":2,"input":2,"output":0,"reasoning":0,"cache":{"write":0,"read":0}},"cost":0.02}}"#;
        let exec = fake_opencode(payload, 0);
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.oc", "list files", None,
            10.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(5), h.completion).await
            .expect("hung")
            .unwrap();
        match outcome {
            WorkerOutcome::Complete(c) => {
                assert_eq!(c.result_summary, "two files: a, b");
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn end_to_end_error_event_becomes_failed() {
        let payload = r#"{"type":"step_start","timestamp":1,"sessionID":"s","part":{"id":"p","messageID":"m","sessionID":"s","snapshot":"x","type":"step-start"}}
{"type":"error","timestamp":2,"sessionID":"s","error":{"name":"UnknownError","data":{"message":"Model not found: no/such-model"}}}"#;
        let exec = fake_opencode(payload, 0);
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.oc", "x", None,
            1.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(5), h.completion).await
            .expect("hung")
            .unwrap();
        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(f.error.contains("Model not found"), "got: {}", f.error);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn end_to_end_nonzero_exit_with_no_output_fails_with_stderr_tail() {
        // Empty stdout + exit 1 (mimics bad argv path, where opencode
        // dumps usage to stderr and exits 1 without any JSON).
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(
            format!("mira-fake-oc-bad-{}.sh", uuid::Uuid::new_v4())
        );
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "#!/bin/bash").unwrap();
            writeln!(f, "echo 'usage error: bad flag' >&2").unwrap();
            writeln!(f, "exit 1").unwrap();
        } // drop the file handle so exec doesn't trip ETXTBSY
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();

        let exec = OpenCodeAdapter::new(OpenCodeConfig::new().with_binary(path));
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.oc", "x", None,
            1.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(5), h.completion).await
            .expect("hung")
            .unwrap();
        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(f.error.contains("opencode exited 1"), "got: {}", f.error);
                assert!(f.error.contains("usage error"), "got: {}", f.error);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn end_to_end_missing_binary_fails_with_spawn_error() {
        let cfg = OpenCodeConfig::new().with_binary("/no/such/opencode");
        let exec = OpenCodeAdapter::new(cfg);
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.oc", "x", None,
            1.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(5), h.completion).await
            .expect("hung")
            .unwrap();
        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(f.error.starts_with("spawn"), "got: {}", f.error);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// Real-binary smoke test. Ignored by default because it requires
    /// `opencode` on PATH AND a working AI provider configured. Run
    /// manually with `cargo test --lib agent::opencode -- --ignored`
    /// after a fresh adapter change to verify reality still matches
    /// the parser.
    #[tokio::test]
    #[ignore]
    async fn end_to_end_real_opencode_returns_completion() {
        let exec = OpenCodeAdapter::new(OpenCodeConfig::new());
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.oc.real",
            "Reply with exactly the word 'pong' and nothing else.", None,
            1.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(120), h.completion).await
            .expect("real opencode hung past 120s")
            .unwrap();
        match outcome {
            WorkerOutcome::Complete(c) => {
                assert!(!c.result_summary.is_empty(),
                    "real opencode returned an empty result");
            }
            other => panic!("real opencode failed: {other:?}"),
        }
    }

    #[tokio::test]
    async fn end_to_end_cost_accumulates_across_step_finish_events() {
        // Two steps at $0.01 + $0.02 = $0.03 total. We can't directly
        // observe MIRA's internal cost tracking from a unit test, but
        // we can verify the worker doesn't blow up + Complete carries
        // the right summary, which means all the Progress events we
        // emitted (with running totals) flowed through cleanly.
        let payload = r#"{"type":"text","timestamp":1,"sessionID":"s","part":{"id":"p1","messageID":"m1","sessionID":"s","type":"text","text":"first","time":{"start":0,"end":1}}}
{"type":"step_finish","timestamp":2,"sessionID":"s","part":{"id":"p2","reason":"stop","snapshot":"x","messageID":"m1","sessionID":"s","type":"step-finish","tokens":{"total":1,"input":1,"output":0,"reasoning":0,"cache":{"write":0,"read":0}},"cost":0.01}}
{"type":"text","timestamp":3,"sessionID":"s","part":{"id":"p3","messageID":"m2","sessionID":"s","type":"text","text":"second","time":{"start":0,"end":1}}}
{"type":"step_finish","timestamp":4,"sessionID":"s","part":{"id":"p4","reason":"stop","snapshot":"x","messageID":"m2","sessionID":"s","type":"step-finish","tokens":{"total":1,"input":1,"output":0,"reasoning":0,"cache":{"write":0,"read":0}},"cost":0.02}}"#;
        let exec = fake_opencode(payload, 0);
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.oc", "x", None,
            10.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(5), h.completion).await
            .expect("hung")
            .unwrap();
        match outcome {
            WorkerOutcome::Complete(c) => assert_eq!(c.result_summary, "second"),
            other => panic!("expected Complete, got {other:?}"),
        }
    }
}
