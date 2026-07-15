// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/schema_lint.rs

//! Tool-schema portability linting.
//!
//! MIRA tools declare their `parameters` in a loose JSON-Schema superset, but
//! the LLM providers accept very different subsets — and a single unsupported
//! keyword makes the provider reject the *entire* tool-enabled request (a 400
//! that takes every tool down at once). We learned this the hard way: Anthropic
//! rejects top-level `oneOf/anyOf/allOf`; Gemini rejects `additionalProperties`,
//! `type` arrays, and `required` inside combinator branches.
//!
//! This module defines the **portable tool-schema dialect** — the safe subset
//! that survives every provider adapter — and a linter that flags deviations.
//! [`ToolRegistry::register`] runs it on every tool at load time and `warn!`s so
//! a non-portable schema is visible immediately (including for MCP tools),
//! instead of surfacing as a provider 400 mid-turn. The per-provider sanitizers
//! ([`crate::providers::anthropic`], [`crate::providers::gemini`]) are the
//! runtime safety net; this linter is the authoring-time guard.
//!
//! ## Portable dialect (what tool authors should write)
//! - Root: `{"type":"object","properties":{…}}` (+ optional `required`).
//! - Property types: a single scalar `type` (`string`/`integer`/`number`/
//!   `boolean`), or `object`/`array` (with `properties` / `items`).
//! - Allowed adjuncts: `description`, `enum`, `required`, `items`, `properties`,
//!   `minimum`/`maximum`, `minLength`/`maxLength`, `format` (common values).
//! - AVOID: `oneOf`/`anyOf`/`allOf`/`not`, `additionalProperties`,
//!   `type` arrays (`["string","null"]` — use one type), `$ref`/`$defs`/
//!   `definitions`, `patternProperties`, `const`, `if`/`then`/`else`.

use serde_json::Value;

/// A single portability problem found in a tool schema.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaIssue {
    /// Dotted JSON path to the offending node (e.g. `properties.hour`).
    pub path: String,
    /// The offending keyword (e.g. `additionalProperties`, `anyOf`).
    pub keyword: String,
    /// Which providers this breaks, for the log line.
    pub breaks: &'static str,
}

impl std::fmt::Display for SchemaIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at `{}` (breaks {})", self.keyword, self.path, self.breaks)
    }
}

/// Keywords that are non-portable, with which providers reject them. `anyOf` is
/// accepted by Gemini for genuine type-unions but MIRA only uses it for
/// constraint branches Gemini then rejects, so it's flagged as non-portable.
fn breakage(keyword: &str) -> Option<&'static str> {
    Some(match keyword {
        "oneOf" | "allOf"          => "Anthropic (top-level) + Gemini",
        "anyOf"                    => "Anthropic (top-level) + Gemini (constraint branches)",
        "additionalProperties"     => "Gemini",
        "$ref" | "$defs" | "definitions" | "patternProperties" | "const"
        | "not" | "if" | "then" | "else" | "$schema" => "Gemini",
        _ => return None,
    })
}

/// Recursively lint a tool `parameters` schema, returning every portability
/// issue. Empty = fully portable across all provider adapters.
pub fn lint_tool_schema(schema: &Value) -> Vec<SchemaIssue> {
    let mut issues = Vec::new();
    walk(schema, "", &mut issues);
    issues
}

fn walk(node: &Value, path: &str, out: &mut Vec<SchemaIssue>) {
    let Some(map) = node.as_object() else { return };
    for (k, v) in map {
        if let Some(breaks) = breakage(k) {
            out.push(SchemaIssue {
                path: if path.is_empty() { k.clone() } else { format!("{path}.{k}") },
                keyword: k.clone(),
                breaks,
            });
        }
        // A `type` that is an array (JSON-Schema union) breaks Gemini.
        if k == "type" && v.is_array() {
            out.push(SchemaIssue {
                path: if path.is_empty() { "type".into() } else { format!("{path}.type") },
                keyword: "type-array".into(),
                breaks: "Gemini",
            });
        }
    }
    // Recurse into the structural children where sub-schemas live.
    if let Some(props) = map.get("properties").and_then(|p| p.as_object()) {
        for (name, sub) in props {
            let p = if path.is_empty() { format!("properties.{name}") }
                    else { format!("{path}.properties.{name}") };
            walk(sub, &p, out);
        }
    }
    if let Some(items) = map.get("items") {
        let p = if path.is_empty() { "items".into() } else { format!("{path}.items") };
        walk(items, &p, out);
    }
    for combinator in ["oneOf", "anyOf", "allOf"] {
        if let Some(arr) = map.get(combinator).and_then(|a| a.as_array()) {
            for (i, sub) in arr.iter().enumerate() {
                let p = if path.is_empty() { format!("{combinator}[{i}]") }
                        else { format!("{path}.{combinator}[{i}]") };
                walk(sub, &p, out);
            }
        }
    }
}

/// Residual keywords Anthropic still rejects *after* its sanitizer — used by the
/// provider's tests to prove the sanitizer output is clean. Anthropic only
/// rejects the combinators at the **top level**.
pub fn residual_for_anthropic(schema: &Value) -> Vec<String> {
    let Some(map) = schema.as_object() else { return vec![] };
    ["oneOf", "anyOf", "allOf"].iter()
        .filter(|k| map.contains_key(**k))
        .map(|k| (*k).to_string())
        .collect()
}

/// Residual keywords Gemini still rejects *after* its sanitizer — checked
/// recursively (Gemini rejects these anywhere in the tree).
pub fn residual_for_gemini(schema: &Value) -> Vec<String> {
    let mut bad = Vec::new();
    fn rec(node: &Value, bad: &mut Vec<String>) {
        let Some(map) = node.as_object() else { return };
        for (k, v) in map {
            match k.as_str() {
                "additionalProperties" | "oneOf" | "anyOf" | "allOf" | "not"
                | "$ref" | "$defs" | "definitions" | "patternProperties"
                | "const" | "if" | "then" | "else" | "$schema" => bad.push(k.clone()),
                "type" if v.is_array() => bad.push("type-array".into()),
                _ => {}
            }
        }
        if let Some(p) = map.get("properties").and_then(|p| p.as_object()) {
            for sub in p.values() { rec(sub, bad); }
        }
        if let Some(items) = map.get("items") { rec(items, bad); }
    }
    rec(schema, &mut bad);
    bad
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn portable_schema_has_no_issues() {
        let s = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "who" },
                "count": { "type": "integer", "minimum": 0 },
                "mode": { "type": "string", "enum": ["a", "b"] },
                "tags": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["name"]
        });
        assert!(lint_tool_schema(&s).is_empty(), "{:?}", lint_tool_schema(&s));
    }

    #[test]
    fn flags_the_real_offenders_we_hit() {
        // companion_briefing_set: top-level anyOf.
        let s = json!({
            "type": "object",
            "properties": { "enabled": { "type": "boolean" }, "hour": { "type": "integer" } },
            "anyOf": [ { "required": ["enabled"] }, { "required": ["hour"] } ]
        });
        let issues = lint_tool_schema(&s);
        assert!(issues.iter().any(|i| i.keyword == "anyOf"));

        // calendar: nested additionalProperties + type-array.
        let s2 = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "from": { "type": ["string", "integer"] } }
        });
        let issues2 = lint_tool_schema(&s2);
        assert!(issues2.iter().any(|i| i.keyword == "additionalProperties"));
        assert!(issues2.iter().any(|i| i.keyword == "type-array" && i.path == "properties.from.type"));
    }

    #[test]
    fn residual_helpers_detect_uncleaned_schemas() {
        let top_anyof = json!({ "type": "object", "anyOf": [ { "required": ["x"] } ] });
        assert_eq!(residual_for_anthropic(&top_anyof), vec!["anyOf"]);
        let nested_addl = json!({ "type": "object", "properties": { "a": { "additionalProperties": false } } });
        assert!(residual_for_gemini(&nested_addl).contains(&"additionalProperties".to_string()));
        // A clean schema yields nothing.
        let clean = json!({ "type": "object", "properties": { "a": { "type": "string" } } });
        assert!(residual_for_anthropic(&clean).is_empty());
        assert!(residual_for_gemini(&clean).is_empty());
    }
}
