// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/degeneracy.rs

//! Degenerate-output guard: detect pathological repetition in model output and
//! ABORT the generation instead of running it to the token cap.
//!
//! A wedged local model (out of GPU memory) once emitted nothing but `/` for
//! 8,192 tokens per prompt, and MIRA had no defence at any layer — it ran every
//! generation to its cap and then fanned the garbage into two more LLM calls
//! (the memory + wiki extractors). See `design-docs/degenerate-output-guard.md`.
//!
//! [`GuardedProvider`] wraps any [`ModelProvider`] and is installed OUTERMOST in
//! `build_provider_chain`, so it covers EVERY LLM call — chat turns, extractors,
//! Guardian triage, tool-loop rounds. On a trip it:
//!   * aborts the upstream request (streaming: the inner task is cancelled, which
//!     drops the HTTP stream and frees the backend slot immediately),
//!   * returns `Err`, which the agent treats as a FAILED turn — the garbage is
//!     not persisted and the post-turn extractors never run (they sit behind the
//!     turn's `?`), and
//!   * logs a WARN from this `mira::providers::*` module, so the trip is counted
//!     by the `llm.error_rate_1h` health detector and Guardian can see a model
//!     that has gone bad.
//!
//! The guard is disable-able and its thresholds are configurable, because a
//! legitimate request ("output a long repeated pattern") must remain possible.

use async_trait::async_trait;
use std::sync::Arc;
use tracing::warn;

use crate::config::DegeneracyGuardConfig;
use crate::providers::ModelProvider;
use crate::types::{ChatMessage, GenerationOptions, GenerationResponse};

/// Wrap `inner` in the degeneracy guard when enabled; otherwise return it as-is
/// (zero overhead when the guard is off). Called once on the assembled provider
/// chain so every downstream call inherits the guard.
pub fn guard(inner: Arc<dyn ModelProvider>, cfg: DegeneracyGuardConfig) -> Arc<dyn ModelProvider> {
    if cfg.enabled {
        Arc::new(GuardedProvider { inner, cfg })
    } else {
        inner
    }
}

/// A provider decorator that trips on degenerate (pathologically repetitive)
/// output. See the module docs.
pub struct GuardedProvider {
    inner: Arc<dyn ModelProvider>,
    cfg:   DegeneracyGuardConfig,
}

impl GuardedProvider {
    fn tripped_error() -> crate::MiraError {
        crate::MiraError::ProviderError(
            "degenerate output detected — the model produced pathologically repetitive text; \
             generation was aborted. The model backend may be unhealthy (e.g. out of memory)."
                .to_string(),
        )
    }
}

#[async_trait]
impl ModelProvider for GuardedProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn generate(
        &self,
        messages: &[ChatMessage],
        options: &GenerationOptions,
    ) -> Result<GenerationResponse, crate::MiraError> {
        let resp = self.inner.generate(messages, options).await?;
        // Non-streaming: no incremental abort possible (the whole reply already
        // arrived), but still gate the fan-out — a degenerate reply must not be
        // returned as a normal result, and it counts as a provider error.
        if is_degenerate(&resp.content, &self.cfg) {
            warn!(
                "degeneracy guard: provider '{}' returned degenerate output ({} chars); \
                 treating as provider error",
                self.inner.name(),
                resp.content.len()
            );
            return Err(Self::tripped_error());
        }
        Ok(resp)
    }

    async fn generate_stream(
        &self,
        messages: &[ChatMessage],
        options: &GenerationOptions,
        on_token: &mut (dyn FnMut(String) + Send),
    ) -> Result<GenerationResponse, crate::MiraError> {
        // Run the inner stream on its own task so we can ABORT it — and thereby
        // drop the HTTP stream and free the backend slot — the instant degeneracy
        // trips, rather than letting it run to `max_tokens` (the wedged model held
        // a llama.cpp slot for 4+ minutes doing exactly that).
        let inner = self.inner.clone();
        let msgs = messages.to_vec();
        let opts = options.clone();
        // Unbounded so the inner task's synchronous `on_token` never blocks the
        // async runtime; tokens are tiny and the parent drains immediately.
        let (tok_tx, mut tok_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let handle = tokio::spawn(async move {
            let mut fwd = move |t: String| {
                let _ = tok_tx.send(t);
            };
            inner.generate_stream(&msgs, &opts, &mut fwd).await
        });

        let mut detector = DegeneracyDetector::new(self.cfg.clone());
        let mut acc = String::new();
        while let Some(tok) = tok_rx.recv().await {
            acc.push_str(&tok);
            on_token(tok); // forward to the real sink as it streams
            if detector.tripped(&acc) {
                handle.abort();
                warn!(
                    "degeneracy guard: provider '{}' streamed degenerate output; aborted after \
                     {} chars instead of running to the token cap",
                    self.inner.name(),
                    acc.len()
                );
                return Err(Self::tripped_error());
            }
        }

        // Inner finished and dropped its sender. Collect its result; also catch a
        // reply that only looks degenerate in aggregate (e.g. it hit the cap
        // before the sliding window tripped).
        match handle.await {
            Ok(Ok(resp)) => {
                if is_degenerate(&resp.content, &self.cfg) {
                    warn!(
                        "degeneracy guard: provider '{}' produced degenerate output ({} chars); \
                         treating as provider error",
                        self.inner.name(),
                        resp.content.len()
                    );
                    return Err(Self::tripped_error());
                }
                Ok(resp)
            }
            Ok(Err(e)) => Err(e),
            Err(join) => Err(crate::MiraError::ProviderError(format!(
                "degeneracy guard: stream task failed to join: {join}"
            ))),
        }
    }

    async fn health_check(&self) -> bool {
        self.inner.health_check().await
    }
}

/// Streaming-friendly degeneracy detector. Cheap: only inspects a bounded tail
/// window, and only past a minimum length, so short legitimate repeats, ASCII
/// art, and small blobs never trip. Throttled so streaming stays O(window)
/// amortized rather than O(n²).
pub struct DegeneracyDetector {
    cfg:        DegeneracyGuardConfig,
    checked_at: usize,
}

impl DegeneracyDetector {
    pub fn new(cfg: DegeneracyGuardConfig) -> Self {
        Self { cfg, checked_at: 0 }
    }

    /// Whether the accumulated output `full` now looks degenerate. Re-checks only
    /// after ~a window of new growth, so per-token cost stays bounded.
    pub fn tripped(&mut self, full: &str) -> bool {
        if !self.cfg.enabled || full.len() < self.cfg.min_chars {
            return false;
        }
        if full.len().saturating_sub(self.checked_at) < 64 {
            return false;
        }
        self.checked_at = full.len();
        is_degenerate(full, &self.cfg)
    }
}

/// Pure degeneracy test over the tail window of `text`. Token-level AND
/// char-level, so it catches both a repeated character (`////…`) and a repeated
/// word/phrase (`the the the…`). Returns false below `min_chars`.
pub fn is_degenerate(text: &str, cfg: &DegeneracyGuardConfig) -> bool {
    if !cfg.enabled || text.len() < cfg.min_chars {
        return false;
    }
    // Tail window, snapped down to a char boundary so we never split a codepoint.
    let want = cfg.window_chars.max(16);
    let mut start = text.len().saturating_sub(want);
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let tail = &text[start..];

    // Char-level collapse: very few DISTINCT non-whitespace chars across a tail
    // that has real substance (not mostly whitespace).
    let non_ws = tail.chars().filter(|c| !c.is_whitespace()).count();
    if non_ws >= cfg.min_chars / 2 {
        let mut distinct = std::collections::HashSet::new();
        for c in tail.chars() {
            if !c.is_whitespace() {
                distinct.insert(c);
            }
            if distinct.len() > cfg.min_distinct_chars {
                break;
            }
        }
        if distinct.len() <= cfg.min_distinct_chars {
            return true;
        }
    }

    // Token-level collapse: enough whitespace-separated tokens but very few
    // DISTINCT ones (a repeated word or short phrase).
    let toks: Vec<&str> = tail.split_whitespace().collect();
    if toks.len() >= cfg.min_window_tokens {
        let distinct: std::collections::HashSet<&str> = toks.iter().copied().collect();
        if distinct.len() <= cfg.min_distinct_tokens {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DegeneracyGuardConfig {
        DegeneracyGuardConfig::default()
    }

    #[test]
    fn trips_on_repeated_single_char() {
        let garbage = "/".repeat(1000);
        assert!(is_degenerate(&garbage, &cfg()));
        // Even with the SSE-style whitespace the wedged model interleaved.
        let spaced = "/ \n".repeat(400);
        assert!(is_degenerate(&spaced, &cfg()));
    }

    #[test]
    fn trips_on_repeated_word() {
        let garbage = "the ".repeat(500);
        assert!(is_degenerate(&garbage, &cfg()));
    }

    #[test]
    fn does_not_trip_below_min_length() {
        // A short run of repeats (ASCII art / a deliberate small pattern) survives.
        let short = "=".repeat(100);
        assert!(!is_degenerate(&short, &cfg()));
    }

    #[test]
    fn does_not_trip_on_normal_prose() {
        let prose = "The quick brown fox jumps over the lazy dog. \
                     MIRA assembled the context, retrieved relevant memories, and \
                     produced a coherent answer for the user across several sentences. "
            .repeat(12);
        assert!(!is_degenerate(&prose, &cfg()));
    }

    #[test]
    fn does_not_trip_on_long_markdown_table() {
        let mut table = String::from("| id | name | value | status |\n|----|------|-------|--------|\n");
        for i in 0..200 {
            table.push_str(&format!("| {i} | item-{i} | {} | ok |\n", i * 7));
        }
        assert!(!is_degenerate(&table, &cfg()));
    }

    #[test]
    fn does_not_trip_on_base64_blob() {
        // A single long high-entropy token (no whitespace) must not trip: many
        // distinct chars → no char collapse; one token → below the token window.
        let blob: String = (0..4000)
            .map(|i| {
                let alpha = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
                alpha[(i * 37 + 11) % alpha.len()] as char
            })
            .collect();
        assert!(!is_degenerate(&blob, &cfg()));
    }

    #[test]
    fn disabled_never_trips() {
        let mut c = cfg();
        c.enabled = false;
        assert!(!is_degenerate(&"/".repeat(1000), &c));
    }

    // ── GuardedProvider behavioural tests ────────────────────────────────────
    use crate::types::{ProviderId, TokenUsage};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A mock provider that streams `token` `count` times (yielding between each,
    /// so the guard can abort mid-stream), recording how many it actually emitted.
    struct RepeatProvider {
        token: String,
        count: usize,
        emitted: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ModelProvider for RepeatProvider {
        fn name(&self) -> &str { "repeat-mock" }
        async fn generate(
            &self, _m: &[ChatMessage], _o: &GenerationOptions,
        ) -> Result<GenerationResponse, crate::MiraError> {
            Ok(resp(self.token.repeat(self.count)))
        }
        async fn generate_stream(
            &self, _m: &[ChatMessage], _o: &GenerationOptions,
            on_token: &mut (dyn FnMut(String) + Send),
        ) -> Result<GenerationResponse, crate::MiraError> {
            let mut content = String::new();
            for _ in 0..self.count {
                on_token(self.token.clone());
                content.push_str(&self.token);
                self.emitted.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await; // an await point so abort can land
            }
            Ok(resp(content))
        }
        async fn health_check(&self) -> bool { true }
    }

    fn resp(content: String) -> GenerationResponse {
        GenerationResponse {
            content,
            tool_calls: None,
            reasoning: None,
            usage: TokenUsage::default(),
            provider_id: ProviderId::Local("mock".into()),
            model_name: "mock".into(),
            fallback: None,
        }
    }

    #[tokio::test]
    async fn streaming_guard_aborts_degenerate_provider_early() {
        let emitted = Arc::new(AtomicUsize::new(0));
        let inner = Arc::new(RepeatProvider {
            token: "/".into(), count: 8192, emitted: emitted.clone(),
        });
        let g = guard(inner, DegeneracyGuardConfig::default());
        let mut forwarded = 0usize;
        let mut on_tok = |_t: String| { forwarded += 1; };
        let r = g.generate_stream(&[ChatMessage::user("hi")], &GenerationOptions::default(), &mut on_tok).await;
        assert!(r.is_err(), "degenerate stream must return Err");
        // Aborted long before the 8192 cap — the inner provider stopped emitting.
        assert!(emitted.load(Ordering::SeqCst) < 2000,
            "inner should have been aborted early, emitted={}", emitted.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn streaming_guard_passes_through_healthy_provider() {
        let emitted = Arc::new(AtomicUsize::new(0));
        // Distinct tokens → never degenerate.
        let inner = Arc::new(RepeatProvider { token: "word ".into(), count: 3, emitted });
        let g = guard(inner, DegeneracyGuardConfig::default());
        let mut got = String::new();
        let mut on_tok = |t: String| { got.push_str(&t); };
        // Short + varied enough that it doesn't trip; content flows through intact.
        let r = g.generate_stream(&[ChatMessage::user("hi")], &GenerationOptions::default(), &mut on_tok).await;
        assert!(r.is_ok());
        assert_eq!(r.unwrap().content, "word word word ");
        assert_eq!(got, "word word word ");
    }

    #[tokio::test]
    async fn non_streaming_guard_trips_on_degenerate_reply() {
        let emitted = Arc::new(AtomicUsize::new(0));
        let inner = Arc::new(RepeatProvider { token: "/".into(), count: 8192, emitted });
        let g = guard(inner, DegeneracyGuardConfig::default());
        let r = g.generate(&[ChatMessage::user("hi")], &GenerationOptions::default()).await;
        assert!(r.is_err(), "degenerate non-streamed reply must return Err");
    }

    #[tokio::test]
    async fn disabled_guard_returns_inner_unchanged() {
        let emitted = Arc::new(AtomicUsize::new(0));
        let inner = Arc::new(RepeatProvider { token: "/".into(), count: 500, emitted });
        let mut cfg = DegeneracyGuardConfig::default();
        cfg.enabled = false;
        let g = guard(inner, cfg);
        // Disabled → the bare inner provider, garbage passes through as Ok.
        let r = g.generate(&[ChatMessage::user("hi")], &GenerationOptions::default()).await;
        assert!(r.is_ok(), "disabled guard must not trip");
    }

    #[test]
    fn streaming_detector_throttles_then_trips() {
        let mut d = DegeneracyDetector::new(cfg());
        let mut acc = String::new();
        let mut tripped_at = None;
        for i in 0..2000 {
            acc.push('/');
            if d.tripped(&acc) {
                tripped_at = Some(i);
                break;
            }
        }
        let at = tripped_at.expect("should trip on a pure-slash stream");
        // Trips well before a 8192-char/token cap — within a few hundred chars.
        assert!(at < 700, "tripped too late: {at}");
        assert!(at >= DegeneracyGuardConfig::default().min_chars - 1);
    }
}
