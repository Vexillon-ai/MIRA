// SPDX-License-Identifier: AGPL-3.0-or-later

// src/bench/longmemeval.rs
//! LongMemEval dataset loader.
//!
//! LongMemEval (Wu et al., 2024 — <https://github.com/xiaowu0162/long-mem-eval>)
//! is the headline long-term-memory benchmark for chat assistants; the
//! published mem0 / Zep / Letta numbers MIRA wants to compare against are
//! reported on it. The dataset is **not redistributable**, so the harness
//! takes a path to a file the operator downloads (`longmemeval_s.json`,
//! `longmemeval_m.json`, or `longmemeval_oracle.json` from the HF release).
//!
//! Schema (a JSON array of question objects):
//! ```json
//! {
//!   "question_id": "...",            // "_abs" suffix ⇒ abstention question
//!   "question_type": "single-session-user" | "single-session-assistant" |
//!                    "single-session-preference" | "multi-session" |
//!                    "temporal-reasoning" | "knowledge-update",
//!   "question": "...",
//!   "answer": "...",
//!   "question_date": "2023/05/20 (Sat) 02:21",
//!   "haystack_dates": ["..."],
//!   "haystack_session_ids": ["..."],
//!   "haystack_sessions": [ [ {"role","content","has_answer"?}, ... ], ... ],
//!   "answer_session_ids": ["..."]
//! }
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::MiraError;

/// One turn in a haystack session.
#[derive(Debug, Clone, Deserialize)]
pub struct Turn {
    pub role:    String,
    pub content: String,
    /// LongMemEval marks the evidence turns that actually contain the answer.
    /// Used for retrieval recall@k scoring.
    #[serde(default)]
    pub has_answer: bool,
}

/// A session is an ordered list of turns.
pub type Session = Vec<Turn>;

/// One benchmark question + its haystack of prior sessions.
#[derive(Debug, Clone, Deserialize)]
pub struct Question {
    pub question_id:   String,
    pub question_type: String,
    pub question:      String,
    /// Gold answer. Usually a string, but some questions answer with a number
    /// ("how many…" → 3) or bool, so accept any JSON value and render to text.
    #[serde(default)]
    pub answer:        serde_json::Value,
    #[serde(default)]
    pub question_date: String,
    #[serde(default)]
    pub haystack_dates: Vec<String>,
    #[serde(default)]
    pub haystack_session_ids: Vec<String>,
    #[serde(default)]
    pub haystack_sessions: Vec<Session>,
    #[serde(default)]
    pub answer_session_ids: Vec<String>,
}

impl Question {
    /// LongMemEval abstention questions (`*_abs`) have no answer in the
    /// haystack — a correct response *declines* to answer. Scored separately.
    pub fn is_abstention(&self) -> bool {
        self.question_id.ends_with("_abs")
    }

    /// Gold answer rendered as text (numbers/bools become their literal form).
    pub fn answer_text(&self) -> String {
        match &self.answer {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null      => String::new(),
            other                        => other.to_string(),
        }
    }

    /// Total turns across all haystack sessions (replay cost proxy).
    pub fn turn_count(&self) -> usize {
        self.haystack_sessions.iter().map(|s| s.len()).sum()
    }

    /// Indices of sessions that contain an answer-bearing turn, by matching
    /// `answer_session_ids` against `haystack_session_ids` (positional). Falls
    /// back to the per-turn `has_answer` flag when ids are absent.
    pub fn answer_session_indices(&self) -> Vec<usize> {
        if !self.answer_session_ids.is_empty() && !self.haystack_session_ids.is_empty() {
            return self.haystack_session_ids.iter().enumerate()
                .filter(|(_, id)| self.answer_session_ids.contains(id))
                .map(|(i, _)| i)
                .collect();
        }
        self.haystack_sessions.iter().enumerate()
            .filter(|(_, s)| s.iter().any(|t| t.has_answer))
            .map(|(i, _)| i)
            .collect()
    }
}

/// Parse a LongMemEval dataset file into questions.
pub fn load_dataset(path: &Path) -> Result<Vec<Question>, MiraError> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        MiraError::ConfigError(format!("reading dataset {}: {e}", path.display()))
    })?;
    let questions: Vec<Question> = serde_json::from_str(&raw).map_err(|e| {
        MiraError::ConfigError(format!(
            "parsing LongMemEval dataset {} (expected a JSON array of question objects): {e}",
            path.display()
        ))
    })?;
    Ok(questions)
}

/// Count questions per `question_type` (for the dataset summary).
pub fn type_histogram(questions: &[Question]) -> BTreeMap<String, usize> {
    let mut h = BTreeMap::new();
    for q in questions {
        *h.entry(q.question_type.clone()).or_insert(0) += 1;
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"[
      {
        "question_id": "q1",
        "question_type": "single-session-user",
        "question": "What city did I say I'm moving to?",
        "answer": "Lisbon",
        "question_date": "2023/05/20 (Sat) 02:21",
        "haystack_session_ids": ["s0", "s1"],
        "haystack_sessions": [
          [ {"role":"user","content":"hi"}, {"role":"assistant","content":"hello"} ],
          [ {"role":"user","content":"I'm moving to Lisbon next month","has_answer":true},
            {"role":"assistant","content":"Exciting!"} ]
        ],
        "answer_session_ids": ["s1"]
      },
      {
        "question_id": "q2_abs",
        "question_type": "knowledge-update",
        "question": "What's my dog's name?",
        "answer": "No information available.",
        "haystack_sessions": [ [ {"role":"user","content":"nothing relevant"} ] ]
      }
    ]"#;

    fn load_fixture() -> Vec<Question> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ds.json");
        std::fs::write(&p, FIXTURE).unwrap();
        load_dataset(&p).unwrap()
    }

    #[test]
    fn parses_questions_and_turns() {
        let qs = load_fixture();
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].question_type, "single-session-user");
        assert_eq!(qs[0].turn_count(), 4);
        assert!(!qs[0].is_abstention());
        assert!(qs[1].is_abstention());
    }

    #[test]
    fn resolves_answer_session_by_id() {
        let qs = load_fixture();
        // s1 is index 1 in haystack_session_ids.
        assert_eq!(qs[0].answer_session_indices(), vec![1]);
    }

    #[test]
    fn answer_session_falls_back_to_has_answer_flag() {
        // No session ids → use the per-turn has_answer flag.
        let q = Question {
            question_id: "x".into(), question_type: "multi-session".into(),
            question: "?".into(), answer: "a".into(), question_date: String::new(),
            haystack_dates: vec![], haystack_session_ids: vec![],
            haystack_sessions: vec![
                vec![Turn { role: "user".into(), content: "no".into(), has_answer: false }],
                vec![Turn { role: "user".into(), content: "yes".into(), has_answer: true }],
            ],
            answer_session_ids: vec![],
        };
        assert_eq!(q.answer_session_indices(), vec![1]);
    }

    #[test]
    fn numeric_answers_render_as_text() {
        // Some LongMemEval answers are numbers ("how many…" → 3), not strings.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("n.json");
        std::fs::write(&p, r#"[{"question_id":"q","question_type":"multi-session",
            "question":"how many?","answer":3,"haystack_sessions":[]}]"#).unwrap();
        let qs = load_dataset(&p).unwrap();
        assert_eq!(qs[0].answer_text(), "3");
    }

    #[test]
    fn type_histogram_counts() {
        let h = type_histogram(&load_fixture());
        assert_eq!(h.get("single-session-user"), Some(&1));
        assert_eq!(h.get("knowledge-update"), Some(&1));
    }
}
