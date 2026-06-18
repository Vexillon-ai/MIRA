// SPDX-License-Identifier: AGPL-3.0-or-later

// src/memory/rollup.rs
//! Daily rollup job for long-term memory consolidation.
//!
//! Per-turn extraction (see `auto_extract::LlmMemoryExtractor`) captures
//! single facts as they appear. Transcript indexing (see
//! `history::indexer`) makes every message searchable. Neither of those
//! consolidates *patterns* that only emerge across a whole day of
//! conversation — "Tarek spent most of Tuesday debugging Axum middleware"
//! isn't a fact the per-turn extractor would write, but it's exactly the
//! kind of thing we'd want to recall weeks later.
//!
//! This module runs a background poller that, at a configurable cadence,
//! finds users with conversation activity on the previous UTC day, asks
//! the model to produce one short consolidation paragraph from that day's
//! user+assistant messages, and stores it as an `AutoExtracted` memory
//! tagged `rollup` + `rollup:YYYY-MM-DD`.
//!
//! Idempotency: before running a user's rollup we check for a live memory
//! with the `rollup:YYYY-MM-DD` tag. A tick that restarts mid-day — or a
//! server that was offline when the first tick should have fired — picks
//! up exactly where it left off without double-writing.
//!
//! Failure policy: everything is fire-and-forget. A single user's LLM
//! failure logs a warning and moves to the next user. The next tick will
//! retry. The poll loop never propagates errors to the caller.

use std::sync::Arc;
use std::time::Duration;

use chrono::{Datelike, NaiveDate, TimeZone, Utc};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::history::HistoryStore;
use crate::memory::storage::{Category, MemorySource, Scope};
use crate::memory::MemorySystem;
use crate::providers::ModelProvider;
use crate::types::{ChatMessage, GenerationOptions};

/// Hard cap on messages fed to one rollup prompt. Beyond this we truncate
/// oldest-first — a 1000-message day is more "everything since Monday"
/// than "one day's conversation" anyway.
const DEFAULT_MAX_MESSAGES: usize = 200;
/// Hard cap on per-message characters sent to the summarizer. Long pastes
/// (code, logs) rarely help a day summary; truncating keeps the prompt
/// bounded without losing the surrounding conversation.
const DEFAULT_MAX_CHARS_PER_MSG: usize = 800;
/// Timeout for one LLM summarization call. A slow provider shouldn't wedge
/// the poll loop across all users.
const LLM_TIMEOUT: Duration = Duration::from_secs(60);
/// Short warmup before the first tick so the provider has time to health-
/// check and the memory DB is already open.
const STARTUP_WARMUP: Duration = Duration::from_secs(30);

/// Runtime configuration. Populated from `config.memory.rollup` in the
/// gateway builder; the defaults here match the config defaults so tests
/// can exercise the module without wiring the config layer in.
#[derive(Debug, Clone)]
pub struct RollupConfig {
    /// Polling interval. The loop wakes this often and checks whether any
    /// user's rollup for the target day is still missing.
    pub interval:              Duration,
    /// How many days back to summarize. `1` = yesterday (UTC), which is
    /// the sane default: yesterday is closed, today is still happening.
    pub day_lag_days:          u64,
    /// Max messages per user per rollup. Oldest are dropped on overflow.
    pub max_messages:          usize,
    /// Per-message character clamp before concatenation.
    pub max_chars_per_message: usize,
    /// Phase C — single-valued contradiction resolution on the knowledge
    /// graph (see `design-docs/memory-research-2026.md` §5). Off by default.
    /// When on, the rollup tick runs the consolidator pass per active user
    /// AFTER their daily summary, so the graph has a single current truth
    /// for things like job/residence/relationship before tomorrow's chats.
    pub consolidate_contradictions: bool,
    /// Phase A — entity dedup on the knowledge graph. Off by default. When
    /// on, the rollup tick runs after Phase C (contradictions first so the
    /// edge counts dedup ranks by are post-resolution).
    pub consolidate_entities: bool,
    /// Size-ratio threshold for Phase A merges (smaller / larger token counts).
    /// 0.6 default — see `ConsolidationConfig`.
    pub entity_dedup_ratio: f64,
    /// Phase D — importance scoring on the knowledge graph. Off by default.
    /// Runs LAST in the tick so it scores the post-dedup edge set.
    pub consolidate_importance: bool,
    /// Half-life (days) for Phase D decay term. 30 default.
    pub importance_half_life_days: f64,
}

impl Default for RollupConfig {
    fn default() -> Self {
        Self {
            interval:              Duration::from_secs(3600),
            day_lag_days:          1,
            max_messages:          DEFAULT_MAX_MESSAGES,
            max_chars_per_message: DEFAULT_MAX_CHARS_PER_MSG,
            consolidate_contradictions: false,
            consolidate_entities:       false,
            entity_dedup_ratio:         0.6,
            consolidate_importance:     false,
            importance_half_life_days:  30.0,
        }
    }
}

/// Handle to the spawned poller. Dropping this does not stop the task —
/// it runs for the life of the process. Call [`Self::stop`] explicitly.
pub struct MemoryRollup {
    pub handle: JoinHandle<()>,
}

impl MemoryRollup {
    /// Spawn the poller. Returns immediately.
    pub fn start(
        history:  Arc<HistoryStore>,
        memory:   Arc<MemorySystem>,
        provider: Arc<dyn ModelProvider>,
        config:   RollupConfig,
    ) -> Self {
        let handle = tokio::spawn(async move {
            tokio::time::sleep(STARTUP_WARMUP).await;
            run_forever(history, memory, provider, config).await;
        });
        Self { handle }
    }

    pub async fn stop(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }
}

async fn run_forever(
    history:  Arc<HistoryStore>,
    memory:   Arc<MemorySystem>,
    provider: Arc<dyn ModelProvider>,
    config:   RollupConfig,
) {
    info!(
        "Memory rollup started (interval={}s, day_lag={}d, max_messages={})",
        config.interval.as_secs(), config.day_lag_days, config.max_messages,
    );
    loop {
        if let Err(e) = tick(&history, &memory, &provider, &config).await {
            warn!("Memory rollup tick failed: {}", e);
        }
        tokio::time::sleep(config.interval).await;
    }
}

/// Exposed for tests and manual triggers. Performs one poll: computes the
/// target day, enumerates users with activity, and runs a rollup for each
/// that doesn't already have one.
pub async fn tick(
    history:  &Arc<HistoryStore>,
    memory:   &Arc<MemorySystem>,
    provider: &Arc<dyn ModelProvider>,
    config:   &RollupConfig,
) -> Result<usize, crate::MiraError> {
    let Some((target_date, start_ms, end_ms)) = target_day_bounds(config.day_lag_days) else {
        // Arithmetic overflow on a huge day_lag value. Bail silently.
        return Ok(0);
    };
    let date_tag = format!("rollup:{}", target_date);

    let users = history.distinct_users_with_messages_between(start_ms, end_ms)?;
    if users.is_empty() {
        debug!("Memory rollup: no users with activity on {}", target_date);
        return Ok(0);
    }

    let mut written = 0usize;
    for user_id in users {
        match memory.has_tag_for_user(&user_id, &date_tag) {
            Ok(true)  => { debug!("rollup: skip {} (already has {})", user_id, date_tag); continue; }
            Ok(false) => {}
            Err(e)    => { warn!("rollup: idempotency check failed for {}: {}", user_id, e); continue; }
        }

        match rollup_user_day(
            history, memory, provider,
            &user_id, &target_date, &date_tag,
            start_ms, end_ms, config,
        ).await {
            Ok(true)  => written += 1,
            Ok(false) => {}
            Err(e)    => warn!("rollup: user={} date={} failed: {}", user_id, target_date, e),
        }

        // Sleep-like consolidation passes (each independently gated). Order
        // matters: Phase C first so dedup's "winner = more edges" tiebreak
        // sees post-resolution counts; Phase A second on the cleaned graph.
        if config.consolidate_contradictions {
            let (groups, closed) = memory.consolidate_contradictions(&user_id);
            if groups > 0 {
                info!("consolidator: user={} resolved {} contradiction group(s), closed {} edge(s)",
                    user_id, groups, closed);
            }
        }
        if config.consolidate_entities {
            let (merged, repointed) = memory.consolidate_entities(&user_id, config.entity_dedup_ratio);
            if merged > 0 {
                info!("consolidator: user={} merged {} entity pair(s), re-pointed {} edge(s)",
                    user_id, merged, repointed);
            }
        }
        if config.consolidate_importance {
            // Run LAST — scores the post-dedup, post-contradiction edge set.
            let scored = memory.consolidate_importance(&user_id, config.importance_half_life_days);
            if scored > 0 {
                info!("consolidator: user={} scored importance for {} live edge(s) (half_life={}d)",
                    user_id, scored, config.importance_half_life_days);
            }
        }
    }

    if written > 0 {
        info!("Memory rollup: wrote {} summaries for {}", written, target_date);
    }
    Ok(written)
}

async fn rollup_user_day(
    history:   &Arc<HistoryStore>,
    memory:    &Arc<MemorySystem>,
    provider:  &Arc<dyn ModelProvider>,
    user_id:   &str,
    target_date: &NaiveDate,
    date_tag:  &str,
    start_ms:  i64,
    end_ms:    i64,
    config:    &RollupConfig,
) -> Result<bool, crate::MiraError> {
    let msgs = history.user_messages_between(
        user_id, start_ms, end_ms,
        &["user", "assistant"],
        config.max_messages as i64,
    )?;
    if msgs.is_empty() {
        return Ok(false);
    }

    let transcript = build_transcript(&msgs, config.max_chars_per_message);
    let prompt = build_rollup_prompt(target_date, &transcript);

    let response = tokio::time::timeout(
        LLM_TIMEOUT,
        provider.generate(
            &[
                ChatMessage::system(ROLLUP_SYSTEM_PROMPT),
                ChatMessage::user(prompt),
            ],
            &GenerationOptions {
                temperature: 0.2,
                max_tokens:  Some(400),
                ..Default::default()
            },
        ),
    ).await;

    let summary = match response {
        Ok(Ok(r))  => r.content.trim().to_owned(),
        Ok(Err(e)) => {
            warn!("rollup: provider call failed for {}: {}", user_id, e);
            return Ok(false);
        }
        Err(_) => {
            warn!("rollup: provider timed out after {:?} for {}", LLM_TIMEOUT, user_id);
            return Ok(false);
        }
    };

    if summary.is_empty() || summary.eq_ignore_ascii_case("none") {
        debug!("rollup: empty/none summary for {} on {} — skipping", user_id, target_date);
        return Ok(false);
    }

    let tags = vec![
        "auto".to_owned(),
        "rollup".to_owned(),
        date_tag.to_owned(),
    ];

    memory.store_scoped(
        summary,
        Category::Fact,
        tags,
        Some(MemorySource::AutoExtracted),
        Scope::User,
        Some(user_id),
        user_id,
        &[user_id.to_owned()],
        None, None, None,
    ).await?;
    Ok(true)
}

// ── Helpers ──────────────────────────────────────────────────────────────────

const ROLLUP_SYSTEM_PROMPT: &str = r#"You produce terse day-summary memories for a personal assistant.
Given one day of conversation between USER and ASSISTANT, output ONE short paragraph
(≤3 sentences) summarising what the USER was doing, thinking about, or working on
that day. Focus on things future-you would want to remember:
- Projects, tasks, decisions
- Preferences or opinions the user expressed
- Notable events or plans

Do NOT:
- Quote the assistant's answers back
- Invent details not in the transcript
- Include meta-commentary ("the user asked…")

If the day has nothing memorable, output exactly: none"#;

fn build_rollup_prompt(target_date: &NaiveDate, transcript: &str) -> String {
    format!(
        "Date: {}\n\nTranscript:\n{}\n\nSummary:",
        target_date, transcript,
    )
}

fn build_transcript(msgs: &[crate::history::Message], max_chars_per_msg: usize) -> String {
    let mut out = String::new();
    for m in msgs {
        let role = match m.role {
            crate::history::MessageRole::User      => "USER",
            crate::history::MessageRole::Assistant => "ASSISTANT",
            _                                      => continue,
        };
        let content = truncate_chars(&m.content, max_chars_per_msg);
        out.push_str(role);
        out.push_str(": ");
        out.push_str(&content);
        out.push('\n');
    }
    out
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars { return s.to_owned(); }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Epoch-ms half-open bounds for `today - lag_days` in UTC. Returns
/// `(date, start_ms, end_ms)` where `end_ms = start_ms + 86_400_000`.
/// `None` on impossible input (huge lag values, clock skew past epoch).
fn target_day_bounds(lag_days: u64) -> Option<(NaiveDate, i64, i64)> {
    let now = Utc::now();
    let today = NaiveDate::from_ymd_opt(now.year(), now.month(), now.day())?;
    let target = today.checked_sub_days(chrono::Days::new(lag_days))?;
    let start_dt = Utc.from_utc_datetime(&target.and_hms_opt(0, 0, 0)?);
    let start_ms = start_dt.timestamp_millis();
    let end_ms   = start_ms.checked_add(86_400_000)?;
    Some((target, start_ms, end_ms))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_day_bounds_is_utc_midnight_and_24h_wide() {
        let (_date, start, end) = target_day_bounds(1).unwrap();
        assert_eq!(end - start, 86_400_000);
        assert_eq!(start % 86_400_000, 0, "start should align to UTC midnight");
    }

    #[test]
    fn target_day_bounds_lag_changes_date() {
        let (d1, s1, _) = target_day_bounds(1).unwrap();
        let (d2, s2, _) = target_day_bounds(2).unwrap();
        assert!(d1 > d2);
        assert_eq!(s1 - s2, 86_400_000);
    }

    #[test]
    fn truncate_chars_is_multibyte_safe() {
        let s = "🔥".repeat(10);
        let got = truncate_chars(&s, 3);
        // 3 flame emoji + ellipsis.
        assert_eq!(got.chars().count(), 4);
        assert!(got.ends_with('…'));
        // Unchanged when under the cap.
        assert_eq!(truncate_chars("hi", 5), "hi");
    }

    #[test]
    fn build_transcript_skips_system_and_tool_roles() {
        use crate::history::{Message, MessageRole};
        let msgs = vec![
            Message {
                id: "1".into(), conversation_id: "c".into(),
                role: MessageRole::System, content: "hidden".into(),
                content_type: "text".into(), token_count: None, model: None,
                tool_calls: None, created_at: 0, metadata: None,
            },
            Message {
                id: "2".into(), conversation_id: "c".into(),
                role: MessageRole::User, content: "hello".into(),
                content_type: "text".into(), token_count: None, model: None,
                tool_calls: None, created_at: 0, metadata: None,
            },
            Message {
                id: "3".into(), conversation_id: "c".into(),
                role: MessageRole::Tool, content: "tool-output".into(),
                content_type: "text".into(), token_count: None, model: None,
                tool_calls: None, created_at: 0, metadata: None,
            },
            Message {
                id: "4".into(), conversation_id: "c".into(),
                role: MessageRole::Assistant, content: "hi there".into(),
                content_type: "text".into(), token_count: None, model: None,
                tool_calls: None, created_at: 0, metadata: None,
            },
        ];
        let t = build_transcript(&msgs, 1000);
        assert!(t.contains("USER: hello"));
        assert!(t.contains("ASSISTANT: hi there"));
        assert!(!t.contains("hidden"));
        assert!(!t.contains("tool-output"));
    }
}
