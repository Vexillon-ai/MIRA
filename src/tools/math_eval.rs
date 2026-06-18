// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/math_eval.rs
//! Bounded arithmetic expression evaluator (Tier 1 — pure).
//!
//! Accepts expressions over the operators `+ - * / %` with parentheses and
//! unary `-`/`+`. Does **not** support variables, function calls, power, or
//! bitwise operators — expanding this grammar crosses into code-execution
//! territory, which belongs in the Tier 4 sandbox.
//!
//! Hard caps (belt + braces against pathological inputs):
//! - expression length ≤ 256 chars
//! - parenthesis nesting depth ≤ 32

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tier, Tool, ToolArgs, ToolResult};
use crate::MiraError;

const MAX_EXPR_LEN:   usize = 256;
const MAX_PAREN_DEPTH: usize = 32;

pub struct MathEvalTool;

impl MathEvalTool {
    pub fn new() -> Self { Self }
}

impl Default for MathEvalTool {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl Tool for MathEvalTool {
    fn name(&self) -> &str { "math_eval" }

    fn description(&self) -> &str {
        "Evaluate a single arithmetic expression. Supports +, -, *, /, %, \
         parentheses, and unary minus/plus. Numbers may be integer or \
         decimal. No variables, no functions, no power operator — this is a \
         calculator, not a scripting language. Prefer this over doing \
         arithmetic in your head."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["expression"],
            "properties": {
                "expression": {
                    "type": "string",
                    "description":
                        "The arithmetic expression to evaluate, e.g. \
                         '(12 + 7) * 3 - 4 / 2'. Max 256 characters."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let expr = args.get("expression").and_then(|v| v.as_str())
            .ok_or_else(|| MiraError::ToolError("math_eval: `expression` is required".into()))?
            .trim();

        if expr.is_empty() {
            return Ok(ToolResult::failure("math_eval: empty expression".to_string()));
        }
        if expr.chars().count() > MAX_EXPR_LEN {
            return Ok(ToolResult::failure(
                format!("math_eval: expression exceeds {}-char cap", MAX_EXPR_LEN),
            ));
        }

        match evaluate(expr) {
            Ok(value) => {
                let body = json!({
                    "expression": expr,
                    "result":     value,
                    "result_str": format_result(value),
                });
                Ok(ToolResult::success(body.to_string()))
            }
            Err(e) => Ok(ToolResult::failure(format!("math_eval: {}", e))),
        }
    }
}

// ── Parser (recursive descent) ───────────────────────────────────────────────

struct Parser<'a> {
    src:       &'a [u8],
    pos:       usize,
    depth:     usize,
}

fn evaluate(src: &str) -> Result<f64, String> {
    let mut p = Parser { src: src.as_bytes(), pos: 0, depth: 0 };
    let v = p.expr()?;
    p.skip_ws();
    if p.pos != p.src.len() {
        return Err(format!("unexpected trailing input at position {}", p.pos));
    }
    if !v.is_finite() {
        return Err("result is not a finite number".into());
    }
    Ok(v)
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.src.len() && (self.src[self.pos] as char).is_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.src.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.pos += 1;
        Some(c)
    }

    /// expr = term (('+' | '-') term)*
    fn expr(&mut self) -> Result<f64, String> {
        let mut acc = self.term()?;
        while let Some(c) = self.peek() {
            match c {
                b'+' => { self.bump(); acc += self.term()?; }
                b'-' => { self.bump(); acc -= self.term()?; }
                _    => break,
            }
        }
        Ok(acc)
    }

    /// term = factor (('*' | '/' | '%') factor)*
    fn term(&mut self) -> Result<f64, String> {
        let mut acc = self.factor()?;
        while let Some(c) = self.peek() {
            match c {
                b'*' => { self.bump(); acc *= self.factor()?; }
                b'/' => {
                    self.bump();
                    let rhs = self.factor()?;
                    if rhs == 0.0 { return Err("division by zero".into()); }
                    acc /= rhs;
                }
                b'%' => {
                    self.bump();
                    let rhs = self.factor()?;
                    if rhs == 0.0 { return Err("modulo by zero".into()); }
                    acc %= rhs;
                }
                _    => break,
            }
        }
        Ok(acc)
    }

    /// factor = ('-' | '+')* atom
    fn factor(&mut self) -> Result<f64, String> {
        let mut sign = 1.0;
        loop {
            match self.peek() {
                Some(b'-') => { self.bump(); sign = -sign; }
                Some(b'+') => { self.bump(); }
                _          => break,
            }
        }
        Ok(sign * self.atom()?)
    }

    /// atom = number | '(' expr ')'
    fn atom(&mut self) -> Result<f64, String> {
        match self.peek() {
            Some(b'(') => {
                self.bump();
                self.depth += 1;
                if self.depth > MAX_PAREN_DEPTH {
                    return Err(format!("parenthesis nesting exceeds {}", MAX_PAREN_DEPTH));
                }
                let v = self.expr()?;
                self.depth -= 1;
                match self.bump() {
                    Some(b')') => Ok(v),
                    Some(c)    => Err(format!("expected ')', got '{}'", c as char)),
                    None       => Err("unclosed parenthesis".into()),
                }
            }
            Some(c) if c.is_ascii_digit() || c == b'.' => self.number(),
            Some(c) => Err(format!("unexpected character '{}' at position {}", c as char, self.pos)),
            None    => Err("unexpected end of expression".into()),
        }
    }

    fn number(&mut self) -> Result<f64, String> {
        self.skip_ws();
        let start = self.pos;
        let mut seen_dot = false;
        while self.pos < self.src.len() {
            let c = self.src[self.pos];
            if c.is_ascii_digit() {
                self.pos += 1;
            } else if c == b'.' && !seen_dot {
                seen_dot = true;
                self.pos += 1;
            } else {
                break;
            }
        }
        let slice = std::str::from_utf8(&self.src[start..self.pos])
            .map_err(|_| "invalid utf-8 in number".to_string())?;
        slice.parse::<f64>().map_err(|e| format!("bad number '{}': {}", slice, e))
    }
}

/// Format the result so integers render without a trailing `.0`.
fn format_result(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e16 {
        format!("{}", v as i64)
    } else {
        format!("{}", v)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(expr: &str) -> f64 { evaluate(expr).unwrap() }

    #[test]
    fn basic_arithmetic() {
        assert_eq!(eval("1 + 2"), 3.0);
        assert_eq!(eval("5 - 3"), 2.0);
        assert_eq!(eval("4 * 2.5"), 10.0);
        assert_eq!(eval("10 / 4"), 2.5);
        assert_eq!(eval("10 % 3"), 1.0);
    }

    #[test]
    fn precedence_and_parens() {
        assert_eq!(eval("2 + 3 * 4"), 14.0);
        assert_eq!(eval("(2 + 3) * 4"), 20.0);
        assert_eq!(eval("2 * (3 + 4) - 5"), 9.0);
    }

    #[test]
    fn unary_minus_and_plus() {
        assert_eq!(eval("-5"), -5.0);
        assert_eq!(eval("--5"), 5.0);
        assert_eq!(eval("+5"), 5.0);
        assert_eq!(eval("3 * -2"), -6.0);
        assert_eq!(eval("3 - -2"), 5.0);
    }

    #[test]
    fn division_by_zero() {
        assert!(evaluate("1/0").unwrap_err().contains("division by zero"));
        assert!(evaluate("1%0").unwrap_err().contains("modulo by zero"));
    }

    #[test]
    fn rejects_trailing_garbage() {
        assert!(evaluate("1 + 2 x").is_err());
        assert!(evaluate("1 +").is_err());
    }

    #[test]
    fn rejects_deep_nesting() {
        let deep = "(".repeat(40) + "1" + &")".repeat(40);
        assert!(evaluate(&deep).is_err());
    }

    #[tokio::test]
    async fn tool_happy_path() {
        let t = MathEvalTool::new();
        let out = t.execute(json!({"expression": "(12 + 7) * 3 - 4 / 2"})).await.unwrap();
        assert!(out.success);
        let v: Value = serde_json::from_str(&out.output).unwrap();
        assert_eq!(v["result"].as_f64().unwrap(), 55.0);
        assert_eq!(v["result_str"], "55");
    }

    #[tokio::test]
    async fn tool_rejects_empty_and_oversize() {
        let t = MathEvalTool::new();
        assert!(!t.execute(json!({"expression": ""})).await.unwrap().success);
        let huge = "1+".repeat(200);
        assert!(!t.execute(json!({"expression": huge})).await.unwrap().success);
    }

    #[tokio::test]
    async fn tool_rejects_non_math_garbage() {
        let t = MathEvalTool::new();
        let out = t.execute(json!({"expression": "import os; os.system('ls')"})).await.unwrap();
        assert!(!out.success);
    }

    #[test]
    fn format_result_integers_look_clean() {
        assert_eq!(format_result(3.0), "3");
        assert_eq!(format_result(-5.0), "-5");
        assert_eq!(format_result(2.5), "2.5");
    }
}
