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
}