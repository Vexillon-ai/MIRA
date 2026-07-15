// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/errors.rs

//! Shared provider error-body extraction.
//!
//! Every provider returns HTTP errors as JSON, but in three shapes:
//! - OpenAI-compatible: `{"error":{"message":"…","type":"…","code":"…"}}`
//!   (OpenAI, OpenRouter, DeepSeek, Groq, LM Studio, …)
//! - OpenAI-simple: `{"error":"…"}` (some self-hosted gateways, Ollama)
//! - Google: `{"error":{"code":400,"message":"…","details":[{fieldViolations}]}}`
//!
//! Dumping the raw JSON body into the log/error message buries the reason. This
//! helper pulls out the human-readable message (plus the first offending field
//! for Google), so a failure reads `anthropic: 400 — model X not found` instead
//! of a 700-byte blob. It also tolerates an SSE `data: {…}` prefix and a
//! top-level `[{…}]` array wrapper (Gemini streams errors this way).

use serde_json::Value;

/// Extract a clean, human-readable error message from a provider error body.
/// Returns `None` when `body` isn't a recognizable provider error payload (the
/// caller should then fall back to the raw body or the HTTP status).
pub fn clean_provider_error(body: &str) -> Option<String> {
    let s = body.trim();
    let s = s.strip_prefix("data:").map(str::trim).unwrap_or(s);
    let v: Value = serde_json::from_str(s).ok()?;

    // Unwrap a `[{…}]` array wrapper (Gemini streaming error frames).
    let obj = if let Some(arr) = v.as_array() {
        arr.iter().find(|e| e.get("error").is_some())?
    } else {
        &v
    };

    let err = obj.get("error")?;

    // `{"error":"some string"}`
    if let Some(msg) = err.as_str() {
        return Some(msg.trim().to_string());
    }

    // `{"error":{"message":…}}`
    let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("(no message)");

    // Google adds `details[].fieldViolations[].field` — the offending schema path.
    let field = err
        .get("details").and_then(|d| d.as_array())
        .and_then(|arr| arr.iter().find_map(|d| {
            d.get("fieldViolations")
                .and_then(|fv| fv.as_array())
                .and_then(|fv| fv.first())
                .and_then(|f| f.get("field"))
                .and_then(|f| f.as_str())
        }));

    Some(match field {
        Some(f) => format!("{msg} (field: {f})"),
        None    => msg.to_string(),
    })
}

/// Convenience: the clean message if extractable, else a trimmed fallback that
/// notes an empty body explicitly (so `— {}` / `— ` never reaches the log).
pub fn provider_error_detail(body: &str) -> String {
    if let Some(clean) = clean_provider_error(body) {
        return clean;
    }
    let t = body.trim();
    if t.is_empty() { "<empty response body>".to_string() } else { t.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_shape() {
        let b = r#"{"error":{"message":"Incorrect API key","type":"invalid_request_error","code":"invalid_api_key"}}"#;
        assert_eq!(clean_provider_error(b).unwrap(), "Incorrect API key");
    }

    #[test]
    fn openai_simple_string() {
        assert_eq!(clean_provider_error(r#"{"error":"model not found"}"#).unwrap(), "model not found");
    }

    #[test]
    fn google_shape_with_field() {
        let b = r#"{"error":{"code":400,"message":"Unknown name \"type\"","details":[{"fieldViolations":[{"field":"tools[0].x"}]}]}}"#;
        let got = clean_provider_error(b).unwrap();
        assert!(got.contains("Unknown name"));
        assert!(got.contains("field: tools[0].x"));
    }

    #[test]
    fn sse_prefix_and_array_wrapper() {
        assert_eq!(clean_provider_error("data: {\"error\":{\"message\":\"boom\"}}").unwrap(), "boom");
        assert_eq!(clean_provider_error("[{\"error\":{\"message\":\"arr\"}}]").unwrap(), "arr");
    }

    #[test]
    fn non_error_and_empty() {
        assert!(clean_provider_error(r#"{"choices":[]}"#).is_none());
        assert!(clean_provider_error("not json").is_none());
        assert_eq!(provider_error_detail(""), "<empty response body>");
        assert_eq!(provider_error_detail("plain text"), "plain text");
    }
}
