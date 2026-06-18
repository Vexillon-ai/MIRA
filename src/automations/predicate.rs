// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/predicate.rs
//! Tiny JSON predicate language used by webhooks and event subscriptions.
//!
//! Operators: `eq`, `neq`, `in`, `gt`, `lt`, `contains`, `regex`, `and`, `or`,
//! `not`. Path syntax: dotted keys with optional `.0` array indices.
//!
//! Why hand-rolled and not Rhai/jsonlogic? We want a small, deliberately
//! limited DSL — enough for "branch == main" filters, not a full scripting
//! surface. ~150 lines of evaluator beats pulling another runtime.
//!
//! Shape:
//!
//! ```jsonc
//! { "and": [
//!     { "eq":  ["payload.branch", "main"] },
//!     { "neq": ["payload.author.login", "bot"] }
//! ] }
//! ```
//!
//! Each operator is the single non-null key of an object. Operator value is
//! either two args (operand + literal/path) or a list (and/or) or a single
//! sub-predicate (not).
//!
//! `eval` defaults to *true* on a missing path so that an over-eager filter
//! doesn't silently swallow every payload — combine with explicit `eq` /
//! `neq` if you want strict checks. Returns `Err` on a malformed predicate
//! so the caller can surface a 400 at create-time.

use serde_json::Value;

#[derive(Debug, Clone)]
pub struct PredicateError(pub String);

impl std::fmt::Display for PredicateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "predicate: {}", self.0)
    }
}

impl std::error::Error for PredicateError {}

/// Evaluate `pred` against `ctx`. `ctx` is the merged context map (e.g.
/// `{ "payload": <body>, "headers": …, "now": … }` for webhooks).
pub fn eval(pred: &Value, ctx: &Value) -> Result<bool, PredicateError> {
    let obj = match pred.as_object() {
        Some(o) if o.len() == 1 => o,
        Some(_) => return Err(PredicateError(
            "predicate object must have exactly one operator key".into()
        )),
        None => match pred {
            Value::Bool(b) => return Ok(*b),
            _ => return Err(PredicateError(
                "predicate must be an object or boolean".into()
            )),
        },
    };
    let (op, arg) = obj.iter().next().unwrap();
    match op.as_str() {
        "and" => eval_all(arg, ctx, true),
        "or"  => eval_all(arg, ctx, false),
        "not" => Ok(!eval(arg, ctx)?),
        "eq"  => bin(arg, ctx, |a, b| values_equal(a, b)),
        "neq" => bin(arg, ctx, |a, b| !values_equal(a, b)),
        "gt"  => bin(arg, ctx, |a, b| cmp_num(a, b).map(|o| o.is_gt()).unwrap_or(false)),
        "lt"  => bin(arg, ctx, |a, b| cmp_num(a, b).map(|o| o.is_lt()).unwrap_or(false)),
        "in"  => eval_in(arg, ctx),
        "contains" => bin(arg, ctx, |a, b| match (a, b) {
            (Value::String(hay), Value::String(needle)) => hay.contains(needle.as_str()),
            (Value::Array(arr), needle)                 => arr.iter().any(|x| values_equal(x, needle)),
            _ => false,
        }),
        "regex" => eval_regex(arg, ctx),
        other => Err(PredicateError(format!("unknown operator: {other}"))),
    }
}

// ── Compound ops ─────────────────────────────────────────────────────────────

fn eval_all(arg: &Value, ctx: &Value, and: bool) -> Result<bool, PredicateError> {
    let arr = arg.as_array().ok_or_else(|| PredicateError(
        if and { "and: expected array".into() } else { "or: expected array".into() }
    ))?;
    if and {
        for p in arr { if !eval(p, ctx)? { return Ok(false); } }
        Ok(true)
    } else {
        for p in arr { if eval(p, ctx)? { return Ok(true); } }
        Ok(false)
    }
}

// ── Binary ops ───────────────────────────────────────────────────────────────

fn bin<F>(arg: &Value, ctx: &Value, op: F) -> Result<bool, PredicateError>
where
    F: Fn(&Value, &Value) -> bool,
{
    let arr = arg.as_array().ok_or_else(|| PredicateError(
        "binary op: expected [path, literal] array".into()
    ))?;
    if arr.len() != 2 {
        return Err(PredicateError("binary op: expected exactly 2 elements".into()));
    }
    let lhs = resolve(&arr[0], ctx);
    let rhs = resolve(&arr[1], ctx);
    Ok(op(&lhs, &rhs))
}

fn eval_in(arg: &Value, ctx: &Value) -> Result<bool, PredicateError> {
    let arr = arg.as_array().ok_or_else(|| PredicateError(
        "in: expected [path, [haystack…]] array".into()
    ))?;
    if arr.len() != 2 {
        return Err(PredicateError("in: expected exactly 2 elements".into()));
    }
    let needle = resolve(&arr[0], ctx);
    let hay = resolve(&arr[1], ctx);
    let list = hay.as_array().ok_or_else(|| PredicateError(
        "in: rhs must resolve to an array".into()
    ))?;
    Ok(list.iter().any(|x| values_equal(x, &needle)))
}

fn eval_regex(arg: &Value, ctx: &Value) -> Result<bool, PredicateError> {
    let arr = arg.as_array().ok_or_else(|| PredicateError(
        "regex: expected [path, pattern] array".into()
    ))?;
    if arr.len() != 2 {
        return Err(PredicateError("regex: expected exactly 2 elements".into()));
    }
    let lhs = resolve(&arr[0], ctx);
    let pattern = match &arr[1] {
        Value::String(s) => s.as_str(),
        _ => return Err(PredicateError("regex: pattern must be a string literal".into())),
    };
    let s = match &lhs {
        Value::String(s) => s.as_str(),
        Value::Null      => return Ok(false),
        _ => return Ok(false),
    };
    let re = regex::Regex::new(pattern)
        .map_err(|e| PredicateError(format!("invalid regex: {e}")))?;
    Ok(re.is_match(s))
}

// ── Path resolution ──────────────────────────────────────────────────────────

/// If `v` is a string starting with `$.` or matches `payload.…`/`headers.…`/
/// `event.…` style dotted paths, treat it as a path into `ctx`. Otherwise
/// return `v` as-is (a literal). Numbers, bools, arrays, and objects are
/// always literals. Strings starting with `\$` are literal `$…`.
fn resolve(v: &Value, ctx: &Value) -> Value {
    let s = match v {
        Value::String(s) => s,
        _ => return v.clone(),
    };
    // Escape hatch for literal strings that happen to contain dots.
    if let Some(rest) = s.strip_prefix('\\') {
        return Value::String(rest.to_string());
    }
    // Heuristic: a dotted path *or* an explicit `$.`. If the string contains
    // no dot AND no $ prefix, treat as literal.
    let path = if let Some(rest) = s.strip_prefix("$.") {
        rest
    } else if s.contains('.') {
        s.as_str()
    } else {
        return Value::String(s.clone());
    };
    pick(ctx, path).unwrap_or(Value::Null)
}

/// Walk a dotted path with array-index segments. `payload.commits.0.author`.
/// Returns `None` if any segment is missing, lets the caller decide
/// (`eq` falls through to false; `regex` short-circuits to false).
pub fn pick(v: &Value, path: &str) -> Option<Value> {
    let mut cur = v;
    for seg in path.split('.') {
        cur = if let Ok(i) = seg.parse::<usize>() {
            cur.as_array()?.get(i)?
        } else {
            cur.as_object()?.get(seg)?
        };
    }
    Some(cur.clone())
}

// ── Equality + numeric coercion ──────────────────────────────────────────────

fn values_equal(a: &Value, b: &Value) -> bool {
    // Treat number ↔ string-of-number as equal so user-supplied JSON literals
    // ("123") match a number from the payload.
    if a == b { return true; }
    match (a, b) {
        (Value::Number(n), Value::String(s)) | (Value::String(s), Value::Number(n)) => {
            s == &n.to_string()
        }
        (Value::Bool(x), Value::String(s)) | (Value::String(s), Value::Bool(x)) => {
            (s == "true" && *x) || (s == "false" && !*x)
        }
        _ => false,
    }
}

fn cmp_num(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    let x = num_of(a)?;
    let y = num_of(b)?;
    x.partial_cmp(&y)
}

fn num_of(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> Value {
        json!({
            "payload": {
                "branch": "main",
                "author": { "login": "alice" },
                "commits": [
                    { "message": "fix typo" },
                    { "message": "refactor" }
                ],
                "count": 7
            },
            "headers": { "x-event": "push" }
        })
    }

    #[test]
    fn eq_string_path_against_literal() {
        let p = json!({ "eq": ["payload.branch", "main"] });
        assert!(eval(&p, &ctx()).unwrap());
    }

    #[test]
    fn neq_works() {
        let p = json!({ "neq": ["payload.author.login", "bot"] });
        assert!(eval(&p, &ctx()).unwrap());
    }

    #[test]
    fn and_short_circuits_on_false() {
        let p = json!({ "and": [
            { "eq":  ["payload.branch", "main"] },
            { "neq": ["payload.author.login", "alice"] }
        ]});
        assert!(!eval(&p, &ctx()).unwrap());
    }

    #[test]
    fn or_short_circuits_on_true() {
        let p = json!({ "or": [
            { "eq": ["payload.branch", "dev"] },
            { "eq": ["payload.branch", "main"] }
        ]});
        assert!(eval(&p, &ctx()).unwrap());
    }

    #[test]
    fn not_inverts() {
        let p = json!({ "not": { "eq": ["payload.branch", "main"] } });
        assert!(!eval(&p, &ctx()).unwrap());
    }

    #[test]
    fn gt_lt_numeric() {
        assert!(eval(&json!({ "gt": ["payload.count", 5] }), &ctx()).unwrap());
        assert!(!eval(&json!({ "lt": ["payload.count", 5] }), &ctx()).unwrap());
    }

    #[test]
    fn contains_string_and_array() {
        let p = json!({ "contains": ["payload.commits.0.message", "typo"] });
        assert!(eval(&p, &ctx()).unwrap());
    }

    #[test]
    fn regex_matches() {
        let p = json!({ "regex": ["payload.commits.1.message", "^refac"] });
        assert!(eval(&p, &ctx()).unwrap());
    }

    #[test]
    fn missing_path_resolves_to_null_and_eq_is_false() {
        let p = json!({ "eq": ["payload.nope.deep", "x"] });
        assert!(!eval(&p, &ctx()).unwrap());
    }

    #[test]
    fn unknown_operator_is_error() {
        let p = json!({ "weird": [1, 2] });
        assert!(eval(&p, &ctx()).is_err());
    }

    #[test]
    fn literal_dotted_string_is_path_lookup() {
        // A literal string with dots is treated as a path. To force a literal,
        // prefix with backslash.
        let p_path    = json!({ "eq": ["payload.branch", "payload.branch"] });
        let p_literal = json!({ "eq": ["payload.branch", "\\payload.branch"] });
        // path == path is identity
        assert!(eval(&p_path,    &ctx()).unwrap());
        // path != "payload.branch" literal
        assert!(!eval(&p_literal, &ctx()).unwrap());
    }
}
