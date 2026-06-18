// SPDX-License-Identifier: AGPL-3.0-or-later

// src/bench/run.rs
//! Replay-mode orchestration for the LongMemEval memory benchmark.
//!
//! Per question: a throwaway MIRA (fresh memory DB + wiki + session store) is
//! built, the haystack sessions are **replayed through the real extraction
//! pipeline** (the same memory + wiki extractors a production turn fires —
//! awaited, so ingestion completes before the question), the question is then
//! asked via the normal turn path so the real memory + wiki *retrieval* runs,
//! and the answer is LLM-judged against the gold answer.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;
use tracing::warn;

use crate::agent::{AgentCore, StreamEvent, TurnContext};
use crate::config::MiraConfig;
use crate::memory::MemorySystem;
use crate::providers::ModelProvider;
use crate::session::SessionStore;
use crate::tools::ToolRegistry;
use crate::wiki::WikiRegistry;
use crate::MiraError;

use super::judge;
use super::longmemeval::{self, Question};
use super::MemoryBenchOptions;

const BENCH_CHANNEL: &str = "bench";
const BENCH_USER: &str = "bench-user";

// ── Report types ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct QuestionResult {
    pub question_id:   String,
    pub question_type: String,
    pub is_abstention: bool,
    pub correct:       bool,
    pub gold_answer:   String,
    pub model_answer:  String,
    // Memories the LLM extractor stored during replay (0 ⇒ nothing to recall).
    pub memories_stored: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct TypeStats {
    pub total:   usize,
    pub correct: usize,
}

#[derive(Debug, Serialize)]
pub struct BenchReport {
    pub dataset:         String,
    pub answer_provider: String,
    pub judge_provider:  String,
    pub total:           usize,
    pub correct:         usize,
    pub accuracy:        f64,
    pub per_type:        BTreeMap<String, TypeStats>,
    pub results:         Vec<QuestionResult>,
}

impl BenchReport {
    // Human-readable summary table.
    pub fn markdown(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "## LongMemEval — MIRA memory benchmark\n\n\
             - Dataset: `{}`\n- Answer model: `{}`  |  Judge: `{}`\n\
             - **Overall: {}/{} = {:.1}%**\n\n\
             | Question type | Correct | Total | Accuracy |\n\
             |---|---:|---:|---:|\n",
            self.dataset, self.answer_provider, self.judge_provider,
            self.correct, self.total, self.accuracy * 100.0,
        ));
        for (ty, st) in &self.per_type {
            let acc = if st.total > 0 { st.correct as f64 / st.total as f64 * 100.0 } else { 0.0 };
            s.push_str(&format!("| {ty} | {} | {} | {:.1}% |\n", st.correct, st.total, acc));
        }
        s
    }
}

// ── Entry point ───────────────────────────────────────────────────────────--

// Run the memory benchmark per [`MemoryBenchOptions`].
pub async fn run_memory_bench(
    opts:   MemoryBenchOptions,
    config: Arc<MiraConfig>,
) -> Result<(), MiraError> {
    let mut questions = longmemeval::load_dataset(&opts.dataset)?;

    // Optional type filter (the dataset is grouped by type, so this is how a
    // smoke run samples a chosen category instead of just the first block).
    if let Some(ty) = &opts.question_type {
        questions.retain(|q| &q.question_type == ty);
        println!("Filtered to question_type = '{ty}' ({} questions).", questions.len());
    }

    // Dataset summary (always printed; the only output for --dry-run).
    let hist = longmemeval::type_histogram(&questions);
    let total_turns: usize = questions.iter().map(|q| q.turn_count()).sum();
    println!(
        "Loaded {} questions ({} haystack turns) from {}",
        questions.len(), total_turns, opts.dataset.display()
    );
    for (ty, n) in &hist {
        println!("  {ty}: {n}");
    }
    if opts.dry_run {
        println!("\n(dry run — no questions executed, no API spend)");
        return Ok(());
    }

    if let Some(limit) = opts.limit {
        if questions.len() > limit {
            questions.truncate(limit);
            println!("Limiting to first {limit} question(s).");
        }
    }

    let answer_provider = build_provider_for(&config, opts.answer_provider.as_deref(), opts.model.as_deref())?;
    let judge_provider = match opts.judge_provider.as_deref() {
        Some(id) => build_provider_for(&config, Some(id), opts.model.as_deref())?,
        None     => Arc::clone(&answer_provider),
    };
    // Extraction provider: when `--extract-model` is given, replay extraction
    // runs on that model while the answer/judge use `--model`. This holds the
    // *stored memories* constant across answer-model variants, so a score delta
    // is attributable to the answerer, not to different extraction. Default:
    // extraction shares the answer provider (production-realistic single model).
    let extract_provider = match opts.extract_model.as_deref() {
        Some(m) => build_provider_for(&config, opts.answer_provider.as_deref(), Some(m))?,
        None    => Arc::clone(&answer_provider),
    };
    println!(
        "Replaying {} question(s). Answer: '{}' | Extract: '{}' | Judge: '{}'.\n\
         This replays every haystack turn through the real extraction pipeline \
         and makes answer+judge calls — real API spend.\n",
        questions.len(), answer_provider.name(), extract_provider.name(), judge_provider.name()
    );

    let mut results: Vec<QuestionResult> = Vec::with_capacity(questions.len());
    for (i, q) in questions.iter().enumerate() {
        print!(
            "[{}/{}] {} ({}, {} turns) … ",
            i + 1, questions.len(), q.question_id, q.question_type, q.turn_count()
        );
        use std::io::Write;
        let _ = std::io::stdout().flush();

        match run_one_question(q, &config, &answer_provider, &extract_provider, &judge_provider).await {
            Ok(r) => {
                println!("{} (stored {} memories)", if r.correct { "CORRECT" } else { "wrong" }, r.memories_stored);
                results.push(r);
            }
            Err(e) => {
                println!("ERROR");
                warn!("bench: question '{}' failed: {e}", q.question_id);
                results.push(QuestionResult {
                    question_id:   q.question_id.clone(),
                    question_type: q.question_type.clone(),
                    is_abstention: q.is_abstention(),
                    correct:       false,
                    gold_answer:   q.answer_text(),
                    model_answer:  format!("<error: {e}>"),
                    memories_stored: 0,
                });
            }
        }
    }

    let report = aggregate(&opts, &answer_provider, &judge_provider, results);
    println!("\n{}", report.markdown());

    if let Some(out) = &opts.out {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|e| MiraError::ConfigError(format!("serialise report: {e}")))?;
        std::fs::write(out, json)
            .map_err(|e| MiraError::ConfigError(format!("write {}: {e}", out.display())))?;
        println!("Wrote JSON report to {}", out.display());
    }

    Ok(())
}

// ── Per-question replay ───────────────────────────────────────────────────--

// Parse a LongMemEval session date ("2023/05/20 (Sat) 02:21" or "2023/05/20")
// into unix-ms at midnight UTC, for the graph's `event_at`. Lenient: returns
// None when the leading token isn't a `YYYY/MM/DD` date.
fn parse_bench_date(s: &str) -> Option<i64> {
    let first = s.trim_start_matches('[').split_whitespace().next()?;
    let d = chrono::NaiveDate::parse_from_str(first, "%Y/%m/%d").ok()?;
    Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis())
}

async fn run_one_question(
    q:                &Question,
    config:           &Arc<MiraConfig>,
    answer_provider:  &Arc<dyn ModelProvider>,
    extract_provider: &Arc<dyn ModelProvider>,
    judge_provider:   &Arc<dyn ModelProvider>,
) -> Result<QuestionResult, MiraError> {
    // Isolated state so questions never contaminate each other.
    let tmp = tempfile::tempdir()
        .map_err(|e| MiraError::ConfigError(format!("tempdir: {e}")))?;
    let data_dir = tmp.path().to_path_buf();

    let memory = Arc::new(
        MemorySystem::new_from_embedding_config(data_dir.join("memory.db"), &config.memory).await?
    );
    let wiki_registry = Arc::new(WikiRegistry::new(data_dir.join("wikis")));
    let sessions = Arc::new(SessionStore::new());
    let tools = Arc::new(ToolRegistry::new());

    // The QA turn must RETRIEVE from the wiki but must NOT extract again: the
    // turn's wiki post_hook is fire-and-forget (tokio::spawn), so it races the
    // per-question temp-dir teardown ("attempt to write a readonly database")
    // and wastes a model call on a result we discard. Build the AgentCore with
    // wiki auto-extract OFF (retrieval is unaffected — it's gated separately);
    // ingestion uses `wiki_cfg` (mode=auto) via the explicit replay calls.
    let mut qa_cfg = (**config).clone();
    qa_cfg.wiki.auto_extract.mode = "off".to_string();
    let agent = Arc::new(AgentCore::new(
        Arc::new(qa_cfg),
        Arc::clone(answer_provider),
        Arc::clone(&memory),
        Arc::clone(&tools),
        Arc::clone(&sessions),
    ));
    let _ = agent.set_wiki(Arc::clone(&wiki_registry));

    // ── Ingest: replay each user→assistant exchange through the real extractors.
    let cats     = config.memory.auto_extract.allowed_categories.clone();
    let min_conf = crate::memory::auto_extract::ConfidenceTier::parse(
        &config.memory.auto_extract.min_confidence,
    );
    let wiki_cfg     = config.wiki.auto_extract.clone();
    let wiki_enabled = config.wiki.enabled && !wiki_cfg.mode.eq_ignore_ascii_case("off");
    let conv_id = format!("bench-{}", q.question_id);
    let mut stored = 0usize;

    for (si, session) in q.haystack_sessions.iter().enumerate() {
        // LongMemEval sessions are timestamped; temporal-reasoning and
        // knowledge-update questions need to know *when* each session happened.
        // Prefix the user message with the session date so the extractors
        // capture the temporal anchor (mirrors LongMemEval's own protocol).
        let date = q.haystack_dates.get(si).map(|s| s.as_str()).unwrap_or("");
        let mut pending_user: Option<String> = None;
        for turn in session {
            match turn.role.as_str() {
                "user" => {
                    let c = if date.is_empty() {
                        turn.content.clone()
                    } else {
                        format!("[{date}] {}", turn.content)
                    };
                    pending_user = Some(c);
                }
                "assistant" => {
                    let user_msg = pending_user.take().unwrap_or_default();
                    let assistant_msg = &turn.content;

                    // The benchmark always uses the LLM extractor (MIRA's best
                    // memory), regardless of the deployment's configured mode,
                    // so the number reflects capability not a degraded config.
                    stored += memory.auto_extract_llm_and_store(
                        extract_provider, &user_msg, assistant_msg,
                        BENCH_USER, BENCH_CHANNEL, None, None, &cats, min_conf,
                    ).await;

                    // Knowledge-graph population — runs alongside flat
                    // extraction so the two can be A/B'd. Off unless the config
                    // flag is set; uses the same extraction model.
                    if config.memory.graph.enabled {
                        let event_at = parse_bench_date(date);
                        memory.graph_extract_and_store(
                            extract_provider, &user_msg, assistant_msg, BENCH_USER, event_at,
                        ).await;
                    }

                    if wiki_enabled {
                        crate::agent::wiki_hook::run_wiki_extraction(
                            Arc::clone(&wiki_registry),
                            Arc::clone(extract_provider),
                            BENCH_USER.to_string(),
                            conv_id.clone(),
                            uuid::Uuid::now_v7().to_string(),
                            user_msg,
                            assistant_msg.clone(),
                            wiki_cfg.clone(),
                        ).await;
                    }
                }
                _ => {}
            }
        }
    }

    // Sleep-like consolidator passes (gated by per-phase config flags),
    // mirroring production's nightly job running between ingestion and next-day
    // retrieval. Phase C first so dedup's edge-count tiebreak sees post-
    // resolution counts. See design-docs/memory-research-2026.md §5.
    if config.memory.graph.enabled {
        if config.memory.consolidation.contradictions_enabled {
            let (groups, closed) = memory.consolidate_contradictions(BENCH_USER);
            if groups > 0 {
                eprint!("[contradict: resolved {groups} groups, closed {closed} edges] ");
            }
        }
        if config.memory.consolidation.entity_dedup_enabled {
            let (merged, repointed) = memory.consolidate_entities(
                BENCH_USER, config.memory.consolidation.entity_dedup_ratio,
            );
            if merged > 0 {
                eprint!("[entity-dedup: merged {merged} pairs, repointed {repointed} edges] ");
            }
        }
        if config.memory.consolidation.importance_enabled {
            let scored = memory.consolidate_importance(
                BENCH_USER, config.memory.consolidation.importance_half_life_days,
            );
            if scored > 0 {
                eprint!("[importance: scored {scored} edges] ");
            }
        }
        let (ents, edges) = memory.graph_counts(BENCH_USER);
        eprint!("[graph: {ents} entities, {edges} edges] ");
        for line in memory.graph_sample(BENCH_USER, 60) {
            eprintln!("\n    · {line}");
        }
    }

    // ── Ask the question: real memory + wiki retrieval, real answer model.
    // Give the model "now" so temporal questions can reason about it.
    let dated_question = if q.question_date.is_empty() {
        q.question.clone()
    } else {
        format!("(Today is {}.) {}", q.question_date, q.question)
    };
    let answer = ask(&agent, &conv_id, &dated_question).await?;

    // ── Judge.
    let gold = q.answer_text();
    let correct = judge::judge_answer(
        judge_provider, &q.question, &gold, &answer, q.is_abstention(),
    ).await?;

    Ok(QuestionResult {
        question_id:   q.question_id.clone(),
        question_type: q.question_type.clone(),
        is_abstention: q.is_abstention(),
        correct,
        gold_answer:   gold,
        model_answer:  answer,
        memories_stored: stored,
    })
}

// Drive one turn to completion and collect the assistant's text.
async fn ask(agent: &Arc<AgentCore>, session_id: &str, question: &str) -> Result<String, MiraError> {
    // Default context = memory + wiki hooks ON (real retrieval).
    let mut rx = agent
        .process_with_context(session_id, BENCH_USER, BENCH_CHANNEL, question, None, TurnContext::default())
        .await?;

    let mut answer = String::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            StreamEvent::Token(t) => answer.push_str(&t),
            StreamEvent::Error(e) => {
                return Err(MiraError::ConfigError(format!("turn error: {e}")));
            }
            StreamEvent::Done { .. } => break,
            _ => {}
        }
    }
    Ok(answer.trim().to_string())
}

// ── Aggregation + provider construction ───────────────────────────────────--

fn aggregate(
    opts:            &MemoryBenchOptions,
    answer_provider: &Arc<dyn ModelProvider>,
    judge_provider:  &Arc<dyn ModelProvider>,
    results:         Vec<QuestionResult>,
) -> BenchReport {
    let total = results.len();
    let correct = results.iter().filter(|r| r.correct).count();
    let mut per_type: BTreeMap<String, TypeStats> = BTreeMap::new();
    for r in &results {
        let e = per_type.entry(r.question_type.clone()).or_default();
        e.total += 1;
        if r.correct { e.correct += 1; }
    }
    BenchReport {
        dataset:         opts.dataset.display().to_string(),
        answer_provider: answer_provider.name().to_string(),
        judge_provider:  judge_provider.name().to_string(),
        total,
        correct,
        accuracy: if total > 0 { correct as f64 / total as f64 } else { 0.0 },
        per_type,
        results,
    }
}

// Build a provider chain, optionally overriding which provider heads it and
// which model that provider uses (so a paid run can pick a cheap model
// without editing the operator's config).
fn build_provider_for(
    config:      &MiraConfig,
    provider_id: Option<&str>,
    model:       Option<&str>,
) -> Result<Arc<dyn ModelProvider>, MiraError> {
    let mut c = config.clone();
    if let Some(id) = provider_id {
        c.primary_provider = id.to_string();
    }
    if let Some(m) = model {
        let id = provider_id.map(str::to_string).unwrap_or_else(|| c.primary_provider.clone());
        let m = m.to_string();
        let p = &mut c.providers;
        match id.as_str() {
            "ollama"     => p.ollama.default_model     = m,
            "lmstudio"   => p.lmstudio.default_model   = m,
            "openrouter" => p.openrouter.default_model = m,
            "openai"     => p.openai.default_model     = m,
            "deepseek"   => p.deepseek.default_model   = m,
            "moonshot"   => p.moonshot.default_model   = m,
            "groq"       => p.groq.default_model       = m,
            "xai"        => p.xai.default_model        = m,
            "anthropic"  => p.anthropic.default_model  = m,
            "gemini"     => p.gemini.default_model     = m,
            other => return Err(MiraError::ConfigError(
                format!("--model override not supported for provider '{other}'"))),
        }
    }
    crate::gateway::builder::build_provider_chain(&c)
}
