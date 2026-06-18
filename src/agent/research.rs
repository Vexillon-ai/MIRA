// SPDX-License-Identifier: AGPL-3.0-or-later

//! Research adapter (slice C5).
//!
//! Unlike the C2/C3/C4 adapters that wrap external CLI tools,
//! `ResearchAdapter` runs the entire workflow in-process: it composes
//! MIRA's existing search backend (`tools::search::SearchBackend`),
//! HTTP-policy fetcher (`tools::http_policy::HttpPolicy`), and a
//! `ModelProvider` to do search → fetch → synthesize → cite.
//!
//! Pipeline:
//!
//!   1. Take the worker's `task` as the research question.
//!   2. Call the configured `SearchBackend` for the top N hits.
//!      Surface each hit URL as a Progress event so the agents UI
//!      shows the search trail.
//!   3. Fetch each hit through a [`ResearchFetcher`] (production wiring
//!      uses `HttpPolicy`; tests stub with a HashMap) — fetches run
//!      concurrently. Failures per-hit are logged and dropped, not
//!      fatal — partial source coverage is better than no answer.
//!   4. Build a synthesis prompt with all the source bodies
//!      truncated to `max_chars_per_source`, plus an instruction to
//!      cite each claim with `[1]`/`[2]`/… numbers tied to the
//!      sources list.
//!   5. Call the `ModelProvider` once. The full LLM cost is reported
//!      as a single Progress event with `llm_spend_usd` set so the
//!      supervisor's session-budget math sees it.
//!   6. Return `WorkerComplete` whose `result_summary` is the model's
//!      synthesis followed by a "## Sources" section that maps each
//!      `[N]` back to its title + URL.
//!
//! Why a `ResearchFetcher` trait instead of taking `Arc<HttpPolicy>`
//! directly: the SSRF-guarded policy is the right thing in production,
//! but tests need to drive the adapter without spinning up a real HTTP
//! server. A two-method trait (`fetch`) keeps the seam narrow.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tracing::{debug, warn};

use crate::agent::supervisor::{
    WorkerAssignment, WorkerComplete, WorkerContext, WorkerFailure, WorkerTask,
};
use crate::providers::ModelProvider;
use crate::tools::http_policy::HttpPolicy;
use crate::tools::search::{SafeSearch, SearchBackend, SearchHit, SearchLimits};
use crate::types::{ChatMessage, GenerationOptions};

/// The synthesis system prompt the adapter ships by default. Tells the
/// model to ground every claim in the numbered sources and to be
/// honest about gaps. Overridable via [`ResearchConfig::system_prompt`].
const DEFAULT_SYNTHESIS_SYSTEM_PROMPT: &str = "\
You are a careful research assistant. The user has asked a question and \
collected several web sources for you. Your job is to synthesise an \
answer **based only on the provided sources**.

Rules:
- Cite every non-trivial claim with `[N]` where N is the source number \
  shown in the prompt. Multiple citations are fine: `[1][3]`.
- If the sources disagree, surface the disagreement explicitly and cite \
  each side.
- If the sources don't answer the question, say so plainly. Do not invent \
  facts to fill the gap.
- Keep the answer focused and not much longer than necessary. The reader \
  will see a Sources section appended automatically — don't repeat the URLs \
  inline unless quoting.";

/// Minimal seam over `HttpPolicy` so tests can mock the fetch step.
/// Production wiring uses [`HttpPolicyFetcher`] which is a thin shim.
#[async_trait]
pub trait ResearchFetcher: Send + Sync {
    /// Fetch one URL. The implementation is responsible for honouring
    /// the SSRF guard / denylist / rate limits — `HttpPolicyFetcher`
    /// delegates to `HttpPolicy::get` for that.
    async fn fetch(&self, url: &str) -> Result<FetchedDoc, String>;
}

/// What a single fetch returns. `text` is plain-text (readability-
/// extracted for HTML), already truncated to whatever cap the fetcher
/// applied.
#[derive(Debug, Clone)]
pub struct FetchedDoc {
    pub final_url: String,
    pub title:     Option<String>,
    pub text:      String,
}

/// Production-grade [`ResearchFetcher`] that delegates to
/// [`HttpPolicy`]. Runs readability-rs over HTML responses; falls back
/// to UTF-8 decoded body otherwise.
pub struct HttpPolicyFetcher {
    policy:    Arc<HttpPolicy>,
    user_id:   String,
    max_chars: usize,
}

impl HttpPolicyFetcher {
    pub fn new(policy: Arc<HttpPolicy>, user_id: impl Into<String>, max_chars: usize) -> Self {
        Self { policy, user_id: user_id.into(), max_chars }
    }
}

#[async_trait]
impl ResearchFetcher for HttpPolicyFetcher {
    async fn fetch(&self, url: &str) -> Result<FetchedDoc, String> {
        let resp = self.policy.get(url, &self.user_id).await
            .map_err(|e| format!("policy: {e}"))?;
        if !(200..300).contains(&resp.status) {
            return Err(format!("http {} for {}", resp.status, resp.final_url));
        }
        let ct = resp.content_type.as_deref().unwrap_or("").to_ascii_lowercase();
        let (title, text) = if ct.contains("html") {
            extract_readable(&resp.body, &resp.final_url)
        } else if is_text_like(&ct) {
            (None, String::from_utf8_lossy(&resp.body).into_owned())
        } else {
            return Err(format!("non-text content-type: {ct}"));
        };
        let text = cap_chars(&text, self.max_chars);
        Ok(FetchedDoc {
            final_url: resp.final_url,
            title,
            text,
        })
    }
}

fn is_text_like(ct: &str) -> bool {
    ct.starts_with("text/")
        || ct.contains("json")
        || ct.contains("xml")
        || ct.contains("javascript")
        || ct.is_empty()
}

fn cap_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars { return s.to_owned(); }
    s.chars().take(max_chars).collect()
}

/// Readability extraction. Mirrors the helper in `tools::web_fetch` —
/// kept local rather than reaching into a private fn so the research
/// module doesn't take a dependency on the internals of another module.
fn extract_readable(body: &[u8], url: &str) -> (Option<String>, String) {
    let url_parsed = url::Url::parse(url);
    let mut cursor = std::io::Cursor::new(body);
    match url_parsed.as_ref() {
        Ok(u) => match readability::extractor::extract(&mut cursor, u) {
            Ok(p) => {
                let title = if p.title.trim().is_empty() { None } else { Some(p.title) };
                let text = if !p.text.trim().is_empty() { p.text }
                           else { strip_html(&p.content) };
                (title, text)
            }
            Err(_) => (None, String::from_utf8_lossy(body).into_owned()),
        },
        Err(_) => (None, String::from_utf8_lossy(body).into_owned()),
    }
}

fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Configuration for a [`ResearchAdapter`]. Constructed once + reused
/// across spawns, so all the heavy infrastructure (policy, model
/// client, backend) is created once.
pub struct ResearchConfig {
    /// Search backend (DuckDuckGo, Brave, SearXNG …). Required.
    pub backend: Arc<dyn SearchBackend>,

    /// How to fetch URLs returned by the search backend. Required.
    /// Production wiring: [`HttpPolicyFetcher`] wrapping the gateway's
    /// existing `HttpPolicy`.
    pub fetcher: Arc<dyn ResearchFetcher>,

    /// LLM provider used for synthesis. Required.
    pub model:   Arc<dyn ModelProvider>,

    /// Generation options passed to the model. Defaults to a low
    /// temperature for fact-grounded synthesis. Override per-call by
    /// supplying your own (e.g. higher temperature for exploratory
    /// research).
    pub options: GenerationOptions,

    /// How many search hits to actually fetch + include. Default 5.
    /// Higher = more grounding sources but more tokens in the prompt.
    pub top_k: usize,

    /// Per-source body truncation. Stops one Wikipedia page from
    /// blowing the context budget. Default 4000 chars.
    pub max_chars_per_source: usize,

    /// Search-backend safe-search level. Default Moderate.
    pub safe_search: SafeSearch,

    /// Search-backend region pin (e.g. "us-en"). None = backend default.
    pub region: Option<String>,

    /// Override the default synthesis system prompt. None = use
    /// [`DEFAULT_SYNTHESIS_SYSTEM_PROMPT`].
    pub system_prompt: Option<String>,

    /// `user_id` passed through to the search backend (some backends
    /// scope rate limits per user). Default `"research-adapter"`.
    pub user_id: String,
}

impl ResearchConfig {
    pub fn new(
        backend: Arc<dyn SearchBackend>,
        fetcher: Arc<dyn ResearchFetcher>,
        model:   Arc<dyn ModelProvider>,
    ) -> Self {
        Self {
            backend, fetcher, model,
            options:              GenerationOptions { temperature: 0.2, ..Default::default() },
            top_k:                5,
            max_chars_per_source: 4000,
            safe_search:          SafeSearch::Moderate,
            region:               None,
            system_prompt:        None,
            user_id:              "research-adapter".into(),
        }
    }

    pub fn with_top_k(mut self, k: usize)                   -> Self { self.top_k = k; self }
    pub fn with_max_chars_per_source(mut self, n: usize)    -> Self { self.max_chars_per_source = n; self }
    pub fn with_safe_search(mut self, s: SafeSearch)        -> Self { self.safe_search = s; self }
    pub fn with_region(mut self, r: impl Into<String>)      -> Self { self.region = Some(r.into()); self }
    pub fn with_system_prompt(mut self, p: impl Into<String>) -> Self { self.system_prompt = Some(p.into()); self }
    pub fn with_user_id(mut self, id: impl Into<String>)    -> Self { self.user_id = id.into(); self }
    pub fn with_options(mut self, o: GenerationOptions)     -> Self { self.options = o; self }
}

pub struct ResearchAdapter {
    config: ResearchConfig,
}

impl ResearchAdapter {
    pub fn new(config: ResearchConfig) -> Arc<Self> {
        Arc::new(Self { config })
    }
}

#[async_trait]
impl WorkerTask for ResearchAdapter {
    async fn run(
        &self,
        assignment: WorkerAssignment,
        ctx:        WorkerContext,
    ) -> Result<WorkerComplete, WorkerFailure> {
        let question = assignment.task.trim();
        if question.is_empty() {
            return Err(WorkerFailure {
                error: "research adapter: empty research question".into(),
                partial_artifacts: vec![], fault: None,
            });
        }

        // ── Step 1: search ───────────────────────────────────────────
        ctx.report_progress(format!("[research] searching: {question}"), None, 0.0);
        let limits = SearchLimits {
            top_k:  self.config.top_k.max(1),
            region: self.config.region.clone(),
            safe:   self.config.safe_search,
        };
        let hits = match self.config.backend.search(question, &limits, &self.config.user_id).await {
            Ok(h)  => h,
            Err(e) => return Err(WorkerFailure {
                error: format!("search backend {} failed: {e}", self.config.backend.id()),
                partial_artifacts: vec![], fault: None,
            }),
        };
        if hits.is_empty() {
            return Err(WorkerFailure {
                error: format!("no search results for {question:?}"),
                partial_artifacts: vec![], fault: None,
            });
        }
        let hits: Vec<SearchHit> = hits.into_iter().take(self.config.top_k).collect();
        ctx.report_progress(
            format!("[research] {} hits — fetching top {}", hits.len(), self.config.top_k),
            None, 0.0,
        );

        // ── Step 2: fetch each hit concurrently ─────────────────────
        let fetcher = Arc::clone(&self.config.fetcher);
        let fetches = hits.iter().enumerate().map(|(i, hit)| {
            let fetcher = Arc::clone(&fetcher);
            let hit     = hit.clone();
            let sender  = ctx.sender_clone();
            async move {
                match fetcher.fetch(&hit.url).await {
                    Ok(doc) => {
                        let _ = sender.send_event(crate::agent::protocol::Event::Progress {
                            step_summary: format!("[fetched {}] {}", i + 1, doc.final_url),
                            percent_done: None,
                            llm_spend_usd: 0.0,
                        });
                        Some((hit, doc))
                    }
                    Err(e) => {
                        warn!("research: fetch {} failed: {e}", hit.url);
                        let _ = sender.send_event(crate::agent::protocol::Event::Progress {
                            step_summary: format!("[fetch failed {}] {}: {e}", i + 1, hit.url),
                            percent_done: None,
                            llm_spend_usd: 0.0,
                        });
                        None
                    }
                }
            }
        });
        let sources: Vec<(SearchHit, FetchedDoc)> = futures::future::join_all(fetches).await
            .into_iter()
            .flatten()
            .collect();

        if sources.is_empty() {
            return Err(WorkerFailure {
                error: format!(
                    "all {} candidate sources failed to fetch",
                    hits.len(),
                ),
                partial_artifacts: vec![], fault: None,
            });
        }

        // ── Step 3: synthesise ──────────────────────────────────────
        ctx.report_progress(
            format!("[research] synthesising answer from {} sources", sources.len()),
            None, 0.0,
        );
        let system_prompt = self.config.system_prompt.as_deref()
            .unwrap_or(DEFAULT_SYNTHESIS_SYSTEM_PROMPT);
        let user_msg = build_synthesis_prompt(question, &sources);
        debug!(
            "research: synthesis prompt = {} chars over {} sources",
            user_msg.len(), sources.len(),
        );
        let messages = vec![
            ChatMessage::system(system_prompt),
            ChatMessage::user(user_msg),
        ];
        let response = match self.config.model.generate(&messages, &self.config.options).await {
            Ok(r)  => r,
            Err(e) => return Err(WorkerFailure {
                error: format!("synthesis model {} failed: {e}", self.config.model.name()),
                partial_artifacts: vec![], fault: None,
            }),
        };

        // Final progress marker so the agents UI shows synthesis
        // landed. We don't currently report dollar cost here —
        // `TokenUsage` exposes only token counts; pricing those off
        // belongs to a per-provider rate sheet that doesn't yet exist
        // in this codebase. When it lands the cost can be folded in
        // by setting `llm_spend_usd` on this Progress.
        ctx.report_progress(
            format!(
                "[research] synthesis complete ({} prompt + {} completion tokens)",
                response.usage.prompt_tokens, response.usage.completion_tokens,
            ),
            Some(1.0),
            0.0,
        );

        // ── Step 4: assemble final result ────────────────────────────
        let summary = format!(
            "{body}\n\n## Sources\n{citations}",
            body      = response.content.trim(),
            citations = format_citations(&sources),
        );
        Ok(WorkerComplete {
            result_summary: summary,
            artifacts:      vec![],
        })
    }
}

/// Pure helper: build the user-facing prompt that gets handed to the
/// synthesis model. Each source is numbered so the model can cite by
/// `[N]`. Identical numbering is reused by `format_citations` to build
/// the trailing Sources section.
fn build_synthesis_prompt(question: &str, sources: &[(SearchHit, FetchedDoc)]) -> String {
    let mut out = String::new();
    out.push_str("Question:\n");
    out.push_str(question);
    out.push_str("\n\nSources:\n");
    for (i, (hit, doc)) in sources.iter().enumerate() {
        let n = i + 1;
        let title = doc.title.clone()
            .or_else(|| Some(hit.title.clone()).filter(|t| !t.is_empty()))
            .unwrap_or_else(|| "(no title)".into());
        out.push_str(&format!(
            "\n--- [{n}] {title}\nURL: {url}\n\n{body}\n",
            url  = doc.final_url,
            body = doc.text,
        ));
    }
    out.push_str("\nAnswer the question based only on these sources. \
                 Cite each non-trivial claim with `[N]`.");
    out
}

/// Pure helper: build the trailing Sources section. Numbered to match
/// `[N]` markers in the synthesis.
fn format_citations(sources: &[(SearchHit, FetchedDoc)]) -> String {
    let mut out = String::new();
    for (i, (hit, doc)) in sources.iter().enumerate() {
        let n = i + 1;
        let title = doc.title.clone()
            .or_else(|| Some(hit.title.clone()).filter(|t| !t.is_empty()))
            .unwrap_or_else(|| doc.final_url.clone());
        out.push_str(&format!("[{n}] {title} — {url}\n", url = doc.final_url));
    }
    out
}

/// In-memory test [`ResearchFetcher`]. Maps URL → canned `FetchedDoc`.
/// Lookups for unknown URLs return Err so the adapter exercises its
/// per-hit fail-soft path. Lives in the main module (not `mod tests`)
/// so other crates / future fixtures can reuse it.
pub struct StubFetcher {
    pub by_url: HashMap<String, FetchedDoc>,
}

impl StubFetcher {
    pub fn new() -> Self { Self { by_url: HashMap::new() } }
    pub fn with(mut self, url: impl Into<String>, doc: FetchedDoc) -> Self {
        self.by_url.insert(url.into(), doc);
        self
    }
}

#[async_trait]
impl ResearchFetcher for StubFetcher {
    async fn fetch(&self, url: &str) -> Result<FetchedDoc, String> {
        self.by_url.get(url)
            .cloned()
            .ok_or_else(|| format!("stub fetcher: no canned doc for {url}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::time::timeout;

    use crate::agent::instance::{Agent, AgentRegistry};
    use crate::agent::supervisor::{Supervisor, WorkerOutcome};
    use crate::providers::ModelProvider;
    use crate::tools::search::SearchBackend;
    use crate::types::{
        ChatMessage, GenerationOptions, GenerationResponse, MessageRole,
        ProviderId, TokenUsage,
    };

    // ── pure helpers ──────────────────────────────────────────────────

    fn hit(rank: usize, url: &str, title: &str) -> SearchHit {
        SearchHit {
            rank,
            title:   title.into(),
            url:     url.into(),
            snippet: String::new(),
            source:  "stub".into(),
        }
    }

    fn doc(url: &str, title: &str, body: &str) -> FetchedDoc {
        FetchedDoc {
            final_url: url.into(),
            title:     if title.is_empty() { None } else { Some(title.into()) },
            text:      body.into(),
        }
    }

    fn pair(rank: usize, url: &str, title: &str, body: &str)
        -> (SearchHit, FetchedDoc)
    {
        (hit(rank, url, title), doc(url, title, body))
    }

    #[test]
    fn build_synthesis_prompt_numbers_sources_one_indexed() {
        let sources = vec![
            pair(1, "https://a.example",  "First",  "Body of first source."),
            pair(2, "https://b.example",  "Second", "Body of second source."),
        ];
        let p = build_synthesis_prompt("What is X?", &sources);
        assert!(p.contains("Question:\nWhat is X?"));
        assert!(p.contains("--- [1] First"));
        assert!(p.contains("--- [2] Second"));
        assert!(p.contains("URL: https://a.example"));
        assert!(p.contains("Body of first source."));
        // Cite-with-N instruction is appended for the model.
        assert!(p.contains("Cite each non-trivial claim with `[N]`"));
    }

    #[test]
    fn build_synthesis_prompt_falls_back_to_hit_title_when_doc_title_missing() {
        let h = hit(1, "https://x", "Search-Result Title");
        let d = FetchedDoc { final_url: "https://x".into(), title: None, text: "body".into() };
        let p = build_synthesis_prompt("Q", &[(h, d)]);
        assert!(p.contains("--- [1] Search-Result Title"));
    }

    #[test]
    fn build_synthesis_prompt_uses_no_title_placeholder_when_both_missing() {
        let h = hit(1, "https://x", "");
        let d = FetchedDoc { final_url: "https://x".into(), title: None, text: "body".into() };
        let p = build_synthesis_prompt("Q", &[(h, d)]);
        assert!(p.contains("--- [1] (no title)"));
    }

    #[test]
    fn format_citations_numbers_one_indexed_with_url() {
        let sources = vec![
            pair(1, "https://a.example", "First Title",  "x"),
            pair(2, "https://b.example", "Second Title", "x"),
        ];
        let c = format_citations(&sources);
        let mut lines = c.lines();
        assert_eq!(lines.next(), Some("[1] First Title — https://a.example"));
        assert_eq!(lines.next(), Some("[2] Second Title — https://b.example"));
    }

    #[test]
    fn format_citations_uses_final_url_when_no_title() {
        let h = hit(1, "https://x", "");
        let d = FetchedDoc { final_url: "https://final.url".into(), title: None, text: "x".into() };
        let c = format_citations(&[(h, d)]);
        assert!(c.contains("[1] https://final.url — https://final.url"));
    }

    #[test]
    fn cap_chars_truncates_when_over_limit() {
        let s = "abcdefghij";
        assert_eq!(cap_chars(s, 5), "abcde");
    }

    #[test]
    fn cap_chars_returns_input_when_under_limit() {
        assert_eq!(cap_chars("abc", 10), "abc");
    }

    #[test]
    fn cap_chars_handles_multibyte_utf8() {
        // 4 emoji at 4 bytes each = 16 bytes but only 4 chars.
        // Truncating to 2 chars must not slice mid-codepoint.
        assert_eq!(cap_chars("🦀🦊🐺🦝", 2), "🦀🦊");
    }

    // ── stub backend + model for end-to-end tests ─────────────────────

    struct StubBackend {
        hits: Vec<SearchHit>,
        fail: Option<String>,
    }
    impl StubBackend {
        fn ok(hits: Vec<SearchHit>) -> Arc<Self> {
            Arc::new(Self { hits, fail: None })
        }
        fn err(msg: &str) -> Arc<Self> {
            Arc::new(Self { hits: vec![], fail: Some(msg.into()) })
        }
    }
    #[async_trait]
    impl SearchBackend for StubBackend {
        fn id(&self) -> &'static str { "stub" }
        fn requires_key(&self) -> bool { false }
        fn is_configured(&self) -> bool { true }
        async fn search(
            &self, _q: &str, _l: &SearchLimits, _u: &str,
        ) -> Result<Vec<SearchHit>, crate::MiraError> {
            if let Some(msg) = &self.fail {
                return Err(crate::MiraError::ToolError(msg.clone()));
            }
            Ok(self.hits.clone())
        }
    }

    /// Records every prompt sent to it and replies with a canned
    /// response. Used to assert the adapter built the prompt right.
    struct StubModel {
        canned:        String,
        seen_messages: Mutex<Vec<ChatMessage>>,
        fail:          Option<String>,
    }
    impl StubModel {
        fn ok(canned: &str) -> Arc<Self> {
            Arc::new(Self {
                canned: canned.into(),
                seen_messages: Mutex::new(Vec::new()),
                fail: None,
            })
        }
        fn err(msg: &str) -> Arc<Self> {
            Arc::new(Self {
                canned: String::new(),
                seen_messages: Mutex::new(Vec::new()),
                fail: Some(msg.into()),
            })
        }
    }
    #[async_trait]
    impl ModelProvider for StubModel {
        fn name(&self) -> &str { "stub-model" }
        async fn generate(
            &self, messages: &[ChatMessage], _opts: &GenerationOptions,
        ) -> Result<GenerationResponse, crate::MiraError> {
            self.seen_messages.lock().unwrap().extend(messages.iter().cloned());
            if let Some(msg) = &self.fail {
                return Err(crate::MiraError::ProviderError(msg.clone()));
            }
            Ok(GenerationResponse {
                content:     self.canned.clone(),
                tool_calls:  None,
                reasoning:   None,
                usage:       TokenUsage::default(),
                provider_id: ProviderId::Local("stub".into()),
                model_name:  "stub".into(),
                fallback: None,
            })
        }
        async fn health_check(&self) -> bool { true }
    }

    fn fixture() -> (Arc<Supervisor>, crate::agent::instance::AgentId, u8) {
        let reg = Arc::new(AgentRegistry::new());
        let root = reg.register(Agent::new_root());
        let (root_id, depth) = { let r = root.read().unwrap(); (r.id, r.depth) };
        let sup = Arc::new(Supervisor::new(reg));
        (sup, root_id, depth)
    }

    // ── end-to-end ────────────────────────────────────────────────────

    #[tokio::test]
    async fn end_to_end_synthesises_with_sources_section() {
        let backend = StubBackend::ok(vec![
            hit(1, "https://wiki.example/X", "Wiki: X"),
            hit(2, "https://blog.example/X", "Blog: X"),
        ]);
        let fetcher = Arc::new(
            StubFetcher::new()
                .with("https://wiki.example/X",
                      doc("https://wiki.example/X", "Wiki: X", "X is defined as foo."))
                .with("https://blog.example/X",
                      doc("https://blog.example/X", "Blog: X", "Some commentary on X."))
        );
        let model   = StubModel::ok("X is foo [1]. Bloggers say it's interesting [2].");

        let cfg = ResearchConfig::new(backend, fetcher, model.clone()).with_top_k(2);
        let exec = ResearchAdapter::new(cfg);
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.research", "What is X?", None,
            1.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(5), h.completion).await
            .expect("hung").unwrap();

        match outcome {
            WorkerOutcome::Complete(c) => {
                // Synthesised body present.
                assert!(c.result_summary.contains("X is foo [1]"), "body missing: {}", c.result_summary);
                // Sources section present, numbered, with URLs.
                assert!(c.result_summary.contains("## Sources"), "no sources header: {}", c.result_summary);
                assert!(c.result_summary.contains("[1] Wiki: X — https://wiki.example/X"),
                    "missing source 1: {}", c.result_summary);
                assert!(c.result_summary.contains("[2] Blog: X — https://blog.example/X"),
                    "missing source 2: {}", c.result_summary);
            }
            other => panic!("expected Complete, got {other:?}"),
        }

        // Double-check the model saw the right prompt structure.
        let seen = model.seen_messages.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].role, MessageRole::System);
        assert_eq!(seen[1].role, MessageRole::User);
        assert!(seen[1].content.contains("Question:\nWhat is X?"),
            "user prompt missing question: {}", seen[1].content);
        assert!(seen[1].content.contains("X is defined as foo."),
            "user prompt missing source body: {}", seen[1].content);
    }

    #[tokio::test]
    async fn empty_search_results_yield_failed() {
        let backend = StubBackend::ok(vec![]);
        let fetcher = Arc::new(StubFetcher::new());
        let model   = StubModel::ok("(unused)");
        let cfg = ResearchConfig::new(backend, fetcher, model);
        let exec = ResearchAdapter::new(cfg);
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.research", "obscure question", None,
            1.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(5), h.completion).await
            .expect("hung").unwrap();
        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(f.error.contains("no search results"), "got: {}", f.error);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_backend_failure_yields_failed_with_backend_id() {
        let backend = StubBackend::err("upstream 500");
        let fetcher = Arc::new(StubFetcher::new());
        let model   = StubModel::ok("(unused)");
        let cfg = ResearchConfig::new(backend, fetcher, model);
        let exec = ResearchAdapter::new(cfg);
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.research", "x", None,
            1.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(5), h.completion).await
            .expect("hung").unwrap();
        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(f.error.contains("search backend stub failed"), "got: {}", f.error);
                assert!(f.error.contains("upstream 500"),               "got: {}", f.error);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn all_fetches_failing_yields_failed_not_partial() {
        // Backend returns 2 hits; fetcher knows zero of them. Should
        // fail rather than synthesising from nothing.
        let backend = StubBackend::ok(vec![
            hit(1, "https://nope.example/1", "n1"),
            hit(2, "https://nope.example/2", "n2"),
        ]);
        let fetcher = Arc::new(StubFetcher::new()); // empty map → all errs
        let model   = StubModel::ok("(unused)");
        let cfg = ResearchConfig::new(backend, fetcher, model);
        let exec = ResearchAdapter::new(cfg);
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.research", "x", None,
            1.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(5), h.completion).await
            .expect("hung").unwrap();
        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(f.error.contains("all 2 candidate sources failed"), "got: {}", f.error);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn partial_fetch_failure_still_synthesises_from_survivors() {
        // 2 hits; only one fetchable. Adapter should drop the dead one,
        // synthesise from the survivor, and still return Complete.
        let backend = StubBackend::ok(vec![
            hit(1, "https://alive.example/", "alive"),
            hit(2, "https://dead.example/",  "dead"),
        ]);
        let fetcher = Arc::new(
            StubFetcher::new().with("https://alive.example/",
                doc("https://alive.example/", "Alive Doc", "Alive content."))
        );
        let model = StubModel::ok("Synthesis from one source [1].");

        let cfg = ResearchConfig::new(backend, fetcher, model);
        let exec = ResearchAdapter::new(cfg);
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.research", "x", None,
            1.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(5), h.completion).await
            .expect("hung").unwrap();
        match outcome {
            WorkerOutcome::Complete(c) => {
                assert!(c.result_summary.contains("Synthesis from one source [1]"));
                // Only the alive source appears in the citations.
                assert!(c.result_summary.contains("[1] Alive Doc — https://alive.example/"),
                    "missing alive source: {}", c.result_summary);
                assert!(!c.result_summary.contains("dead.example"),
                    "dead source leaked into citations: {}", c.result_summary);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_failure_yields_failed_with_model_name() {
        let backend = StubBackend::ok(vec![hit(1, "https://x.example/", "X")]);
        let fetcher = Arc::new(StubFetcher::new()
            .with("https://x.example/", doc("https://x.example/", "X", "body")));
        let model   = StubModel::err("rate limit");
        let cfg = ResearchConfig::new(backend, fetcher, model);
        let exec = ResearchAdapter::new(cfg);
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.research", "x", None,
            1.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(5), h.completion).await
            .expect("hung").unwrap();
        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(f.error.contains("synthesis model stub-model failed"),
                    "got: {}", f.error);
                assert!(f.error.contains("rate limit"), "got: {}", f.error);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_question_yields_failed() {
        let backend = StubBackend::ok(vec![]);
        let fetcher = Arc::new(StubFetcher::new());
        let model   = StubModel::ok("(unused)");
        let cfg = ResearchConfig::new(backend, fetcher, model);
        let exec = ResearchAdapter::new(cfg);
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.research", "   ", None,
            1.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(5), h.completion).await
            .expect("hung").unwrap();
        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(f.error.contains("empty research question"), "got: {}", f.error);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn top_k_caps_the_number_of_sources_kept() {
        let backend = StubBackend::ok(vec![
            hit(1, "https://a/", "A"),
            hit(2, "https://b/", "B"),
            hit(3, "https://c/", "C"),
        ]);
        let fetcher = Arc::new(
            StubFetcher::new()
                .with("https://a/", doc("https://a/", "A", "x"))
                .with("https://b/", doc("https://b/", "B", "x"))
                .with("https://c/", doc("https://c/", "C", "x"))
        );
        let model = StubModel::ok("ans");
        let cfg = ResearchConfig::new(backend, fetcher, model.clone()).with_top_k(2);
        let exec = ResearchAdapter::new(cfg);
        let (sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.test.research", "x", None,
            1.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(5), h.completion).await
            .expect("hung").unwrap();
        match outcome {
            WorkerOutcome::Complete(c) => {
                // Only 2 sources should make it into the citations
                // section, even though the backend returned 3.
                assert!( c.result_summary.contains("[1] A"));
                assert!( c.result_summary.contains("[2] B"));
                assert!(!c.result_summary.contains("[3]"),
                    "third source leaked: {}", c.result_summary);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }
}
