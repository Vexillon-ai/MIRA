// SPDX-License-Identifier: AGPL-3.0-or-later

//! Subprocess adapter framework (slice C1).
//!
//! Wraps an external CLI (Claude Code, OpenCode, HERMES, custom shell
//! scripts, …) as a [`crate::agent::supervisor::WorkerTask`]. The
//! supervisor spawns one of these when a Skill backed by an external
//! tool is selected; the adapter takes care of launching the child,
//! piping the task into it, streaming stdout as `Progress` events, and
//! translating the exit code into `Complete` / `Failed`.
//!
//! Per-tool subclasses (Claude Code, OpenCode, …) live in C2+: they
//! parameterise [`AdapterConfig`] with the right command, args, and
//! parsing strategy. C1 only ships the generic runner + a plain-text
//! mode that's correct for "any tool that prints status to stdout."
//!
//! What the adapter guarantees:
//!   - **Stdout lines stream as Progress.** Each newline-delimited
//!     chunk becomes `report_progress(line, None, 0.0)`. Tools that
//!     emit cost data later can switch to a structured format
//!     (slice C2 wraps Claude Code's NDJSON for this).
//!   - **Stderr is captured to a buffer.** On failure the buffer is
//!     truncated to ~4 KiB (last lines win — typical CLIs print the
//!     real error at the end) and folded into the `WorkerFailure`.
//!   - **Interrupts kill the child.** When the supervisor cancels the
//!     adapter's future, the `Child` handle is dropped which (combined
//!     with `kill_on_drop(true)`) terminates the process. No zombies.
//!   - **Bounded output.** Both stdout and stderr are capped per stream
//!     so a runaway tool can't blow the manager's memory. Once over
//!     the cap, further bytes are dropped (logged once at warn level).
//!
//! Out of scope for C1:
//!   - Sandbox integration. The subprocess inherits the agent runtime's
//!     uid / cgroups / namespaces. Skills that declare
//!     `permissions.subprocess_allowlist` will gate the spawn at the
//!     Skill layer (already implemented in `src/skills/permissions.rs`)
//!     before the adapter is ever constructed.
//!   - Per-call cost reporting from the adapter itself. We emit Progress
//!     with `llm_spend_usd = 0.0`; per-tool subclasses parse the cost
//!     out of their tool's structured output.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::agent::supervisor::{
    WorkerAssignment, WorkerComplete, WorkerContext, WorkerFailure, WorkerTask,
};

/// How to package the [`WorkerAssignment`] for the child process.
///
/// - `Stdin` writes the rendered task to the child's stdin and closes
///   it. The vast majority of CLI agent tools (Claude Code, OpenCode,
///   bash one-liners) read instructions from stdin.
/// - `LastArg` appends the rendered task as the final argv slot.
///   Useful for tools that take "the prompt" as a positional.
/// - `EnvVar(name)` sets the named env var to the rendered task and
///   passes nothing on stdin / argv. Some tools (HERMES) use this so
///   long prompts don't trip shell length limits.
#[derive(Debug, Clone)]
pub enum AssignmentChannel {
    Stdin,
    LastArg,
    EnvVar(String),
}

/// How to interpret what the child writes to stdout. v1 ships a single
/// mode; later slices add JsonLines + custom parsers.
#[derive(Debug, Clone, Copy, Default)]
pub enum OutputFormat {
    /// Each newline-delimited line is a Progress update; the *last*
    /// non-empty line becomes the `result_summary`. Default — works
    /// for any tool that just prints to stdout.
    #[default]
    PlainLines,
}

/// Hard cap on bytes captured per stream. ~256 KiB is plenty for the
/// largest plausible terminal output of a one-shot agent run; anything
/// past it is almost always a runaway loop and we'd rather drop bytes
/// than OOM the manager.
const MAX_STREAM_BYTES: usize = 256 * 1024;

/// Configuration for one subprocess adapter instance. Cheap to clone —
/// the actual child is spawned per `run` call.
#[derive(Debug, Clone)]
pub struct AdapterConfig {
    /// Path or command name (resolved against `PATH` if not absolute).
    pub command: PathBuf,
    /// Static argv to pass before any task-derived args.
    pub args: Vec<String>,
    /// Working directory for the child. None = inherit from manager.
    pub cwd: Option<PathBuf>,
    /// Extra environment to set (merged on top of the inherited env).
    pub env: HashMap<String, String>,
    /// How to deliver the task text to the child.
    pub assignment_channel: AssignmentChannel,
    /// How to interpret stdout.
    pub output_format: OutputFormat,
    /// Render template for the assignment. `{task}` is replaced with the
    /// `WorkerAssignment::task` string; `{context}` with the JSON of
    /// `assignment.context` (or "null"). Tools that want raw task text
    /// pass `"{task}"`.
    pub assignment_template: String,
}

impl AdapterConfig {
    /// Convenience constructor for the common "just run this command,
    /// pipe task to stdin, parse plain lines" case.
    pub fn shell_stdin(command: impl Into<PathBuf>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd:  None,
            env:  HashMap::new(),
            assignment_channel: AssignmentChannel::Stdin,
            output_format:      OutputFormat::PlainLines,
            assignment_template: "{task}".into(),
        }
    }

    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = Some(cwd);
        self
    }

    pub fn with_env(mut self, env: HashMap<String, String>) -> Self {
        self.env = env;
        self
    }

    pub fn with_assignment_channel(mut self, ch: AssignmentChannel) -> Self {
        self.assignment_channel = ch;
        self
    }

    pub fn with_assignment_template(mut self, tpl: impl Into<String>) -> Self {
        self.assignment_template = tpl.into();
        self
    }
}

/// Generic subprocess-backed worker. Newer adapters (Claude Code etc.)
/// either embed an `AdapterConfig` directly or wrap this type to inject
/// tool-specific output parsing.
pub struct SubprocessAdapter {
    config: AdapterConfig,
}

impl SubprocessAdapter {
    pub fn new(config: AdapterConfig) -> Arc<Self> {
        Arc::new(Self { config })
    }
}

/// Render an assignment template into the actual text that goes to the
/// child. `{task}` and `{context}` are the only substitutions — keep
/// it simple to avoid a templating-engine dependency.
fn render_assignment_template(tpl: &str, asn: &WorkerAssignment) -> String {
    let ctx_json = asn.context.as_ref()
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".into());
    tpl.replace("{task}", &asn.task).replace("{context}", &ctx_json)
}

#[async_trait]
impl WorkerTask for SubprocessAdapter {
    async fn run(
        &self,
        assignment: WorkerAssignment,
        ctx:        WorkerContext,
    ) -> Result<WorkerComplete, WorkerFailure> {
        let rendered = render_assignment_template(&self.config.assignment_template, &assignment);

        // Build the Command. Stdin/stdout/stderr always piped — we own
        // the byte streams from here on.
        let mut cmd = Command::new(&self.config.command);
        cmd.args(&self.config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Critical: when our future is dropped (interrupt), the
            // Child drops too and tokio sends SIGKILL.
            .kill_on_drop(true);

        if let Some(cwd) = &self.config.cwd { cmd.current_dir(cwd); }
        for (k, v) in &self.config.env { cmd.env(k, v); }

        // Channel-specific finalisation: argv / env / stdin all happen
        // before spawn, except the actual stdin write which needs the
        // spawned child's handle.
        match &self.config.assignment_channel {
            AssignmentChannel::LastArg     => { cmd.arg(&rendered); }
            AssignmentChannel::EnvVar(key) => { cmd.env(key, &rendered); }
            AssignmentChannel::Stdin       => { /* written below */ }
        }

        debug!(
            "subprocess adapter spawning {:?} (channel={:?}, cwd={:?})",
            self.config.command, self.config.assignment_channel, self.config.cwd
        );
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return Err(WorkerFailure {
                error: format!(
                    "spawn {:?}: {e}",
                    self.config.command,
                ),
                partial_artifacts: vec![], fault: None,
            }),
        };

        // Write the assignment to stdin if that's the channel.
        if matches!(self.config.assignment_channel, AssignmentChannel::Stdin) {
            if let Some(mut stdin) = child.stdin.take() {
                if let Err(e) = stdin.write_all(rendered.as_bytes()).await {
                    return Err(WorkerFailure {
                        error: format!("write stdin: {e}"),
                        partial_artifacts: vec![], fault: None,
                    });
                }
                // Closing stdin signals EOF — tools that read until EOF
                // (most do) will start working as soon as we drop it.
                drop(stdin);
            } else {
                return Err(WorkerFailure {
                    error: "child has no stdin handle".into(),
                    partial_artifacts: vec![], fault: None,
                });
            }
        }

        // Take ownership of stdout + stderr handles. Handing them to
        // separate tasks lets us read both concurrently — otherwise a
        // tool that fills stderr's pipe buffer would deadlock waiting
        // on us to drain it before it can write more to stdout.
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        let last_line = Arc::new(Mutex::new(String::new()));
        let stdout_task = {
            let last_line = Arc::clone(&last_line);
            let agent_id  = ctx.agent_id;
            let sender    = ctx.sender_clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout).lines();
                let mut total_bytes = 0usize;
                let mut hit_cap = false;
                while let Ok(Some(line)) = reader.next_line().await {
                    if total_bytes + line.len() > MAX_STREAM_BYTES {
                        if !hit_cap {
                            hit_cap = true;
                            warn!(
                                agent_id = %agent_id,
                                "subprocess stdout exceeded {} bytes — dropping further output",
                                MAX_STREAM_BYTES
                            );
                        }
                        continue;
                    }
                    total_bytes += line.len();
                    if !line.trim().is_empty() {
                        *last_line.lock().await = line.clone();
                    }
                    let _ = sender.send_event(crate::agent::protocol::Event::Progress {
                        step_summary: line,
                        percent_done: None,
                        llm_spend_usd: 0.0,
                    });
                }
            })
        };

        let stderr_task = {
            tokio::spawn(async move {
                let mut buf = String::new();
                let mut reader = BufReader::new(stderr).lines();
                let mut total_bytes = 0usize;
                let mut hit_cap = false;
                while let Ok(Some(line)) = reader.next_line().await {
                    if total_bytes + line.len() > MAX_STREAM_BYTES {
                        if !hit_cap {
                            hit_cap = true;
                            warn!(
                                "subprocess stderr exceeded {} bytes — truncating",
                                MAX_STREAM_BYTES
                            );
                        }
                        continue;
                    }
                    total_bytes += line.len();
                    buf.push_str(&line);
                    buf.push('\n');
                }
                buf
            })
        };

        // Wait for the child to exit. `wait()` consumes any remaining
        // stream output by virtue of the reader tasks running in parallel.
        let status = match child.wait().await {
            Ok(s)  => s,
            Err(e) => return Err(WorkerFailure {
                error: format!("child.wait: {e}"),
                partial_artifacts: vec![], fault: None,
            }),
        };

        // Both reader tasks should have ended by now (EOF on the pipes).
        let _ = stdout_task.await;
        let stderr_buf = stderr_task.await.unwrap_or_default();

        let summary = last_line.lock().await.clone();

        if status.success() {
            Ok(WorkerComplete {
                result_summary: if summary.is_empty() {
                    "subprocess exited 0 (no stdout)".into()
                } else { summary },
                artifacts: vec![],
            })
        } else {
            let exit_code = status.code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into());
            // Trim stderr to a digestible tail. `MAX_STREAM_BYTES` was
            // already enforced by the reader; this keeps the failure
            // message at a UI-friendly size.
            let tail = tail_chars(&stderr_buf, 1024);
            Err(WorkerFailure {
                error: format!(
                    "subprocess exited with {exit_code}: {tail}",
                ),
                partial_artifacts: vec![], fault: None,
            })
        }
    }
}

/// Return the last `n` chars of `s` — char-aware so we don't slice in
/// the middle of a UTF-8 sequence.
fn tail_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n { return s.to_string(); }
    let skip = s.chars().count() - n;
    s.chars().skip(skip).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::timeout;

    use crate::agent::instance::{Agent, AgentRegistry};
    use crate::agent::protocol::InterruptReason;
    use crate::agent::supervisor::{Supervisor, WorkerOutcome};

    fn fixture() -> (Arc<Supervisor>, crate::agent::instance::AgentId, u8) {
        let reg = Arc::new(AgentRegistry::new());
        let root = reg.register(Agent::new_root());
        let (root_id, depth) = { let r = root.read().unwrap(); (r.id, r.depth) };
        let sup = Arc::new(Supervisor::new(reg));
        (sup, root_id, depth)
    }

    #[tokio::test]
    async fn echoes_task_to_stdout_and_completes() {
        // `cat` reads stdin and writes it to stdout, then exits 0 on EOF.
        let cfg = AdapterConfig::shell_stdin("cat");
        let exec = SubprocessAdapter::new(cfg);

        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.cat", "hello world", None,
            1.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(5), h.completion).await
            .expect("worker hung")
            .unwrap();

        match outcome {
            WorkerOutcome::Complete(c) => assert_eq!(c.result_summary, "hello world"),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn nonzero_exit_becomes_failed_with_stderr_tail() {
        // `bash -c "echo whoops 1>&2; exit 7"` → exit 7, stderr "whoops".
        let cfg = AdapterConfig {
            command: "bash".into(),
            args: vec!["-c".into(), "echo whoops 1>&2; exit 7".into()],
            cwd: None,
            env: HashMap::new(),
            assignment_channel: AssignmentChannel::Stdin,
            output_format: OutputFormat::PlainLines,
            assignment_template: "{task}".into(),
        };
        let exec = SubprocessAdapter::new(cfg);

        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.fail", "doesn't matter", None,
            1.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(5), h.completion).await
            .expect("worker hung")
            .unwrap();

        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(f.error.contains("exited with 7"), "got: {}", f.error);
                assert!(f.error.contains("whoops"),        "got: {}", f.error);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_command_fails_with_spawn_error() {
        let cfg = AdapterConfig::shell_stdin("/no/such/binary/exists/here");
        let exec = SubprocessAdapter::new(cfg);

        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.missing", "x", None,
            1.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(5), h.completion).await
            .expect("worker hung")
            .unwrap();

        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(f.error.starts_with("spawn"), "got: {}", f.error);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn interrupt_terminates_long_running_subprocess() {
        // `sleep 60` runs forever from the test's perspective. Interrupt
        // should drop the future, which (via kill_on_drop) SIGKILLs it
        // and the manager loop reports a Failed outcome quickly.
        let cfg = AdapterConfig {
            command: "sleep".into(),
            args: vec!["60".into()],
            cwd:  None,
            env:  HashMap::new(),
            assignment_channel: AssignmentChannel::Stdin,
            output_format: OutputFormat::PlainLines,
            assignment_template: "{task}".into(),
        };
        let exec = SubprocessAdapter::new(cfg);

        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.sleep", "x", None,
            10.0, None, exec,
        );
        let agent_id = h.agent_id;

        // Let it get going.
        tokio::time::sleep(Duration::from_millis(100)).await;
        sup.interrupt(agent_id, InterruptReason::User).await.expect("interrupt ok");

        let outcome = timeout(Duration::from_secs(3), h.completion).await
            .expect("interrupt didn't tear it down within 3s")
            .unwrap();

        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(f.error.contains("interrupted"), "got: {}", f.error);
            }
            other => panic!("expected Failed (interrupted), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stdout_lines_arrive_as_progress_events() {
        // `printf "one\ntwo\nthree\n"` emits three lines; the last one
        // (after trim) should land in result_summary.
        let cfg = AdapterConfig {
            command: "bash".into(),
            args: vec!["-c".into(), "printf 'one\\ntwo\\nthree\\n'".into()],
            cwd:  None,
            env:  HashMap::new(),
            assignment_channel: AssignmentChannel::Stdin,
            output_format: OutputFormat::PlainLines,
            assignment_template: "{task}".into(),
        };
        let exec = SubprocessAdapter::new(cfg);

        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.lines", "x", None,
            1.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(5), h.completion).await
            .expect("worker hung")
            .unwrap();

        match outcome {
            WorkerOutcome::Complete(c) => assert_eq!(c.result_summary, "three"),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn template_substitutes_task_and_context() {
        let asn = WorkerAssignment {
            task:    "Refactor the renderer".into(),
            context: Some(serde_json::json!({"file": "src/render.rs"})),
            budget_usd:  1.0,
            deadline_ms: None,
            ..Default::default()
        };
        let out = render_assignment_template("TASK={task} CTX={context}", &asn);
        assert_eq!(out, r#"TASK=Refactor the renderer CTX={"file":"src/render.rs"}"#);
    }

    #[test]
    fn template_renders_null_context_when_unset() {
        let asn = WorkerAssignment {
            task:    "do stuff".into(),
            context: None,
            budget_usd:  1.0,
            deadline_ms: None,
            ..Default::default()
        };
        let out = render_assignment_template("{task}/{context}", &asn);
        assert_eq!(out, "do stuff/null");
    }

    #[test]
    fn tail_chars_returns_last_n_when_string_is_longer() {
        let s = "abcdefghij";
        assert_eq!(tail_chars(s, 3), "hij");
    }

    #[test]
    fn tail_chars_returns_whole_string_when_shorter() {
        assert_eq!(tail_chars("abc", 10), "abc");
    }

    #[test]
    fn tail_chars_handles_multibyte_utf8() {
        // 4 emoji = 4 chars (16 bytes). Tail of 2 should be the last 2.
        assert_eq!(tail_chars("🦀🦊🐺🦝", 2), "🐺🦝");
    }
}
