// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/template.rs
//! Tiny `{{ path }}` substitution engine for action templates.
//!
//! Used by:
//! - `Action::HttpPost.body_template`
//! - `Action::ChannelMessage.text_template`
//! - `Webhook.payload_template`
//!
//! Deliberately limited to substitution. No filters, conditionals, or loops:
//! if a user wants logic they reach for the predicate evaluator. Every value
//! renders as its JSON-string form (string → unquoted; numbers/bools → their
//! lexical form; objects/arrays → compact JSON). Missing paths render as the
//! empty string so a template never blows up at fire time.
//!
//! Path syntax: dotted, with optional integer indices for arrays.
//! `payload.items.0.name`, `schedule.name`, `now`.

use serde_json::Value;

// Render `template` against `ctx`, substituting every `{{ path }}` block.
// Whitespace inside the braces is trimmed, so both `{{x}}` and `{{ x }}`
// resolve identically.
// // Unknown paths render as the empty string. This is a pragmatic choice:
// templates are authored ahead of fire time and we'd rather quietly drop a
// missing field than fail the action.
pub fn render(template: &str, ctx: &Value) -> String {
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < bytes.len() {
        // Look for the next `{{`. Two-byte window so we never split a
        // multibyte UTF-8 char — `{` and `}` are ASCII so byte indexing is
        // safe at the open marker.
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            if let Some(close_rel) = find_close(&bytes[i + 2..]) {
                let path_bytes = &bytes[i + 2..i + 2 + close_rel];
                // SAFETY: path_bytes is a slice of `template` between two
                // ASCII markers; it stays valid UTF-8.
                let path = std::str::from_utf8(path_bytes).unwrap_or("").trim();
                out.push_str(&resolve(ctx, path));
                i += 2 + close_rel + 2; // skip `{{ … }}`
                continue;
            }
        }
        // Push one full UTF-8 codepoint at a time so we don't slice
        // mid-character. The original `template` is &str, so we walk by
        // char_indices semantics: find the codepoint length here.
        let ch_len = utf8_char_len(bytes[i]);
        out.push_str(&template[i..i + ch_len]);
        i += ch_len;
    }
    out
}

// Look up `path` in `ctx` and stringify the result. Empty path resolves to
// the whole context. Missing → empty.
fn resolve(ctx: &Value, path: &str) -> String {
    if path.is_empty() {
        return value_to_string(ctx);
    }
    let mut cur = ctx;
    for seg in path.split('.') {
        cur = match cur {
            Value::Object(map) => match map.get(seg) {
                Some(v) => v,
                None    => return String::new(),
            },
            Value::Array(arr) => match seg.parse::<usize>() {
                Ok(idx) => match arr.get(idx) {
                    Some(v) => v,
                    None    => return String::new(),
                },
                Err(_) => return String::new(),
            },
            _ => return String::new(),
        };
    }
    value_to_string(cur)
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::Null         => String::new(),
        Value::String(s)    => s.clone(),
        Value::Bool(b)      => b.to_string(),
        Value::Number(n)    => n.to_string(),
        Value::Array(_) | Value::Object(_) => v.to_string(),
    }
}

// Return the byte index (relative to `slice`) of the start of the closing
// `}}` marker, or `None` if there isn't one.
fn find_close(slice: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 1 < slice.len() {
        if slice[i] == b'}' && slice[i + 1] == b'}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

// Length in bytes of the UTF-8 codepoint that *starts* with `b`.
fn utf8_char_len(b: u8) -> usize {
    if b < 0x80      { 1 }
    else if b < 0xC0 { 1 } // continuation byte — should not happen at boundary
    else if b < 0xE0 { 2 }
    else if b < 0xF0 { 3 }
    else             { 4 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn substitutes_simple_path() {
        let ctx = json!({"name": "world"});
        assert_eq!(render("hello {{ name }}", &ctx), "hello world");
    }

    #[test]
    fn substitutes_nested_path() {
        let ctx = json!({"payload": {"branch": "main"}});
        assert_eq!(render("branch={{payload.branch}}", &ctx), "branch=main");
    }

    #[test]
    fn substitutes_array_index() {
        let ctx = json!({"items": ["a", "b", "c"]});
        assert_eq!(render("first={{items.0}} third={{items.2}}", &ctx), "first=a third=c");
    }

    #[test]
    fn missing_path_is_empty() {
        let ctx = json!({"a": 1});
        assert_eq!(render("[{{missing.thing}}]", &ctx), "[]");
    }

    #[test]
    fn renders_numbers_and_bools() {
        let ctx = json!({"n": 42, "ok": true});
        assert_eq!(render("{{n}}/{{ok}}", &ctx), "42/true");
    }

    #[test]
    fn renders_object_as_json() {
        let ctx = json!({"obj": {"k": 1}});
        assert_eq!(render("{{obj}}", &ctx), r#"{"k":1}"#);
    }

    #[test]
    fn unmatched_open_passes_through() {
        let ctx = json!({});
        assert_eq!(render("oops {{ unfinished", &ctx), "oops {{ unfinished");
    }

    #[test]
    fn empty_template_renders_empty() {
        let ctx = json!({});
        assert_eq!(render("", &ctx), "");
    }

    #[test]
    fn handles_utf8_around_substitution() {
        // The byte iterator must not slice across a multibyte codepoint.
        let ctx = json!({"name": "Zoë"});
        assert_eq!(render("héllo {{name}} ✓", &ctx), "héllo Zoë ✓");
    }

    #[test]
    fn multiple_substitutions_per_template() {
        let ctx = json!({"a": 1, "b": 2, "c": 3});
        assert_eq!(render("{{a}}/{{b}}/{{c}}", &ctx), "1/2/3");
    }

    #[test]
    fn array_path_with_non_index_segment_is_empty() {
        let ctx = json!({"arr": ["x", "y"]});
        assert_eq!(render("[{{arr.notanindex}}]", &ctx), "[]");
    }

    #[test]
    fn empty_path_resolves_to_whole_context() {
        let ctx = json!({"k": 1});
        assert_eq!(render("{{}}", &ctx), r#"{"k":1}"#);
    }

    #[test]
    fn null_renders_as_empty() {
        let ctx = json!({"x": null});
        assert_eq!(render("[{{x}}]", &ctx), "[]");
    }
}
