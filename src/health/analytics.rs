// SPDX-License-Identifier: AGPL-3.0-or-later

// src/health/analytics.rs
//! 0.110.0 — slice 5c. Forecast / anomaly / correlation enrichment for
//! detector reports.
//!
//! Each function reads from `health.db` snapshot history (and the
//! watchdog incidents table for correlation). All three are best-
//! effort: insufficient history → None, never an error.

use std::collections::HashMap;

use super::{DetectorAnalytics, DetectorReport, HealthSnapshot};

/// Run all three analytics passes against a freshly-built report. Reads
/// the last 7 days of full snapshots from `health.db` plus same-window
/// incident overlap from `watchdog_incidents`. Mutates the report in
/// place to attach `analytics`.
pub fn enrich(
    report:       &mut DetectorReport,
    history:      &[HealthSnapshot],
    threshold_red: Option<f64>,
    correlated:   Vec<String>,
) {
    // Skip detectors with no numeric value — there's nothing to project.
    let Some(current) = report.value else { return };
    let mut analytics = DetectorAnalytics::default();

    // ── Anomaly: z-score against last 7d of values ───────────────────
    let recent_values: Vec<f64> = history.iter()
        .filter_map(|snap| snap.reports.iter().find(|r| r.name == report.name))
        .filter_map(|r| r.value)
        .collect();
    if recent_values.len() >= 4 {
        let n = recent_values.len() as f64;
        let mean = recent_values.iter().sum::<f64>() / n;
        let variance = recent_values.iter()
            .map(|v| (v - mean).powi(2))
            .sum::<f64>() / n;
        let stddev = variance.sqrt();
        if stddev > 1e-9 {
            let z = (current - mean) / stddev;
            // Surface only meaningful outliers — small z's are noise.
            if z.abs() >= 1.0 {
                analytics.anomaly_z = Some(z);
            }
        }
    }

    // ── Forecast: simple linear regression vs time ───────────────────
    // Only meaningful when we know a red threshold AND the trend is
    // moving toward it. Fit y=mx+b on (taken_at, value); project when
    // y crosses red_threshold.
    if let Some(red_at) = threshold_red {
        let mut points: Vec<(f64, f64)> = history.iter()
            .filter_map(|snap| {
                let r = snap.reports.iter().find(|r| r.name == report.name)?;
                let v = r.value?;
                Some((snap.taken_at as f64, v))
            })
            .collect();
        // Include the just-taken point so the regression covers "now".
        points.push((chrono::Utc::now().timestamp() as f64, current));
        if points.len() >= 4 {
            if let Some(slope_per_sec) = linear_regression_slope(&points) {
                if slope_per_sec.abs() > 1e-12 {
                    let dist = red_at - current;
                    let secs_to_red = dist / slope_per_sec;
                    // Only forecast when the projection is in the future
                    // AND within the next 24h. Past or too-far values
                    // are useless to surface.
                    if secs_to_red > 0.0 && secs_to_red < 24.0 * 3600.0 {
                        analytics.forecast_red_in_hours = Some(secs_to_red / 3600.0);
                    }
                }
            }
        }
    }

    analytics.correlated_detectors = correlated;

    // Attach only when at least one field is populated — keeps green
    // reports clean.
    if analytics.anomaly_z.is_some()
        || analytics.forecast_red_in_hours.is_some()
        || !analytics.correlated_detectors.is_empty()
    {
        report.analytics = Some(analytics);
    }
}

/// Build a per-detector correlation table once per audit (much cheaper
/// than per-detector queries). For each pair (A, B) of detectors that
/// both tripped within ±10 min of each other ≥3 times in the last 7
/// days, store the relationship. Returns a map from detector_name →
/// list of correlated detector names (top 5 by overlap count).
pub fn compute_correlations(
    automations: &crate::automations::AutomationsStore,
) -> HashMap<String, Vec<String>> {
    let week_ago = chrono::Utc::now().timestamp() - 7 * 24 * 3600;
    let incidents = match automations.list_health_incidents_since(week_ago) {
        Ok(v)  => v,
        Err(_) => return HashMap::new(),
    };
    if incidents.len() < 6 { return HashMap::new(); }

    // Bucket incidents by detector name + sort by created_at.
    let mut by_detector: HashMap<String, Vec<i64>> = HashMap::new();
    for inc in &incidents {
        let name = inc.module.strip_prefix("health/").unwrap_or(&inc.module).to_string();
        by_detector.entry(name).or_default().push(inc.created_at);
    }

    // For each pair, count overlaps within ±10 min.
    let mut counts: HashMap<(String, String), usize> = HashMap::new();
    let names: Vec<&String> = by_detector.keys().collect();
    const WINDOW: i64 = 600;
    for i in 0..names.len() {
        for j in (i + 1)..names.len() {
            let a = by_detector.get(names[i]).unwrap();
            let b = by_detector.get(names[j]).unwrap();
            let mut hits = 0usize;
            for &ta in a {
                for &tb in b {
                    if (ta - tb).abs() <= WINDOW { hits += 1; break; }
                }
            }
            if hits >= 3 {
                counts.insert((names[i].clone(), names[j].clone()), hits);
            }
        }
    }

    // Invert: per-detector list of correlated peers, top 5 by count.
    let mut out: HashMap<String, Vec<(String, usize)>> = HashMap::new();
    for ((a, b), n) in counts {
        out.entry(a.clone()).or_default().push((b.clone(), n));
        out.entry(b).or_default().push((a, n));
    }
    out.into_iter().map(|(k, mut v)| {
        v.sort_by(|a, b| b.1.cmp(&a.1));
        (k, v.into_iter().take(5).map(|(name, _)| name).collect())
    }).collect()
}

/// Slope of the best-fit line over `(x, y)` points. Returns None for
/// degenerate input (single point, all-x-equal).
fn linear_regression_slope(points: &[(f64, f64)]) -> Option<f64> {
    let n = points.len() as f64;
    if n < 2.0 { return None; }
    let mean_x = points.iter().map(|p| p.0).sum::<f64>() / n;
    let mean_y = points.iter().map(|p| p.1).sum::<f64>() / n;
    let mut num = 0.0;
    let mut den = 0.0;
    for &(x, y) in points {
        num += (x - mean_x) * (y - mean_y);
        den += (x - mean_x).powi(2);
    }
    if den.abs() < 1e-12 { return None; }
    Some(num / den)
}

/// Look up the red threshold the collector would apply for a detector,
/// preferring an admin override over the detector's own report. Used
/// by the forecast computer.
pub fn resolve_red_threshold(
    detector_name: &str,
    overrides:     &HashMap<String, super::store::ThresholdRow>,
) -> Option<f64> {
    overrides.get(detector_name).and_then(|t| t.red_at)
}

/// 0.110.0 — render an analytics block as markdown for the LLM trend
/// analyst. Returns empty string when there's nothing useful to add.
pub fn render_for_prompt(analytics: &DetectorAnalytics) -> String {
    if analytics.anomaly_z.is_none()
        && analytics.forecast_red_in_hours.is_none()
        && analytics.correlated_detectors.is_empty()
    {
        return String::new();
    }
    let mut s = String::from("\n\n**Predictive analytics:**\n");
    if let Some(z) = analytics.anomaly_z {
        s.push_str(&format!(
            "- Anomaly z-score: **{:.2}σ** vs last 7d for this detector ({})\n",
            z, if z.abs() >= 3.0 { "extreme outlier" }
               else if z.abs() >= 2.0 { "moderate outlier" }
               else { "borderline" },
        ));
    }
    if let Some(h) = analytics.forecast_red_in_hours {
        s.push_str(&format!(
            "- Forecast: at the current trend, value crosses red threshold in **~{:.1}h**\n", h,
        ));
    }
    if !analytics.correlated_detectors.is_empty() {
        s.push_str(&format!(
            "- Correlated detectors (tripped within ±10 min of this one ≥3× last 7d): {}\n",
            analytics.correlated_detectors.iter()
                .map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", "),
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slope_basic() {
        let pts = vec![(0.0, 0.0), (1.0, 2.0), (2.0, 4.0), (3.0, 6.0)];
        let s = linear_regression_slope(&pts).unwrap();
        assert!((s - 2.0).abs() < 1e-9);
    }

    #[test]
    fn slope_none_when_x_collapsed() {
        let pts = vec![(1.0, 5.0), (1.0, 7.0)];
        assert!(linear_regression_slope(&pts).is_none());
    }
}
