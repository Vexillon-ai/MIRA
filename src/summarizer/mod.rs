// SPDX-License-Identifier: AGPL-3.0-or-later

// src/summarizer/mod.rs

//! Context summarization for long conversations
//! 
//! Provides:
//! - Automatic conversation summarization
//! - Rolling summary that preserves key information
//! - Token-efficient context management

use serde::{Deserialize, Serialize};
use tracing::{debug, info};

/// Summary of a conversation segment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSummary {
    /// The summary text
    pub summary: String,
    /// Number of turns summarized
    pub turn_count: usize,
    /// Estimated token count of original content
    pub original_tokens: usize,
    /// Estimated token count of summary
    pub summary_tokens: usize,
}

impl ConversationSummary {
    pub fn new(summary: String, turn_count: usize) -> Self {
        let summary_tokens = estimate_tokens(&summary);
        let original_tokens = turn_count * 50; // Rough estimate: ~50 tokens per turn
        
        Self {
            summary,
            turn_count,
            original_tokens,
            summary_tokens,
        }
    }
    
    /// Compression ratio (lower is better)
    pub fn compression_ratio(&self) -> f32 {
        if self.original_tokens == 0 {
            return 1.0;
        }
        self.summary_tokens as f32 / self.original_tokens as f32
    }
}

/// Estimate token count for a string (rough approximation)
pub fn estimate_tokens(text: &str) -> usize {
    // Rough estimate: ~4 characters per token on average
    text.len() / 4 + text.split_whitespace().count()
}

// ─────────────────────────────────────────────────────────────────────────────
// Phase-2 anchored structured summary
// ─────────────────────────────────────────────────────────────────────────────

/// A structured, anchored rolling summary of the compacted (evicted) portion of
/// a conversation. Unlike naive recursive prose summaries — which drift and
/// lose exact figures as they're re-summarized — this keeps stable sections and
/// a verbatim "hard facts" list that is *unioned* (never rewritten away) on each
/// compaction, so numbers and constraints survive.
///
/// Serialized as JSON and persisted per conversation (history DB) so it survives
/// session eviction / restart. `covered_messages` is the watermark: how many of
/// the oldest history messages are already folded in, so each compaction only
/// processes the newly-evicted tail (incremental, cheap).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StructuredSummary {
    /// Exact figures, limits, IDs, constraints — kept verbatim, unioned across
    /// compactions so they're never paraphrased away.
    #[serde(default)]
    pub hard_facts: Vec<String>,
    /// Choices the user/assistant settled on.
    #[serde(default)]
    pub decisions: Vec<String>,
    /// Unresolved questions / pending tasks.
    #[serde(default)]
    pub open_threads: Vec<String>,
    /// Stated user preferences (tone, format, constraints on behaviour).
    #[serde(default)]
    pub preferences: Vec<String>,
    /// Short prose of what happened, for continuity.
    #[serde(default)]
    pub narrative: String,
    /// Watermark: count of oldest history messages already represented here.
    #[serde(default)]
    pub covered_messages: usize,
}

impl StructuredSummary {
    pub fn is_empty(&self) -> bool {
        self.hard_facts.is_empty()
            && self.decisions.is_empty()
            && self.open_threads.is_empty()
            && self.preferences.is_empty()
            && self.narrative.trim().is_empty()
    }

    /// Render as a compact context block for injection into the prompt
    /// (between memory and the verbatim recent turns). Empty sections are
    /// omitted so the block stays lean.
    pub fn render(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut s = String::from("\n\n## Conversation summary so far\n");
        s.push_str(
            "(earlier turns, compacted — the most recent turns follow verbatim below)\n",
        );
        let section = |out: &mut String, title: &str, items: &[String]| {
            if !items.is_empty() {
                out.push_str(&format!("\n**{title}:**\n"));
                for it in items {
                    out.push_str(&format!("- {}\n", it.trim()));
                }
            }
        };
        section(&mut s, "Established facts", &self.hard_facts);
        section(&mut s, "Decisions", &self.decisions);
        section(&mut s, "Open threads", &self.open_threads);
        section(&mut s, "Preferences", &self.preferences);
        if !self.narrative.trim().is_empty() {
            s.push_str(&format!("\n**Context:** {}\n", self.narrative.trim()));
        }
        s
    }

    /// Estimated token cost of the rendered block.
    pub fn estimated_tokens(&self) -> usize {
        estimate_tokens(&self.render())
    }
}

/// Parse the summarizer model's labelled output back into sections. Tolerant:
/// unknown lines before any header land in the narrative; a model that ignores
/// the format entirely still yields a usable narrative-only summary.
fn parse_sections(text: &str) -> StructuredSummary {
    let mut out = StructuredSummary::default();
    #[derive(PartialEq)]
    enum Sec { None, Facts, Decisions, Threads, Prefs, Narrative }
    let mut cur = Sec::None;
    let mut narrative = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        let lower = line.to_ascii_lowercase();
        let header = lower.trim_start_matches('#').trim().trim_end_matches(':').trim();
        match header {
            "hard facts" | "established facts" | "facts" => { cur = Sec::Facts; continue; }
            "decisions" => { cur = Sec::Decisions; continue; }
            "open threads" | "threads" | "todo" | "todos" => { cur = Sec::Threads; continue; }
            "preferences" | "prefs" => { cur = Sec::Prefs; continue; }
            "narrative" | "context" | "summary" => { cur = Sec::Narrative; continue; }
            _ => {}
        }
        if line.is_empty() { continue; }
        let item = line.trim_start_matches(['-', '*', '•']).trim();
        if item.is_empty() { continue; }
        match cur {
            Sec::Facts      => out.hard_facts.push(item.to_string()),
            Sec::Decisions  => out.decisions.push(item.to_string()),
            Sec::Threads    => out.open_threads.push(item.to_string()),
            Sec::Prefs      => out.preferences.push(item.to_string()),
            Sec::Narrative  => { narrative.push_str(item); narrative.push(' '); }
            Sec::None       => { narrative.push_str(item); narrative.push(' '); }
        }
    }
    out.narrative = narrative.trim().to_string();
    out
}

/// Case-insensitive de-dup that preserves order and caps list length so a
/// section can't grow without bound across many compactions.
fn dedup_cap(mut items: Vec<String>, cap: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    items.retain(|s| {
        let k = s.trim().to_ascii_lowercase();
        !k.is_empty() && seen.insert(k)
    });
    if items.len() > cap {
        // Keep the most recent (tail) — newer facts supersede older phrasing.
        items = items.split_off(items.len() - cap);
    }
    items
}

/// Summarizer that uses LLM to compress conversation history
pub struct ContextSummarizer {
    /// System prompt for summarization
    system_prompt: String,
    /// Minimum turns before allowing summary
    min_turns_for_summary: usize,
}

impl ContextSummarizer {
    pub fn new() -> Self {
        let system_prompt = "You are a conversation summarizer. Your task is to create a concise summary of the following conversation. Guidelines: 1) Capture key facts, decisions, and important details. 2) Preserve names, places, and specific information. 3) Note any preferences or constraints mentioned. 4) Keep it brief but informative (aim for 20-50% compression). 5) Write in third person past tense.";

        Self {
            system_prompt: system_prompt.to_string(),
            min_turns_for_summary: 10,         // Need at least 10 turns to summarize
        }
    }
    
    /// Create summary from conversation turns
    pub async fn summarize(
        &self,
        provider: &impl crate::providers::ModelProvider,
        messages: &[crate::ChatMessage],
    ) -> Result<ConversationSummary, String> {
        if messages.len() < self.min_turns_for_summary {
            return Err(format!(
                "Not enough turns to summarize (have {}, need {})",
                messages.len(),
                self.min_turns_for_summary
            ));
        }
        
        // Build conversation text
        let conv_text: String = messages
            .iter()
            .map(|m| format!("{}: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n");
        
        info!("Summarizing {} turns (~{} tokens)", messages.len(), estimate_tokens(&conv_text));
        
        // Create summarization request
        let summary_messages = vec![
            crate::ChatMessage::system(self.system_prompt.clone()),
            crate::ChatMessage::user(format!(
                "Please summarize this conversation:\n\n{}", conv_text
            )),
        ];
        
        // Generate summary
        let options = crate::GenerationOptions {
            temperature: 0.3,  // Lower temp for more consistent summaries
            max_tokens: Some(500),
            ..Default::default()
        };
        
        match provider.generate(&summary_messages, &options).await {
            Ok(response) => {
                let summary = ConversationSummary::new(response.content, messages.len());
                debug!("Generated summary with compression ratio: {:.2}", summary.compression_ratio());
                Ok(summary)
            }
            Err(e) => Err(format!("Failed to generate summary: {}", e)),
        }
    }
    
    /// Create a rolling summary that combines previous summary with new content
    pub async fn update_summary(
        &self,
        provider: &impl crate::providers::ModelProvider,
        previous_summary: &str,
        new_messages: &[crate::ChatMessage],
    ) -> Result<ConversationSummary, String> {
        let conv_text: String = new_messages
            .iter()
            .map(|m| format!("{}: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n");
        
        let update_prompt = format!(
            "Previous conversation summary:\n{}\n\nNew conversation segment:\n{}\n\nPlease create an updated summary that combines the previous context with this new information. Focus on preserving important details while maintaining brevity.",
            previous_summary,
            conv_text
        );
        
        let messages = vec![
            crate::ChatMessage::system(self.system_prompt.clone()),
            crate::ChatMessage::user(update_prompt),
        ];
        
        let options = crate::GenerationOptions {
            temperature: 0.3,
            max_tokens: Some(500),
            ..Default::default()
        };
        
        match provider.generate(&messages, &options).await {
            Ok(response) => {
                let total_turns = new_messages.len(); // Approximate
                Ok(ConversationSummary::new(response.content, total_turns))
            }
            Err(e) => Err(format!("Failed to update summary: {}", e)),
        }
    }

    /// Phase-2 anchored rewrite: fold `new_messages` (the just-evicted turns)
    /// into `previous`, producing an updated [`StructuredSummary`]. The model
    /// rewrites the sections from (previous + new); the "hard facts" section is
    /// *unioned* with the previous one afterwards so exact numbers/constraints
    /// can never be dropped by a lossy rewrite. Runs on a cheap/local model.
    pub async fn rewrite(
        &self,
        provider: &dyn crate::providers::ModelProvider,
        previous: &StructuredSummary,
        new_messages: &[crate::ChatMessage],
        max_summary_tokens: usize,
    ) -> Result<StructuredSummary, String> {
        if new_messages.is_empty() {
            return Ok(previous.clone());
        }
        let new_text: String = new_messages
            .iter()
            .map(|m| format!("{}: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n");

        let prev_render = if previous.is_empty() {
            "(none yet)".to_string()
        } else {
            previous.render()
        };

        let sys = "You maintain a running, structured summary of the earlier part \
            of a conversation so it can be dropped from the model's context without \
            losing information. Rewrite the summary by MERGING the new turns into \
            the previous summary. Rules: preserve every exact number, limit, name, \
            ID and constraint verbatim in HARD FACTS; do not paraphrase figures; \
            keep it terse; drop nothing important; never invent facts. Output ONLY \
            these sections, each on its own lines, using '- ' bullets:\n\
            ## HARD FACTS\n## DECISIONS\n## OPEN THREADS\n## PREFERENCES\n## NARRATIVE";
        let user = format!(
            "PREVIOUS SUMMARY:\n{prev}\n\nNEW TURNS TO FOLD IN:\n{new}\n\n\
             Produce the updated summary now, staying under about {cap} tokens total.",
            prev = prev_render, new = new_text, cap = max_summary_tokens.max(256),
        );

        let messages = vec![
            crate::ChatMessage::system(sys.to_string()),
            crate::ChatMessage::user(user),
        ];
        // Cap output near the summary budget (chars≈4×tokens → tokens for the
        // model's max_tokens). Deterministic (temp 0) for stable prefixes.
        let options = crate::GenerationOptions {
            temperature: 0.0,
            max_tokens: Some(max_summary_tokens.clamp(256, 4096) as u32),
            ..Default::default()
        };

        let response = provider
            .generate(&messages, &options)
            .await
            .map_err(|e| format!("compaction summarize failed: {e}"))?;

        let mut merged = parse_sections(&response.content);
        // Anchor: union hard facts with the previous set so figures survive
        // even if the model dropped them from its rewrite.
        let mut facts = previous.hard_facts.clone();
        facts.extend(merged.hard_facts.drain(..));
        merged.hard_facts = dedup_cap(facts, 64);
        merged.decisions    = dedup_cap(merged.decisions, 32);
        merged.open_threads = dedup_cap(merged.open_threads, 32);
        // Preferences accumulate across the whole conversation → union too.
        let mut prefs = previous.preferences.clone();
        prefs.extend(merged.preferences.drain(..));
        merged.preferences = dedup_cap(prefs, 24);
        // If the model produced nothing usable, keep the previous summary
        // rather than regress to empty.
        if merged.is_empty() {
            return Ok(previous.clone());
        }
        merged.covered_messages = previous.covered_messages;
        info!(
            "Compaction rewrite: folded {} new turns; facts={} decisions={} threads={} (~{} tok)",
            new_messages.len(), merged.hard_facts.len(), merged.decisions.len(),
            merged.open_threads.len(), merged.estimated_tokens(),
        );
        Ok(merged)
    }
}

impl Default for ContextSummarizer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_estimation() {
        let text = "Hello, how are you today?";
        let tokens = estimate_tokens(text);
        assert!(tokens > 0);
        assert!(tokens < text.len()); // Should be less than character count
    }
    
    #[test]
    fn test_summary_compression_ratio() {
        let summary = ConversationSummary::new(
            "The user asked about the weather.".to_string(),
            10,
        );
        
        assert!(summary.compression_ratio() < 1.0);
        assert!(summary.turn_count == 10);
    }
    
    #[test]
    fn test_summarizer_creation() {
        let summarizer = ContextSummarizer::new();
        assert_eq!(summarizer.min_turns_for_summary, 10);
        assert!(summarizer.system_prompt.contains("summarize"));
    }

    #[test]
    fn structured_summary_parses_labelled_sections() {
        let text = "## HARD FACTS\n- budget is $5000\n- deadline 2026-08-01\n\
            ## DECISIONS\n- use Postgres\n## OPEN THREADS\n- pick a host\n\
            ## PREFERENCES\n- terse replies\n## NARRATIVE\nUser scoped the project.";
        let s = parse_sections(text);
        assert_eq!(s.hard_facts, vec!["budget is $5000", "deadline 2026-08-01"]);
        assert_eq!(s.decisions, vec!["use Postgres"]);
        assert_eq!(s.open_threads, vec!["pick a host"]);
        assert_eq!(s.preferences, vec!["terse replies"]);
        assert!(s.narrative.contains("scoped the project"));
    }

    #[test]
    fn unlabelled_output_falls_back_to_narrative() {
        let s = parse_sections("The user asked about pricing and we discussed tiers.");
        assert!(s.hard_facts.is_empty());
        assert!(s.narrative.contains("pricing"));
    }

    #[test]
    fn render_omits_empty_sections_and_is_empty_when_blank() {
        assert!(StructuredSummary::default().render().is_empty());
        let mut s = StructuredSummary::default();
        s.hard_facts = vec!["port is 8087".into()];
        let r = s.render();
        assert!(r.contains("Established facts"));
        assert!(r.contains("port is 8087"));
        assert!(!r.contains("Decisions")); // empty section omitted
    }

    #[test]
    fn dedup_cap_dedupes_case_insensitively_and_caps() {
        let items = vec!["A".into(), "a".into(), "B".into(), "c".into()];
        assert_eq!(dedup_cap(items, 10), vec!["A", "B", "c"]);
        let many: Vec<String> = (0..10).map(|i| i.to_string()).collect();
        let capped = dedup_cap(many, 3);
        assert_eq!(capped, vec!["7", "8", "9"]); // keeps most-recent tail
    }
}