// SPDX-License-Identifier: AGPL-3.0-or-later

//! Verification wrapper (slice C6).
//!
//! `VerifyingAdapter` is a [`WorkerTask`] decorator. It wraps any
//! inner adapter (Claude Code, OpenCode, HERMES, the shell adapter,
//! the research adapter, your own) plus a [`Verifier`], and inserts
//! a smoke-check step in between the inner's `Complete` and the
//! manager seeing `Complete`:
//!
//!   - Inner returns `Failed` → wrapper passes the failure through
//!     unchanged. Verification is skipped (no point checking work
//!     that already failed).
//!   - Inner returns `Complete` → wrapper runs the verifier. If the
//!     verifier returns `Ok(())`, `Complete` propagates with the
//!     original summary annotated with `[verified]`. If the verifier
//!     returns `Err(reason)`, the wrapper converts the outcome to
//!     `Failed` whose error says "verification failed: …" and
//!     preserves the inner's artifacts.
//!
//! The design-doc motivation (C6 in `design-docs/skills-and-agents.md`):
//!
//! > After an adapter reports `complete`, the worker runs a
//! > verification check (build, run tests, smoke test) before
//! > reporting back to the manager.
//!
//! The decorator stays out of the inner adapter's hot path — it
//! captures the channel sender + agent id up front, then hands
//! ownership of `WorkerContext` to the inner. That keeps inner
//! semantics identical to running un-wrapped: progress events,
//! interrupt handling, child spawns all behave the same.
//!
//! Two verifier implementations ship in this slice:
//!
//!   - [`SubprocessVerifier`] — run a shell command in a directory.
//!     Exit 0 = pass; otherwise the trimmed stdout+stderr tail is
//!     the failure reason. Covers the standard "cargo build && cargo
//!     test" / "npm test" / "pytest" patterns.
//!   - [`NoOpVerifier`] — always passes. Useful for tests that want
//!     to exercise the decorator without standing up a real check,
//!     and as a "verification disabled" sentinel.
//!
//! User-supplied verifiers can do anything — query a healthcheck
//! endpoint, parse a manifest for invariants, run a typechecker,
//! whatever — by implementing the [`Verifier`] trait.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;
use tracing::{debug, warn};

use crate::agent::instance::AgentId;
use crate::agent::protocol::Event;
use crate::agent::supervisor::{
    WorkerAssignment, WorkerComplete, WorkerContext, WorkerFailure, WorkerTask,
};

/// Soft cap on captured-stream bytes from a subprocess verifier.
/// Verifier output is meant to be a short pass/fail signal — if your
/// "cargo test" prints 4 MiB of output the failure reason is going
/// to be the tail anyway, so cap accordingly.
const MAX_STREAM_BYTES: usize = 256 * 1024;

/// Soft cap on the chars folded into a `WorkerFailure::error` from the
/// captured stream. Keeps the agents-UI table cell + audit log
/// readable. The full stream still reaches the worker's logs at debug.
const FAILURE_TAIL_CHARS: usize = 1024;

/// Context passed to a [`Verifier::verify`] call. Cheap to construct;
/// the verifier reads what it needs.
#[derive(Debug, Clone)]
pub struct VerificationContext {
    /// The original task string the worker was given. Useful for
    /// verifiers that match on intent (e.g. "skip the test step for
    /// docs-only tasks").
    pub task: String,
    /// The worker agent's id. Verifiers that emit their own audit
    /// events (Phase D) can attribute them correctly.
    pub agent_id: AgentId,
    /// The workspace the inner adapter was operating in, when known.
    /// `SubprocessVerifier` ignores this and uses its own configured
    /// `cwd` — but a verifier that needs to react to where the work
    /// happened (e.g. "run tests in the same git worktree the
    /// adapter spawned") can read it here.
    pub workspace: Option<PathBuf>,
}

/// What every verifier implements. Returns `Ok(())` if the inner
/// adapter's claim of `Complete` is corroborated; `Err(reason)` if
/// it isn't. Best-effort by design — a verifier that itself errors
/// (unable to spawn the test command, network down) returns Err
/// rather than panicking.
#[async_trait]
pub trait Verifier: Send + Sync {
    async fn verify(
        &self,
        complete: &WorkerComplete,
        ctx:      &VerificationContext,
    ) -> Result<(), String>;
}

/// Always passes. Useful for testing the wrapper plumbing and as a
/// "verification disabled" marker that's clearer than `Option<dyn>`.
pub struct NoOpVerifier;

#[async_trait]
impl Verifier for NoOpVerifier {
    async fn verify(
        &self, _complete: &WorkerComplete, _ctx: &VerificationContext,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// Run a shell command and treat exit 0 as pass. The most common
/// verifier: `cargo build && cargo test`, `npm test`, `pytest`,
/// `make check`, etc.
#[derive(Debug, Clone)]
pub struct SubprocessVerifier {
    /// Shell expression to run. Always invoked as `bash -c "<command>"`
    /// so `&&`, env interpolation, and the like all work. The shell
    /// dependency is intentional — verifiers are short scripts and
    /// the convenience outweighs the cost of a one-shot process.
    pub command: String,
    /// Working directory. None = inherit from manager. In practice
    /// always set to the project root the inner adapter modified.
    pub cwd: Option<PathBuf>,
    /// Hard cap on how long the verifier may run before being killed.
    /// `Some(secs)` enforces; `None` means "trust the supervisor's
    /// own deadline." Default 300s — long enough for a full test
    /// suite, short enough to cap a runaway loop.
    pub timeout_secs: Option<u64>,
}

impl SubprocessVerifier {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command:      command.into(),
            cwd:          None,
            timeout_secs: Some(300),
        }
    }
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
    pub fn with_timeout(mut self, secs: Option<u64>) -> Self {
        self.timeout_secs = secs;
        self
    }
}

#[async_trait]
impl Verifier for SubprocessVerifier {
    async fn verify(
        &self,
        _complete: &WorkerComplete,
        _ctx:      &VerificationContext,
    ) -> Result<(), String> {
        debug!("verify: running {:?} (cwd={:?})", self.command, self.cwd);

        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(&self.command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &self.cwd { cmd.current_dir(cwd); }

        let mut child = cmd.spawn()
            .map_err(|e| format!("spawn bash: {e}"))?;

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // Drain both pipes in parallel so the child can't deadlock on
        // a full OS buffer waiting for us to read.
        let stdout_task = tokio::spawn(read_capped(stdout, "stdout"));
        let stderr_task = tokio::spawn(read_capped(stderr, "stderr"));

        let wait_fut = child.wait();
        let exit_status = match self.timeout_secs {
            Some(secs) => match tokio::time::timeout(
                std::time::Duration::from_secs(secs), wait_fut,
            ).await {
                Ok(r)  => r.map_err(|e| format!("child.wait: {e}"))?,
                Err(_) => return Err(format!(
                    "verifier timed out after {secs}s",
                )),
            },
            None => wait_fut.await.map_err(|e| format!("child.wait: {e}"))?,
        };

        let stdout_buf = stdout_task.await.unwrap_or_default();
        let stderr_buf = stderr_task.await.unwrap_or_default();

        if exit_status.success() {
            return Ok(());
        }

        // Failure path: surface a tail that includes both streams,
        // labelled, so the user can tell which side carried the error.
        let exit_str = exit_status.code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        let combined = format!(
            "exit {exit_str}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            stdout = stdout_buf.trim_end(),
            stderr = stderr_buf.trim_end(),
        );
        Err(tail_chars(&combined, FAILURE_TAIL_CHARS))
    }
}

async fn read_capped(
    mut pipe: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    label: &'static str,
) -> String {
    let mut buf = Vec::with_capacity(4096);
    let mut reader = BufReader::new(&mut pipe);
    let mut chunk = [0u8; 4096];
    let mut total = 0usize;
    let mut hit_cap = false;
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0)  => break,
            Ok(n)  => n,
            Err(_) => break,
        };
        if total + n > MAX_STREAM_BYTES {
            if !hit_cap {
                hit_cap = true;
                warn!("verifier {label} exceeded {MAX_STREAM_BYTES}B — truncating");
            }
            continue;
        }
        total += n;
        buf.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn tail_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n { return s.to_string(); }
    let skip = s.chars().count() - n;
    s.chars().skip(skip).collect()
}

/// Decorator that wraps an inner [`WorkerTask`] with a [`Verifier`].
/// Construct once with both, register as a normal `WorkerTask` —
/// nothing else in the supervisor / executor resolution path knows
/// or cares this layer exists.
pub struct VerifyingAdapter {
    inner:    Arc<dyn WorkerTask>,
    verifier: Arc<dyn Verifier>,
    /// Workspace the verifier should know about. Fed into the
    /// `VerificationContext` for verifiers that care; ignored by
    /// `SubprocessVerifier` (which uses its own cwd config).
    workspace: Option<PathBuf>,
}

impl VerifyingAdapter {
    /// Wrap `inner` with `verifier`. Workspace is optional metadata
    /// passed to the verifier through [`VerificationContext::workspace`];
    /// `SubprocessVerifier` ignores it (it has its own cwd config),
    /// but custom verifiers that need to know where the work happened
    /// can read it.
    pub fn new(
        inner:     Arc<dyn WorkerTask>,
        verifier:  Arc<dyn Verifier>,
        workspace: Option<PathBuf>,
    ) -> Arc<Self> {
        Arc::new(Self { inner, verifier, workspace })
    }
}

#[async_trait]
impl WorkerTask for VerifyingAdapter {
    async fn run(
        &self,
        assignment: WorkerAssignment,
        ctx:        WorkerContext,
    ) -> Result<WorkerComplete, WorkerFailure> {
        // Capture what we need before handing ctx to inner. agent_id
        // is Copy; sender_clone() returns a fresh ChannelSender we can
        // use after inner.run consumes ctx.
        let agent_id = ctx.agent_id;
        let sender   = ctx.sender_clone();
        let task_str = assignment.task.clone();

        // Run inner. Note assignment is cloned because WorkerAssignment
        // is Clone — the inner gets the canonical copy; we kept a clone
        // for use in the verification context if it succeeds.
        let inner_result = self.inner.run(assignment, ctx).await;

        let complete = match inner_result {
            Ok(c)  => c,
            // Inner failures pass through unchanged — no point
            // verifying work that already failed.
            Err(f) => return Err(f),
        };

        // Inner says Complete. Run verification before propagating.
        let _ = sender.send_event(Event::Progress {
            step_summary: "[verify] running verification".into(),
            percent_done: None,
            llm_spend_usd: 0.0,
        });

        let vctx = VerificationContext {
            task:      task_str,
            agent_id,
            workspace: self.workspace.clone(),
        };
        match self.verifier.verify(&complete, &vctx).await {
            Ok(()) => {
                let _ = sender.send_event(Event::Progress {
                    step_summary: "[verify] passed".into(),
                    percent_done: Some(1.0),
                    llm_spend_usd: 0.0,
                });
                // Annotate the summary so the manager / audit log can
                // tell at a glance the result was verification-gated.
                Ok(WorkerComplete {
                    result_summary: format!("[verified] {}", complete.result_summary),
                    artifacts:      complete.artifacts,
                })
            }
            Err(reason) => {
                let _ = sender.send_event(Event::Progress {
                    step_summary: format!("[verify] failed: {}", short_reason(&reason)),
                    percent_done: Some(1.0),
                    llm_spend_usd: 0.0,
                });
                // Convert Complete → Failed but preserve any artifacts
                // the inner left behind — partial work is still useful
                // for the user to inspect.
                Err(WorkerFailure {
                    error: format!("verification failed: {reason}"),
                    partial_artifacts: complete.artifacts,
                    fault: None,
                })
            }
        }
    }
}

/// First line + ellipsis if there's more — for the Progress event
/// summary when verification fails (full reason still goes into
/// WorkerFailure::error).
fn short_reason(s: &str) -> String {
    let first = s.lines().next().unwrap_or("(no detail)");
    if s.lines().count() > 1 || first.chars().count() > 120 {
        let head: String = first.chars().take(120).collect();
        format!("{head}…")
    } else {
        first.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::time::timeout;

    use crate::agent::instance::{Agent, AgentRegistry};
    use crate::agent::supervisor::{Supervisor, WorkerOutcome};

    // ── Test executors that stand in for the inner WorkerTask ────────

    /// Inner that always succeeds with a canned summary.
    struct AlwaysComplete(&'static str);
    #[async_trait]
    impl WorkerTask for AlwaysComplete {
        async fn run(
            &self, _: WorkerAssignment, _: WorkerContext,
        ) -> Result<WorkerComplete, WorkerFailure> {
            Ok(WorkerComplete {
                result_summary: self.0.into(),
                artifacts:      vec!["a/b.txt".into(), "c/d.txt".into()],
            })
        }
    }

    /// Inner that always fails with a canned error.
    struct AlwaysFail(&'static str);
    #[async_trait]
    impl WorkerTask for AlwaysFail {
        async fn run(
            &self, _: WorkerAssignment, _: WorkerContext,
        ) -> Result<WorkerComplete, WorkerFailure> {
            Err(WorkerFailure {
                error: self.0.into(),
                partial_artifacts: vec!["partial.log".into()], fault: None,
            })
        }
    }

    /// Records every call so we can assert the verifier was (or wasn't) invoked.
    struct RecordingVerifier {
        outcome: Result<(), String>,
        calls:   Mutex<Vec<String>>, // captures ctx.task per call
    }
    impl RecordingVerifier {
        fn new(outcome: Result<(), String>) -> Arc<Self> {
            Arc::new(Self { outcome, calls: Mutex::new(Vec::new()) })
        }
    }
    #[async_trait]
    impl Verifier for RecordingVerifier {
        async fn verify(
            &self, _c: &WorkerComplete, ctx: &VerificationContext,
        ) -> Result<(), String> {
            self.calls.lock().unwrap().push(ctx.task.clone());
            self.outcome.clone()
        }
    }

    fn fixture() -> (Arc<Supervisor>, crate::agent::instance::AgentId, u8) {
        let reg  = Arc::new(AgentRegistry::new());
        let root = reg.register(Agent::new_root());
        let (root_id, depth) = { let r = root.read().unwrap(); (r.id, r.depth) };
        let sup = Arc::new(Supervisor::new(reg));
        (sup, root_id, depth)
    }

    // ── Decorator behaviour ──────────────────────────────────────────

    #[tokio::test]
    async fn complete_plus_passing_verifier_propagates_complete_with_verified_tag() {
        let inner    = Arc::new(AlwaysComplete("did the thing"));
        let verifier = RecordingVerifier::new(Ok(()));
        let exec     = VerifyingAdapter::new(inner, verifier.clone(), None);
        let (sup, root_id, depth) = fixture();

        let h = sup.spawn_worker(
            root_id, depth, "com.test.verify", "task text", None,
            1.0, None, exec,
        );
        let outcome = timeout(crate::agent::supervisor::WORKER_JOIN_TIMEOUT, h.completion).await
            .expect("hung").unwrap();

        match outcome {
            WorkerOutcome::Complete(c) => {
                assert_eq!(c.result_summary, "[verified] did the thing");
                // Artifacts pass through.
                assert_eq!(c.artifacts.len(), 2);
            }
            other => panic!("expected Complete, got {other:?}"),
        }

        // Verifier was called exactly once with the original task.
        let calls = verifier.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], "task text");
    }

    #[tokio::test]
    async fn complete_plus_failing_verifier_becomes_failed_preserving_artifacts() {
        let inner    = Arc::new(AlwaysComplete("claims success"));
        let verifier = RecordingVerifier::new(Err("cargo test: 3 failed".into()));
        let exec     = VerifyingAdapter::new(inner, verifier, None);
        let (sup, root_id, depth) = fixture();

        let h = sup.spawn_worker(
            root_id, depth, "com.test.verify", "x", None,
            1.0, None, exec,
        );
        let outcome = timeout(crate::agent::supervisor::WORKER_JOIN_TIMEOUT, h.completion).await
            .expect("hung").unwrap();

        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(f.error.starts_with("verification failed:"), "got: {}", f.error);
                assert!(f.error.contains("cargo test: 3 failed"),    "got: {}", f.error);
                // Inner's artifacts surface via partial_artifacts.
                assert_eq!(f.partial_artifacts.len(), 2);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn inner_failure_passes_through_without_running_verifier() {
        let inner    = Arc::new(AlwaysFail("inner explosion"));
        let verifier = RecordingVerifier::new(Ok(())); // would pass if asked
        let exec     = VerifyingAdapter::new(inner, verifier.clone(), None);
        let (sup, root_id, depth) = fixture();

        let h = sup.spawn_worker(
            root_id, depth, "com.test.verify", "x", None,
            1.0, None, exec,
        );
        let outcome = timeout(crate::agent::supervisor::WORKER_JOIN_TIMEOUT, h.completion).await
            .expect("hung").unwrap();

        match outcome {
            WorkerOutcome::Failed(f) => {
                assert_eq!(f.error, "inner explosion");
                assert_eq!(f.partial_artifacts, vec!["partial.log"]);
            }
            other => panic!("expected Failed (passthrough), got {other:?}"),
        }

        // Verifier was *not* called — no work to verify.
        assert!(verifier.calls.lock().unwrap().is_empty(),
            "verifier should not run when inner failed");
    }

    #[tokio::test]
    async fn workspace_is_threaded_through_to_verifier_context() {
        struct WsCheck { workspace: Mutex<Option<PathBuf>> }
        #[async_trait]
        impl Verifier for WsCheck {
            async fn verify(
                &self, _c: &WorkerComplete, ctx: &VerificationContext,
            ) -> Result<(), String> {
                *self.workspace.lock().unwrap() = ctx.workspace.clone();
                Ok(())
            }
        }
        let v = Arc::new(WsCheck { workspace: Mutex::new(None) });

        let inner = Arc::new(AlwaysComplete("ok"));
        let exec  = VerifyingAdapter::new(
            inner, v.clone(), Some(PathBuf::from("/tmp/example-project")),
        );
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.verify", "x", None,
            1.0, None, exec,
        );
        let _ = timeout(crate::agent::supervisor::WORKER_JOIN_TIMEOUT, h.completion).await
            .expect("hung").unwrap();

        let saw = v.workspace.lock().unwrap().clone();
        assert_eq!(saw, Some(PathBuf::from("/tmp/example-project")));
    }

    #[tokio::test]
    async fn no_op_verifier_lets_complete_through_unchanged_apart_from_tag() {
        let inner = Arc::new(AlwaysComplete("happy"));
        let exec  = VerifyingAdapter::new(inner, Arc::new(NoOpVerifier), None);
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.verify", "x", None,
            1.0, None, exec,
        );
        let outcome = timeout(crate::agent::supervisor::WORKER_JOIN_TIMEOUT, h.completion).await
            .expect("hung").unwrap();
        match outcome {
            WorkerOutcome::Complete(c) => assert_eq!(c.result_summary, "[verified] happy"),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    // ── SubprocessVerifier behaviour ─────────────────────────────────

    #[tokio::test]
    async fn subprocess_verifier_passes_on_exit_zero() {
        let v = SubprocessVerifier::new("true");
        let ctx = VerificationContext {
            task: "x".into(), agent_id: AgentId::new(), workspace: None,
        };
        let complete = WorkerComplete::default();
        v.verify(&complete, &ctx).await.expect("true should pass");
    }

    #[tokio::test]
    async fn subprocess_verifier_fails_on_exit_nonzero_with_combined_tail() {
        let v = SubprocessVerifier::new(
            "echo TEST_OUT; echo TEST_ERR >&2; exit 7",
        );
        let ctx = VerificationContext {
            task: "x".into(), agent_id: AgentId::new(), workspace: None,
        };
        let complete = WorkerComplete::default();
        let err = v.verify(&complete, &ctx).await.unwrap_err();
        assert!(err.contains("exit 7"),    "got: {err}");
        assert!(err.contains("TEST_OUT"),  "got: {err}");
        assert!(err.contains("TEST_ERR"),  "got: {err}");
    }

    #[tokio::test]
    async fn subprocess_verifier_honors_cwd_for_relative_commands() {
        // The verifier's bash inherits cwd from `cmd.current_dir`. We
        // assert by comparing $(pwd) against the path we configured.
        let dir = std::env::temp_dir();
        let v = SubprocessVerifier::new(
            format!(r#"test "$(pwd)" = "{}""#, dir.display()),
        ).with_cwd(&dir);
        let ctx = VerificationContext {
            task: "x".into(), agent_id: AgentId::new(), workspace: None,
        };
        v.verify(&WorkerComplete::default(), &ctx).await
            .expect("cwd was honoured");
    }

    #[tokio::test]
    async fn subprocess_verifier_times_out_long_running_commands() {
        let v = SubprocessVerifier::new("sleep 60").with_timeout(Some(1));
        let ctx = VerificationContext {
            task: "x".into(), agent_id: AgentId::new(), workspace: None,
        };
        let err = v.verify(&WorkerComplete::default(), &ctx).await.unwrap_err();
        assert!(err.contains("timed out after 1s"), "got: {err}");
    }

    #[tokio::test]
    async fn subprocess_verifier_propagates_spawn_failure() {
        // bash exists; the trick is to set a cwd that doesn't exist.
        // `bash -c` will run but the command itself will fail; we
        // exercise the spawn-error path differently: point at a
        // non-existent directory.
        let v = SubprocessVerifier::new("true").with_cwd("/no/such/dir/at/all");
        let ctx = VerificationContext {
            task: "x".into(), agent_id: AgentId::new(), workspace: None,
        };
        let err = v.verify(&WorkerComplete::default(), &ctx).await.unwrap_err();
        // Either spawn error (most platforms) or non-zero exit; we just
        // assert it's a structured failure, not a panic.
        assert!(!err.is_empty());
    }

    // ── short_reason helper ──────────────────────────────────────────

    #[test]
    fn short_reason_returns_first_line_when_short_and_single_line() {
        assert_eq!(short_reason("oh no"), "oh no");
    }

    #[test]
    fn short_reason_truncates_long_first_line_with_ellipsis() {
        let long = "a".repeat(200);
        let s = short_reason(&long);
        assert_eq!(s.chars().count(), 121); // 120 + ellipsis
        assert!(s.ends_with('…'));
    }

    #[test]
    fn short_reason_truncates_when_multiple_lines_present() {
        let s = short_reason("first\nsecond\nthird");
        assert!(s.starts_with("first"));
        assert!(s.ends_with('…'));
    }

    // ── tail_chars helper ────────────────────────────────────────────

    #[test]
    fn tail_chars_returns_input_unchanged_when_short() {
        assert_eq!(tail_chars("abc", 10), "abc");
    }

    #[test]
    fn tail_chars_keeps_only_last_n_when_long() {
        assert_eq!(tail_chars("abcdefghij", 3), "hij");
    }

    #[test]
    fn tail_chars_handles_multibyte_utf8_safely() {
        assert_eq!(tail_chars("🦀🦊🐺🦝", 2), "🐺🦝");
    }
}
