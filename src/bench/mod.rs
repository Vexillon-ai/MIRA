// SPDX-License-Identifier: AGPL-3.0-or-later

// src/bench/mod.rs
//! Memory benchmarking harness (roadmap #9).
//!
//! Runs MIRA's *real* memory stack against the LongMemEval benchmark so the
//! wiki-memory story can be backed by numbers comparable to the published
//! mem0 / Zep / Letta scores, and re-run as memory evolves.
//!
//! Flow per question (**replay mode** — faithful):
//!   1. Spin up an isolated MIRA instance (throwaway memory DB + wiki +
//!      history + session store) so questions don't contaminate each other.
//!   2. **Replay** every haystack session as real conversation turns through
//!      the full agent pipeline, so auto-extraction + wiki + memory populate
//!      exactly as they do in production.
//!   3. Ask the benchmark question as a final turn; capture the answer.
//!   4. LLM-judge the answer against the gold answer (LongMemEval protocol).
//!   5. Aggregate per-question-type + overall accuracy, plus retrieval recall.
//!
//! Exposed as `mira bench memory --dataset <path> [--limit N] …`. The dataset
//! is not redistributable; the operator supplies the path.

pub mod longmemeval;
pub mod run;
pub mod judge;
pub mod context;

pub use context::ContextBenchOptions;

use std::path::PathBuf;

/// CLI-supplied options for a memory benchmark run.
#[derive(Debug, Clone)]
pub struct MemoryBenchOptions {
    /// Path to the LongMemEval dataset JSON (operator-supplied).
    pub dataset: PathBuf,
    /// Cap the number of questions (smoke runs). `None` = all.
    pub limit: Option<usize>,
    /// Only run questions of this `question_type` (the dataset is grouped by
    /// type, so this is how a smoke run samples a specific category rather than
    /// just the first block). `None` = all types.
    pub question_type: Option<String>,
    /// Provider id (from `mira_config.json`) used to *answer* questions.
    /// `None` = the configured default/failover provider.
    pub answer_provider: Option<String>,
    /// Provider id used to *judge* answers. `None` = same as answer provider.
    pub judge_provider: Option<String>,
    /// Override the chosen provider's model for this run (e.g. pick a cheap
    /// model on OpenRouter without editing the operator's config). Applies to
    /// both the answer and judge providers. `None` = the provider's configured
    /// default model.
    pub model: Option<String>,
    /// Pin the *extraction* model independently of the answer model. When set,
    /// haystack replay (memory + wiki extraction) uses this model while the QA
    /// answer + judge use `model`. This isolates the answer model's effect from
    /// extraction quality (diagnostic: "is the gap retrieval or the answerer?").
    /// `None` = extraction uses the same provider/model as answering.
    pub extract_model: Option<String>,
    /// Where to write the JSON results report. `None` = stdout summary only.
    pub out: Option<PathBuf>,
    /// Only print the dataset summary (counts per type) and exit — no API spend.
    pub dry_run: bool,
}
