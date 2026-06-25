// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/providers.rs
//! GET /api/providers/health  — provider health checks
//! GET /api/providers/models  — model list from LM Studio
//! GET /api/providers/openrouter/models — OpenRouter catalog with pricing

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{Extension, extract::Query, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};

use crate::agent::AgentCore;
use crate::auth::AuthUser;
use crate::providers::openrouter::{Catalog, OpenRouterProvider};
use crate::server::handlers::onboarding::DataDir;
use crate::web::LiveConfig;

// ── DTOs ──────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ProviderHealth {
    pub name:       String,
    pub healthy:    bool,
    pub latency_ms: Option<u64>,
    pub model:      String,
    pub url:        Option<String>,
}

#[derive(Serialize)]
pub struct HealthResponse {
    pub providers: Vec<ProviderHealth>,
}

#[derive(Serialize)]
pub struct ModelInfo {
    pub id:       String,
    pub provider: String,
}

// ── GET /api/providers/health ─────────────────────────────────────────────────

pub async fn providers_health(
    // Require login (was fully open). Any authenticated user may see provider
    // health/model availability — they need it for the model picker — but it
    // shouldn't be world-readable. No secrets are returned either way.
    _user:               AuthUser,
    Extension(_agent):   Extension<Arc<AgentCore>>,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
) -> impl IntoResponse {
    use crate::providers::ModelProvider;
    use crate::providers::openai_compat::{AuthHeader, OpenAiCompatClient, OpenAiCompatConfig};

    let cfg = live_cfg.get().await;

    // Probe each enabled provider in parallel. Each probe is gated
    // by the same predicate build_provider_chain uses (enabled +
    // key-or-local-url), so the health grid mirrors what's actually
    // registered. The probe itself piggy-backs on each provider
    // client's existing `health_check()` — the OpenAI-compatible
    // ones GET /models (cheap), Anthropic POSTs a 1-token /messages
    // (small charge, real signal), Gemini GETs /v1beta/models/{id}.
    let mut futs: Vec<tokio::task::JoinHandle<Option<ProviderHealth>>> = Vec::new();

    // Local providers — keyless, URL is the contract.
    if cfg.providers.lmstudio.enabled {
        let url   = cfg.providers.lmstudio.url.clone();
        let model = cfg.providers.lmstudio.default_model.clone();
        futs.push(tokio::spawn(async move {
            let p = crate::providers::lmstudio::LmStudioProvider::new(url.clone(), model.clone());
            let t0 = Instant::now();
            let ok = p.health_check().await;
            Some(ProviderHealth {
                name:       "lmstudio".into(),
                healthy:    ok,
                latency_ms: if ok { Some(t0.elapsed().as_millis() as u64) } else { None },
                model,
                url:        Some(url),
            })
        }));
    }
    if cfg.providers.ollama.enabled {
        let url   = cfg.providers.ollama.url.clone();
        let model = cfg.providers.ollama.default_model.clone();
        futs.push(tokio::spawn(async move {
            let p = crate::providers::local::OllamaProvider::new(url.clone(), model.clone());
            let t0 = Instant::now();
            let ok = p.health_check().await;
            Some(ProviderHealth {
                name:       "ollama".into(),
                healthy:    ok,
                latency_ms: if ok { Some(t0.elapsed().as_millis() as u64) } else { None },
                model,
                url:        Some(url),
            })
        }));
    }

    // OpenRouter — keep the existing fast HEAD-style probe (avoids
    // burning model tokens just for a health pill).
    if let Some(ref key) = cfg.providers.openrouter.api_key {
        if cfg.providers.openrouter.enabled && !key.is_empty() {
            let key   = key.clone();
            let model = cfg.providers.openrouter.default_model.clone();
            futs.push(tokio::spawn(async move {
                let t0 = Instant::now();
                let ok = check_openrouter(&key).await;
                Some(ProviderHealth {
                    name:       "openrouter".into(),
                    healthy:    ok,
                    latency_ms: if ok { Some(t0.elapsed().as_millis() as u64) } else { None },
                    model,
                    url:        Some("https://openrouter.ai/api/v1".into()),
                })
            }));
        }
    }

    // OpenAI-compatible cloud providers — instantiate a one-shot
    // client, hit its health_check(). All of these GET /models or
    // similar; cheap.
    macro_rules! probe_openai_compat {
        ($slug:expr, $cfg:expr) => {
            if let Some(ref key) = $cfg.api_key {
                if $cfg.enabled && !key.is_empty() {
                    let cfg_p = OpenAiCompatConfig {
                        provider_name: $slug.into(),
                        base_url:      $cfg.base_url.clone(),
                        api_key:       key.clone(),
                        model:         $cfg.default_model.clone(),
                        timeout_secs:  $cfg.timeout_secs,
                        auth_header:   AuthHeader::Bearer,
                        extra_headers: vec![],
                    };
                    let url   = $cfg.base_url.clone();
                    let model = $cfg.default_model.clone();
                    futs.push(tokio::spawn(async move {
                        let client = OpenAiCompatClient::new(cfg_p);
                        let t0 = Instant::now();
                        let ok = client.health_check().await;
                        Some(ProviderHealth {
                            name:       $slug.into(),
                            healthy:    ok,
                            latency_ms: if ok { Some(t0.elapsed().as_millis() as u64) } else { None },
                            model,
                            url:        Some(url),
                        })
                    }));
                }
            }
        }
    }
    probe_openai_compat!("openai",   cfg.providers.openai);
    probe_openai_compat!("deepseek", cfg.providers.deepseek);
    probe_openai_compat!("moonshot", cfg.providers.moonshot);
    probe_openai_compat!("groq",     cfg.providers.groq);
    probe_openai_compat!("xai",      cfg.providers.xai);

    // Anthropic + Gemini have their own client types.
    if let Some(ref key) = cfg.providers.anthropic.api_key {
        if cfg.providers.anthropic.enabled && !key.is_empty() {
            let key   = key.clone();
            let base  = cfg.providers.anthropic.base_url.clone();
            let model = cfg.providers.anthropic.default_model.clone();
            let ts    = cfg.providers.anthropic.timeout_secs;
            futs.push(tokio::spawn(async move {
                let p = crate::providers::anthropic::AnthropicProvider::new(
                    key, model.clone(), base.clone(), ts,
                );
                let t0 = Instant::now();
                let ok = p.health_check().await;
                Some(ProviderHealth {
                    name:       "anthropic".into(),
                    healthy:    ok,
                    latency_ms: if ok { Some(t0.elapsed().as_millis() as u64) } else { None },
                    model,
                    url:        Some(base),
                })
            }));
        }
    }
    if let Some(ref key) = cfg.providers.gemini.api_key {
        if cfg.providers.gemini.enabled && !key.is_empty() {
            let key   = key.clone();
            let base  = cfg.providers.gemini.base_url.clone();
            let model = cfg.providers.gemini.default_model.clone();
            let ts    = cfg.providers.gemini.timeout_secs;
            futs.push(tokio::spawn(async move {
                let p = crate::providers::gemini::GeminiProvider::new(
                    key, model.clone(), base.clone(), ts,
                );
                let t0 = Instant::now();
                let ok = p.health_check().await;
                Some(ProviderHealth {
                    name:       "gemini".into(),
                    healthy:    ok,
                    latency_ms: if ok { Some(t0.elapsed().as_millis() as u64) } else { None },
                    model,
                    url:        Some(base),
                })
            }));
        }
    }

    // Catch-all openai_compat — render under the user-chosen name,
    // not the config slug, so the health card matches the provider
    // rollup label below it.
    {
        let cc = &cfg.providers.openai_compat;
        if cc.enabled && !cc.name.is_empty() && !cc.base_url.is_empty() {
            let auth = match cc.auth_style.to_ascii_lowercase().as_str() {
                "azure" | "azure_openai" | "api-key" | "api_key" => AuthHeader::AzureApiKey,
                "none"  | "anonymous"                            => AuthHeader::None,
                _                                                => AuthHeader::Bearer,
            };
            let has_key = cc.api_key.as_deref().map(|k| !k.is_empty()).unwrap_or(false);
            let auth_none = matches!(auth, AuthHeader::None);
            if has_key || auth_none {
                let cfg_p = OpenAiCompatConfig {
                    provider_name: cc.name.clone(),
                    base_url:      cc.base_url.clone(),
                    api_key:       cc.api_key.clone().unwrap_or_default(),
                    model:         cc.default_model.clone(),
                    timeout_secs:  cc.timeout_secs,
                    auth_header:   auth,
                    extra_headers: vec![],
                };
                let name  = cc.name.clone();
                let url   = cc.base_url.clone();
                let model = cc.default_model.clone();
                futs.push(tokio::spawn(async move {
                    let client = OpenAiCompatClient::new(cfg_p);
                    let t0 = Instant::now();
                    let ok = client.health_check().await;
                    Some(ProviderHealth {
                        name,
                        healthy:    ok,
                        latency_ms: if ok { Some(t0.elapsed().as_millis() as u64) } else { None },
                        model,
                        url:        Some(url),
                    })
                }));
            }
        }
    }

    let mut providers: Vec<ProviderHealth> = Vec::with_capacity(futs.len());
    for f in futs {
        if let Ok(Some(h)) = f.await { providers.push(h); }
    }

    axum::Json(HealthResponse { providers })
}

// ── GET /api/providers/models ─────────────────────────────────────────────────
//
// Returns one entry per ENABLED provider, using that provider's
// configured `default_model`. Backs the chat-page model dropdown.
// Filtering on `enabled` (rather than "actually reachable") means
// the dropdown reflects what the user *configured* — they pick a
// model in Settings, hit save, restart, and the entry appears here.
// If the provider is unreachable at request time, the failover
// chain falls back to the next provider (existing behaviour).
//
// Cloud providers also require an api_key — without one, the
// provider isn't actually registered in the chain at startup, so
// surfacing it in the dropdown would be misleading. We mirror the
// same predicate `build_provider_chain` uses.

pub async fn providers_models(
    _user:               AuthUser,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
) -> impl IntoResponse {
    let cfg = live_cfg.get().await;
    let mut out: Vec<ModelInfo> = Vec::new();

    // Helper: emit one ModelInfo per id in `available_models`. Falls
    // back to `[default_model]` when the available list is empty —
    // existing configs that predate the available_models field still
    // see at least one entry per provider this way.
    fn emit(
        out:              &mut Vec<ModelInfo>,
        slug:             &str,
        available_models: &[String],
        default_model:    &str,
    ) {
        if !available_models.is_empty() {
            for id in available_models {
                if id.is_empty() { continue; }
                out.push(ModelInfo {
                    id:       id.clone(),
                    provider: slug.to_owned(),
                });
            }
        } else if !default_model.is_empty() {
            out.push(ModelInfo {
                id:       default_model.to_owned(),
                provider: slug.to_owned(),
            });
        }
    }

    // Local providers — keyless. Enabled flag is the only gate.
    if cfg.providers.lmstudio.enabled {
        emit(&mut out, "lmstudio",
             &cfg.providers.lmstudio.available_models,
             &cfg.providers.lmstudio.default_model);
    }
    if cfg.providers.ollama.enabled {
        emit(&mut out, "ollama",
             &cfg.providers.ollama.available_models,
             &cfg.providers.ollama.default_model);
    }

    // Cloud providers — `enabled && api_key.is_some_and(!empty)`,
    // matching the registration predicate in build_provider_chain.
    macro_rules! push_cloud {
        ($slug:expr, $cfg:expr) => {
            if $cfg.enabled
                && $cfg.api_key.as_deref().map(|k| !k.is_empty()).unwrap_or(false)
            {
                emit(&mut out, $slug,
                     &$cfg.available_models, &$cfg.default_model);
            }
        }
    }
    push_cloud!("openrouter", cfg.providers.openrouter);
    push_cloud!("openai",     cfg.providers.openai);
    push_cloud!("anthropic",  cfg.providers.anthropic);
    push_cloud!("gemini",     cfg.providers.gemini);
    push_cloud!("deepseek",   cfg.providers.deepseek);
    push_cloud!("moonshot",   cfg.providers.moonshot);
    push_cloud!("groq",       cfg.providers.groq);
    push_cloud!("xai",        cfg.providers.xai);

    // Catch-all openai_compat block — also gated on a non-empty
    // `name` slug since the slug is what surfaces in the dropdown.
    {
        let cc = &cfg.providers.openai_compat;
        if cc.enabled && !cc.name.is_empty() {
            let has_key = cc.api_key.as_deref().map(|k| !k.is_empty()).unwrap_or(false);
            let auth_none = cc.auth_style.eq_ignore_ascii_case("none")
                || cc.auth_style.eq_ignore_ascii_case("anonymous");
            if has_key || auth_none {
                emit(&mut out, &cc.name, &cc.available_models, &cc.default_model);
            }
        }
    }

    // Hoist the configured primary's first entry to the front so the
    // dropdown's pre-selected option matches what the chat handler
    // will use when the user hasn't picked a model this session.
    let primary = cfg.primary_provider.as_str();
    if let Some(idx) = out.iter().position(|m| m.provider == primary) {
        if idx != 0 { out.swap(0, idx); }
    }

    axum::Json(out)
}

// ── GET /api/providers/openrouter/models?refresh=0|1 ──────────────────────────

#[derive(Deserialize, Default)]
pub struct CatalogQuery {
    // When true, bypass the cache and re-fetch from OpenRouter.
    #[serde(default)]
    pub refresh: bool,
}

// Whether an *automatic* catalog load may contact this provider upstream —
// mirrors the registration predicate (`build_provider_chain` / the model-list
// probe): local providers gate on `enabled`; cloud providers on
// `enabled && api_key`. A provider the operator disabled (or a cloud provider
// with no key) is never contacted on auto-load — only an explicit Test
// (`?refresh=true`) probes it. Unknown slugs are allowed (preserve behaviour).
fn catalog_fetch_allowed(cfg: &crate::config::MiraConfig, slug: &str) -> bool {
    let p = &cfg.providers;
    let has_key = |k: &Option<String>| k.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
    match slug {
        "lmstudio"   => p.lmstudio.enabled,
        "ollama"     => p.ollama.enabled,
        "openrouter" => p.openrouter.enabled && has_key(&p.openrouter.api_key),
        "openai"     => p.openai.enabled     && has_key(&p.openai.api_key),
        "anthropic"  => p.anthropic.enabled  && has_key(&p.anthropic.api_key),
        "gemini"     => p.gemini.enabled     && has_key(&p.gemini.api_key),
        "deepseek"   => p.deepseek.enabled   && has_key(&p.deepseek.api_key),
        "moonshot"   => p.moonshot.enabled   && has_key(&p.moonshot.api_key),
        "groq"       => p.groq.enabled       && has_key(&p.groq.api_key),
        "xai"        => p.xai.enabled        && has_key(&p.xai.api_key),
        other => {
            // Custom openai_compat provider surfaces under its configured name.
            let cc = &p.openai_compat;
            if cc.enabled && cc.name == other {
                let auth_none = cc.auth_style.eq_ignore_ascii_case("none")
                    || cc.auth_style.eq_ignore_ascii_case("anonymous");
                has_key(&cc.api_key) || auth_none
            } else {
                true // unknown slug — let the handler try (preserves prior behaviour)
            }
        }
    }
}

#[derive(Serialize)]
pub struct CatalogResponse {
    pub fetched_at: u64,
    pub count:      usize,
    pub models:     Vec<crate::providers::openrouter::CatalogEntry>,
}

impl From<Catalog> for CatalogResponse {
    fn from(c: Catalog) -> Self {
        Self { fetched_at: c.fetched_at, count: c.models.len(), models: c.models }
    }
}

pub async fn openrouter_models(
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
    Extension(data_dir): Extension<DataDir>,
    Query(q):            Query<CatalogQuery>,
) -> Result<axum::Json<CatalogResponse>, (StatusCode, String)> {
    let cfg = live_cfg.get().await;
    let or  = &cfg.providers.openrouter;

    let api_key = or.api_key.as_deref().unwrap_or("");
    let provider = OpenRouterProvider::new(api_key.to_string(), or.default_model.clone());

    match provider.catalog(data_dir.0.as_path(), q.refresh, or.catalog_refresh_hours).await {
        Ok(cat) => Ok(axum::Json(cat.into())),
        Err(e)  => Err((StatusCode::BAD_GATEWAY, e.to_string())),
    }
}

// ── GET /api/admin/embedding-models ──────────────────────────────────────────
//
// Powers the Settings page's embedding-model combobox. Returns
// `[{id, dim?, source}]` for the requested provider:
//
// * `internal`   — hardcoded fastembed list (5 known models, dims known)
// * `lmstudio`   — GET <url>/models, filter by `type=embeddings` or name heuristic
// * `ollama`     — GET <url>/api/tags, filter by name heuristic
// * `openai`     — GET <url>/models, filter by name (no type field on OpenAI)
// * `openrouter` — empty (OpenRouter doesn't proxy embeddings today)
//
// Dim is only set for `internal` (fastembed exposes it). For HTTP
// providers callers should keep the manual `embedding_dim` field —
// /v1/models doesn't carry vector size.

#[derive(Deserialize, Default)]
pub struct EmbeddingModelsQuery {
    pub provider: String,
    // Override the provider URL. Falls back to LiveConfig defaults.
    #[serde(default)]
    pub url:      Option<String>,
    // Required for `openai` (and any other keyed provider). Empty
    // means "use whatever's in LiveConfig", not "send no key".
    #[serde(default)]
    pub api_key:  Option<String>,
}

#[derive(Serialize)]
pub struct EmbeddingModel {
    pub id:     String,
    // Vector dimensionality. Only set for providers where we know
    // it without an extra round-trip (today: only `internal`).
    pub dim:    Option<usize>,
    // Where the entry came from — `"hardcoded"` for fastembed,
    // `"upstream"` when fetched from the provider.
    pub source: &'static str,
}

#[derive(Serialize)]
pub struct EmbeddingModelsResponse {
    pub provider: String,
    pub models:   Vec<EmbeddingModel>,
    // Set when we couldn't reach the provider — UI surfaces it
    // so the user knows the dropdown is stale and can fall back
    // to typing a model name manually.
    pub error:    Option<String>,
}

pub async fn list_embedding_models(
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
    Query(q):            Query<EmbeddingModelsQuery>,
) -> impl IntoResponse {
    let provider = q.provider.to_lowercase();
    let cfg = live_cfg.get().await;

    // ── internal: hardcoded fastembed list ──────────────────────────
    if provider == "internal" {
        let models = vec![
            EmbeddingModel { id: "BGE-small-en-v1.5".into(),     dim: Some(384), source: "hardcoded" },
            EmbeddingModel { id: "BGE-base-en-v1.5".into(),      dim: Some(768), source: "hardcoded" },
            EmbeddingModel { id: "all-MiniLM-L6-v2".into(),      dim: Some(384), source: "hardcoded" },
            EmbeddingModel { id: "all-MiniLM-L12-v2".into(),     dim: Some(384), source: "hardcoded" },
            EmbeddingModel { id: "nomic-embed-text-v1.5".into(), dim: Some(768), source: "hardcoded" },
        ];
        return axum::Json(EmbeddingModelsResponse {
            provider, models, error: None,
        }).into_response();
    }

    // ── openrouter: no embeddings ───────────────────────────────────
    if provider == "openrouter" {
        return axum::Json(EmbeddingModelsResponse {
            provider, models: Vec::new(),
            error: Some("OpenRouter does not proxy embedding endpoints — pick another provider.".into()),
        }).into_response();
    }

    // ── HTTP providers: query the upstream model list ────────────────
    let base_url = q.url.clone().unwrap_or_else(|| match provider.as_str() {
        "ollama"   => "http://localhost:11434/v1".to_string(),
        "openai"   => "https://api.openai.com/v1".to_string(),
        _          => cfg.providers.lmstudio.url.clone(),
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    // API key only sourced from the query param — the SettingsPage
    // already has the embedding api_key in form state and can pass
    // it through. Sourcing from LiveConfig would mean reading the
    // saved (possibly stale) key and silently using it for an
    // unsaved provider switch; explicit pass-through is cleaner.
    let key = q.api_key.clone().unwrap_or_default();

    // Ollama uses /api/tags with a different shape; everything else
    // speaks OpenAI-compatible /v1/models.
    let result = if provider == "ollama" {
        fetch_ollama_embedding_models(&client, &base_url).await
    } else {
        fetch_openai_compat_embedding_models(&client, &base_url, &key, &provider).await
    };

    match result {
        Ok(models) => axum::Json(EmbeddingModelsResponse {
            provider, models, error: None,
        }).into_response(),
        Err(e) => axum::Json(EmbeddingModelsResponse {
            provider, models: Vec::new(), error: Some(e),
        }).into_response(),
    }
}

async fn fetch_openai_compat_embedding_models(
    client:   &reqwest::Client,
    base_url: &str,
    api_key:  &str,
    provider: &str,
) -> Result<Vec<EmbeddingModel>, String> {
    #[derive(Deserialize)]
    struct Resp { data: Vec<Entry> }
    #[derive(Deserialize)]
    struct Entry {
        id: String,
        #[serde(rename = "type", default)]
        model_type: String,
    }

    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut req = client.get(&url);
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }
    let resp = req.send().await.map_err(|e| format!("{provider}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{provider}: HTTP {}", resp.status()));
    }
    let parsed: Resp = resp.json().await.map_err(|e| format!("{provider}: parse: {e}"))?;

    let out: Vec<EmbeddingModel> = parsed.data.into_iter()
        .filter(|m| is_embedding_model_id(&m.id, &m.model_type))
        .map(|m| EmbeddingModel { id: m.id, dim: None, source: "upstream" })
        .collect();
    Ok(out)
}

async fn fetch_ollama_embedding_models(
    client:   &reqwest::Client,
    base_url: &str,
) -> Result<Vec<EmbeddingModel>, String> {
    // Ollama's tags endpoint is at the *root* (not /v1). Strip a
    // trailing /v1 if present so users can paste either form.
    let root = base_url
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_string();
    let url = format!("{root}/api/tags");

    #[derive(Deserialize)]
    struct Resp { models: Vec<Entry> }
    #[derive(Deserialize)]
    struct Entry { name: String }

    let resp = client.get(&url).send().await.map_err(|e| format!("ollama: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("ollama: HTTP {}", resp.status()));
    }
    let parsed: Resp = resp.json().await.map_err(|e| format!("ollama: parse: {e}"))?;
    let out: Vec<EmbeddingModel> = parsed.models.into_iter()
        .filter(|m| is_embedding_model_id(&m.name, ""))
        .map(|m| EmbeddingModel { id: m.name, dim: None, source: "upstream" })
        .collect();
    Ok(out)
}

// Heuristic: which model IDs look like embedding models? LM Studio
// ≥ 0.3 sets `type=embeddings` for us; for other providers we have
// to go by name. Keep this conservative — false negatives mean the
// user types the name themselves; false positives surface non-
// embedding models in the dropdown.
fn is_embedding_model_id(id: &str, model_type: &str) -> bool {
    if model_type == "embeddings" || model_type == "embedding" { return true; }
    if model_type == "llm" || model_type == "vlm" || model_type == "rerank" { return false; }
    let lower = id.to_lowercase();
    lower.contains("embed")
        || lower.contains("bge-")
        || lower.contains("nomic-embed")
        || lower.starts_with("text-embedding-")
}

// ── helpers ───────────────────────────────────────────────────────────────────

async fn check_openrouter(api_key: &str) -> bool {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    client
.get("https://openrouter.ai/api/v1/models")
        .bearer_auth(api_key)
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

// ── GET /api/providers/{slug}/catalog?refresh=0|1 ─────────────────────────────
//
// Per-provider model catalog. Backs the Settings page's
// per-provider model dropdown.
//
// Routing dispatches by slug:
// - openai / deepseek / moonshot / groq / xai   → OpenAiCompatClient.fetch_model_ids
// - anthropic                                    → AnthropicProvider.fetch_model_ids
// - gemini                                       → GeminiProvider.fetch_model_ids
// - lmstudio                                     → LmStudioProvider.fetch_model_ids
// - openai_compat (catch-all)                    → OpenAiCompatClient.fetch_model_ids
//                                                  (with the user's configured name/url)
//
// OpenRouter and Ollama use their own legacy endpoints — out of
// scope for this PR.

use axum::extract::Path as AxumPath;

pub async fn provider_catalog(
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
    Extension(data_dir): Extension<DataDir>,
    AxumPath(slug):      AxumPath<String>,
    Query(q):            Query<CatalogQuery>,
) -> Result<axum::Json<crate::providers::catalog::ModelCatalog>, (StatusCode, String)> {
    use crate::providers::catalog::{
        apply_overlay, load_any, load_if_fresh, save_catalog, ModelCatalog,
    };
    use crate::providers::openai_compat::{
        AuthHeader, OpenAiCompatClient, OpenAiCompatConfig,
    };
    use crate::providers::anthropic::AnthropicProvider;
    use crate::providers::gemini::GeminiProvider;
    use crate::providers::lmstudio::LmStudioProvider;

    let cfg = live_cfg.get().await;
    let dd  = data_dir.0.as_path();

    // 24h cache TTL across the board — provider catalogs don't change
    // hourly. The `?refresh=true` query param forces a re-fetch for
    // the "I just enabled a new model" case.
    const TTL_HOURS: u64 = 24;

    if !q.refresh {
        if let Some(cat) = load_if_fresh(dd, &slug, TTL_HOURS) {
            return Ok(axum::Json(cat));
        }
    }

    // A provider the operator disabled (or a cloud provider with no key) must
    // not be contacted on an automatic load — serve cache/empty quietly and
    // never WARN (avoids e.g. a disabled 'anthropic' 401-ing on every Settings
    // open). An explicit Test (?refresh=true) still probes, so configuring a
    // provider gives real feedback.
    if !q.refresh && !catalog_fetch_allowed(&cfg, &slug) {
        let cat = load_any(dd, &slug)
            .unwrap_or_else(|| ModelCatalog::new(&slug, vec![], "disabled"));
        return Ok(axum::Json(cat));
    }

    // Build a fetcher closure per supported slug.
    let fetched: Result<Vec<crate::providers::catalog::ModelEntry>, crate::MiraError> = match slug.as_str() {
        "openai" => {
            let p = &cfg.providers.openai;
            let key = p.api_key.clone().unwrap_or_default();
            let client = OpenAiCompatClient::new(OpenAiCompatConfig {
                provider_name: "openai".into(),
                base_url:      p.base_url.clone(),
                api_key:       key,
                model:         p.default_model.clone(),
                timeout_secs:  p.timeout_secs,
                auth_header:   AuthHeader::Bearer,
                extra_headers: vec![],
            });
            client.fetch_model_ids().await
        }
        "deepseek" => {
            let p = &cfg.providers.deepseek;
            let key = p.api_key.clone().unwrap_or_default();
            let client = OpenAiCompatClient::new(OpenAiCompatConfig {
                provider_name: "deepseek".into(),
                base_url:      p.base_url.clone(),
                api_key:       key,
                model:         p.default_model.clone(),
                timeout_secs:  p.timeout_secs,
                auth_header:   AuthHeader::Bearer,
                extra_headers: vec![],
            });
            client.fetch_model_ids().await
        }
        "moonshot" => {
            let p = &cfg.providers.moonshot;
            let key = p.api_key.clone().unwrap_or_default();
            let client = OpenAiCompatClient::new(OpenAiCompatConfig {
                provider_name: "moonshot".into(),
                base_url:      p.base_url.clone(),
                api_key:       key,
                model:         p.default_model.clone(),
                timeout_secs:  p.timeout_secs,
                auth_header:   AuthHeader::Bearer,
                extra_headers: vec![],
            });
            client.fetch_model_ids().await
        }
        "groq" => {
            let p = &cfg.providers.groq;
            let key = p.api_key.clone().unwrap_or_default();
            let client = OpenAiCompatClient::new(OpenAiCompatConfig {
                provider_name: "groq".into(),
                base_url:      p.base_url.clone(),
                api_key:       key,
                model:         p.default_model.clone(),
                timeout_secs:  p.timeout_secs,
                auth_header:   AuthHeader::Bearer,
                extra_headers: vec![],
            });
            client.fetch_model_ids().await
        }
        "xai" => {
            let p = &cfg.providers.xai;
            let key = p.api_key.clone().unwrap_or_default();
            let client = OpenAiCompatClient::new(OpenAiCompatConfig {
                provider_name: "xai".into(),
                base_url:      p.base_url.clone(),
                api_key:       key,
                model:         p.default_model.clone(),
                timeout_secs:  p.timeout_secs,
                auth_header:   AuthHeader::Bearer,
                extra_headers: vec![],
            });
            client.fetch_model_ids().await
        }
        "openai_compat" => {
            let p = &cfg.providers.openai_compat;
            if p.name.is_empty() || p.base_url.is_empty() {
                return Err((StatusCode::BAD_REQUEST,
                    "openai_compat catch-all not configured (name/base_url empty)".into()));
            }
            let auth = match p.auth_style.to_ascii_lowercase().as_str() {
                "azure" | "azure_openai" | "api-key" | "api_key" => AuthHeader::AzureApiKey,
                "none"  | "anonymous"                            => AuthHeader::None,
                _                                                => AuthHeader::Bearer,
            };
            let client = OpenAiCompatClient::new(OpenAiCompatConfig {
                provider_name: p.name.clone(),
                base_url:      p.base_url.clone(),
                api_key:       p.api_key.clone().unwrap_or_default(),
                model:         p.default_model.clone(),
                timeout_secs:  p.timeout_secs,
                auth_header:   auth,
                extra_headers: vec![],
            });
            client.fetch_model_ids().await
        }
        "anthropic" => {
            let p = &cfg.providers.anthropic;
            let client = AnthropicProvider::new(
                p.api_key.clone().unwrap_or_default(),
                p.default_model.clone(),
                p.base_url.clone(),
                p.timeout_secs,
            );
            client.fetch_model_ids().await
        }
        "gemini" => {
            let p = &cfg.providers.gemini;
            let client = GeminiProvider::new(
                p.api_key.clone().unwrap_or_default(),
                p.default_model.clone(),
                p.base_url.clone(),
                p.timeout_secs,
            );
            client.fetch_model_ids().await
        }
        "lmstudio" => {
            let p = &cfg.providers.lmstudio;
            let client = LmStudioProvider::new(p.url.clone(), p.default_model.clone());
            client.fetch_model_ids().await
        }
        "openrouter" => {
            // OpenRouter has its own legacy catalog system (with
            // pricing baked in from the upstream /models response).
            // Reuse it rather than re-fetching from scratch — adapt
            // the returned shape to ModelEntry. Short-circuits the
            // overlay + cache write below since OpenRouter's catalog
            // is already cached at <data_dir>/cache/openrouter-models.json
            // by its own `catalog()` method.
            let or  = &cfg.providers.openrouter;
            let key = or.api_key.clone().unwrap_or_default();
            let provider = crate::providers::openrouter::OpenRouterProvider::new(
                key, or.default_model.clone(),
            );
            match provider.catalog(dd, q.refresh, or.catalog_refresh_hours).await {
                Ok(cat) => {
                    use crate::providers::catalog::{ModelCatalog, ModelEntry};
                    let entries = cat.models.into_iter().map(|m| ModelEntry {
                        id:                  m.id,
                        display_name:        if m.name.is_empty() { None } else { Some(m.name) },
                        context_window:      Some(m.context_length as u32),
                        // OpenRouter prices arrive per-token; * 1M to
                        // match the rest of the catalog surface.
                        input_price_per_1m:  Some(m.pricing.prompt     * 1_000_000.0),
                        output_price_per_1m: Some(m.pricing.completion * 1_000_000.0),
                        notes:               if m.modality.is_empty() { None } else { Some(m.modality) },
                    }).collect();
                    return Ok(axum::Json(ModelCatalog {
                        provider:   "openrouter".into(),
                        entries,
                        fetched_at: cat.fetched_at,
                        source:     "openrouter-catalog".into(),
                    }));
                }
                Err(e) => return Err((StatusCode::BAD_GATEWAY, e.to_string())),
            }
        }
        "ollama" => {
            // Ollama uses /api/tags (its own shape, no pricing data,
            // names map straight across to ModelEntry ids).
            let p = &cfg.providers.ollama;
            let url = format!("{}/api/tags", p.url.trim_end_matches('/'));
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(p.timeout_secs.max(1)))
                .build()
                .expect("ollama catalog: reqwest build");
            #[derive(serde::Deserialize)]
            struct TagsResponse { models: Vec<TagRow> }
            #[derive(serde::Deserialize)]
            struct TagRow { name: String }
            async fn fetch_ollama(client: reqwest::Client, url: String)
                -> Result<Vec<crate::providers::catalog::ModelEntry>, crate::MiraError>
            {
                let resp = client.get(&url).send().await
                    .map_err(|e| crate::MiraError::ProviderError(
                        format!("ollama: catalog fetch connect failed: {e}")
                    ))?;
                if !resp.status().is_success() {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    return Err(crate::MiraError::ProviderError(
                        format!("ollama: {status}: {body}")
                    ));
                }
                let body: TagsResponse = resp.json().await
                    .map_err(|e| crate::MiraError::ProviderError(
                        format!("ollama: catalog parse failed: {e}")
                    ))?;
                Ok(body.models.into_iter()
                    .map(|m| crate::providers::catalog::ModelEntry::id_only(m.name))
                    .collect())
            }
            fetch_ollama(client, url).await
        }
        other => {
            return Err((StatusCode::NOT_FOUND,
                format!("unknown provider slug '{other}' — supported: openai, anthropic, gemini, \
                         deepseek, moonshot, groq, xai, openai_compat, lmstudio, openrouter, ollama")));
        }
    };

    match fetched {
        Ok(mut entries) => {
            // Apply the per-provider pricing overlay. For
            // openai_compat catch-all and lmstudio this is a no-op
            // (empty table).
            apply_overlay(&mut entries, crate::providers::overlays::for_provider(&slug));
            let cat = ModelCatalog::new(&slug, entries, "live");
            // Best-effort cache write — don't fail the request if
            // the disk is full / unwritable.
            if let Err(e) = save_catalog(dd, &cat) {
                tracing::warn!("provider_catalog: cache write failed for '{slug}': {e}");
            }
            Ok(axum::Json(cat))
        }
        Err(e) => {
            // Upstream failure — fall back to stale cache when one
            // exists so the dropdown doesn't empty out. If there's no
            // cache either, surface the upstream error so the UI can
            // tell the user what went wrong.
            if let Some(mut stale) = load_any(dd, &slug) {
                stale.source = "stale-cache".into();
                tracing::warn!("provider_catalog: '{slug}' fetch failed ({e}); serving stale cache");
                return Ok(axum::Json(stale));
            }
            Err((StatusCode::BAD_GATEWAY, e.to_string()))
        }
    }
}
