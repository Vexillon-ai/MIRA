// SPDX-License-Identifier: AGPL-3.0-or-later

// src/memory/semantic.rs
//! Semantic memory — vector similarity search and embedding providers.
//!
//! # Embedding providers
//! All providers implement [`EmbeddingProvider`].  The active provider is held
//! inside [`SemanticMemorySystem`] as a `Box<dyn EmbeddingProvider>`, so the
//! rest of the memory system is completely agnostic of the concrete backend:
//!
//! | Config `provider` value | Concrete type             |
//! |-------------------------|---------------------------|
//! | `"internal"`            | `FastEmbedProvider`       |
//! | `"lmstudio"` / `"ollama"` / `"openai"` / `"openrouter"` | `HttpEmbeddingProvider` |

use std::cmp::Ordering;
use std::num::NonZeroUsize;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use async_trait::async_trait;
use lru::LruCache;
use tracing::{debug, info};

use crate::memory::vector_backend::VectorStoreBackend;

pub type Embedding = Vec<f32>;

// ── EmbeddingProvider trait ───────────────────────────────────────────────────

/// Common interface for all embedding backends.
///
/// Implementations must be `Send + Sync` so they can be held behind an
/// `Arc` or `Box` and called from async tasks.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Embed `text` into a dense vector.
    async fn embed(&self, text: &str) -> Result<Embedding, String>;

    /// Dimensionality of the embedding vectors this provider produces.
    fn dimension(&self) -> usize;

    /// Stable label for this provider+model, stored alongside persisted
    /// vectors so callers can detect a model switch. Default is the provider
    /// type name; concrete backends override with `"<provider>:<model>"`.
    fn label(&self) -> String {
        std::any::type_name::<Self>().to_owned()
    }
}

// ── NoopEmbeddingProvider ─────────────────────────────────────────────────────

/// Provider used when the configured embedding backend is unavailable
/// at boot — for `provider="internal"` this happens when libonnxruntime
/// isn't installed (no managed `~/.mira/deps/onnxruntime/`, no
/// `ORT_DYLIB_PATH`). Embedding calls return an error so callers
/// can surface "semantic search disabled" without crashing the
/// process. The dimension is preserved from config so any persisted
/// vectors aren't mis-aligned when the real provider comes back.
pub struct NoopEmbeddingProvider {
    dim:   usize,
    label: String,
}

impl NoopEmbeddingProvider {
    pub fn new(dim: usize, original_label: &str) -> Self {
        Self {
            dim,
            label: format!("noop:{}", original_label),
        }
    }
}

#[async_trait]
impl EmbeddingProvider for NoopEmbeddingProvider {
    async fn embed(&self, _text: &str) -> Result<Embedding, String> {
        Err("embedding provider unavailable — install required dependency \
             (e.g. POST /api/admin/deps/onnxruntime/install) and restart"
            .to_owned())
    }

    fn dimension(&self) -> usize {
        self.dim
    }

    fn label(&self) -> String {
        self.label.clone()
    }
}

// ── VectorStore (in-memory index) ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SemanticMemory {
    pub id:        u64,
    pub content:   String,
    pub category:  String,
    pub tags:      Vec<String>,
    pub embedding: Embedding,
    pub source:    Option<String>,
}

pub struct VectorStore {
    memories: std::collections::HashMap<u64, SemanticMemory>,
    dim:      usize,
}

impl VectorStore {
    pub fn new(dim: usize) -> Self {
        info!("Initializing vector store with dimension {}", dim);
        Self { memories: std::collections::HashMap::new(), dim }
    }

    pub fn add(&mut self, id: u64, memory: SemanticMemory) -> Result<(), String> {
        if memory.embedding.len() != self.dim {
            return Err(format!(
                "Embedding dimension mismatch: expected {}, got {}",
                self.dim, memory.embedding.len()
            ));
        }
        self.memories.insert(id, memory);
        Ok(())
    }

    pub fn search(&self, query_embedding: &Embedding, top_k: usize) -> Vec<(u64, f32)> {
        if query_embedding.len() != self.dim { return vec![]; }
        let mut results: Vec<(u64, f32)> = self.memories
            .values()
            .map(|m| (m.id, cosine_similarity(query_embedding, &m.embedding)))
            .collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
        results.truncate(top_k);
        results
    }

    pub fn get(&self, id: u64) -> Option<&SemanticMemory> {
        self.memories.get(&id)
    }

    pub fn delete(&mut self, id: u64) {
        self.memories.remove(&id);
    }

    pub fn len(&self) -> usize {
        self.memories.len()
    }
}

// ── Math helpers ──────────────────────────────────────────────────────────────

pub fn cosine_similarity(a: &Embedding, b: &Embedding) -> f32 {
    if a.len() != b.len() || a.is_empty() { return 0.0; }
    let mut dot    = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot    += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    let na = norm_a.sqrt();
    let nb = norm_b.sqrt();
    if na < 1e-8 || nb < 1e-8 { return 0.0; }
    dot / (na * nb)
}

pub fn normalize(v: &Embedding) -> Embedding {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n < 1e-8 { return v.clone(); }
    v.iter().map(|x| x / n).collect()
}

// ── HttpEmbeddingProvider (LM Studio / Ollama / OpenAI-compatible) ─────────────

/// Embedding provider that calls any OpenAI-compatible `/v1/embeddings` endpoint.
///
/// Used for `provider = "lmstudio"`, `"ollama"`, `"openai"`, and `"openrouter"`.
pub struct HttpEmbeddingProvider {
    client:   reqwest::Client,
    base_url: String,
    model:    String,
    dim:      usize,
    cache:    StdMutex<LruCache<String, Embedding>>,
}

impl HttpEmbeddingProvider {
    pub fn new(base_url: String, model: String) -> Self {
        Self::with_cache_size(base_url, model, 1000)
    }

    pub fn with_cache_size(base_url: String, model: String, cache_size: usize) -> Self {
        info!("Initializing HTTP embedding provider (url={}, model={})", base_url, model);
        let cache_size = NonZeroUsize::new(cache_size.max(1)).unwrap();
        Self {
            client: reqwest::ClientBuilder::new()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            base_url,
            model,
            dim: 384, // default; actual dim is determined by the first response
            cache: StdMutex::new(LruCache::new(cache_size)),
        }
    }

    /// Insert a vector into the cache — useful in tests to avoid HTTP calls.
    pub fn cache_insert(&self, key: String, val: Embedding) {
        self.cache.lock().unwrap().put(key, val);
    }

    /// Check the cache — useful in tests.
    pub fn cache_get(&self, key: &str) -> Option<Embedding> {
        self.cache.lock().unwrap().get(key).cloned()
    }
}

#[async_trait]
impl EmbeddingProvider for HttpEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<Embedding, String> {
        if let Some(cached) = self.cache.lock().unwrap().get(text).cloned() {
            debug!("Embedding cache hit: {} chars", text.len());
            return Ok(cached);
        }

        let url = format!("{}/embeddings", self.base_url.trim_end_matches('/'));
        let payload = serde_json::json!({ "model": self.model, "input": text });

        let response = self.client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("Embedding request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body   = response.text().await.unwrap_or_default();
            return Err(format!("Embedding API error {}: {}", status, body));
        }

        #[derive(serde::Deserialize)] struct ER { data: Vec<ED> }
        #[derive(serde::Deserialize)] struct ED { embedding: Embedding }

        let resp: ER = response.json().await
            .map_err(|e| format!("Failed to parse embedding response: {}", e))?;

        let embedding = resp.data.into_iter().next()
            .ok_or_else(|| "No embeddings in response".to_string())?.embedding;

        debug!("Generated embedding dim={} for {} chars", embedding.len(), text.len());
        self.cache.lock().unwrap().put(text.to_string(), embedding.clone());
        Ok(embedding)
    }

    fn dimension(&self) -> usize { self.dim }

    fn label(&self) -> String {
        format!("http:{}", self.model)
    }
}

/// Backward-compat alias used in lib.rs re-exports and tests.
pub type LmStudioEmbeddingProvider = HttpEmbeddingProvider;

// ── SemanticMemorySystem ──────────────────────────────────────────────────────

/// Manages the in-memory vector index and delegates embedding to a
/// [`EmbeddingProvider`] chosen at construction time.
pub struct SemanticMemorySystem {
    vector_store:         VectorStore,
    provider:             Box<dyn EmbeddingProvider>,
    similarity_threshold: f32,
    backend:              Option<crate::memory::vector_backend::SqliteVectorBackend>,
}

impl SemanticMemorySystem {
    /// Construct with any [`EmbeddingProvider`].
    pub fn new(provider: Box<dyn EmbeddingProvider>, threshold: f32) -> Self {
        let dim = provider.dimension();
        Self {
            vector_store: VectorStore::new(dim),
            provider,
            similarity_threshold: threshold,
            backend: None,
        }
    }

    /// Attach a SQLite backend and load all persisted vectors into the in-memory store.
    pub fn with_sqlite_backend(
        mut self,
        backend: crate::memory::vector_backend::SqliteVectorBackend,
    ) -> Self {
        if let Ok(entries) = backend.load_all() {
            for (id, vec, category) in entries {
                let mem = SemanticMemory {
                    id, content: String::new(), category,
                    tags: vec![], embedding: vec, source: None,
                };
                self.vector_store.add(id, mem).ok();
            }
            info!("Loaded {} vectors from SQLite backend", self.vector_store.len());
        }
        self.backend = Some(backend);
        self
    }

    pub async fn add_memory(
        &mut self,
        id:       u64,
        content:  String,
        category: String,
    ) -> Result<(), String> {
        let embedding = self.provider.embed(&content).await?;
        if let Some(ref backend) = self.backend {
            backend.upsert(id, &embedding, &category).map_err(|e| e.to_string())?;
        }
        let memory = SemanticMemory {
            id,
            content: content.clone(),
            category,
            tags: vec![],
            embedding,
            source: None,
        };
        self.vector_store.add(id, memory)?;
        debug!("Added semantic memory id={}", id);
        Ok(())
    }

    pub fn delete_memory(&mut self, id: u64) {
        self.vector_store.delete(id);
        if let Some(ref backend) = self.backend {
            backend.delete(id).ok();
        }
    }

    /// Run the configured provider's `embed` directly, without touching the
    /// vector store. Used by the transcript indexer so it can reuse the
    /// already-loaded model instead of building its own copy.
    pub async fn embed_raw(&self, text: &str) -> Result<Embedding, String> {
        self.provider.embed(text).await
    }

    /// Embedding dimensionality advertised by the underlying provider.
    pub fn embedding_dim(&self) -> usize {
        self.provider.dimension()
    }

    /// Human-readable label for the embedding model — "<provider>:<model>".
    /// Used to stamp transcript vector rows so the indexer can detect a
    /// model switch and skip stale rows during search.
    pub fn embedding_model_label(&self) -> String {
        self.provider.label()
    }

    pub async fn search(
        &self,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<(u64, String, f32)>, String> {
        let query_embedding = self.provider.embed(query).await?;
        let results = self.vector_store.search(&query_embedding, top_k);
        let mut formatted = Vec::new();
        for (id, similarity) in results {
            if similarity >= self.similarity_threshold {
                if let Some(memory) = self.vector_store.get(id) {
                    formatted.push((id, memory.content.clone(), similarity));
                }
            }
        }
        debug!(
            "Semantic search for '{}': {} results above threshold {}",
            query, formatted.len(), self.similarity_threshold
        );
        Ok(formatted)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedding_cache_hit_avoids_recompute() {
        let provider = HttpEmbeddingProvider::new(
            "http://localhost:1234/v1".to_string(),
            "all-minilm".to_string(),
        );
        provider.cache_insert("hello world".to_string(), vec![0.1, 0.2, 0.3]);
        let cached = provider.cache_get("hello world");
        assert!(cached.is_some());
        assert_eq!(cached.unwrap()[0], 0.1f32);
    }

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0, 3.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_empty() {
        let a: Vec<f32> = vec![];
        let b: Vec<f32> = vec![];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn test_vector_store_dimension_mismatch() {
        let mut store = VectorStore::new(3);
        let memory = SemanticMemory {
            id: 1,
            content:   "test".to_string(),
            category:  "fact".to_string(),
            tags:      vec![],
            embedding: vec![1.0, 2.0], // wrong dim
            source:    None,
        };
        assert!(store.add(1, memory).is_err());
    }
}
