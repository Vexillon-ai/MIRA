// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/recall.rs
//! Model-callable transcript recall.
//!
//! Exposes [`RecallHistoryTool`] so the model can semantic-search the user's
//! past conversations when they ask for something it can't find in
//! short-term memory. Returns a compact JSON block of hits (conversation
//! id, role, date, preview, score) — the model formats this for the user.
//!
//! Scoping is strict: the chat handler injects `_user_id` into the tool
//! args, and the tool refuses to run without it. Cross-user recall is
//! impossible by construction.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use tracing::debug;

use super::{Tool, ToolArgs, ToolResult, ToolVisibility};
use crate::history::HistoryStore;
use crate::memory::MemorySystem;
use crate::MiraError;

/// Default / cap for `top_k`. Larger values waste context — the model can
/// re-query with a narrower date window if it needs more breadth.
const DEFAULT_TOP_K: usize = 5;
const MAX_TOP_K:     usize = 20;

/// Max characters from each hit's `content` returned to the model. Full
/// content is still fetchable via the regular history API; the tool hands
/// back previews so the model can decide whether to quote directly.
const CONTENT_PREVIEW_CHARS: usize = 400;

pub struct RecallHistoryTool {
    history: Arc<HistoryStore>,
    memory:  Arc<MemorySystem>,
}

impl RecallHistoryTool {
    pub fn new(history: Arc<HistoryStore>, memory: Arc<MemorySystem>) -> Self {
        Self { history, memory }
    }
}

#[async_trait]
impl Tool for RecallHistoryTool {
    fn name(&self) -> &str { "recall_history" }

    fn description(&self) -> &str {
        "Search your long-term conversation history by meaning. Returns past \
         messages (user and assistant) that semantically match the query, \
         optionally restricted to a date range. Use this when the user asks \
         about something they mentioned before but isn't in the current \
         conversation's visible context — e.g. \"what was that book I \
         recommended last month?\". Results come back as a JSON array of \
         hits; relay the meaningful ones back to the user in natural \
         language. Does not work for the very latest messages if the \
         transcript indexer hasn't caught up yet."
    }

    fn visibility(&self) -> ToolVisibility { ToolVisibility::User }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {
                    "type": "string",
                    "description":
                        "Natural-language description of what to look for. \
                         Shorter, content-bearing phrases ('fish soup recipe') \
                         outperform full questions."
                },
                "top_k": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_TOP_K as i64,
                    "description":
                        "Max hits to return (default 5, hard-capped at 20)."
                },
                "since": {
                    "type": "string",
                    "description":
                        "Earliest message date to consider. Accepts ISO 8601 \
                         ('2024-01-15' or '2024-01-15T09:00:00Z') or an \
                         epoch-millisecond integer as a string. Omit for open-ended."
                },
                "until": {
                    "type": "string",
                    "description":
                        "Latest message date to consider. Same formats as 'since'."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = args.get("_user_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MiraError::ToolError(
                "recall_history called without _user_id (chat handler must inject)".to_string()
            ))?
            .to_owned();

        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("").trim();
        if query.is_empty() {
            return Ok(ToolResult::failure("recall_history: `query` is required"));
        }

        let top_k = args.get("top_k")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_TOP_K)
            .clamp(1, MAX_TOP_K);

        let since_ms = match parse_date_arg(args.get("since")) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::failure(format!("recall_history: bad `since`: {}", e))),
        };
        let until_ms = match parse_date_arg(args.get("until")) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::failure(format!("recall_history: bad `until`: {}", e))),
        };
        if let (Some(s), Some(u)) = (since_ms, until_ms) {
            if s > u {
                return Ok(ToolResult::failure(
                    "recall_history: `since` must be earlier than `until`"
                ));
            }
        }

        // Embed the query once through the memory system's loaded provider.
        let Some(qvec) = self.memory.embed(query).await else {
            return Ok(ToolResult::failure(
                "recall_history: semantic embedding is disabled in this deployment"
            ));
        };

        debug!(
            "recall_history: user={} top_k={} since={:?} until={:?}",
            user_id, top_k, since_ms, until_ms,
        );

        let hits = self.history.search_message_vectors(
            &qvec, &user_id, top_k, since_ms, until_ms,
        )?;

        if hits.is_empty() {
            return Ok(ToolResult::success(
                json!({ "hits": [] }).to_string(),
            ));
        }

        // Hydrate full message content for previews.
        let ids: Vec<String> = hits.iter().map(|h| h.message_id.clone()).collect();
        let messages = self.history.get_messages_by_ids(&ids)?;
        let msg_by_id: std::collections::HashMap<String, &crate::history::Message> =
            messages.iter().map(|m| (m.id.clone(), m)).collect();

        let formatted: Vec<RecallHit> = hits.iter().filter_map(|h| {
            let msg = msg_by_id.get(&h.message_id)?;
            let preview = truncate_preview(&msg.content, CONTENT_PREVIEW_CHARS);
            Some(RecallHit {
                message_id:      h.message_id.clone(),
                conversation_id: h.conversation_id.clone(),
                role:            h.role.clone(),
                created_at_ms:   h.created_at,
                created_at_iso:  iso8601(h.created_at),
                score:           (h.score * 1000.0).round() / 1000.0, // 3 d.p.
                content_preview: preview,
            })
        }).collect();

        let body = json!({ "hits": formatted }).to_string();
        Ok(ToolResult::success(body))
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct RecallHit {
    message_id:      String,
    conversation_id: String,
    role:            String,
    created_at_ms:   i64,
    created_at_iso:  String,
    score:           f32,
    content_preview: String,
}

/// Parse a date arg into epoch milliseconds. Accepts:
/// - `null` / missing → `None`
/// - a stringified integer (epoch ms)
/// - a plain integer (epoch ms)
/// - an ISO 8601 date (`YYYY-MM-DD` — interpreted as start-of-day UTC)
/// - an ISO 8601 datetime (any RFC 3339 value `chrono::DateTime` accepts)
fn parse_date_arg(v: Option<&Value>) -> Result<Option<i64>, String> {
    let Some(v) = v else { return Ok(None); };
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => n.as_i64()
            .map(Some)
            .ok_or_else(|| format!("numeric date does not fit i64: {}", n)),
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() { return Ok(None); }

            // Pure integer string first (epoch ms).
            if let Ok(n) = s.parse::<i64>() {
                return Ok(Some(n));
            }

            // RFC 3339 / ISO 8601 with time + offset.
            if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                return Ok(Some(dt.with_timezone(&Utc).timestamp_millis()));
            }

            // Date-only → start of day UTC.
            if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                let ts = d.and_hms_opt(0, 0, 0)
                    .and_then(|ndt| Utc.from_local_datetime(&ndt).single())
                    .ok_or_else(|| format!("cannot construct UTC midnight for '{}'", s))?;
                return Ok(Some(ts.timestamp_millis()));
            }

            Err(format!("unrecognised date format: '{}'", s))
        }
        other => Err(format!("expected string or integer, got: {}", other)),
    }
}

fn iso8601(epoch_ms: i64) -> String {
    match Utc.timestamp_millis_opt(epoch_ms).single() {
        Some(dt) => dt.to_rfc3339(),
        None     => epoch_ms.to_string(),
    }
}

fn truncate_preview(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    // Char-safe truncation.
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_date_arg_handles_formats() {
        assert_eq!(parse_date_arg(None).unwrap(), None);
        assert_eq!(parse_date_arg(Some(&json!(null))).unwrap(), None);
        assert_eq!(parse_date_arg(Some(&json!(""))).unwrap(), None);

        // Integer epoch ms.
        assert_eq!(parse_date_arg(Some(&json!(1700000000000i64))).unwrap(), Some(1700000000000));
        assert_eq!(parse_date_arg(Some(&json!("1700000000000"))).unwrap(), Some(1700000000000));

        // ISO date — start of day UTC.
        // 2024-01-15 00:00:00 UTC = 1705276800000 ms.
        let got = parse_date_arg(Some(&json!("2024-01-15"))).unwrap().unwrap();
        assert_eq!(got, 1705276800000);

        // RFC 3339 datetime.
        let rfc = parse_date_arg(Some(&json!("2024-01-15T12:00:00Z"))).unwrap().unwrap();
        assert_eq!(rfc, 1705276800000 + 12 * 3600 * 1000);

        // Garbage is a hard error (not silently dropped).
        assert!(parse_date_arg(Some(&json!("yesterday"))).is_err());
    }

    #[test]
    fn truncate_preview_char_safe_for_multibyte() {
        let emoji_heavy = "🔥".repeat(5);
        let got = truncate_preview(&emoji_heavy, 3);
        // 3 emoji + ellipsis — byte length is not what matters; char-length is.
        assert_eq!(got.chars().count(), 4); // 3 emoji + ellipsis
        assert!(got.ends_with('…'));

        let short = "hi";
        assert_eq!(truncate_preview(short, 10), "hi"); // unchanged
    }

    #[test]
    fn iso8601_roundtrip() {
        let s = iso8601(1705276800000);
        assert!(s.starts_with("2024-01-15"));
    }
}
