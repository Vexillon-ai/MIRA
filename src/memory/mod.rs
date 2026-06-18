// SPDX-License-Identifier: AGPL-3.0-or-later

// src/memory/mod.rs

//! Memory system for MIRA
//!
//! Provides persistent, hierarchical memory storage with:
//! - SQLite backend
//! - Auto-categorization (Fact, Preference, Skill, Relationship, Project)
//! - Tagged memories for flexible organization
//! - Keyword-based retrieval
//! - Semantic similarity search (vector-based embeddings)
//!
//! # Embedding providers
//! Configured via `memory.embedding` in `mira_config.json`:
//!
//! | `provider`     | Backend                         | Requires        |
//! |----------------|---------------------------------|-----------------|
//! | `"internal"`   | fastembed (ONNX, built-in)      | nothing         |
//! | `"lmstudio"`   | LM Studio `/v1/embeddings`      | LM Studio server|
//! | `"ollama"`     | Ollama `/v1/embeddings`         | Ollama server   |
//! | `"openai"`     | OpenAI embeddings API           | API key         |
//! | `"openrouter"` | OpenRouter embeddings endpoint  | API key         |

pub mod auto_extract;
pub mod categorizer;
pub mod fastembed_provider;
pub mod graph;
pub mod rollup;
pub mod semantic;
pub mod storage;
pub mod vector_backend;

pub use auto_extract::HeuristicExtractor;
pub use semantic::{cosine_similarity, EmbeddingProvider, LmStudioEmbeddingProvider, SemanticMemorySystem};
pub use storage::{Category, ListSort, MemoryItem, MemorySource, MemoryStorage, Scope};
pub use vector_backend::{SqliteVectorBackend, VectorStoreBackend};

use std::path::PathBuf;
use std::sync::Mutex as StdMutex;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

// High-level memory system interface
pub struct MemorySystem {
    // SQLite storage — wrapped in a std Mutex so MemorySystem is Sync across
    // async task boundaries (rusqlite::Connection contains RefCell internally).
    storage: StdMutex<MemoryStorage>,
    categorizer: categorizer::MemoryCategorizer,
    max_results: usize,
    // Optional semantic memory for vector search (Mutex allows async mutation)
    semantic_memory: Option<Mutex<SemanticMemorySystem>>,
    // Knowledge-graph memory active (`memory.graph.enabled`). When set, the
    // per-turn extractor also writes triples and retrieval augments context
    // with graph edges. Off for the bare constructors.
    graph_enabled: bool,
    // Recency re-rank tuning (`memory.recency`); see [`recency_blended_score`].
    // Defaults to the module constants; overridden from config in
    // [`Self::new_from_embedding_config`].
    recency_weight: f32,
    recency_half_life_days: f32,
}

// Fallback recency tuning for memory systems built without a full config
// (tests, keyword-only constructors). The production path overrides these
// from `memory.recency` in [`MemorySystem::new_from_embedding_config`].
const DEFAULT_RECENCY_WEIGHT: f32 = 0.25;
const DEFAULT_RECENCY_HALF_LIFE_DAYS: f32 = 30.0;

// Blend semantic similarity with an age-based recency boost. `weight` is the
// recency term's share (`0.0` = pure similarity); `half_life_days` controls
// how fast the boost decays from `created_at`. This is what lets
// recently-formed memories surface in recall instead of only the
// most-reinforced ones (the cause of stale, repetitive check-ins).
fn recency_blended_score(
    item:           &MemoryItem,
    now:            chrono::DateTime<chrono::Utc>,
    weight:         f32,
    half_life_days: f32,
) -> f32 {
    let age_days = (now - item.created_at).num_seconds().max(0) as f32 / 86_400.0;
    let hl       = if half_life_days > 0.0 { half_life_days } else { DEFAULT_RECENCY_HALF_LIFE_DAYS };
    let recency  = 2f32.powf(-age_days / hl);
    (1.0 - weight) * item.relevance_score + weight * recency
}

impl MemorySystem {
    // Create a new memory system backed by SQLite at `db_path` for the default user.
    pub fn new(db_path: PathBuf) -> Result<Self, crate::MiraError> {
        info!("Initializing memory system at {:?}", db_path);
        let storage = MemoryStorage::new(&db_path)?;
        Ok(Self {
            storage: StdMutex::new(storage),
            categorizer: categorizer::MemoryCategorizer::new(),
            max_results: 10,
            semantic_memory: None,
            graph_enabled: false,
            recency_weight: DEFAULT_RECENCY_WEIGHT,
            recency_half_life_days: DEFAULT_RECENCY_HALF_LIFE_DAYS,
        })
    }

    // Create memory system for a specific user.
    // When `per_user_isolation` is true, `raw_user_id` scopes all queries.
    // When `share_across_channels` is true, the channel prefix is stripped so
    // "tg-12345" and "cli-12345" resolve to the same underlying user_id.
    pub fn new_for_user(
        data_dir: &std::path::Path,
        raw_user_id: &str,
        per_user_isolation: bool,
        share_across_channels: bool,
    ) -> Result<Self, crate::MiraError> {
        let effective_user_id = if per_user_isolation {
            if share_across_channels {
                raw_user_id.splitn(2, '-').nth(1).unwrap_or(raw_user_id).to_string()
            } else {
                raw_user_id.to_string()
            }
        } else {
            "default".to_string()
        };

        let db_path = data_dir.join(format!("memory_{}.db", effective_user_id));
        info!("Memory DB for user '{}' → {:?}", effective_user_id, db_path);
        let storage = MemoryStorage::new_for_user(&db_path, &effective_user_id)?;
        Ok(Self {
            storage: StdMutex::new(storage),
            categorizer: categorizer::MemoryCategorizer::new(),
            max_results: 10,
            semantic_memory: None,
            graph_enabled: false,
            recency_weight: DEFAULT_RECENCY_WEIGHT,
            recency_half_life_days: DEFAULT_RECENCY_HALF_LIFE_DAYS,
        })
    }

    // Create a memory system with semantic (vector) search, using the full
    // [`crate::config::EmbeddingConfig`] from `mira_config.json`.
    //     // When `provider = "internal"` the fastembed model is downloaded on first
    // use to `model_cache_dir`.  All other providers make HTTP requests to the
    // configured `provider_url`.
    //     // This is an async function because the `"internal"` provider init is
    // offloaded to a blocking thread via [`tokio::task::spawn_blocking`].
    pub async fn new_from_embedding_config(
        db_path:   PathBuf,
        config:    &crate::config::MemoryConfig,
    ) -> Result<Self, crate::MiraError> {
        use crate::config::resolve_state_path;
        use fastembed_provider::FastEmbedProvider;
        use semantic::HttpEmbeddingProvider;

        let emb    = &config.embedding;
        let thresh = config.similarity_threshold;
        let cache_size = config.embedding_cache_size;

        info!(
            "Initializing memory system: provider='{}' model='{}' threshold={}",
            emb.provider, emb.model, thresh
        );

        let provider: Box<dyn EmbeddingProvider> = match emb.provider.as_str() {
            "internal" => {
                // Self-heal: with `panic = "abort"` in the release profile,
                // a missed dlopen of libonnxruntime takes the whole
                // process down. Probe the lib up-front and fall back to
                // the noop provider when it isn't present, so the server
                // boots and the operator can install the dep via
                // `POST /api/admin/deps/onnxruntime/install` (or the
                // Settings page dialog) without an out-of-band rescue.
                if !crate::install::deps::is_onnxruntime_available() {
                    tracing::warn!(
                        "embedding provider 'internal' selected but libonnxruntime is not \
                         available (no ORT_DYLIB_PATH and ~/.mira/deps/onnxruntime/ missing). \
                         Falling back to noop embeddings — semantic search is DISABLED. \
                         Install via: POST /api/admin/deps/onnxruntime/install \
                         (or run `mira deps install`), then restart."
                    );
                    let dim = config.embedding_dim;
                    let label = format!("internal:{}", emb.model);
                    Box::new(semantic::NoopEmbeddingProvider::new(dim, &label))
                } else {
                    let cache_dir = resolve_state_path(&emb.model_cache_dir);
                    let model_name = emb.model.clone();

                    // Model loading is blocking — run it off the Tokio thread pool.
                    eprintln!(
                        "  Embedding: loading fastembed model '{}' (first run may download ~24 MB)…",
                        model_name
                    );
                    let provider = tokio::task::spawn_blocking(move || {
                        FastEmbedProvider::try_new(&model_name, &cache_dir, cache_size)
                    })
                    .await
                    .map_err(|e| crate::MiraError::MemoryError(
                        format!("fastembed thread panicked: {}", e)
                    ))?
                    .map_err(|e| crate::MiraError::MemoryError(e))?;

                    Box::new(provider)
                }
            }

            // All HTTP-based providers share the same OpenAI-compatible API surface.
            provider_name => {
                let url = emb.provider_url.clone().unwrap_or_else(|| {
                    match provider_name {
                        "ollama"     => "http://localhost:11434/v1".to_string(),
                        "openai"     => "https://api.openai.com/v1".to_string(),
                        "openrouter" => "https://openrouter.ai/api/v1".to_string(),
                        _            => "http://localhost:1234/v1".to_string(), // lmstudio default
                    }
                });
                info!("Using HTTP embedding provider: {} @ {}", emb.model, url);
                Box::new(HttpEmbeddingProvider::with_cache_size(url, emb.model.clone(), cache_size))
            }
        };

        let mut sys = Self::build_with_provider(db_path, provider, thresh)?;
        sys.graph_enabled = config.graph.enabled;
        sys.recency_weight = config.recency.weight;
        sys.recency_half_life_days = config.recency.half_life_days;
        Ok(sys)
    }

    // Create a memory system with LM Studio HTTP embeddings.
    //     // Kept for backward compatibility. Prefer [`Self::new_from_embedding_config`]
    // for new code.
    pub fn new_with_semantic(
        db_path:         PathBuf,
        lmstudio_url:    String,
        embedding_model: String,
    ) -> Result<Self, crate::MiraError> {
        info!("Initializing memory system with LM Studio embeddings @ {}", lmstudio_url);
        let provider = Box::new(
            semantic::HttpEmbeddingProvider::new(lmstudio_url, embedding_model)
        );
        Self::build_with_provider(db_path, provider, 0.6)
    }

    // Internal: wire a concrete provider into the full memory stack.
    fn build_with_provider(
        db_path:   PathBuf,
        provider:  Box<dyn EmbeddingProvider>,
        threshold: f32,
    ) -> Result<Self, crate::MiraError> {
        let storage = MemoryStorage::new(&db_path)?;
        let backend = SqliteVectorBackend::new(&db_path)
            .map_err(|e| crate::MiraError::MemoryError(e.to_string()))?;
        let semantic = SemanticMemorySystem::new(provider, threshold)
            .with_sqlite_backend(backend);

        Ok(Self {
            storage: StdMutex::new(storage),
            categorizer: categorizer::MemoryCategorizer::new(),
            max_results: 10,
            semantic_memory: Some(Mutex::new(semantic)),
            graph_enabled: false, // set by new_from_embedding_config from config
            recency_weight: DEFAULT_RECENCY_WEIGHT, // overridden from config below
            recency_half_life_days: DEFAULT_RECENCY_HALF_LIFE_DAYS,
        })
    }

    // Store a memory with an explicit category and tags.
    // Also adds the memory to the semantic vector store if enabled.
    pub async fn store(
        &self,
        content: String,
        category: Category,
        tags: Vec<String>,
    ) -> Result<u64, crate::MiraError> {
        let id = self.storage.lock().unwrap().store(content.clone(), category.clone(), tags, None)?;
        debug!("Stored memory {}: {}", id, content);
        info!("[Memory stored: {}]", category);

        // Add to semantic (vector) store so similarity search finds it.
        if let Some(ref sem) = self.semantic_memory {
            let mut sem = sem.lock().await;
            if let Err(e) = sem.add_memory(id, content, category.as_str().to_string()).await {
                warn!("Failed to add memory {} to semantic store: {}", id, e);
            }
        }

        Ok(id)
    }

    // Store a memory with automatic categorization.
    pub async fn store_auto(&self, content: String) -> Result<u64, crate::MiraError> {
        let category = self.categorizer.categorize(&content);
        self.store(content, category, vec![]).await
    }

    // Store a memory with an explicit source annotation.
    pub async fn store_with_source(
        &self,
        content: String,
        category: Category,
        tags: Vec<String>,
        source: MemorySource,
    ) -> Result<u64, crate::MiraError> {
        let id = self.storage.lock().unwrap().store(content.clone(), category.clone(), tags, Some(source.clone()))?;
        debug!("Stored memory {} from {:?}: {}", id, source, content);

        if let Some(ref sem) = self.semantic_memory {
            let mut sem = sem.lock().await;
            if let Err(e) = sem.add_memory(id, content, category.as_str().to_string()).await {
                warn!("Failed to add memory {} to semantic store: {}", id, e);
            }
        }

        Ok(id)
    }

    // Keyword search (SQL LIKE) — synchronous, no API calls.
    pub fn search(&self, query: &str) -> Vec<MemoryItem> {
        match self.storage.lock().unwrap().search(query) {
            Ok(items) => items.into_iter().take(self.max_results).collect(),
            Err(e) => {
                debug!("Search failed: {}", e);
                vec![]
            }
        }
    }

    // Embed arbitrary text through the currently-configured embedding
    // provider. Returns `None` when semantic search is disabled (keyword-only
    // mode), so callers outside the memory system — e.g. the transcript
    // indexer — can reuse the loaded model without re-instantiating it.
    pub async fn embed(&self, text: &str) -> Option<Vec<f32>> {
        let sem = self.semantic_memory.as_ref()?;
        let sem = sem.lock().await;
        // SemanticMemorySystem doesn't expose its provider; use `search` as a
        // side-effect-free embed is not available. Instead we depend on the
        // provider's embed directly: expose it here via `embed_through`.
        sem.embed_raw(text).await.ok()
    }

    // Dimensionality of the vectors produced by the active embedding
    // provider. `None` when semantic search is disabled.
    pub async fn embedding_dim(&self) -> Option<usize> {
        let sem = self.semantic_memory.as_ref()?;
        let sem = sem.lock().await;
        Some(sem.embedding_dim())
    }

    // Human-readable label for the active embedding model — stored alongside
    // each transcript vector so the indexer can detect model switches.
    pub async fn embedding_model_label(&self) -> Option<String> {
        let sem = self.semantic_memory.as_ref()?;
        let sem = sem.lock().await;
        Some(sem.embedding_model_label())
    }

    // Semantic (vector) search — async, requires LM Studio embeddings.
    // Falls back to keyword search with a dummy score when semantic is disabled.
    pub async fn semantic_search(&self, query: &str, top_k: usize) -> Result<Vec<(u64, String, f32)>, crate::MiraError> {
        if let Some(ref sem) = self.semantic_memory {
            let sem = sem.lock().await;
            sem.search(query, top_k).await
                .map_err(|e| crate::MiraError::ProviderError(format!("Semantic search failed: {}", e)))
        } else {
            warn!("Semantic search not enabled, falling back to keyword search");
            Ok(self.search(query)
                .into_iter()
                .map(|m| (m.id, m.content, 1.0f32))
                .collect())
        }
    }

    // Get memories filtered by category.
    pub fn get_by_category(&self, category: &Category) -> Vec<MemoryItem> {
        match self.storage.lock().unwrap().get_by_category(category) {
            Ok(items) => items.into_iter().take(self.max_results).collect(),
            Err(e) => {
                debug!("Get by category failed: {}", e);
                vec![]
            }
        }
    }

    // Retrieve memories relevant to a query for context injection.
    pub fn retrieve_for_context(&self, query: &str) -> Vec<MemoryItem> {
        self.search(query)
    }

    // ── AgentCore helpers ─────────────────────────────────────────────────────

    // Keyword-only memory system, no embedding provider.
    // Useful for tests and environments where semantic search is unavailable.
    pub fn new_keyword_only(db_path: PathBuf) -> Result<Self, crate::MiraError> {
        Self::new(db_path)
    }

    // Visibility-aware retrieval for agent context injection.
    //     // Returns memories the caller (`user_id` + their `group_ids`) can see,
    // ranked by semantic similarity when embeddings are available, falling
    // back to keyword search ordered by decay-aware strength. Every surfaced
    // memory is reinforced (access_count++, strength nudged toward 1.0,
    // last_reinforced = now) so frequently-used facts don't decay out.
    pub async fn search_visible_for_context(
        &self,
        query:      &str,
        user_id:    &str,
        group_ids:  &[String],
        top_k:      usize,
    ) -> Result<Vec<MemoryItem>, crate::MiraError> {
        // Semantic first (if enabled), gated by visibility. We call semantic_search
        // for the candidate ids/scores, then filter each through get_visible so
        // rows from other users/groups are dropped.
        if let Some(ref sem) = self.semantic_memory {
            let sem = sem.lock().await;
            if let Ok(hits) = sem.search(query, top_k * 3).await {
                // Pull all visible candidates first (3× top_k), then re-rank by
                // a similarity+recency blend before trimming to top_k — so the
                // recency boost can promote a fresh memory the pure-similarity
                // top_k would have dropped.
                let mut items: Vec<MemoryItem> = hits.into_iter()
                    .filter_map(|(id, _c, score)| {
                        let mut item = self.storage.lock().unwrap()
                            .get_visible(id, user_id, group_ids).ok()??;
                        item.relevance_score = score;
                        Some(item)
                    })
                    .collect();
                // `semantic_search` already gated on `similarity_threshold`, so
                // any survivor is relevant — use the semantic results whenever
                // it returned something (was a hardcoded 0.55 floor that forced
                // a keyword fallback for low-similarity aggregation queries).
                if !items.is_empty() {
                    // Recency-aware re-rank, then trim to the requested top_k.
                    let now = chrono::Utc::now();
                    let (w, hl) = (self.recency_weight, self.recency_half_life_days);
                    items.sort_by(|a, b| {
                        recency_blended_score(b, now, w, hl)
                            .total_cmp(&recency_blended_score(a, now, w, hl))
                    });
                    items.truncate(top_k);
                    // Topic-grouped expansion: aggregation/counting needs the
                    // complete set of a category, but semantic top-k only
                    // surfaces the most-similar members. Pull every sibling
                    // sharing a hit's `topic:` tag so the model can count/total
                    // correctly instead of from a partial sample.
                    let items = self.expand_by_topic(items, user_id);
                    for item in &items { self.reinforce(item.id, user_id); }
                    return Ok(items);
                }
            }
        }

        // Keyword fallback — already ordered by decay-aware strength.
        let items: Vec<MemoryItem> = self.storage.lock().unwrap()
            .search_visible(query, user_id, group_ids)?
            .into_iter().take(top_k).collect();
        for item in &items { self.reinforce(item.id, user_id); }
        Ok(items)
    }

    // Topic-grouped retrieval expansion. Given the semantic hits, pull every
    // sibling memory sharing their `topic:` tags so aggregation/counting sees
    // the *complete* category, not just the top-k most-similar members. The
    // semantic hits keep their order (most relevant first); topic siblings are
    // appended and deduped by id. Bounded by `MAX_CONTEXT_ITEMS` so an
    // over-broad topic can't blow the prompt budget. Non-fatal: a storage
    // error just returns the original hits.
    fn expand_by_topic(&self, mut items: Vec<MemoryItem>, user_id: &str) -> Vec<MemoryItem> {
        // Per-prompt ceiling: enough to hold a full personal topic (expenses,
        // plants, trips rarely exceed a couple dozen) without unbounded bloat.
        const MAX_CONTEXT_ITEMS: usize = 60;

        let mut topics: Vec<String> = Vec::new();
        for it in &items {
            for t in &it.tags {
                if let Some(slug) = t.strip_prefix("topic:") {
                    if !slug.is_empty() && !topics.iter().any(|s| s == slug) {
                        topics.push(slug.to_string());
                    }
                }
            }
        }
        if topics.is_empty() {
            return items;
        }

        let siblings = match self.storage.lock().unwrap().list_by_topic_tags(user_id, &topics) {
            Ok(v)  => v,
            Err(e) => { debug!("topic expansion failed (non-fatal): {}", e); return items; }
        };

        let mut seen: std::collections::HashSet<u64> = items.iter().map(|i| i.id).collect();
        for mut sib in siblings {
            if items.len() >= MAX_CONTEXT_ITEMS { break; }
            if seen.insert(sib.id) {
                // Topic siblings weren't semantically scored against this query;
                // zero their score so any score-based re-sort keeps them after
                // the genuine semantic hits.
                sib.relevance_score = 0.0;
                items.push(sib);
            }
        }
        items
    }

    // Extract memories from a conversation turn and store them.
    //     // Returns the number of memories stored. Errors are non-fatal — callers
    // should log and continue rather than propagate.
    pub async fn auto_extract_and_store(
        &self,
        text:    &str,
        user_id: &str,
        channel: &str,
    ) -> Result<usize, crate::MiraError> {
        let extractor = auto_extract::HeuristicExtractor::new();
        let candidates = extractor.extract(text);

        let mut count = 0;
        for candidate in candidates {
            self.store_auto(candidate.content).await?;
            count += 1;
        }

        let _ = (user_id, channel); // tagging not yet implemented

        debug!("auto_extract_and_store: {} memories for user '{}' channel '{}'", count, user_id, channel);
        Ok(count)
    }

    // Run the structured LLM extractor over one turn, then persist each
    // candidate through the conflict-aware path.
    //     // For every candidate, the existing memory tagged `entity:<entity>`
    // for this user is looked up. If found, [`Self::supersede`] replaces
    // it with the new content — that's how we stop duplicate facts from
    // accumulating on every turn that mentions the same topic. If not,
    // the candidate is stored fresh with `AutoExtracted` provenance.
    //     // All memory rows are `scope=User`, owned by `user_id`, with tags
    // `["auto", "entity:<name>", "confidence:<tier>"]` so the review
    // surface can filter them and the model can reason about trust.
    //     // Returns the number of candidates persisted (either new or as a
    // supersede). Non-fatal: provider failures, parse errors, and per-row
    // write errors all log and continue, so an extractor hiccup can
    // never block the chat turn that already streamed to the user.
    #[allow(clippy::too_many_arguments)]
    pub async fn auto_extract_llm_and_store(
        &self,
        provider:           &std::sync::Arc<dyn crate::providers::ModelProvider>,
        user_msg:           &str,
        assistant_msg:      &str,
        user_id:            &str,
        channel:            &str,
        conversation_id:    Option<&str>,
        user_message_id:    Option<&str>,
        allowed_categories: &[String],
        min_confidence:     auto_extract::ConfidenceTier,
    ) -> usize {
        use auto_extract::LlmMemoryExtractor;

        let extractor = LlmMemoryExtractor::new();
        let candidates = extractor.extract(
            provider, user_msg, assistant_msg, allowed_categories, min_confidence,
        ).await;
        if candidates.is_empty() {
            return 0;
        }

        let mut written = 0usize;
        for c in candidates {
            // Map the textual category to the storage enum. An unknown
            // category here would only happen if the LLM emitted something
            // outside the allowed set; the extractor already filters those,
            // so default to Fact just as a belt-and-braces fallback.
            let category = match c.category.as_str() {
                "fact"         => Category::Fact,
                "preference"   => Category::Preference,
                "skill"        => Category::Skill,
                "relationship" => Category::Relationship,
                "project"      => Category::Project,
                _              => Category::Fact,
            };
            let mut tags = vec![
                "auto".to_owned(),
                format!("entity:{}", c.entity),
                format!("confidence:{}", c.confidence.to_ascii_lowercase()),
            ];
            // Coarse topic tag (when present) powers topic-grouped retrieval:
            // siblings sharing a topic are pulled together for aggregation.
            if !c.topic.is_empty() {
                tags.push(format!("topic:{}", c.topic));
            }

            // Conflict check — does this user already have a memory for
            // this entity? If yes, supersede; if no, insert.
            let existing = self.storage.lock().unwrap()
                .find_by_entity_tag(user_id, &c.entity)
                .ok()
                .flatten();

            let result = if let Some(old) = existing {
                // Supersede preserves audit history: the old row stays but
                // points to the new one via `superseded_by`.
                debug!(
                    "auto_extract_llm: superseding memory #{} (entity='{}') for user='{}'",
                    old.id, c.entity, user_id,
                );
                self.supersede(
                    old.id,
                    c.content.clone(),
                    category.clone(),
                    tags.clone(),
                    Some(MemorySource::AutoExtracted),
                    user_id,
                ).await.map(|_| ())
            } else {
                debug!(
                    "auto_extract_llm: storing new memory (entity='{}') for user='{}'",
                    c.entity, user_id,
                );
                self.store_scoped(
                    c.content.clone(),
                    category,
                    tags,
                    Some(MemorySource::AutoExtracted),
                    Scope::User,
                    Some(user_id),
                    user_id,
                    &[user_id.to_owned()],
                    Some(channel),
                    conversation_id,
                    user_message_id,
                ).await.map(|_| ())
            };

            match result {
                Ok(()) => written += 1,
                Err(e) => warn!(
                    "auto_extract_llm: write failed for entity='{}': {}", c.entity, e,
                ),
            }
        }

        info!(
            "auto_extract_llm: persisted {} memory updates for user '{}' (channel '{}')",
            written, user_id, channel,
        );
        written
    }

    // Extract typed triples from one turn and persist them to the knowledge
    // graph (`design-docs/graph-memory.md`). Resolves each triple's subject/object to
    // `kg_entities` (creating as needed) and inserts a `kg_edges` row.
    //     // Caller-gated by `memory.graph.enabled` — this runs alongside, not
    // instead of, flat extraction during  so the two can be A/B'd.
    // `event_at` (unix-ms) is the turn's date when known. Non-fatal: per-triple
    // errors log and continue. Returns the number of edges stored.
    pub async fn graph_extract_and_store(
        &self,
        provider:      &std::sync::Arc<dyn crate::providers::ModelProvider>,
        user_msg:      &str,
        assistant_msg: &str,
        user_id:       &str,
        event_at:      Option<i64>,
    ) -> usize {
        let triples = graph::extract_triples(provider, user_msg, assistant_msg, event_at).await;
        if triples.is_empty() {
            return 0;
        }
        let mut stored = 0usize;
        for t in triples {
            let store = self.storage.lock().unwrap();
            let subject_id = match store.graph_ensure_entity(user_id, &t.subject, &t.subject_type) {
                Ok(id) => id,
                Err(e) => { debug!("graph: subject resolve failed for '{}': {}", t.subject, e); continue; }
            };
            let object_id = match &t.object {
                Some(obj) => store.graph_ensure_entity(user_id, obj, "thing").ok(),
                None      => None,
            };
            match store.graph_add_edge(
                user_id, subject_id, &t.predicate, object_id,
                t.value_num, t.value_unit.as_deref(), &t.fact_text, t.event_at,
                Some("auto_extract"),
            ) {
                Ok(_)  => stored += 1,
                Err(e) => debug!("graph: edge write failed: {}", e),
            }
        }
        if stored > 0 {
            debug!("graph: stored {} edges for user '{}'", stored, user_id);
        }
        stored
    }

    // Retrieve knowledge-graph context for a query: match the question's words
    // against entity names + grouping types, then return the **complete** live
    // edge set for every matched entity. This is the exact-membership input
    // aggregation needs ("how many plants" → all `plant`-typed entities' edges,
    // not a top-k sample). Empty when graph is disabled or nothing matches.
    pub fn graph_context_for_query(&self, query: &str, user_id: &str, limit: usize) -> Vec<String> {
        if !self.graph_enabled {
            return vec![];
        }
        let stems = graph::query_stems(query);
        if stems.is_empty() {
            return vec![];
        }
        let store = self.storage.lock().unwrap();
        let entities = match store.graph_entities_for_user(user_id, 3000) {
            Ok(e)  => e,
            Err(e) => { debug!("graph retrieval: entity scan failed: {}", e); return vec![]; }
        };
        // Match on the grouping TYPE (so "how many plants" pulls every plant) and
        // on the entity NAME (so a named subject is found directly).
        let matched: Vec<i64> = entities.into_iter()
            .filter(|(_, name, etype)| {
                graph::text_matches_query(etype, &stems) || graph::text_matches_query(name, &stems)
            })
            .map(|(id, _, _)| id)
            .collect();
        if matched.is_empty() {
            return vec![];
        }
        let result = store.graph_context_for_subjects(user_id, &matched, limit);
        // Phase D — always-on reinforcement tracking: bump access_count +
        // last_reinforced for every edge of the matched subjects, so the
        // nightly importance consolidator has signal to score against. No-op
        // when Phase D's scoring isn't enabled (it's just unused columns).
        if let Err(e) = store.graph_track_access(user_id, &matched) {
            debug!("graph retrieval: access tracking failed (non-fatal): {}", e);
        }
        match result {
            Ok(lines) => lines,
            Err(e)    => { debug!("graph retrieval: edge gather failed: {}", e); vec![] }
        }
    }

    // Sleep-like consolidator — Phase C: resolve contradictions in single-
    // valued predicates (`works_at`, `lives_in`, …) by closing older edges'
    // `valid_to`. Deterministic, no LLM. Returns `(groups_resolved,
    // edges_closed)`. See `design-docs/memory-research-2026.md` §5 (Direction 1,
    // Phase C — newly ordered first per the implementation plan, ahead of
    // Phase A entity dedup).
    pub fn consolidate_contradictions(&self, user_id: &str) -> (usize, usize) {
        match self.storage.lock().unwrap().graph_consolidate_contradictions(user_id) {
            Ok(pair) => pair,
            Err(e)   => { warn!("consolidate_contradictions (non-fatal): {}", e); (0, 0) }
        }
    }

    // Sleep-like consolidator — Phase A: merge near-duplicate entities within
    // the same `entity_type` via strict-token-subset + size-ratio rule (high
    // precision, no LLM). Returns `(entities_merged, edges_repointed)`. See
    // `design-docs/memory-research-2026.md` §5 (Direction 1, Phase A).
    pub fn consolidate_entities(&self, user_id: &str, threshold: f64) -> (usize, usize) {
        match self.storage.lock().unwrap().graph_consolidate_entities(user_id, threshold) {
            Ok(pair) => pair,
            Err(e)   => { warn!("consolidate_entities (non-fatal): {}", e); (0, 0) }
        }
    }

    // Sleep-like consolidator — Phase D: compute importance scores for every
    // live edge using `ln(1 + access_count) × exp(-age_days / half_life)`.
    // The retrieval path already orders by `importance DESC`, so scoring
    // nightly biases context toward frequently-reinforced + recent facts.
    // Returns the number of edges scored. See `design-docs/memory-research-2026.md`
    // §5 (Direction 1, Phase D).
    pub fn consolidate_importance(&self, user_id: &str, half_life_days: f64) -> usize {
        match self.storage.lock().unwrap().graph_consolidate_importance(user_id, half_life_days) {
            Ok(n)    => n,
            Err(e)   => { warn!("consolidate_importance (non-fatal): {}", e); 0 }
        }
    }

    // (entity_count, edge_count) for `user_id` in the knowledge graph.
    pub fn graph_counts(&self, user_id: &str) -> (i64, i64) {
        let s = self.storage.lock().unwrap();
        (s.graph_entity_count(user_id).unwrap_or(0), s.graph_edge_count(user_id).unwrap_or(0))
    }

    // Human-readable sample of stored graph edges (diagnostic).
    pub fn graph_sample(&self, user_id: &str, limit: usize) -> Vec<String> {
        self.storage.lock().unwrap().graph_sample_edges(user_id, limit).unwrap_or_default()
    }

    // Delete a memory by ID.
    pub async fn delete(&self, id: u64) -> Result<bool, crate::MiraError> {
        let deleted = self.storage.lock().unwrap().delete(id)?;
        if deleted {
            if let Some(ref sem) = self.semantic_memory {
                let mut sem = sem.lock().await;
                sem.delete_memory(id);
            }
        }
        Ok(deleted)
    }

    // Bulk-delete imported memories by `source_detail` for one subject user.
    // Used by onboarding reset to wipe seeds a user asked to start fresh
    // from. Keeps the semantic vector store in sync.
    pub async fn delete_by_source_detail(
        &self,
        detail:  &str,
        user_id: &str,
    ) -> Result<usize, crate::MiraError> {
        let (count, ids) = self.storage.lock().unwrap().delete_by_source_detail(detail, user_id)?;
        if !ids.is_empty() {
            if let Some(ref sem) = self.semantic_memory {
                let mut sem = sem.lock().await;
                for id in ids {
                    sem.delete_memory(id);
                }
            }
        }
        Ok(count)
    }

    // Get a single memory item by ID.
    pub fn get(&self, id: u64) -> Option<MemoryItem> {
        self.storage.lock().unwrap().get(id).ok().flatten()
    }

    // List all memories with pagination.
    pub fn list_all(&self, limit: usize, offset: usize) -> Vec<MemoryItem> {
        match self.storage.lock().unwrap().list_all(limit as u64, offset as u64) {
            Ok(items) => items,
            Err(e) => { debug!("list_all failed: {}", e); vec![] }
        }
    }

    // Update content, category, and tags of an existing memory.
    pub async fn update(
        &self,
        id: u64,
        content: String,
        category: Category,
        tags: Vec<String>,
    ) -> Result<bool, crate::MiraError> {
        let updated = self.storage.lock().unwrap().update(id, content, category, tags)?;
        Ok(updated)
    }

    // Total number of stored memories.
    pub fn count(&self) -> Result<u64, crate::MiraError> {
        self.storage.lock().unwrap().count()
    }

    // ── Visibility-aware delegates ──────────────────────────────────────────
    // These forward straight to MemoryStorage, which enforces the visibility
    // chokepoint over (scope, scope_id).

    pub fn list_visible(
        &self,
        user_id:   &str,
        group_ids: &[String],
        limit:     u64,
        offset:    u64,
    ) -> Result<Vec<MemoryItem>, crate::MiraError> {
        self.storage.lock().unwrap().list_visible(user_id, group_ids, limit, offset)
    }

    // Visibility-aware list with a chosen sort (decay-aware strength by
    // default, or plain chronological via [`ListSort::Recent`]).
    pub fn list_visible_sorted(
        &self,
        user_id:   &str,
        group_ids: &[String],
        limit:     u64,
        offset:    u64,
        sort:      ListSort,
    ) -> Result<Vec<MemoryItem>, crate::MiraError> {
        self.storage.lock().unwrap().list_visible_sorted(user_id, group_ids, limit, offset, sort)
    }

    // Reinforce a memory: bump access_count, push last_reinforced = now, and
    // nudge strength back toward 1.0. Errors are swallowed and returned as
    // `None` so a failing audit/write can't bring down retrieval.
    pub fn reinforce(&self, id: u64, actor_user_id: &str) -> Option<f32> {
        match self.storage.lock().unwrap().reinforce(id, actor_user_id) {
            Ok(s)  => s,
            Err(e) => { warn!("reinforce({}) failed: {}", id, e); None }
        }
    }

    pub fn count_visible(
        &self,
        user_id:   &str,
        group_ids: &[String],
    ) -> Result<u64, crate::MiraError> {
        self.storage.lock().unwrap().count_visible(user_id, group_ids)
    }

    pub fn get_visible(
        &self,
        id:        u64,
        user_id:   &str,
        group_ids: &[String],
    ) -> Result<Option<MemoryItem>, crate::MiraError> {
        self.storage.lock().unwrap().get_visible(id, user_id, group_ids)
    }

    pub fn search_visible(
        &self,
        query:     &str,
        user_id:   &str,
        group_ids: &[String],
    ) -> Result<Vec<MemoryItem>, crate::MiraError> {
        self.storage.lock().unwrap().search_visible(query, user_id, group_ids)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn store_scoped(
        &self,
        content:          String,
        category:         Category,
        tags:             Vec<String>,
        source:           Option<MemorySource>,
        scope:            Scope,
        scope_id:         Option<&str>,
        created_by:       &str,
        subject_user_ids: &[String],
        source_channel:        Option<&str>,
        source_conversation_id: Option<&str>,
        source_message_id:      Option<&str>,
    ) -> Result<u64, crate::MiraError> {
        let id = self.storage.lock().unwrap().store_scoped(
            content.clone(), category.clone(), tags, source,
            scope, scope_id, created_by, subject_user_ids,
            source_channel, source_conversation_id, source_message_id,
        )?;

        // Keep semantic index in sync.
        if let Some(ref sem) = self.semantic_memory {
            let mut sem = sem.lock().await;
            if let Err(e) = sem.add_memory(id, content, category.as_str().to_string()).await {
                warn!("Failed to add scoped memory {} to semantic store: {}", id, e);
            }
        }
        Ok(id)
    }

    // Append-only update: create a newer memory that supersedes an existing
    // one. The new memory inherits the old scope/scope_id.
    pub async fn supersede(
        &self,
        old_id:       u64,
        new_content:  String,
        new_category: Category,
        new_tags:     Vec<String>,
        source:       Option<MemorySource>,
        actor_user_id: &str,
    ) -> Result<u64, crate::MiraError> {
        let new_id = self.storage.lock().unwrap().supersede(
            old_id, new_content.clone(), new_category.clone(), new_tags, source, actor_user_id,
        )?;
        if let Some(ref sem) = self.semantic_memory {
            let mut sem = sem.lock().await;
            // Remove old vector, add new.
            sem.delete_memory(old_id);
            if let Err(e) = sem.add_memory(new_id, new_content, new_category.as_str().to_string()).await {
                warn!("Failed to add superseding memory {} to semantic store: {}", new_id, e);
            }
        }
        Ok(new_id)
    }

    // Admin-only soft delete: hides from every visibility read but keeps
    // the row in place for audit.
    pub fn soft_delete(&self, id: u64, actor_user_id: &str) -> Result<bool, crate::MiraError> {
        self.storage.lock().unwrap().soft_delete(id, actor_user_id)
    }

    // Proxy: does `user_id` own a live memory with exactly this tag? Used
    // by the daily rollup job for idempotency (`rollup:YYYY-MM-DD`).
    pub fn has_tag_for_user(&self, user_id: &str, tag: &str) -> Result<bool, crate::MiraError> {
        self.storage.lock().unwrap().has_tag_for_user(user_id, tag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> PathBuf {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_memory.db");
        // Keep tempdir alive by leaking it; fine for unit tests
        std::mem::forget(dir);
        path
    }

    fn item_at(score: f32, created_at: chrono::DateTime<chrono::Utc>) -> MemoryItem {
        MemoryItem {
            id: 1, content: String::new(), category: Category::Fact, tags: vec![],
            source: None, created_at, relevance_score: score, scope: Scope::User,
            scope_id: None, created_by: None, supersedes: None, superseded_by: None,
            strength: 1.0, effective_strength: 1.0, access_count: 0, last_reinforced: 0,
            stability: "stable".into(), source_channel: None,
            source_conversation_id: None, source_message_id: None,
        }
    }

    #[test]
    fn recency_weight_zero_is_pure_similarity() {
        let now = chrono::Utc::now();
        let old = now - chrono::Duration::days(365);
        // weight 0 → recency ignored: score == similarity regardless of age.
        let s = recency_blended_score(&item_at(0.8, old), now, 0.0, 30.0);
        assert!((s - 0.8).abs() < 1e-6, "got {s}");
    }

    #[test]
    fn recency_boost_promotes_fresher_item() {
        let now = chrono::Utc::now();
        // Two equally-similar memories; the newer one must rank higher once a
        // recency weight is applied.
        let fresh = recency_blended_score(&item_at(0.7, now), now, 0.25, 30.0);
        let stale = recency_blended_score(
            &item_at(0.7, now - chrono::Duration::days(120)), now, 0.25, 30.0,
        );
        assert!(fresh > stale, "fresh {fresh} should beat stale {stale}");
        // A zero/negative half-life falls back to the default rather than NaN/inf.
        let safe = recency_blended_score(&item_at(0.5, now), now, 0.25, 0.0);
        assert!(safe.is_finite(), "got {safe}");
    }

    #[tokio::test]
    async fn test_vectors_survive_restart() {
        use crate::memory::vector_backend::{SqliteVectorBackend, VectorStoreBackend};
        let db = temp_db();
        let backend = SqliteVectorBackend::new(&db).unwrap();
        backend.upsert(1, &vec![1.0_f32, 0.0, 0.0], "fact").unwrap();
        backend.upsert(2, &vec![0.0_f32, 1.0, 0.0], "preference").unwrap();

        // "Restart": load from same DB
        let backend2 = SqliteVectorBackend::new(&db).unwrap();
        let all = backend2.load_all().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn test_new_memory_system_starts_empty() {
        let db = temp_db();
        let mem = MemorySystem::new(db).unwrap();
        assert_eq!(mem.count().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_store_increments_count() {
        let db = temp_db();
        let mem = MemorySystem::new(db).unwrap();
        mem.store("I like coffee".to_string(), Category::Preference, vec![]).await.unwrap();
        assert_eq!(mem.count().unwrap(), 1);
        mem.store("My name is Tarek".to_string(), Category::Fact, vec![]).await.unwrap();
        assert_eq!(mem.count().unwrap(), 2);
    }

    #[tokio::test]
    async fn test_store_auto_categorizes_preference() {
        let db = temp_db();
        let mem = MemorySystem::new(db).unwrap();
        let id = mem.store_auto("I love hiking in the mountains".to_string()).await.unwrap();
        let items = mem.get_by_category(&Category::Preference);
        assert!(items.iter().any(|m| m.id == id));
    }

    #[tokio::test]
    async fn test_store_auto_categorizes_skill() {
        let db = temp_db();
        let mem = MemorySystem::new(db).unwrap();
        mem.store_auto("I can speak Arabic fluently".to_string()).await.unwrap();
        let items = mem.get_by_category(&Category::Skill);
        assert_eq!(items.len(), 1);
        assert!(items[0].content.contains("Arabic"));
    }

    #[tokio::test]
    async fn test_store_auto_categorizes_relationship() {
        let db = temp_db();
        let mem = MemorySystem::new(db).unwrap();
        mem.store_auto("My brother Ahmed lives in Cairo".to_string()).await.unwrap();
        let items = mem.get_by_category(&Category::Relationship);
        assert_eq!(items.len(), 1);
    }

    #[tokio::test]
    async fn test_store_auto_categorizes_project() {
        let db = temp_db();
        let mem = MemorySystem::new(db).unwrap();
        mem.store_auto("I am working on a Rust AI agent".to_string()).await.unwrap();
        let items = mem.get_by_category(&Category::Project);
        assert_eq!(items.len(), 1);
    }

    #[tokio::test]
    async fn test_store_auto_categorizes_fact() {
        let db = temp_db();
        let mem = MemorySystem::new(db).unwrap();
        mem.store_auto("My email is tarek@example.com".to_string()).await.unwrap();
        let items = mem.get_by_category(&Category::Fact);
        assert_eq!(items.len(), 1);
    }

    #[tokio::test]
    async fn test_keyword_search_finds_match() {
        let db = temp_db();
        let mem = MemorySystem::new(db).unwrap();
        mem.store("I like Rust programming".to_string(), Category::Preference, vec![]).await.unwrap();
        mem.store("Python is also good".to_string(), Category::Fact, vec![]).await.unwrap();
        let results = mem.search("Rust");
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("Rust"));
    }

    #[tokio::test]
    async fn test_keyword_search_returns_empty_on_no_match() {
        let db = temp_db();
        let mem = MemorySystem::new(db).unwrap();
        mem.store("I like Rust programming".to_string(), Category::Preference, vec![]).await.unwrap();
        let results = mem.search("JavaScript");
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_get_by_category_filters_correctly() {
        let db = temp_db();
        let mem = MemorySystem::new(db).unwrap();
        mem.store("I love music".to_string(), Category::Preference, vec![]).await.unwrap();
        mem.store("My name is Tarek".to_string(), Category::Fact, vec![]).await.unwrap();
        mem.store("I prefer tea over coffee".to_string(), Category::Preference, vec![]).await.unwrap();

        let prefs = mem.get_by_category(&Category::Preference);
        assert_eq!(prefs.len(), 2);
        let facts = mem.get_by_category(&Category::Fact);
        assert_eq!(facts.len(), 1);
        let skills = mem.get_by_category(&Category::Skill);
        assert!(skills.is_empty());
    }

    #[tokio::test]
    async fn test_delete_removes_memory() {
        let db = temp_db();
        let mem = MemorySystem::new(db).unwrap();
        let id = mem.store("To be deleted".to_string(), Category::Fact, vec![]).await.unwrap();
        assert_eq!(mem.count().unwrap(), 1);
        let deleted = mem.delete(id).await.unwrap();
        assert!(deleted);
        assert_eq!(mem.count().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_delete_nonexistent_returns_false() {
        let db = temp_db();
        let mem = MemorySystem::new(db).unwrap();
        let deleted = mem.delete(9999).await.unwrap();
        assert!(!deleted);
    }

    #[tokio::test]
    async fn test_store_with_source() {
        let db = temp_db();
        let mem = MemorySystem::new(db).unwrap();
        let id = mem.store_with_source(
            "Imported fact".to_string(),
            Category::Fact,
            vec!["imported".to_string()],
            MemorySource::Imported("backup.json".to_string()),
        ).await.unwrap();
        assert!(id > 0);
        assert_eq!(mem.count().unwrap(), 1);
    }

    #[tokio::test]
    async fn test_semantic_search_fallback_without_lmstudio() {
        // Without LM Studio configured, semantic_search falls back to keyword search
        let db = temp_db();
        let mem = MemorySystem::new(db).unwrap();
        mem.store("I like Rust".to_string(), Category::Preference, vec![]).await.unwrap();
        let results = mem.semantic_search("Rust", 5).await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].1.contains("Rust"));
        // Fallback always returns score 1.0
        assert_eq!(results[0].2, 1.0f32);
    }

    #[tokio::test]
    async fn test_retrieve_for_context() {
        let db = temp_db();
        let mem = MemorySystem::new(db).unwrap();
        mem.store("Cairo is the capital of Egypt".to_string(), Category::Fact, vec![]).await.unwrap();
        let ctx = mem.retrieve_for_context("Cairo");
        assert_eq!(ctx.len(), 1);
    }
}
