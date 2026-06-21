// SPDX-License-Identifier: AGPL-3.0-or-later

//! HERMES adapter (slice C4).
//!
//! Wraps the `hermes` CLI (Hermes Agent) as a
//! [`crate::agent::supervisor::WorkerTask`]. Unlike Claude Code (C2)
//! and OpenCode (C3), HERMES has no structured JSON output — its
//! `-z / --oneshot` mode prints only the final response text to
//! stdout. So this adapter is thin: stream stdout lines as Progress,
//! capture stderr, surface the full stdout as the final
//! `result_summary` on success.
//!
//! Spawn shape:
//!
//!   hermes -z <prompt> [-m MODEL] [--provider PROVIDER] \
//!          [-t TOOLSETS] [-s SKILLS] \
//!          [--yolo] [--accept-hooks] \
//!          [--ignore-user-config] [--ignore-rules]
//!
//! Failure signals (`exit_code` is unreliable — HERMES exits 0 even
//! on auth errors, similar to OpenCode):
//!
//!   - **Python traceback in stderr** → Failed. The CLI is a Python
//!     program; uncaught exceptions print "Traceback (most recent
//!     call last):" to stderr. We treat that as the canonical
//!     failure signal and surface the last error line as the
//!     WorkerFailure error message.
//!   - **Empty stdout + non-empty stderr** → Failed (with stderr tail).
//!     Same root cause; the traceback lands but we didn't match the
//!     pattern, fallback path catches it.
//!   - **Non-zero exit** → Failed (with exit code + stderr tail).
//!     Caught for the "bad argv" / "wrong binary" path.
//!   - **Clean exit + non-empty stdout + no traceback** → Complete
//!     with the full stdout text as `result_summary`.
//!
//! Limitations vs C2/C3:
//!
//!   - **No tool-use streaming.** HERMES doesn't expose mid-flight
//!     tool calls in oneshot mode. Each Progress event is just a
//!     plain stdout line; the agents UI won't see "[tool_use] ..."
//!     entries the way Claude Code / OpenCode show them.
//!   - **No cost reporting.** HERMES doesn't print cost in oneshot
//!     mode. All Progress events carry `llm_spend_usd: 0.0`. The
//!     supervisor's session-budget kill won't trigger from HERMES
//!     spend; the per-agent budget is a soft cap honoured only by
//!     the wall-clock deadline (which the manager loop enforces).
//!   - **No model alias forwarding by default.** Pass `-m / --model`
//!     via [`HermesConfig::with_model`] to override. (Future slice
//!     could resolve `LlmChoice` → HERMES model string.)

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{debug, warn};

use crate::agent::protocol::Event;
use crate::agent::supervisor::{
    WorkerAssignment, WorkerComplete, WorkerContext, WorkerFailure, WorkerTask,
};

/// Soft cap on captured-stream bytes. HERMES output is typically a
/// short final response (a few KB), but tools that print their full
/// output before HERMES summarises can exceed that. 4 MiB matches the
/// other adapters.
const MAX_STREAM_BYTES: usize = 4 * 1024 * 1024;

/// What we look for in stderr to declare a failure even when exit was
/// 0. HERMES is a Python program — uncaught exceptions print this
/// canonical traceback header.
const TRACEBACK_MARKER: &str = "Traceback (most recent call last):";

/// Configuration for the HERMES adapter. Cheap to clone.
#[derive(Debug, Clone)]
pub struct HermesConfig {
    /// Path to the `hermes` binary. Default: `"hermes"` (PATH lookup).
    pub binary: PathBuf,

    /// Working directory. None = inherit from manager. HERMES reads
    /// AGENTS.md / SOUL.md / .cursorrules from CWD on startup, so this
    /// matters more than for the JSON-streamed adapters — set it to
    /// the project root the agent should reason about.
    pub cwd: Option<PathBuf>,

    /// `-m / --model`. None = use HERMES's configured default. Format
    /// is provider-specific (e.g. `anthropic/claude-sonnet-4.6`).
    pub model: Option<String>,

    /// `--provider`. None = HERMES picks via auto. Useful when the
    /// model name is ambiguous across providers.
    pub provider: Option<String>,

    /// `-t / --toolsets`. Comma-separated; passed verbatim. None =
    /// HERMES's default toolset.
    pub toolsets: Option<String>,

    /// `-s / --skills`. Comma-separated. None = no preloaded skills.
    pub skills: Option<String>,

    /// `--yolo` — bypass dangerous-command approval prompts.
    /// Required for unattended runs (no human to click "approve").
    pub yolo: bool,

    /// `--accept-hooks` — auto-approve unseen shell hooks declared in
    /// HERMES config.yaml. Without this, headless runs can hang on
    /// the hook-approval prompt.
    pub accept_hooks: bool,

    /// `--ignore-user-config` — skip ~/.hermes/config.yaml so the
    /// adapter run uses only built-in defaults plus what we pass via
    /// argv. Reproducible across users / machines.
    pub ignore_user_config: bool,

    /// `--ignore-rules` — skip auto-injection of AGENTS.md, SOUL.md,
    /// .cursorrules, memory, preloaded skills. Useful when the manager
    /// agent has already provided full context in the prompt and
    /// doesn't want HERMES adding more.
    pub ignore_rules: bool,
}

impl Default for HermesConfig {
    fn default() -> Self {
        Self {
            binary:             "hermes".into(),
            cwd:                None,
            model:              None,
            provider:           None,
            toolsets:           None,
            skills:             None,
            yolo:               false,
            accept_hooks:       false,
            ignore_user_config: false,
            ignore_rules:       false,
        }
    }
}

impl HermesConfig {
    pub fn new() -> Self { Self::default() }

    pub fn with_binary(mut self, path: impl Into<PathBuf>)        -> Self { self.binary = path.into(); self }
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>)            -> Self { self.cwd = Some(cwd.into()); self }
    pub fn with_model(mut self, model: impl Into<String>)         -> Self { self.model = Some(model.into()); self }
    pub fn with_provider(mut self, provider: impl Into<String>)   -> Self { self.provider = Some(provider.into()); self }
    pub fn with_toolsets(mut self, toolsets: impl Into<String>)   -> Self { self.toolsets = Some(toolsets.into()); self }
    pub fn with_skills(mut self, skills: impl Into<String>)       -> Self { self.skills = Some(skills.into()); self }
    pub fn with_yolo(mut self, on: bool)                          -> Self { self.yolo = on; self }
    pub fn with_accept_hooks(mut self, on: bool)                  -> Self { self.accept_hooks = on; self }
    pub fn with_ignore_user_config(mut self, on: bool)            -> Self { self.ignore_user_config = on; self }
    pub fn with_ignore_rules(mut self, on: bool)                  -> Self { self.ignore_rules = on; self }
}

pub struct HermesAdapter {
    config:               HermesConfig,
    secrets:              Option<Arc<crate::skills::SecretsStore>>,
    skill_id_for_secrets: String,
}

impl HermesAdapter {
    pub fn new(config: HermesConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            secrets: None,
            skill_id_for_secrets: "com.mira.hermes".to_string(),
        })
    }

    /// Plug in a [`SecretsStore`] for env-var injection. See
    /// `ClaudeCodeAdapter::with_secrets` for semantics.
    pub fn with_secrets(
        mut self: Arc<Self>,
        store: Arc<crate::skills::SecretsStore>,
    ) -> Arc<Self> {
        let inner = Arc::get_mut(&mut self)
            .expect("HermesAdapter::with_secrets called on aliased Arc");
        inner.secrets = Some(store);
        self
    }

    pub fn with_skill_id(
        mut self: Arc<Self>,
        skill_id: impl Into<String>,
    ) -> Arc<Self> {
        let inner = Arc::get_mut(&mut self)
            .expect("HermesAdapter::with_skill_id called on aliased Arc");
        inner.skill_id_for_secrets = skill_id.into();
        self
    }
}

#[async_trait]
impl WorkerTask for HermesAdapter {
    async fn run(
        &self,
        assignment: WorkerAssignment,
        ctx:        WorkerContext,
    ) -> Result<WorkerComplete, WorkerFailure> {
        let prompt = build_prompt(&assignment);

        let mut cmd = Command::new(&self.config.binary);
        cmd.arg("-z").arg(&prompt)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(cwd)      = &self.config.cwd      { cmd.current_dir(cwd); }
        if let Some(model)    = &self.config.model    { cmd.arg("-m").arg(model); }
        if let Some(provider) = &self.config.provider { cmd.arg("--provider").arg(provider); }
        if let Some(toolsets) = &self.config.toolsets { cmd.arg("-t").arg(toolsets); }
        if let Some(skills)   = &self.config.skills   { cmd.arg("-s").arg(skills); }
        if self.config.yolo               { cmd.arg("--yolo"); }
        if self.config.accept_hooks       { cmd.arg("--accept-hooks"); }
        if self.config.ignore_user_config { cmd.arg("--ignore-user-config"); }
        if self.config.ignore_rules       { cmd.arg("--ignore-rules"); }

        // Skill secrets — env vars from the vault, scoped to this
        // worker's user (or system-wide when unset).
        let injected_env_keys: Vec<String> = if let Some(store) = &self.secrets {
            let env = store.env_vars_for(
                assignment.user_id.as_deref(),
                &self.skill_id_for_secrets,
            );
            let keys: Vec<String> = env.keys().cloned().collect();
            for (k, v) in env {
                cmd.env(k, v);
            }
            keys
        } else { vec![] };

        debug!(
            "hermes: spawning {:?} cwd={:?} model={:?} provider={:?} env_keys={:?}",
            self.config.binary, self.config.cwd, self.config.model, self.config.provider,
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

        // Drain stderr in parallel — HERMES tracebacks land here and
        // we need them to detect failure post-hoc.
        let stderr_task = tokio::spawn(async move {
            let mut buf = String::new();
            let mut reader = BufReader::new(stderr).lines();
            let mut total = 0usize;
            let mut hit_cap = false;
            while let Ok(Some(line)) = reader.next_line().await {
                if total + line.len() > MAX_STREAM_BYTES {
                    if !hit_cap {
                        warn!("hermes stderr exceeded {MAX_STREAM_BYTES}B — truncating");
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

        // Stream stdout lines as Progress + accumulate the full text
        // for the final result_summary. HERMES `-z` prints the entire
        // final response in one batch at the end (no streaming), so
        // the user sees a single Progress event per line of the
        // final answer rather than tool-by-tool updates. Better than
        // nothing for the agents UI.
        let mut reader = BufReader::new(stdout).lines();
        let mut full_output = String::new();
        let mut total_bytes = 0usize;
        let mut over_cap = false;
        while let Ok(Some(line)) = reader.next_line().await {
            if total_bytes + line.len() > MAX_STREAM_BYTES {
                if !over_cap {
                    over_cap = true;
                    warn!("hermes stdout exceeded {MAX_STREAM_BYTES}B — dropping further lines");
                }
                continue;
            }
            total_bytes += line.len();
            full_output.push_str(&line);
            full_output.push('\n');
            if !line.trim().is_empty() {
                let _ = ctx.sender_clone().send_event(Event::Progress {
                    step_summary: line,
                    percent_done: None,
                    llm_spend_usd: 0.0,
                });
            }
        }

        let exit_status = child.wait().await;
        let stderr_buf  = stderr_task.await.unwrap_or_default();

        match classify_outcome(&exit_status, &full_output, &stderr_buf) {
            HermesOutcome::Success { summary } => Ok(WorkerComplete {
                result_summary: summary,
                artifacts: vec![],
            }),
            HermesOutcome::Failure { error } => Err(WorkerFailure {
                error: format!("hermes: {error}"),
                partial_artifacts: vec![], fault: None,
            }),
        }
    }
}

/// Build the prompt text passed to HERMES via `-z`. Same shape as the
/// other adapters: task verbatim, plus a fenced JSON context block
/// when `assignment.context` is set.
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

/// Pure: decide the terminal outcome from exit + stdout + stderr.
/// Pulled out of `run` so it's unit-testable without a child process.
pub(crate) fn classify_outcome(
    exit_status: &Result<std::process::ExitStatus, std::io::Error>,
    stdout:      &str,
    stderr:      &str,
) -> HermesOutcome {
    // Python traceback wins regardless of exit code — HERMES exits 0
    // even on auth errors that print a traceback to stderr.
    if stderr.contains(TRACEBACK_MARKER) {
        let last = last_meaningful_line(stderr);
        return HermesOutcome::Failure {
            error: format!("Python traceback in stderr: {last}"),
        };
    }

    // Non-zero exit + no useful stdout = pathological (bad argv,
    // missing binary, crash before any output).
    let exited_clean = matches!(exit_status, Ok(s) if s.success());
    if !exited_clean && stdout.trim().is_empty() {
        let exit_str = match exit_status {
            Ok(s)  => s.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
            Err(e) => format!("wait error: {e}"),
        };
        let tail = tail_chars(stderr, 1024);
        return HermesOutcome::Failure {
            error: format!("exited {exit_str} with no stdout (stderr tail: {tail})"),
        };
    }

    // Non-zero exit but we got stdout — HERMES's oneshot does this
    // when the answer arrived but a teardown step failed. Treat as
    // success and surface the answer; the user can debug from logs.
    if stdout.trim().is_empty() {
        return HermesOutcome::Failure {
            error: "no response text returned from hermes oneshot".into(),
        };
    }

    HermesOutcome::Success {
        summary: stdout.trim_end().to_string(),
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum HermesOutcome {
    Success { summary: String },
    Failure { error:   String },
}

/// Return the last non-empty, non-indented line of `s` — that's
/// usually the actual exception message in a Python traceback (the
/// indented frames above it are noise for a UI summary).
fn last_meaningful_line(s: &str) -> String {
    s.lines()
        .rev()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && !l.starts_with("File ") && !l.starts_with("at "))
        .unwrap_or("(no error message)")
        .to_string()
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
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::timeout;

    use crate::agent::instance::{Agent, AgentRegistry};
    use crate::agent::supervisor::{Supervisor, WorkerOutcome};

    fn ok_exit() -> Result<ExitStatus, std::io::Error> {
        Ok(ExitStatus::from_raw(0))
    }
    fn fail_exit(code: i32) -> Result<ExitStatus, std::io::Error> {
        // On Linux, ExitStatus::from_raw expects a wait()-style word
        // where the low byte is the signal and the high byte is the
        // exit code. Shift the code into the right slot.
        Ok(ExitStatus::from_raw(code << 8))
    }

    // ── classify_outcome unit tests ──

    #[test]
    fn clean_exit_with_stdout_yields_success() {
        let out = "The answer is 42.";
        assert_eq!(
            classify_outcome(&ok_exit(), out, ""),
            HermesOutcome::Success { summary: "The answer is 42.".into() },
        );
    }

    #[test]
    fn clean_exit_trims_trailing_whitespace_in_summary() {
        let out = "answer\n\n";
        match classify_outcome(&ok_exit(), out, "") {
            HermesOutcome::Success { summary } => assert_eq!(summary, "answer"),
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[test]
    fn traceback_in_stderr_overrides_clean_exit_with_failure() {
        // Captured from the live `hermes -z "..."` call when no API
        // key is configured. Exit was 0, stderr had a traceback ending
        // with "AuthError: No inference provider configured...".
        let stderr = "Traceback (most recent call last):\n  \
                      File \"/path/to/hermes/cli.py\", line 10, in main\n    \
                      sys.exit(run_oneshot(...))\n\
                      AuthError: No inference provider configured. Run 'hermes model' to choose a provider.";
        match classify_outcome(&ok_exit(), "", stderr) {
            HermesOutcome::Failure { error } => {
                assert!(error.contains("Python traceback"), "got: {error}");
                assert!(error.contains("AuthError"),       "got: {error}");
            }
            other => panic!("expected Failure, got {other:?}"),
        }
    }

    #[test]
    fn traceback_wins_even_when_stdout_has_partial_output() {
        // HERMES sometimes prints partial output before tracebacking
        // (e.g. a banner gets through then the agent crashes). The
        // traceback is still authoritative — don't surface partial
        // text as a "successful" result.
        let stderr = "Traceback (most recent call last):\n  ValueError: bad input";
        match classify_outcome(&ok_exit(), "partial banner", stderr) {
            HermesOutcome::Failure { error } => {
                assert!(error.contains("ValueError"), "got: {error}");
            }
            other => panic!("expected Failure, got {other:?}"),
        }
    }

    #[test]
    fn nonzero_exit_with_no_stdout_yields_failure_with_stderr_tail() {
        match classify_outcome(&fail_exit(2), "", "usage error\n") {
            HermesOutcome::Failure { error } => {
                assert!(error.contains("exited 2"), "got: {error}");
                assert!(error.contains("usage error"), "got: {error}");
            }
            other => panic!("expected Failure, got {other:?}"),
        }
    }

    #[test]
    fn nonzero_exit_but_stdout_present_treated_as_success() {
        // HERMES has been observed to exit non-zero on a teardown
        // step even after the agent successfully replied. Trust the
        // stdout — the user can debug the teardown from logs.
        match classify_outcome(&fail_exit(1), "the answer is 42", "") {
            HermesOutcome::Success { summary } => {
                assert_eq!(summary, "the answer is 42");
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[test]
    fn empty_stdout_with_clean_exit_yields_no_response_failure() {
        match classify_outcome(&ok_exit(), "  \n  ", "") {
            HermesOutcome::Failure { error } => {
                assert!(error.contains("no response text"), "got: {error}");
            }
            other => panic!("expected Failure, got {other:?}"),
        }
    }

    #[test]
    fn last_meaningful_line_strips_python_frames_and_finds_exception() {
        let stderr = "Traceback (most recent call last):\n  \
                      File \"/x.py\", line 5, in foo\n    raise ValueError(\"boom\")\n\
                      ValueError: boom";
        assert_eq!(last_meaningful_line(stderr), "ValueError: boom");
    }

    #[test]
    fn last_meaningful_line_returns_placeholder_on_empty_input() {
        assert_eq!(last_meaningful_line(""), "(no error message)");
    }

    // ── build_prompt tests (sibling helper, deliberately duplicated
    //    across adapters so each is self-contained) ──

    #[test]
    fn build_prompt_returns_task_alone_when_no_context() {
        let asn = WorkerAssignment {
            task: "Hi".into(),
            context: None,
            budget_usd: 1.0,
            deadline_ms: None,
            ..Default::default()
        };
        assert_eq!(build_prompt(&asn), "Hi");
    }

    #[test]
    fn build_prompt_appends_fenced_json_context_block() {
        let asn = WorkerAssignment {
            task: "Refactor".into(),
            context: Some(serde_json::json!({"file": "x.rs"})),
            budget_usd: 1.0,
            deadline_ms: None,
            ..Default::default()
        };
        let p = build_prompt(&asn);
        assert!(p.starts_with("Refactor\n\nContext:\n```json\n"));
        assert!(p.contains(r#""file": "x.rs""#));
    }

    // ── End-to-end with fake binary ──

    fn fixture() -> (Arc<Supervisor>, crate::agent::instance::AgentId, u8) {
        let reg = Arc::new(AgentRegistry::new());
        let root = reg.register(Agent::new_root());
        let (root_id, depth) = { let r = root.read().unwrap(); (r.id, r.depth) };
        let sup = Arc::new(Supervisor::new(reg));
        (sup, root_id, depth)
    }

    /// Make an adapter whose binary is a tempfile shebang script that
    /// writes `stdout_payload` to stdout, `stderr_payload` to stderr,
    /// and exits with `exit_code`. Lets us drive the full pipe without
    /// a real `hermes` binary.
    fn fake_hermes(stdout_payload: &str, stderr_payload: &str, exit_code: i32)
        -> Arc<HermesAdapter>
    {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(
            format!("mira-fake-hermes-{}.sh", uuid::Uuid::new_v4())
        );
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "#!/bin/bash").unwrap();
            // Print stdout via heredoc.
            writeln!(f, "cat <<'STDOUT_EOF'").unwrap();
            f.write_all(stdout_payload.as_bytes()).unwrap();
            writeln!(f, "\nSTDOUT_EOF").unwrap();
            // Print stderr via heredoc.
            writeln!(f, "cat >&2 <<'STDERR_EOF'").unwrap();
            f.write_all(stderr_payload.as_bytes()).unwrap();
            writeln!(f, "\nSTDERR_EOF").unwrap();
            writeln!(f, "exit {exit_code}").unwrap();
        } // drop file handle so exec doesn't hit ETXTBSY
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        HermesAdapter::new(HermesConfig::new().with_binary(path))
    }

    #[tokio::test]
    async fn end_to_end_success_completes_with_stdout_text() {
        let exec = fake_hermes("The answer is 42.\n", "", 0);
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.hermes", "what is 6 * 7", None,
            1.0, None, exec,
        );
        // Generous deadline: with the fake executor these complete in <1ms;
        // the timeout only guards against a true hang. Kept high so the lib
        // suite doesn't flake when the worker is CPU-starved under a fully
        // parallel `cargo test` run (the blocking CI gate runs the whole suite).
        let outcome = timeout(Duration::from_secs(30), h.completion).await
            .expect("hung").unwrap();
        match outcome {
            WorkerOutcome::Complete(c) => assert_eq!(c.result_summary, "The answer is 42."),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn end_to_end_traceback_in_stderr_becomes_failed() {
        // Mirrors the live `hermes -z "..."` AuthError shape captured
        // from `hermes_cli.auth.AuthError: No inference provider...`.
        let stderr = "Traceback (most recent call last):\n  \
                      File \"/home/user/.local/bin/hermes\", line 10, in <module>\n    \
                      sys.exit(main())\n\
                      hermes_cli.auth.AuthError: No inference provider configured. Run 'hermes model' to choose a provider.";
        let exec = fake_hermes("", stderr, 0);
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.hermes", "x", None,
            1.0, None, exec,
        );
        // Generous deadline: with the fake executor these complete in <1ms;
        // the timeout only guards against a true hang. Kept high so the lib
        // suite doesn't flake when the worker is CPU-starved under a fully
        // parallel `cargo test` run (the blocking CI gate runs the whole suite).
        let outcome = timeout(Duration::from_secs(30), h.completion).await
            .expect("hung").unwrap();
        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(f.error.contains("AuthError"), "got: {}", f.error);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn end_to_end_streams_stdout_lines_as_progress_events() {
        // Multiline stdout — each line should fire a Progress event,
        // and the final summary should be the joined trimmed text.
        let exec = fake_hermes("first line\nsecond line\nthird line", "", 0);
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.hermes", "x", None,
            1.0, None, exec,
        );
        // Generous deadline: with the fake executor these complete in <1ms;
        // the timeout only guards against a true hang. Kept high so the lib
        // suite doesn't flake when the worker is CPU-starved under a fully
        // parallel `cargo test` run (the blocking CI gate runs the whole suite).
        let outcome = timeout(Duration::from_secs(30), h.completion).await
            .expect("hung").unwrap();
        match outcome {
            WorkerOutcome::Complete(c) => {
                assert!(c.result_summary.contains("first line"),  "got: {}", c.result_summary);
                assert!(c.result_summary.contains("second line"), "got: {}", c.result_summary);
                assert!(c.result_summary.contains("third line"),  "got: {}", c.result_summary);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn end_to_end_missing_binary_fails_with_spawn_error() {
        let cfg = HermesConfig::new().with_binary("/no/such/hermes");
        let exec = HermesAdapter::new(cfg);
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.hermes", "x", None,
            1.0, None, exec,
        );
        // Generous deadline: with the fake executor these complete in <1ms;
        // the timeout only guards against a true hang. Kept high so the lib
        // suite doesn't flake when the worker is CPU-starved under a fully
        // parallel `cargo test` run (the blocking CI gate runs the whole suite).
        let outcome = timeout(Duration::from_secs(30), h.completion).await
            .expect("hung").unwrap();
        match outcome {
            WorkerOutcome::Failed(f) => assert!(f.error.starts_with("spawn"), "got: {}", f.error),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn end_to_end_nonzero_exit_with_no_output_surfaces_stderr_tail() {
        let exec = fake_hermes("", "usage: hermes [-h] ...\n", 2);
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.hermes", "x", None,
            1.0, None, exec,
        );
        // Generous deadline: with the fake executor these complete in <1ms;
        // the timeout only guards against a true hang. Kept high so the lib
        // suite doesn't flake when the worker is CPU-starved under a fully
        // parallel `cargo test` run (the blocking CI gate runs the whole suite).
        let outcome = timeout(Duration::from_secs(30), h.completion).await
            .expect("hung").unwrap();
        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(f.error.contains("exited 2"),    "got: {}", f.error);
                assert!(f.error.contains("usage:"),      "got: {}", f.error);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// Real-binary smoke test against the installed `hermes`. Ignored
    /// by default — requires HERMES configured with a working API key.
    /// Run with `cargo test --lib agent::hermes -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn end_to_end_real_hermes_returns_completion() {
        let exec = HermesAdapter::new(HermesConfig::new());
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.hermes.real",
            "Reply with exactly the word 'pong' and nothing else.", None,
            1.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(120), h.completion).await
            .expect("real hermes hung past 120s")
            .unwrap();
        match outcome {
            WorkerOutcome::Complete(c) => {
                assert!(!c.result_summary.is_empty(),
                    "real hermes returned empty result");
            }
            // Acceptable in a no-API-key environment: surfaces the
            // AuthError from stderr. Just verify we got a structured
            // failure rather than hanging or crashing.
            WorkerOutcome::Failed(f) => {
                assert!(
                    f.error.contains("AuthError")
                        || f.error.contains("No inference provider"),
                    "real hermes failed in an unexpected way: {}", f.error,
                );
            }
        }
    }
}
