// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/datetime.rs
//! Date / time tools (Tier 1 — pure).
//!
//! - [`NowTool`]: current instant, optionally expressed in a target timezone.
//! - [`DateMathTool`]: add or subtract an amount of time from a base instant.
//!
//! Both are deterministic except for `NowTool`'s reliance on wall-clock. They
//! never touch the network or filesystem — the only external read is
//! `LocalAuthService::get_profile` (SQLite-local) to resolve the caller's
//! preferred timezone when no explicit override is given.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Datelike, Duration, FixedOffset, Months, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::{json, Value};

use super::{Tier, Tool, ToolArgs, ToolResult};
use crate::auth::LocalAuthService;
use crate::MiraError;

// ── NowTool ──────────────────────────────────────────────────────────────────

/// Returns the current UTC instant, and — if the caller has a stored timezone
/// preference or passes one explicitly — the local-time equivalent.
pub struct NowTool {
    auth: Option<Arc<LocalAuthService>>,
}

impl NowTool {
    pub fn new(auth: Option<Arc<LocalAuthService>>) -> Self {
        Self { auth }
    }
}

#[async_trait]
impl Tool for NowTool {
    fn name(&self) -> &str { "now" }

    fn description(&self) -> &str {
        "Return the current date and time. Always returns UTC; if a timezone \
         is given (IANA name like 'Europe/London', a UTC offset like '+10:00', \
         or the literal 'UTC'), also returns the local-time equivalent. When \
         no argument is passed the tool falls back to the caller's stored \
         timezone preference if one exists. Use this whenever the user says \
         'today', 'now', 'this week', etc. — do not guess the date."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "timezone": {
                    "type": "string",
                    "description":
                        "IANA timezone name ('Europe/London'), a UTC offset \
                         ('+10:00'), or 'UTC'. Optional — falls back to the \
                         caller's profile timezone, then UTC."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let explicit_tz = args.get("timezone").and_then(|v| v.as_str())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());

        // Fall back to the caller's profile timezone if one is stored.
        let tz_arg: Option<String> = explicit_tz.or_else(|| {
            let user_id = args.get("_user_id").and_then(|v| v.as_str())?;
            let auth    = self.auth.as_ref()?;
            auth.get_profile(user_id).ok().flatten().and_then(|p| p.timezone)
        });

        let now_utc: DateTime<Utc> = Utc::now();

        let body = match tz_arg.as_deref() {
            None => json!({
                "utc":      now_utc.to_rfc3339(),
                "utc_ms":   now_utc.timestamp_millis(),
                "timezone": "UTC",
                "local":    now_utc.to_rfc3339(),
            }),
            Some(tz_str) => match resolve_timezone(tz_str) {
                Ok(resolved) => {
                    let local_rfc = resolved.format(now_utc);
                    json!({
                        "utc":      now_utc.to_rfc3339(),
                        "utc_ms":   now_utc.timestamp_millis(),
                        "timezone": resolved.label(),
                        "local":    local_rfc,
                    })
                }
                Err(e) => return Ok(ToolResult::failure(
                    format!("now: invalid timezone '{}': {}", tz_str, e),
                )),
            },
        };

        Ok(ToolResult::success(body.to_string()))
    }
}

// ── DateMathTool ─────────────────────────────────────────────────────────────

/// Add or subtract an amount of time from a base instant. Linear units
/// (seconds..weeks) use fixed-duration arithmetic; calendar units (months,
/// years) clamp to the end-of-month to avoid non-existent dates like
/// `2025-03-31 + 1 month = 2025-04-31`.
pub struct DateMathTool;

impl DateMathTool {
    pub fn new() -> Self { Self }
}

impl Default for DateMathTool {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl Tool for DateMathTool {
    fn name(&self) -> &str { "date_math" }

    fn description(&self) -> &str {
        "Add or subtract time from a base instant. Supports seconds, minutes, \
         hours, days, weeks, months, years. Month/year arithmetic is \
         calendar-aware and clamps to the end of the month. Use this for \
         questions like 'what day was 90 days ago', 'when is 6 months from \
         now', etc. — never guess."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["operation", "amount", "unit"],
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["add", "subtract"],
                    "description": "Whether to add or subtract the amount."
                },
                "amount": {
                    "type": "integer",
                    "description": "Non-negative integer amount of `unit` to apply."
                },
                "unit": {
                    "type": "string",
                    "enum": ["seconds", "minutes", "hours", "days", "weeks", "months", "years"],
                    "description": "Unit the `amount` is measured in."
                },
                "from": {
                    "type": "string",
                    "description":
                        "Base instant. ISO 8601 ('2026-04-24' or \
                         '2026-04-24T10:00:00Z'), or epoch ms as an integer \
                         or stringified integer. Omit for 'now in UTC'."
                },
                "timezone": {
                    "type": "string",
                    "description":
                        "IANA name or offset. Only affects the `result_local` \
                         field in the output — the computation itself is \
                         performed in UTC."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let op = match args.get("operation").and_then(|v| v.as_str()) {
            Some("add")      => 1i64,
            Some("subtract") => -1i64,
            Some(other)      => return Ok(ToolResult::failure(
                format!("date_math: operation must be 'add' or 'subtract', got '{}'", other)
            )),
            None             => return Ok(ToolResult::failure(
                "date_math: `operation` is required".to_string()
            )),
        };

        let amount_raw = args.get("amount").and_then(|v| v.as_i64())
            .ok_or_else(|| MiraError::ToolError("date_math: `amount` is required and must be an integer".into()))?;
        if amount_raw < 0 {
            return Ok(ToolResult::failure(
                "date_math: `amount` must be non-negative — use `operation` to negate".to_string(),
            ));
        }

        let unit = args.get("unit").and_then(|v| v.as_str()).unwrap_or("");

        let from_ts = match parse_instant(args.get("from")) {
            Ok(Some(t)) => t,
            Ok(None)    => Utc::now(),
            Err(e)      => return Ok(ToolResult::failure(
                format!("date_math: invalid `from`: {}", e)
            )),
        };

        let signed = op * amount_raw;
        let result = match unit {
            "seconds" => from_ts.checked_add_signed(Duration::seconds(signed)),
            "minutes" => from_ts.checked_add_signed(Duration::minutes(signed)),
            "hours"   => from_ts.checked_add_signed(Duration::hours(signed)),
            "days"    => from_ts.checked_add_signed(Duration::days(signed)),
            "weeks"   => from_ts.checked_add_signed(Duration::weeks(signed)),
            "months"  => apply_calendar_months(from_ts, signed),
            "years"   => apply_calendar_months(from_ts, signed.saturating_mul(12)),
            other     => return Ok(ToolResult::failure(
                format!("date_math: unsupported unit '{}'", other)
            )),
        };

        let Some(result) = result else {
            return Ok(ToolResult::failure(
                "date_math: result out of range (overflow)".to_string(),
            ));
        };

        let tz_arg = args.get("timezone").and_then(|v| v.as_str())
            .map(str::trim).filter(|s| !s.is_empty());

        let (tz_label, local_str) = match tz_arg {
            None => ("UTC".to_string(), result.to_rfc3339()),
            Some(s) => match resolve_timezone(s) {
                Ok(r)  => (r.label(), r.format(result)),
                Err(e) => return Ok(ToolResult::failure(
                    format!("date_math: invalid timezone '{}': {}", s, e),
                )),
            },
        };

        let body = json!({
            "result_utc":    result.to_rfc3339(),
            "result_utc_ms": result.timestamp_millis(),
            "timezone":      tz_label,
            "result_local":  local_str,
        });
        Ok(ToolResult::success(body.to_string()))
    }
}

// ── Timezone resolution (shared) ─────────────────────────────────────────────

enum ResolvedTz {
    Named(Tz),
    Fixed(FixedOffset),
    Utc,
}

impl ResolvedTz {
    fn label(&self) -> String {
        match self {
            ResolvedTz::Named(t) => t.name().to_owned(),
            ResolvedTz::Fixed(f) => f.to_string(),
            ResolvedTz::Utc      => "UTC".to_owned(),
        }
    }
    fn format(&self, instant: DateTime<Utc>) -> String {
        match self {
            ResolvedTz::Named(t) => instant.with_timezone(t).to_rfc3339(),
            ResolvedTz::Fixed(f) => instant.with_timezone(f).to_rfc3339(),
            ResolvedTz::Utc      => instant.to_rfc3339(),
        }
    }
}

fn resolve_timezone(s: &str) -> Result<ResolvedTz, String> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("utc") || s == "Z" {
        return Ok(ResolvedTz::Utc);
    }
    if let Ok(tz) = s.parse::<Tz>() {
        return Ok(ResolvedTz::Named(tz));
    }
    if let Some(off) = parse_offset(s) {
        return Ok(ResolvedTz::Fixed(off));
    }
    Err(format!("not an IANA name or '+HH:MM' offset: {}", s))
}

fn parse_offset(s: &str) -> Option<FixedOffset> {
    // Accept "+HH:MM", "-HH:MM", "+HHMM", "-HHMM", "+HH".
    let (sign, rest) = match s.chars().next()? {
        '+' => (1, &s[1..]),
        '-' => (-1, &s[1..]),
        _   => return None,
    };
    let (hours, minutes) = if let Some((h, m)) = rest.split_once(':') {
        (h.parse::<i32>().ok()?, m.parse::<i32>().ok()?)
    } else if rest.len() == 4 {
        (rest[..2].parse::<i32>().ok()?, rest[2..].parse::<i32>().ok()?)
    } else {
        (rest.parse::<i32>().ok()?, 0)
    };
    if !(0..=14).contains(&hours) || !(0..=59).contains(&minutes) {
        return None;
    }
    let secs = sign * (hours * 3600 + minutes * 60);
    FixedOffset::east_opt(secs)
}

// ── Instant parsing + calendar math ──────────────────────────────────────────

fn parse_instant(v: Option<&Value>) -> Result<Option<DateTime<Utc>>, String> {
    let Some(v) = v else { return Ok(None); };
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => n.as_i64()
            .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
            .map(Some)
            .ok_or_else(|| format!("numeric instant out of range: {}", n)),
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() { return Ok(None); }
            if let Ok(ms) = s.parse::<i64>() {
                return Utc.timestamp_millis_opt(ms).single()
                    .map(Some)
                    .ok_or_else(|| format!("epoch ms out of range: {}", ms));
            }
            if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                return Ok(Some(dt.with_timezone(&Utc)));
            }
            if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                return Ok(d.and_hms_opt(0, 0, 0)
                    .and_then(|ndt| Utc.from_local_datetime(&ndt).single())
                    .map(Some)
                    .ok_or_else(|| format!("cannot build UTC midnight for '{}'", s))?);
            }
            Err(format!("unrecognised date format: '{}'", s))
        }
        other => Err(format!("expected string or integer, got: {}", other)),
    }
}

/// Calendar-aware month add/subtract. Uses chrono's `Months`, which clamps
/// day-of-month to the last valid day of the target month (e.g. Jan 31 + 1mo
/// → Feb 28 or Feb 29 depending on year).
fn apply_calendar_months(from: DateTime<Utc>, signed_months: i64) -> Option<DateTime<Utc>> {
    let (magnitude, forward) = if signed_months >= 0 {
        (signed_months as u64, true)
    } else {
        (signed_months.unsigned_abs(), false)
    };
    let months_u32 = u32::try_from(magnitude).ok()?;
    let months = Months::new(months_u32);
    if forward {
        from.checked_add_months(months)
    } else {
        from.checked_sub_months(months)
    }
    .filter(|r| r.year() >= 0)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn now_utc_default() {
        let tool = NowTool::new(None);
        let out = tool.execute(json!({})).await.unwrap();
        assert!(out.success);
        let v: Value = serde_json::from_str(&out.output).unwrap();
        assert_eq!(v["timezone"], "UTC");
        assert!(v["utc"].as_str().unwrap().ends_with("+00:00"));
        assert_eq!(v["utc"], v["local"]);
    }

    #[tokio::test]
    async fn now_iana_tz() {
        let tool = NowTool::new(None);
        let out = tool.execute(json!({"timezone": "Europe/London"})).await.unwrap();
        assert!(out.success);
        let v: Value = serde_json::from_str(&out.output).unwrap();
        assert_eq!(v["timezone"], "Europe/London");
        assert!(v["local"].as_str().unwrap().contains("T"));
    }

    #[tokio::test]
    async fn now_fixed_offset() {
        let tool = NowTool::new(None);
        let out = tool.execute(json!({"timezone": "+10:30"})).await.unwrap();
        assert!(out.success);
        let v: Value = serde_json::from_str(&out.output).unwrap();
        assert!(v["local"].as_str().unwrap().contains("+10:30"));
    }

    #[tokio::test]
    async fn now_bad_tz_fails_soft() {
        let tool = NowTool::new(None);
        let out = tool.execute(json!({"timezone": "Mars/Olympus"})).await.unwrap();
        assert!(!out.success);
        assert!(out.error.unwrap().contains("Mars/Olympus"));
    }

    #[tokio::test]
    async fn date_math_days_linear() {
        let tool = DateMathTool::new();
        let out = tool.execute(json!({
            "operation": "subtract",
            "amount": 90,
            "unit": "days",
            "from": "2026-04-24T00:00:00Z",
        })).await.unwrap();
        assert!(out.success);
        let v: Value = serde_json::from_str(&out.output).unwrap();
        // 2026-04-24 minus 90 days = 2026-01-24
        assert!(v["result_utc"].as_str().unwrap().starts_with("2026-01-24"));
    }

    #[tokio::test]
    async fn date_math_months_calendar_clamps_to_eom() {
        // Jan 31 + 1 month → Feb 28 (non-leap 2025).
        let tool = DateMathTool::new();
        let out = tool.execute(json!({
            "operation": "add", "amount": 1, "unit": "months",
            "from": "2025-01-31T00:00:00Z",
        })).await.unwrap();
        assert!(out.success);
        let v: Value = serde_json::from_str(&out.output).unwrap();
        assert!(v["result_utc"].as_str().unwrap().starts_with("2025-02-28"),
            "got {}", v["result_utc"]);
    }

    #[tokio::test]
    async fn date_math_years_uses_months() {
        let tool = DateMathTool::new();
        // 2024-02-29 (leap) + 1 year → 2025-02-28 (clamped).
        let out = tool.execute(json!({
            "operation": "add", "amount": 1, "unit": "years",
            "from": "2024-02-29T00:00:00Z",
        })).await.unwrap();
        assert!(out.success);
        let v: Value = serde_json::from_str(&out.output).unwrap();
        assert!(v["result_utc"].as_str().unwrap().starts_with("2025-02-28"));
    }

    #[tokio::test]
    async fn date_math_rejects_negative_amount() {
        let tool = DateMathTool::new();
        let out = tool.execute(json!({
            "operation": "add", "amount": -5, "unit": "days",
        })).await.unwrap();
        assert!(!out.success);
    }

    #[test]
    fn parse_offset_variants() {
        assert_eq!(parse_offset("+10:00").unwrap().local_minus_utc(), 10 * 3600);
        assert_eq!(parse_offset("-05:30").unwrap().local_minus_utc(), -(5 * 3600 + 30 * 60));
        assert_eq!(parse_offset("+0930").unwrap().local_minus_utc(), 9 * 3600 + 30 * 60);
        assert_eq!(parse_offset("+10").unwrap().local_minus_utc(), 10 * 3600);
        assert!(parse_offset("10:00").is_none(), "no sign → None");
        assert!(parse_offset("+99:00").is_none(), "hours > 14 → None");
    }
}
