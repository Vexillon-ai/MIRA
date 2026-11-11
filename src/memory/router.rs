// SPDX-License-Identifier: AGPL-3.0-or-later

//! `MemoryRouter` — fans a recall across N memory providers and merges the
//! results into one structured outcome for the turn hook to render (Part B1b).
//!
//! It is the single place that:
//! - reads from every enabled provider, keeping each fact tagged with its
//!   `source_id`;
//! - enforces the two **earned** claims — a fact's `complete_set` is dropped
//!   unless its provider declares `can_assert_complete`, and `Verified` trust is
//!   kept only when the provider declares `citations` (otherwise downgraded to
//!   `Unverified`);
//! - keeps the three statuses distinct — `Absent` (healthy, nothing) renders
//!   nothing, while `Unavailable`/`Err` become an explicit "store unavailable"
//!   signal so the model never reads a down store as "the user has no such
//!   memory" (the single most important correctness property of Part B);
//! - merges `Facts` across providers by score, de-duplicated, and caps the
//!   merged list to a byte budget.
//!
//! Rendering (the header text, per-line ages, the unavailable block) stays in
//! the turn hook so layering points one way (agent → memory).

use std::sync::Arc;

use super::provider::{
    MemoryFact, MemoryProvider, MemoryQuery, MemoryRetrieval, Trust,
};

/// What a recall produced, structured for the hook to render.
#[derive(Debug, Default, Clone)]
pub struct RecalledMemory {
    /// Merged, de-duplicated, budget-capped facts (highest score first).
    pub facts: Vec<MemoryFact>,
    /// Providers that returned an unavailable/error status this turn, with the
    /// reason — rendered as an explicit "store unavailable" block, never dropped.
    pub unavailable: Vec<(String, String)>,
    /// Providers that WITHHELD a result this turn (had related material but
    /// didn't return it — e.g. a near/ambiguous match), with the reason. Rendered
    /// as "a result was withheld: …", never as "nothing recorded".
    pub withheld: Vec<(String, String)>,
    /// Provider ids that actually contributed ≥1 fact (for UI pills / logs).
    pub served: Vec<String>,
}

impl RecalledMemory {
    pub fn is_empty(&self) -> bool { self.facts.is_empty() && self.unavailable.is_empty() }
    /// Does any surviving fact legitimately claim to be the complete set?
    pub fn has_complete_set(&self) -> bool { self.facts.iter().any(|f| f.complete_set) }
}

/// Orchestrates recall across the configured memory providers.
pub struct MemoryRouter {
    read_providers: Vec<Arc<dyn MemoryProvider>>,
}

impl MemoryRouter {
    /// Build from the enabled read providers, in priority order (first = highest).
    pub fn new(read_providers: Vec<Arc<dyn MemoryProvider>>) -> Self {
        Self { read_providers }
    }

    /// Convenience: a single-provider router (today's built-in-only setup).
    pub fn single(provider: Arc<dyn MemoryProvider>) -> Self {
        Self { read_providers: vec![provider] }
    }

    /// Recall across all read providers and merge. `budget_bytes` caps the
    /// rendered size of the merged facts (0 = no cap).
    pub async fn recall(&self, q: &MemoryQuery, budget_bytes: usize) -> RecalledMemory {
        use std::time::Duration;
        // Each provider gets a bounded slice of the turn: a slow/hung external
        // store can't stall recall. Providers run CONCURRENTLY (was sequential,
        // so N HTTP providers added their latencies).
        const PER_PROVIDER_TIMEOUT: Duration = Duration::from_millis(2500);

        let futs = self.read_providers.iter()
            .filter(|p| p.capabilities().read)
            .map(|p| {
                let p = Arc::clone(p);
                async move {
                    let caps = p.capabilities();
                    let res = match tokio::time::timeout(PER_PROVIDER_TIMEOUT, p.retrieve(q)).await {
                        Ok(Ok(r))  => r,
                        Ok(Err(e)) => MemoryRetrieval::Unavailable { reason: e.to_string() },
                        Err(_)     => MemoryRetrieval::Unavailable { reason: "timed out".into() },
                    };
                    (p.id().to_string(), caps, res)
                }
            })
            .collect::<Vec<_>>();
        let results = futures::future::join_all(futs).await;

        let mut out = RecalledMemory::default();
        for (id, caps, res) in results {
            match res {
                MemoryRetrieval::Facts(mut fs) => {
                    // Sort by the provider's OWN score first so the rank reflects
                    // its ranking regardless of the order it returned facts in.
                    fs.sort_by(|a, b| {
                        b.score.unwrap_or(0.0)
                            .partial_cmp(&a.score.unwrap_or(0.0))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    let n = fs.len().max(1) as f32;
                    // Provider-reported score floor (L8): scale this provider's
                    // whole rank-normalised list by its own best raw score (clamped
                    // to [0,1]) so a provider whose top hit is weak doesn't tie a
                    // provider whose top hit is strong. A provider that reports no
                    // scores gets a neutral 1.0 (falls back to pure rank). This
                    // keeps incomparable raw scales interleaving fairly while still
                    // letting a confident hit outrank a weak one across providers.
                    let raw_floor = {
                        // A provider that reports NO scores gets a neutral mid
                        // floor (0.5) — not 1.0, which would make its #1 always
                        // outrank a strong built-in match (L8). A provider that
                        // does score uses its own clamped top score.
                        let top = fs.first().and_then(|f| f.score);
                        match top {
                            Some(t) if t.is_finite() && t > 0.0 => t.min(1.0),
                            _ => 0.5,
                        }
                    };
                    for (idx, f) in fs.iter_mut().enumerate() {
                        f.source_id = id.clone();
                        // Earned completeness: drop the claim unless declared.
                        if f.complete_set && !caps.can_assert_complete { f.complete_set = false; }
                        // Earned trust: only a citations-capable provider may
                        // present a fact as Verified; otherwise it's a note.
                        if matches!(f.trust, Trust::Verified) && !caps.citations {
                            f.trust = Trust::Unverified;
                        }
                        // Rank-NORMALISE within the provider, then scale by the
                        // provider's own confidence floor (above).
                        f.score = Some(raw_floor * (n - idx as f32) / n);
                    }
                    if !fs.is_empty() {
                        out.served.push(id.clone());
                        out.facts.extend(fs);
                    }
                }
                // Healthy, nothing for this query — renders no block (abstention
                // framing lives in the header, not here).
                MemoryRetrieval::Absent { .. } => {}
                MemoryRetrieval::Withheld { reason } => {
                    out.withheld.push((
                        id,
                        reason.unwrap_or_else(|| "a related result was withheld".into()),
                    ));
                }
                MemoryRetrieval::Unavailable { reason } => {
                    out.unavailable.push((id, reason));
                }
            }
        }

        // Merge across providers: highest (normalised) score first, then
        // de-duplicate on normalised content (keeping the first/highest copy).
        out.facts.sort_by(|a, b| {
            b.score.unwrap_or(0.0)
                .partial_cmp(&a.score.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        dedup_by_content(&mut out.facts);

        // GLOBAL top-k cap: `k` is the TOTAL facts to inject, not per provider —
        // N providers must not inject N×k.
        if q.k > 0 {
            out.facts.truncate(q.k);
        }

        // Budget: drop lowest-ranked facts once the cumulative rendered size
        // exceeds the cap (whole facts, never mid-sentence).
        if budget_bytes > 0 {
            let mut used = 0usize;
            out.facts.retain(|f| {
                let size = f.content.len() + 24; // + a little for the "• (recorded …)" chrome
                if used + size <= budget_bytes { used += size; true } else { false }
            });
        }
        out
    }
}

fn normalise(s: &str) -> String {
    s.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

fn dedup_by_content(facts: &mut Vec<MemoryFact>) {
    let mut seen = std::collections::HashSet::new();
    facts.retain(|f| seen.insert(normalise(&f.content)));
}

/// Allocate a byte budget across N priority-ordered labelled sources (the
/// generalisation of the old two-source `cap_rag_context`). Items are filled in
/// the given order (first = highest priority) until the total is exhausted; the
/// first item that would overflow is truncated with an ellipsis, and lower-
/// priority items past that point are dropped. `total_bytes = 0` = no cap.
pub fn allocate_budget(items: &[(String, String)], total_bytes: usize) -> Vec<(String, String)> {
    if total_bytes == 0 {
        return items.to_vec();
    }
    let mut out = Vec::new();
    let mut used = 0usize;
    for (label, text) in items {
        if used >= total_bytes { break; }
        let remaining = total_bytes - used;
        if text.len() <= remaining {
            used += text.len();
            out.push((label.clone(), text.clone()));
        } else if remaining > 1 {
            let mut t: String = text.chars().take(remaining.saturating_sub(1)).collect();
            t.push('…');
            used = total_bytes;
            out.push((label.clone(), t));
        } else {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::provider::{
        ForgetSelector, MemoryCapabilities, MemoryError, MemoryHealth, MemoryLocality,
        Speaker, TurnRecord,
    };
    use async_trait::async_trait;

    /// A configurable stub provider for router tests.
    struct Stub {
        id: String,
        caps: MemoryCapabilities,
        result: MemoryRetrieval,
    }
    #[async_trait]
    impl MemoryProvider for Stub {
        fn id(&self) -> &str { &self.id }
        fn capabilities(&self) -> MemoryCapabilities { self.caps }
        fn locality(&self) -> MemoryLocality { MemoryLocality::Loopback }
        async fn health(&self) -> MemoryHealth { MemoryHealth::Ready }
        async fn retrieve(&self, _q: &MemoryQuery) -> Result<MemoryRetrieval, MemoryError> {
            Ok(self.result.clone())
        }
        async fn ingest(&self, _t: &TurnRecord) -> Result<usize, MemoryError> { Ok(0) }
        async fn forget(&self, _s: &ForgetSelector) -> Result<usize, MemoryError> { Ok(0) }
    }

    fn fact(content: &str, score: f32) -> MemoryFact {
        MemoryFact {
            id: None, content: content.into(), score: Some(score), recorded_at: None,
            valid_at: None, source_id: String::new(), provenance: None, confidence: None,
            speaker: Speaker::User, trust: Trust::Unverified, complete_set: false,
            provider_meta: serde_json::Value::Null,
        }
    }

    fn stub(id: &str, caps: MemoryCapabilities, result: MemoryRetrieval) -> Arc<dyn MemoryProvider> {
        Arc::new(Stub { id: id.into(), caps, result })
    }

    #[tokio::test]
    async fn unavailable_is_recorded_not_dropped() {
        let r = MemoryRouter::single(stub(
            "ext-http", MemoryCapabilities::read_only(),
            MemoryRetrieval::Unavailable { reason: "connection refused".into() },
        ));
        let got = r.recall(&MemoryQuery::new("u", "q", 5), 0).await;
        assert!(got.facts.is_empty());
        assert_eq!(got.unavailable.len(), 1, "unavailable must surface, never collapse to empty");
        assert_eq!(got.unavailable[0].0, "ext-http");
        assert!(!got.is_empty(), "a down store is NOT an empty recall");
    }

    #[tokio::test]
    async fn absent_renders_nothing() {
        let r = MemoryRouter::single(stub(
            "builtin", MemoryCapabilities::read_only(),
            MemoryRetrieval::Absent { reason: None },
        ));
        let got = r.recall(&MemoryQuery::new("u", "q", 5), 0).await;
        assert!(got.is_empty(), "healthy-but-empty is nothing to render");
    }

    #[tokio::test]
    async fn complete_set_dropped_without_capability() {
        let mut f = fact("the full list", 0.9);
        f.complete_set = true;
        let caps = MemoryCapabilities::read_only(); // can_assert_complete = false
        let r = MemoryRouter::single(stub("x", caps, MemoryRetrieval::Facts(vec![f])));
        let got = r.recall(&MemoryQuery::new("u", "q", 5), 0).await;
        assert!(!got.has_complete_set(), "completeness claim dropped without can_assert_complete");

        let mut f2 = fact("the full list", 0.9);
        f2.complete_set = true;
        let caps2 = MemoryCapabilities { can_assert_complete: true, ..MemoryCapabilities::read_only() };
        let r2 = MemoryRouter::single(stub("x", caps2, MemoryRetrieval::Facts(vec![f2])));
        let got2 = r2.recall(&MemoryQuery::new("u", "q", 5), 0).await;
        assert!(got2.has_complete_set(), "completeness honoured when declared");
    }

    #[tokio::test]
    async fn verified_trust_downgraded_without_citations() {
        let mut f = fact("a stamped fact", 0.9);
        f.trust = Trust::Verified;
        let caps = MemoryCapabilities::read_only(); // citations = false
        let r = MemoryRouter::single(stub("x", caps, MemoryRetrieval::Facts(vec![f])));
        let got = r.recall(&MemoryQuery::new("u", "q", 5), 0).await;
        assert_eq!(got.facts[0].trust, Trust::Unverified, "verified needs citations capability");
    }

    #[tokio::test]
    async fn merges_by_score_and_dedups_across_providers() {
        let a = stub("a", MemoryCapabilities::read_only(),
            MemoryRetrieval::Facts(vec![fact("likes tea", 0.6), fact("owns a bike", 0.9)]));
        let b = stub("b", MemoryCapabilities::read_only(),
            MemoryRetrieval::Facts(vec![fact("Likes  tea", 0.5), fact("plays guitar", 0.7)]));
        let r = MemoryRouter::new(vec![a, b]);
        let got = r.recall(&MemoryQuery::new("u", "q", 10), 0).await;
        // "likes tea" deduped (normalised), ordered by score desc.
        assert_eq!(got.facts.len(), 3);
        assert_eq!(got.facts[0].content, "owns a bike");   // 0.9
        assert_eq!(got.facts[1].content, "plays guitar");  // 0.7
        assert_eq!(got.facts[2].content.to_lowercase().replace("  ", " "), "likes tea"); // 0.6 copy kept
    }

    #[tokio::test]
    async fn k_is_a_global_cap_not_per_provider() {
        // Two providers, 2 facts each (4 total); k=2 must cap the MERGED set to 2,
        // not return per-provider k (which would be 4).
        let a = stub("a", MemoryCapabilities::read_only(),
            MemoryRetrieval::Facts(vec![fact("a1", 0.9), fact("a2", 0.5)]));
        let b = stub("b", MemoryCapabilities::read_only(),
            MemoryRetrieval::Facts(vec![fact("b1", 0.8), fact("b2", 0.4)]));
        let r = MemoryRouter::new(vec![a, b]);
        let got = r.recall(&MemoryQuery::new("u", "q", 2), 0).await;
        assert_eq!(got.facts.len(), 2, "k is the global cap on the merged set");
    }

    #[tokio::test]
    async fn router_preserves_the_user_scope_key() {
        // Isolation is enforced at the storage layer (get_visible filters by
        // user_id — covered by storage tests). This proves the seam passes the
        // isolation key through to the provider unchanged rather than scrambling
        // or dropping it.
        use std::sync::Mutex;
        struct Capture(Arc<Mutex<Option<String>>>);
        #[async_trait]
        impl MemoryProvider for Capture {
            fn id(&self) -> &str { "cap" }
            fn capabilities(&self) -> MemoryCapabilities { MemoryCapabilities::read_only() }
            fn locality(&self) -> MemoryLocality { MemoryLocality::Loopback }
            async fn health(&self) -> MemoryHealth { MemoryHealth::Ready }
            async fn retrieve(&self, q: &MemoryQuery) -> Result<MemoryRetrieval, MemoryError> {
                *self.0.lock().unwrap() = Some(q.user_id.clone());
                Ok(MemoryRetrieval::Absent { reason: None })
            }
            async fn ingest(&self, _t: &TurnRecord) -> Result<usize, MemoryError> { Ok(0) }
            async fn forget(&self, _s: &ForgetSelector) -> Result<usize, MemoryError> { Ok(0) }
        }
        let seen = Arc::new(Mutex::new(None));
        let r = MemoryRouter::single(Arc::new(Capture(seen.clone())));
        let _ = r.recall(&MemoryQuery::new("alice", "q", 5), 0).await;
        assert_eq!(seen.lock().unwrap().as_deref(), Some("alice"));
    }

    #[test]
    fn allocate_budget_respects_priority_and_cap() {
        let items = vec![
            ("wiki".into(),   "W".repeat(40)),
            ("builtin".into(),"B".repeat(40)),
            ("extmem".into(), "A".repeat(40)),
        ];
        // Budget fits wiki fully + a truncated builtin; extmem dropped.
        let got = allocate_budget(&items, 60);
        assert_eq!(got[0].0, "wiki");
        assert_eq!(got[0].1.len(), 40);
        assert_eq!(got[1].0, "builtin");
        assert!(got[1].1.ends_with('…') && got[1].1.chars().count() == 20);
        assert_eq!(got.len(), 2, "extmem dropped past the cap");
        // No cap → everything passes through.
        assert_eq!(allocate_budget(&items, 0).len(), 3);
    }
}
