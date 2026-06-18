// SPDX-License-Identifier: AGPL-3.0-or-later

//! Manager↔worker message types (slice B2).
//!
//! See `design-docs/skills-and-agents.md` §"Manager/worker protocol" for the
//! authoritative table of which side sends which message.
//!
//! Three categories:
//!
//! - **`Request`** — expects a `Response` back. Carries an envelope id
//!   so the sender can correlate the reply with the call it issued.
//! - **`Response`** — reply to a Request. Same envelope id as the
//!   original Request.
//! - **`Event`** — fire-and-forget. No reply expected.
//!
//! Manager → worker requests: `Assign`, `Interrupt`, `Pause`, `Resume`,
//! `QueryStatus`.
//! Worker → manager requests: `RequestReview`, `RequestUserInput`,
//! `SpawnChild`. (These need a manager *decision* to proceed, so
//! they're requests rather than events.)
//! Worker → manager events: `Progress`, `Complete`, `Failed`.
//!
//! The same enums cover both directions — only one variant per name —
//! so the transport doesn't need direction-specific generics. Senders
//! that emit a variant they shouldn't are caught in code review and by
//! the agent loop ignoring out-of-band messages, not by the type system.

use serde::{Deserialize, Serialize};

use crate::agent::instance::AgentId;

// ─── Envelope ──────────────────────────────────────────────────────────

/// Envelope id. Monotonic per-channel; a sender allocates these from an
/// `AtomicU64` and uses them to match `Response`s to outstanding
/// `Request`s. `0` is reserved for "no correlation needed" (events).
pub type EnvelopeId = u64;

/// Wire-level container. Every message a peer sends is one of these.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Envelope {
    Request  { id: EnvelopeId, payload: Request  },
    Response { id: EnvelopeId, payload: Response },
    Event    {                  payload: Event    },
}

// ─── Requests ──────────────────────────────────────────────────────────

/// Reasons an interrupt was raised. Surfaced in the worker's failure
/// reason so audit logs can distinguish a user-stop from a budget
/// runaway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptReason {
    User,
    Timeout,
    Budget,
    Policy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// Manager → worker: hand the worker its task. The worker accepts
    /// or rejects; rejection short-circuits the spawn.
    Assign {
        task:        String,
        context:     Option<serde_json::Value>,
        budget_usd:  f64,
        /// Wall-clock deadline as unix ms. None = no deadline (the
        /// budget is the cap).
        deadline_ms: Option<i64>,
        /// User on whose behalf this worker runs. Adapters use it to
        /// look up per-user secrets (e.g. `ANTHROPIC_API_KEY` in the
        /// skill secrets vault). None for system-internal workers
        /// (heartbeats, tests).
        #[serde(default)]
        user_id:     Option<String>,
        /// Reverse-DNS skill id, mirrored from `Agent.skill_id`. The
        /// adapter could read it off the agent registry instead, but
        /// duplicating it here keeps adapters from needing a
        /// registry handle just to find their own row.
        #[serde(default)]
        skill_id:    Option<String>,
    },
    /// Manager → worker: stop work; persist state; report status as
    /// Interrupted. Worker has 10s to ack; transport / supervisor (B5)
    /// SIGKILL after.
    Interrupt { reason: InterruptReason },
    /// Manager → worker: finish in-flight LLM call, persist, halt.
    Pause,
    /// Manager → worker: come back to life after a Pause.
    Resume,
    /// Manager → worker: snapshot. Cheap; can be polled.
    QueryStatus,

    /// Worker → manager: I produced an artifact and want a go/no-go
    /// before continuing. Manager replies with `ReviewDecision`.
    RequestReview {
        artifact_path: String,
        summary:       String,
    },
    /// Worker → manager: I need the user's input on a question. Manager
    /// is responsible for surfacing the question through whatever path
    /// makes sense (chat for the root, escalate to its own manager for
    /// nested workers).
    RequestUserInput {
        question: String,
        #[serde(default)]
        options:  Vec<String>,
    },
    /// Worker → manager: I want to spawn a sub-worker. Manager checks
    /// recursion depth + session budget before approving.
    SpawnChild {
        skill_id:   String,
        task:       String,
        budget_usd: f64,
    },
}

// ─── Responses ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    /// Reply to `Assign`: worker accepted and started.
    Accepted,
    /// Reply to `Assign`: worker refused (e.g. unsupported task shape).
    Rejected { reason: String },
    /// Generic ack for `Interrupt` / `Pause` / `Resume`.
    Ack,
    /// Reply to `QueryStatus`.
    Status {
        status:          AgentStateSnapshot,
        current_step:    Option<String>,
        llm_spend_usd:   f64,
        child_agents:    Vec<AgentId>,
    },
    /// Reply to `RequestReview`. `approved=false` with `reason` lets
    /// the worker either retry or report failure.
    ReviewDecision { approved: bool, reason: Option<String> },
    /// Reply to `RequestUserInput`.
    UserResponse { response: String },
    /// Reply to `SpawnChild`. On approval, the manager has registered
    /// the child Agent and the worker is given its id so it can route
    /// follow-up requests to that specific child.
    SpawnDecision {
        approved:         bool,
        reason:           Option<String>,
        spawned_agent_id: Option<AgentId>,
    },
    /// Generic error response — used when a Request can't be served at
    /// all (malformed, unrecognised variant, peer in wrong state).
    Error { message: String },
}

/// Lightweight subset of `AgentStatus` for the wire — keeps the
/// protocol enum independent of the `Agent` struct's evolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStateSnapshot {
    Pending, Running, Paused, Completed, Failed, Interrupted,
}

impl From<crate::agent::AgentStatus> for AgentStateSnapshot {
    fn from(s: crate::agent::AgentStatus) -> Self {
        use crate::agent::AgentStatus as S;
        match s {
            S::Pending     => Self::Pending,
            S::Running     => Self::Running,
            S::Paused      => Self::Paused,
            S::Completed   => Self::Completed,
            S::Failed      => Self::Failed,
            S::Interrupted => Self::Interrupted,
        }
    }
}

// ─── Events ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// Worker → manager: streamed periodically while running.
    Progress {
        step_summary:  String,
        /// 0.0–1.0 when the worker can estimate; None when it can't.
        percent_done:  Option<f32>,
        llm_spend_usd: f64,
    },
    /// Worker → manager: terminal success. Worker exits after this.
    Complete {
        result_summary: String,
        #[serde(default)]
        artifacts:      Vec<String>,
    },
    /// Worker → manager: terminal failure. Worker exits after this.
    Failed {
        error:              String,
        #[serde(default)]
        partial_artifacts:  Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T: Serialize + for<'de> Deserialize<'de> + std::fmt::Debug + PartialEq>(v: T) {
        let json = serde_json::to_string(&v).expect("serialise");
        let back: T = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(v, back, "round-trip changed value: {json}");
    }

    // ── Manual PartialEq for the variants we care about in tests ──
    impl PartialEq for Request {
        fn eq(&self, other: &Self) -> bool {
            serde_json::to_value(self).unwrap() == serde_json::to_value(other).unwrap()
        }
    }
    impl PartialEq for Response {
        fn eq(&self, other: &Self) -> bool {
            serde_json::to_value(self).unwrap() == serde_json::to_value(other).unwrap()
        }
    }
    impl PartialEq for Event {
        fn eq(&self, other: &Self) -> bool {
            serde_json::to_value(self).unwrap() == serde_json::to_value(other).unwrap()
        }
    }
    impl PartialEq for Envelope {
        fn eq(&self, other: &Self) -> bool {
            serde_json::to_value(self).unwrap() == serde_json::to_value(other).unwrap()
        }
    }

    #[test]
    fn assign_request_roundtrips() {
        round_trip(Request::Assign {
            task:        "Build a Flask app".into(),
            context:     Some(serde_json::json!({"project_dir": "/tmp/proj"})),
            budget_usd:  1.50,
            deadline_ms: Some(1_777_700_000_000),
            user_id:     Some("alice".into()),
            skill_id:    Some("com.mira.claudecode".into()),
        });
    }

    #[test]
    fn interrupt_carries_reason() {
        round_trip(Request::Interrupt { reason: InterruptReason::Budget });
        round_trip(Request::Interrupt { reason: InterruptReason::User });
    }

    #[test]
    fn worker_requests_roundtrip() {
        round_trip(Request::RequestReview {
            artifact_path: "/projects/xyz/src/main.rs".into(),
            summary:       "Initial scaffold".into(),
        });
        round_trip(Request::RequestUserInput {
            question: "Use Postgres or SQLite?".into(),
            options:  vec!["postgres".into(), "sqlite".into()],
        });
        round_trip(Request::SpawnChild {
            skill_id:   "com.mira.research".into(),
            task:       "find papers on X".into(),
            budget_usd: 0.50,
        });
    }

    #[test]
    fn responses_roundtrip() {
        round_trip(Response::Accepted);
        round_trip(Response::Rejected { reason: "no auth".into() });
        round_trip(Response::Ack);
        round_trip(Response::ReviewDecision { approved: false, reason: Some("missing tests".into()) });
        round_trip(Response::UserResponse { response: "postgres".into() });
        round_trip(Response::SpawnDecision {
            approved: true, reason: None, spawned_agent_id: Some(AgentId::new()),
        });
        round_trip(Response::Error { message: "unsupported".into() });
    }

    #[test]
    fn events_roundtrip() {
        round_trip(Event::Progress {
            step_summary: "Fetched 12 sources".into(),
            percent_done: Some(0.4),
            llm_spend_usd: 0.12,
        });
        round_trip(Event::Complete {
            result_summary: "Built app at /projects/xyz".into(),
            artifacts:      vec!["/projects/xyz".into()],
        });
        round_trip(Event::Failed {
            error: "tests failed".into(),
            partial_artifacts: vec!["/projects/xyz/partial".into()],
        });
    }

    #[test]
    fn envelope_carries_correlation_id() {
        let env = Envelope::Request {
            id: 42,
            payload: Request::Pause,
        };
        round_trip(env);
    }

    #[test]
    fn agent_status_snapshot_is_complete() {
        // Mirrors must keep up with new variants. Compile-time check
        // would be nice; this test catches it at run time at least.
        for s in [
            crate::agent::AgentStatus::Pending,
            crate::agent::AgentStatus::Running,
            crate::agent::AgentStatus::Paused,
            crate::agent::AgentStatus::Completed,
            crate::agent::AgentStatus::Failed,
            crate::agent::AgentStatus::Interrupted,
        ] {
            // Doesn't panic = mapping is total.
            let _: AgentStateSnapshot = s.into();
        }
    }
}
