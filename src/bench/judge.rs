// SPDX-License-Identifier: AGPL-3.0-or-later

// src/bench/judge.rs
//! LLM-as-judge scoring for LongMemEval, following the benchmark's own
//! protocol: a strong model decides whether the assistant's answer is correct
//! given the question and the gold answer. Using an LLM judge (rather than
//! string match) is what the published mem0/Zep/Letta numbers use, so this
//! keeps the comparison fair. Abstention questions (`*_abs`) invert the test:
//! a correct response *declines* to answer.

use std::sync::Arc;

use crate::providers::ModelProvider;
use crate::types::{ChatMessage, GenerationOptions};
use crate::MiraError;

/// Judge one answer. Returns `Ok(true)` when the response is correct.
pub async fn judge_answer(
    provider:      &Arc<dyn ModelProvider>,
    question:      &str,
    gold_answer:   &str,
    model_answer:  &str,
    is_abstention: bool,
) -> Result<bool, MiraError> {
    let prompt = if is_abstention {
        format!(
            "You are grading a memory assistant. The question CANNOT be answered from \
             what the user told the assistant — the correct behaviour is to say it does \
             not know / has no record, or to ask for clarification, rather than inventing \
             an answer.\n\n\
             Question: {question}\n\
             Assistant's response: {model_answer}\n\n\
             Did the assistant correctly decline / indicate it has no information \
             (instead of fabricating)? Reply with exactly `yes` or `no`."
        )
    } else {
        format!(
            "You are grading a memory assistant. Decide whether the assistant's response \
             is correct, judged against the gold answer. The response is correct if it \
             conveys the gold answer's key fact(s), even if worded differently or with \
             extra detail. It is incorrect if it contradicts, omits, or fabricates the \
             key fact.\n\n\
             Question: {question}\n\
             Gold answer: {gold_answer}\n\
             Assistant's response: {model_answer}\n\n\
             Is the assistant's response correct? Reply with exactly `yes` or `no`."
        )
    };

    let messages = [ChatMessage::user(prompt)];
    let opts = GenerationOptions { temperature: 0.0, max_tokens: Some(8), ..Default::default() };
    let resp = provider.generate(&messages, &opts).await?;
    Ok(parse_verdict(&resp.content))
}

/// Pull a yes/no verdict out of the judge's reply. Tolerant of reasoning
/// preambles / punctuation; defaults to `false` (incorrect) when ambiguous so
/// the score never over-counts.
fn parse_verdict(raw: &str) -> bool {
    let lower = raw.trim().to_ascii_lowercase();
    // Prefer the first standalone yes/no token.
    for tok in lower.split(|c: char| !c.is_ascii_alphabetic()) {
        match tok {
            "yes" | "correct" | "true" => return true,
            "no"  | "incorrect" | "false" => return false,
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::parse_verdict;

    #[test]
    fn parses_plain_and_decorated_verdicts() {
        assert!(parse_verdict("yes"));
        assert!(parse_verdict("Yes."));
        assert!(parse_verdict("**yes**"));
        assert!(!parse_verdict("no"));
        assert!(!parse_verdict("No, the answer is wrong."));
        // First token wins: a "yes" reasoning then "no" verdict is unusual;
        // we take the first definitive token.
        assert!(parse_verdict("yes — matches the gold fact"));
    }

    #[test]
    fn ambiguous_defaults_to_incorrect() {
        assert!(!parse_verdict("hmm, hard to say"));
        assert!(!parse_verdict(""));
    }
}
