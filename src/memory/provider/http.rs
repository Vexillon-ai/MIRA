// SPDX-License-Identifier: AGPL-3.0-or-later

//! `HttpMemoryProvider` — serve memory from any HTTP memory service that speaks
//! MIRA's provider-neutral memory SPI (see `docs/guides/memory-provider-spi.md`).
//! Any service matching the wire shape plugs in with no code change.
//!
//! Wire contract (loopback by default, `X-Api-Key` from a key file):
//! - `POST /search` `{query,user_id,k,consumer_model?}` → `{state, facts[], provider_meta}`
//!   where `state` ∈ served/absent/withheld/unavailable; a fact carries
//!   `content`, a generic `trust` level (`verified`|`reported`|`unverified`) with an
//!   optional `trust_label`, and `provenance/recorded_at/…`.
//! - `POST /add` `{user_id,text,source}` / `POST /ingest` `{user_id,texts,source}`.
//! - `POST /forget` `{user_id,item_id}`.
//! - `GET /health`; `GET /capabilities`; `GET|POST /catalog`.
//! - any non-2xx → unavailable (never absent); 400 → client error.
//!
//! The HTTP call is behind [`HttpCaller`] so the mapping logic is unit- and
//! conformance-testable without a live service; [`ReqwestHttpCaller`] is the
//! production impl (loopback, api-key header) on the shared rustls client.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{
    CatalogParams, CatalogResult, ForgetSelector, MemoryCapabilities, MemoryError, MemoryFact,
    MemoryHealth, MemoryLocality, MemoryProvider, MemoryQuery, MemoryRetrieval, Speaker, Trust,
    TurnRecord,
};

/// One HTTP response: status + parsed JSON body (Null if the body wasn't JSON).
pub struct HttpResp {
    pub status: u16,
    pub body: Value,
}

/// Abstracts "make one HTTP request to the memory service", so the adapter is
/// testable without a live server.
#[async_trait]
pub trait HttpCaller: Send + Sync {
    async fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<HttpResp, MemoryError>;
}

/// Which account identity this provider sends as the `user_id` on the wire —
/// so a service keyed on the username or email (not MIRA's UUID) can match.
/// `Id` (the UUID) is the default and preserves prior behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserIdField { Id, Username, Email }

impl UserIdField {
    /// Parse a config value; unknown/empty → `Id` (safe default).
    pub fn parse(s: Option<&str>) -> Self {
        match s.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("username") => Self::Username,
            Some("email")    => Self::Email,
            _                => Self::Id,
        }
    }
}

/// Capabilities a service declared via `GET /capabilities`. Cached at boot by
/// [`HttpMemoryProvider::probe`]; `None` means not-yet-probed or unreachable, in
/// which case [`HttpMemoryProvider::capabilities`] falls back to the optimistic
/// legacy default (so a service without the endpoint keeps working).
#[derive(Clone, Copy, Debug)]
struct ProbedCaps {
    read: bool,
    write: bool,
    forget: bool,
    catalog: bool,
    citations: bool,
}

pub struct HttpMemoryProvider {
    id: String,
    caller: Arc<dyn HttpCaller>,
    read_only: bool,
    locality: MemoryLocality,
    user_id_field: UserIdField,
    probed: std::sync::RwLock<Option<ProbedCaps>>,
}

impl HttpMemoryProvider {
    pub fn new(
        id: impl Into<String>, caller: Arc<dyn HttpCaller>, read_only: bool,
        locality: MemoryLocality, user_id_field: UserIdField,
    ) -> Self {
        Self { id: id.into(), caller, read_only, locality, user_id_field, probed: std::sync::RwLock::new(None) }
    }

    /// The identity to send to the service: the configured field, with a
    /// fall-back to the UUID when the chosen field is absent/blank (so a user
    /// without an email never breaks recall).
    fn pick_user_id<'a>(&self, user_id: &'a str, username: Option<&'a str>, email: Option<&'a str>) -> &'a str {
        let pick = match self.user_id_field {
            UserIdField::Id       => None,
            UserIdField::Username => username,
            UserIdField::Email    => email,
        };
        pick.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(user_id)
    }

    /// The extra wire key to mirror the identity under, named after the
    /// configured `user_id_field` — for a service that reads identity from a
    /// field called `username`/`email` rather than the canonical `user_id`.
    /// `None` for the default (`id`), which only uses `user_id`.
    fn identity_mirror_key(&self) -> Option<&'static str> {
        match self.user_id_field {
            UserIdField::Id       => None,
            UserIdField::Username => Some("username"),
            UserIdField::Email    => Some("email"),
        }
    }

    /// The identity to send for recall query `q`.
    fn wire_user_id<'a>(&self, q: &'a MemoryQuery) -> &'a str {
        self.pick_user_id(&q.user_id, q.username.as_deref(), q.email.as_deref())
    }
}

/// Map a wire fact into a `MemoryFact`. Trust `"verified"` → Verified (the
/// service attests it); `"reported"` → Reported; anything else → Unverified (a
/// user-stated write is never verified). The human `trust_label` and the
/// consumer-certification fields ride in `provider_meta`, surfaced not stripped.
fn wire_fact(id: &str, v: &Value) -> Option<MemoryFact> {
    let content = v.get("content").and_then(|c| c.as_str()).unwrap_or("").trim().to_string();
    if content.is_empty() { return None; }
    let trust = match v.get("trust").and_then(|t| t.as_str()) {
        Some("verified") => Trust::Verified,
        Some("reported") => Trust::Reported,
        Some("unknown")  => Trust::Unknown,
        _                => Trust::Unverified,
    };
    // Preserve whatever the service sent that MIRA doesn't model under
    // provider_meta: the human trust_label + consumer-certification fields, and
    // the documented SPI descriptors (`source`, `subject`, `relation`) that the
    // fact otherwise drops — so the catalog/tool can surface them.
    let mut meta = v.get("provider_meta").cloned().unwrap_or(Value::Null);
    for k in ["trust_label", "consumer_certified", "consumer_model", "source", "subject", "relation"] {
        if let Some(val) = v.get(k) {
            if !meta.is_object() { meta = json!({}); }
            meta.as_object_mut().unwrap().insert(k.to_string(), val.clone());
        }
    }
    Some(MemoryFact {
        id: v.get("id").and_then(|x| x.as_str().map(String::from)),
        content,
        score: v.get("score").and_then(|x| x.as_f64()).map(|f| f as f32),
        recorded_at: v.get("recorded_at").and_then(|x| x.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&chrono::Utc)),
        valid_at: None,
        source_id: id.to_string(),
        provenance: v.get("provenance").map(|p| p.to_string()),
        confidence: v.get("confidence").and_then(|x| x.as_f64()).map(|f| f as f32),
        speaker: Speaker::External,
        trust,
        complete_set: false,
        provider_meta: meta,
    })
}

#[async_trait]
impl MemoryProvider for HttpMemoryProvider {
    fn id(&self) -> &str { &self.id }

    fn capabilities(&self) -> MemoryCapabilities {
        // If the service declared its capabilities at boot (`GET /capabilities`,
        // cached by `probe`), honour them exactly — MIRA gates features on them.
        // Otherwise (no such endpoint, or unreachable at boot) fall back to the
        // optimistic legacy default so a minimal/older service keeps working: a
        // missing `/catalog` still degrades to unavailable, a config `read_only`
        // still suppresses writes. The read-only flag always wins over a declared
        // write/forget (operator intent overrides the service's own claim).
        if let Some(c) = *self.probed.read().unwrap() {
            MemoryCapabilities {
                read: c.read,
                write: c.write && !self.read_only,
                forget: c.forget && !self.read_only,
                citations: c.citations,
                catalog: c.catalog,
                ..MemoryCapabilities::none()
            }
        } else {
            // Never successfully probed (unreachable at boot). Don't advertise
            // catalog unconfirmed — registering `memory_catalog` ungated was the
            // M6 concern — but keep read/write/citations usable so recall still
            // works; a periodic re-probe upgrades catalog once the service answers.
            MemoryCapabilities {
                read: true,
                write: !self.read_only,
                forget: !self.read_only,
                citations: true, // SPI services carry a per-fact trust level + provenance
                catalog: false,   // unconfirmed → off until a probe says otherwise
                ..MemoryCapabilities::none()
            }
        }
    }

    async fn probe(&self) {
        // One-shot capability discovery. A 200 declaration is authoritative and
        // gates catalog/citations. A missing endpoint (any non-200) is NOT a
        // downgrade: we leave the cache empty so `capabilities` keeps the
        // optimistic default and a service without `/capabilities` still offers
        // catalog (its `/catalog` 404 degrades gracefully). This is the documented
        // fail-soft behaviour.
        match self.caller.request("GET", "/capabilities", None).await {
            Ok(r) if r.status == 200 => {
                let b = &r.body;
                // A service may declare a flag two ways: a flat boolean
                // (`"catalog": true`), or by listing the endpoint under
                // `routes_live` / `verbs.{read,write}` (the shape a richer SPI
                // service uses). Honour either; a flat boolean wins when present.
                let mut routes: Vec<String> = Vec::new();
                let mut collect = |v: Option<&Value>| if let Some(a) = v.and_then(|x| x.as_array()) {
                    routes.extend(a.iter().filter_map(|s| s.as_str().map(str::to_string)));
                };
                collect(b.get("routes_live"));
                collect(b.get("verbs").and_then(|v| v.get("read")));
                collect(b.get("verbs").and_then(|v| v.get("write")));
                let has_route = |p: &str| routes.iter().any(|r| r == p);
                let flag = |k: &str| b.get(k).and_then(|v| v.as_bool());
                let caps = ProbedCaps {
                    read:      flag("read").unwrap_or(true),
                    write:     flag("write").unwrap_or_else(|| has_route("/add") || has_route("/ingest")),
                    forget:    flag("forget").unwrap_or_else(|| has_route("/forget")),
                    catalog:   flag("catalog").unwrap_or_else(|| has_route("/catalog")),
                    // Every SPI service carries a per-fact trust level by contract,
                    // so citations defaults ON unless the service explicitly opts out.
                    citations: flag("citations").unwrap_or(true),
                };
                tracing::info!("memory provider '{}' declared capabilities: {:?}", self.id, caps);
                *self.probed.write().unwrap() = Some(caps);
            }
            other => {
                let st = other.as_ref().ok().map(|r| r.status);
                tracing::debug!(
                    "memory provider '{}' /capabilities not available ({:?}) — keeping optimistic defaults",
                    self.id, st,
                );
            }
        }
    }

    fn locality(&self) -> MemoryLocality { self.locality }

    async fn health(&self) -> MemoryHealth {
        match self.caller.request("GET", "/health", None).await {
            Ok(r) if r.status == 200 => {
                let ready = r.body.get("ready").and_then(|b| b.as_bool()).unwrap_or(true);
                if ready { MemoryHealth::Ready }
                else { MemoryHealth::Warming { detail: "store loading".into() } }
            }
            Ok(r) if r.status == 503 => MemoryHealth::Warming { detail: "service not ready (503)".into() },
            Ok(r) => MemoryHealth::Unavailable { reason: format!("health HTTP {}", r.status) },
            Err(e) => MemoryHealth::Unavailable { reason: e.to_string() },
        }
    }

    async fn retrieve(&self, q: &MemoryQuery) -> Result<MemoryRetrieval, MemoryError> {
        if q.user_id.trim().is_empty() {
            return Err(MemoryError::InvalidRequest("user_id is required".into()));
        }
        // Send the resolved identity value as `user_id` (SPI canonical), and
        // mirror it under the configured field name (`username`/`email`) for a
        // service that reads identity from that key directly.
        let ident = self.wire_user_id(q);
        let mut body = json!({ "query": q.query, "user_id": ident, "k": q.k });
        let obj = body.as_object_mut().unwrap();
        if let Some(k) = self.identity_mirror_key() { obj.insert(k.into(), json!(ident)); }
        if let Some(m) = &q.consumer_model {
            obj.insert("consumer_model".into(), json!(m));
        }
        let resp = match self.caller.request("POST", "/search", Some(body)).await {
            Ok(r) => r,
            Err(e) => return Ok(MemoryRetrieval::Unavailable { reason: e.to_string() }),
        };
        // Status mapping. 400 is a client/protocol error (our bug). EVERY other
        // non-2xx is a provider FAILURE and must read as Unavailable, never as
        // Absent — a bad API key (401/403) or a wrong path (404) must not be
        // rendered to the user as "you have no such memory". Only a 2xx body
        // may carry a state (which can itself be `absent`).
        if resp.status == 400 {
            return Err(MemoryError::InvalidRequest(
                resp.body.get("reason").and_then(|r| r.as_str()).unwrap_or("bad request").to_string()));
        }
        if !(200..300).contains(&resp.status) {
            let reason = resp.body.get("reason").and_then(|r| r.as_str()).map(str::to_string)
                .unwrap_or_else(|| match resp.status {
                    401 | 403 => "auth rejected (check the API key)".into(),
                    404       => "endpoint not found".into(),
                    s         => format!("service error (HTTP {s})"),
                });
            return Ok(MemoryRetrieval::Unavailable { reason });
        }
        let state = resp.body.get("state").and_then(|s| s.as_str()).unwrap_or("served");
        match state {
            "unavailable" => Ok(MemoryRetrieval::Unavailable {
                reason: resp.body.get("reason").and_then(|r| r.as_str()).unwrap_or("unavailable").to_string() }),
            "served" | "served_and_withheld" => {
                let facts: Vec<MemoryFact> = resp.body.get("facts").and_then(|f| f.as_array())
                    .map(|a| a.iter().filter_map(|v| wire_fact(&self.id, v)).collect())
                    .unwrap_or_default();
                if facts.is_empty() {
                    Ok(MemoryRetrieval::Absent { reason: Some(state.to_string()) })
                } else {
                    Ok(MemoryRetrieval::Facts(facts))
                }
            }
            // `withheld` — the source HAS related material but didn't return it
            // for this query (near/ambiguous match). Surface the reason; it must
            // NOT read as "no such memory".
            "withheld" => Ok(MemoryRetrieval::Withheld {
                reason: resp.body.get("reason").and_then(|r| r.as_str()).map(str::to_string) }),
            // absent / anything else — healthy, nothing served.
            other => Ok(MemoryRetrieval::Absent {
                reason: Some(resp.body.get("reason").and_then(|r| r.as_str()).unwrap_or(other).to_string()) }),
        }
    }

    async fn ingest(&self, turn: &TurnRecord) -> Result<usize, MemoryError> {
        if self.read_only { return Ok(0); }
        let text = turn.user_msg.trim();
        if text.is_empty() { return Ok(0); }
        let body = json!({ "user_id": turn.user_id, "text": text, "source": "mira" });
        match self.caller.request("POST", "/add", Some(body)).await {
            Ok(r) if (200..300).contains(&r.status) => Ok(1),
            // 404 = the service's derived write-gate is closed; treat as a no-op,
            // never fatal to the turn.
            Ok(r) if r.status == 404 => { tracing::warn!("{} write-gate closed (404): {}", self.id, r.body); Ok(0) }
            Ok(r) => Err(MemoryError::Backend(format!("add HTTP {}", r.status))),
            Err(e) => Err(MemoryError::Unavailable(e.to_string())),
        }
    }

    async fn forget(&self, sel: &ForgetSelector) -> Result<usize, MemoryError> {
        if self.read_only {
            return Err(MemoryError::InvalidRequest("provider is read-only".into()));
        }
        // Always key forget on the AUTHENTICATED caller, never on any user part
        // embedded in the id — a crafted `other_user/item` must not reach another
        // user's data. `item_id` is the trailing segment of the SPI id.
        let (caller, item, uname, email) = match sel {
            ForgetSelector::Id { caller_user_id, item, caller_username, caller_email } =>
                (caller_user_id.as_str(), item.as_str(), caller_username.as_deref(), caller_email.as_deref()),
            ForgetSelector::AllForUser(_) =>
                return Err(MemoryError::InvalidRequest("http provider: forget-all not supported".into())),
        };
        let item_id = item.rsplit_once('/').map(|(_, i)| i).unwrap_or(item);
        // Key on the caller's OWN identity, mapped through `user_id_field` (so a
        // service keyed on username/email matches), mirrored under the field name
        // like search does. Never trust an identity embedded in the id.
        let ident = self.pick_user_id(caller, uname, email);
        let mut body = json!({ "user_id": ident, "item_id": item_id });
        if let (Some(obj), Some(k)) = (body.as_object_mut(), self.identity_mirror_key()) {
            obj.insert(k.into(), json!(ident));
        }
        match self.caller.request("POST", "/forget", Some(body)).await {
            Ok(r) if (200..300).contains(&r.status) => Ok(1),
            Ok(r) if r.status == 404 => Ok(0),
            Ok(r) => Err(MemoryError::Backend(format!("forget HTTP {}", r.status))),
            Err(e) => Err(MemoryError::Unavailable(e.to_string())),
        }
    }

    async fn catalog(&self, p: &CatalogParams) -> Result<CatalogResult, MemoryError> {
        if p.user_id.trim().is_empty() {
            return Err(MemoryError::InvalidRequest("user_id is required".into()));
        }
        // POST (not GET) so the user_id and any filter stay out of the URL/logs,
        // the same reason the SPI prefers POST for /search.
        let ident = self.pick_user_id(&p.user_id, p.username.as_deref(), p.email.as_deref());
        let mut body = json!({
            "user_id": ident,
            "page": p.page, "per_page": p.per_page,
        });
        let obj = body.as_object_mut().unwrap();
        // Mirror the identity under the configured field name (username/email).
        if let Some(k) = self.identity_mirror_key() { obj.insert(k.into(), json!(ident)); }
        // prefix XOR contains is the service's rule; pass through whatever is set
        // (both-set is the caller's error and surfaces as a 400 below).
        for (k, v) in [("subject", &p.subject), ("prefix", &p.prefix), ("contains", &p.contains)] {
            if let Some(s) = v.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                obj.insert(k.to_string(), json!(s));
            }
        }
        let resp = match self.caller.request("POST", "/catalog", Some(body)).await {
            Ok(r) => r,
            Err(e) => return Err(MemoryError::Unavailable(e.to_string())),
        };
        // 404 → this service predates /catalog; report as not-available (the
        // route turns that into `available:false`, not an error to the user).
        if resp.status == 404 {
            return Err(MemoryError::InvalidRequest("catalog route not available".into()));
        }
        if resp.status == 400 {
            return Err(MemoryError::InvalidRequest(
                resp.body.get("reason").and_then(|r| r.as_str()).unwrap_or("bad request").to_string()));
        }
        if resp.status >= 500 {
            return Err(MemoryError::Unavailable(
                resp.body.get("reason").and_then(|r| r.as_str()).unwrap_or("service error").to_string()));
        }
        let state = resp.body.get("state").and_then(|s| s.as_str()).unwrap_or("served").to_string();
        let catalog = resp.body.get("catalog").cloned().unwrap_or(Value::Null);
        Ok(CatalogResult { state, catalog })
    }
}

/// Production [`HttpCaller`] over the shared rustls `reqwest` client. Loopback
/// enforcement is applied at construction in `ProviderSet` via the locality gate.
pub struct ReqwestHttpCaller {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl ReqwestHttpCaller {
    /// `api_key_file`, when given, is read once; a missing/blank file is an error.
    /// `tls_ca_file`, when given, is a PEM CA/cert added to the TLS roots so a
    /// service behind a self-signed / private-CA certificate is trusted — MIRA's
    /// reqwest client uses the bundled webpki roots, NOT the OS trust store, so a
    /// cert installed via `update-ca-certificates` is otherwise rejected.
    pub fn new(
        base_url: impl Into<String>,
        api_key_file: Option<&str>,
        tls_ca_file: Option<&str>,
    ) -> Result<Self, MemoryError> {
        let api_key = match api_key_file {
            Some(p) if !p.trim().is_empty() => {
                let key = std::fs::read_to_string(crate::config::expand_path(p))
                    .map_err(|e| MemoryError::InvalidRequest(format!("api_key_file: {e}")))?
                    .trim().to_string();
                if key.is_empty() { return Err(MemoryError::InvalidRequest("api_key_file is empty".into())); }
                Some(key)
            }
            _ => None,
        };
        let mut builder = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(10));
        if let Some(p) = tls_ca_file.map(str::trim).filter(|s| !s.is_empty()) {
            let pem = std::fs::read(crate::config::expand_path(p))
                .map_err(|e| MemoryError::InvalidRequest(format!("tls_ca_file: {e}")))?;
            let cert = reqwest::Certificate::from_pem(&pem)
                .map_err(|e| MemoryError::InvalidRequest(format!("tls_ca_file not a valid PEM certificate: {e}")))?;
            builder = builder.add_root_certificate(cert);
        }
        let client = builder.build()
            .map_err(|e| MemoryError::Backend(format!("http client: {e}")))?;
        Ok(Self { client, base_url: base_url.into().trim_end_matches('/').to_string(), api_key })
    }
}

#[async_trait]
impl HttpCaller for ReqwestHttpCaller {
    async fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<HttpResp, MemoryError> {
        let url = format!("{}{}", self.base_url, path);
        let m = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| MemoryError::InvalidRequest(format!("bad method: {e}")))?;
        let mut req = self.client.request(m, &url);
        if let Some(k) = &self.api_key { req = req.header("X-Api-Key", k); }
        if let Some(b) = body { req = req.json(&b); }
        let resp = req.send().await
            .map_err(|e| {
                // Log the transport reason (e.g. a TLS trust failure) — it
                // otherwise only rides the "unavailable" context block.
                let reason = e.without_url().to_string();
                tracing::warn!("memory http {method} {path} failed: {reason}");
                MemoryError::Unavailable(reason)
            })?;
        let status = resp.status().as_u16();
        let body = resp.json::<Value>().await.unwrap_or(Value::Null);
        Ok(HttpResp { status, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Mock {
        last: Mutex<Option<(String, String, Option<Value>)>>,
        get: std::collections::HashMap<String, HttpResp>,
        post: std::collections::HashMap<String, HttpResp>,
    }
    #[async_trait]
    impl HttpCaller for Mock {
        async fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<HttpResp, MemoryError> {
            *self.last.lock().unwrap() = Some((method.into(), path.into(), body));
            let map = if method == "GET" { &self.get } else { &self.post };
            let r = map.get(path).ok_or_else(|| MemoryError::Unavailable("no mock".into()))?;
            Ok(HttpResp { status: r.status, body: r.body.clone() })
        }
    }
    fn provider(post: Vec<(&str, u16, Value)>, read_only: bool) -> (HttpMemoryProvider, Arc<Mock>) {
        let mock = Arc::new(Mock {
            last: Mutex::new(None),
            get: std::collections::HashMap::new(),
            post: post.into_iter().map(|(p, s, b)| (p.to_string(), HttpResp { status: s, body: b })).collect(),
        });
        (HttpMemoryProvider::new("ext-http", mock.clone(), read_only, MemoryLocality::Loopback, UserIdField::Id), mock)
    }

    #[tokio::test]
    async fn search_served_maps_facts_and_trust() {
        let resp = json!({ "state": "served", "facts": [
            { "content": "User works at Anthropic", "trust": "verified", "trust_label": "attested by source", "provenance": "records:abc" }
        ]});
        let (p, mock) = provider(vec![("/search", 200, resp)], true);
        let got = p.retrieve(&MemoryQuery::new("tarek", "employer", 5)).await.unwrap();
        let (_, path, body) = mock.last.lock().unwrap().clone().unwrap();
        assert_eq!(path, "/search");
        assert_eq!(body.unwrap()["user_id"], "tarek");
        match got {
            MemoryRetrieval::Facts(f) => {
                assert_eq!(f[0].content, "User works at Anthropic");
                assert_eq!(f[0].trust, Trust::Verified);
                assert_eq!(f[0].source_id, "ext-http");
                assert_eq!(f[0].provider_meta["trust_label"], "attested by source");
            }
            other => panic!("expected facts, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn probe_infers_catalog_from_routes_and_defaults_citations_on() {
        // M6: a service that declares capabilities via `routes_live`/`verbs`
        // (not flat booleans) must still have catalog gated ON, and citations
        // default ON (trust is inherent to the SPI).
        let caps_body = json!({
            "read": true, "write": true,
            "verbs": { "read": ["/search", "/catalog"], "write": ["/add", "/forget"] },
            "routes_live": ["/health", "/search", "/catalog", "/add", "/forget"]
        });
        let mock = Arc::new(Mock {
            last: Mutex::new(None),
            get: std::collections::HashMap::from([("/capabilities".to_string(), HttpResp { status: 200, body: caps_body })]),
            post: std::collections::HashMap::new(),
        });
        let p = HttpMemoryProvider::new("ext-http", mock, false, MemoryLocality::Loopback, UserIdField::Id);
        p.probe().await;
        let c = p.capabilities();
        assert!(c.catalog, "catalog inferred from routes_live/verbs");
        assert!(c.citations, "citations default ON for an SPI service");
        assert!(c.write && c.forget, "write/forget inferred from verbs");
    }

    #[tokio::test]
    async fn probe_missing_endpoint_defaults_catalog_off_keeps_recall() {
        // No /capabilities (the Mock errors) → never advertise catalog
        // unconfirmed (M6), but read/citations stay usable so recall still works.
        let (p, _m) = provider(vec![], true);
        p.probe().await;
        let c = p.capabilities();
        assert!(!c.catalog, "catalog is off until a probe confirms it");
        assert!(c.read && c.citations, "recall + citations remain usable");
    }

    #[tokio::test]
    async fn search_maps_reported_trust_and_carries_spi_fields() {
        // M7: `reported` must not collapse to unverified, and the documented SPI
        // descriptors (source/subject/relation) must survive in provider_meta.
        let resp = json!({ "state": "served", "facts": [
            { "content": "User's sister is Leila", "trust": "reported",
              "source": "news", "subject": "Leila", "relation": "sibling" }
        ]});
        let (p, _mock) = provider(vec![("/search", 200, resp)], true);
        let got = p.retrieve(&MemoryQuery::new("tarek", "sister", 5)).await.unwrap();
        match got {
            MemoryRetrieval::Facts(f) => {
                assert_eq!(f[0].trust, Trust::Reported);
                assert_eq!(f[0].provider_meta["subject"], "Leila");
                assert_eq!(f[0].provider_meta["relation"], "sibling");
                assert_eq!(f[0].provider_meta["source"], "news");
            }
            other => panic!("expected facts, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn absent_state_is_absent_not_unavailable() {
        let (p, _) = provider(vec![("/search", 200, json!({ "state": "absent", "reason": "no_gated_free_text_door__D037_QUEUED" }))], true);
        match p.retrieve(&MemoryQuery::new("tarek", "x", 5)).await.unwrap() {
            MemoryRetrieval::Absent { reason } => assert!(reason.unwrap().contains("D037")),
            other => panic!("expected absent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_503_and_state_unavailable_are_unavailable() {
        let (p, _) = provider(vec![("/search", 503, json!({ "reason": "lost key" }))], true);
        assert!(matches!(p.retrieve(&MemoryQuery::new("t", "x", 5)).await.unwrap(), MemoryRetrieval::Unavailable { .. }));
        let (p2, _) = provider(vec![("/search", 200, json!({ "state": "unavailable", "reason": "store not built" }))], true);
        assert!(matches!(p2.retrieve(&MemoryQuery::new("t", "x", 5)).await.unwrap(), MemoryRetrieval::Unavailable { .. }));
    }

    #[tokio::test]
    async fn auth_and_notfound_statuses_are_unavailable_not_absent() {
        // A bad key (401/403), a wrong path (404) or a bad gateway (502) must
        // never read as "the user has no memories".
        for code in [401u16, 403, 404, 502] {
            let (p, _) = provider(vec![("/search", code, serde_json::json!({}))], true);
            assert!(matches!(
                p.retrieve(&MemoryQuery::new("t", "x", 5)).await.unwrap(),
                MemoryRetrieval::Unavailable { .. }),
                "HTTP {code} must map to Unavailable, not Absent");
        }
    }

    #[tokio::test]
    async fn status_400_is_err() {
        let (p, _) = provider(vec![("/search", 400, json!({ "reason": "malformed reference" }))], true);
        assert!(matches!(p.retrieve(&MemoryQuery::new("t", "x", 5)).await, Err(MemoryError::InvalidRequest(_))));
    }

    #[tokio::test]
    async fn read_only_never_writes() {
        let (p, _) = provider(vec![], true);
        assert!(!p.capabilities().write);
        assert_eq!(p.ingest(&TurnRecord {
            user_id: "t".into(), agent_id: None, thread_id: None, channel: "c".into(),
            user_msg: "remember this".into(), assistant_msg: String::new(), conversation_id: None,
        }).await.unwrap(), 0, "read-only ingest is a no-op");
    }

    #[tokio::test]
    async fn writable_add_posts_and_gate_404_is_noop() {
        let (p, mock) = provider(vec![("/add", 200, json!({ "ok": true }))], false);
        assert!(p.capabilities().write);
        assert_eq!(p.ingest(&TurnRecord {
            user_id: "tarek".into(), agent_id: None, thread_id: None, channel: "web".into(),
            user_msg: "I prefer window seats".into(), assistant_msg: String::new(), conversation_id: None,
        }).await.unwrap(), 1);
        let (_, path, body) = mock.last.lock().unwrap().clone().unwrap();
        assert_eq!(path, "/add");
        assert_eq!(body.unwrap()["text"], "I prefer window seats");

        // write-gate closed → 404 → no-op, not fatal.
        let (p2, _) = provider(vec![("/add", 404, json!({ "reason": "no_acceptance_artifact" }))], false);
        assert_eq!(p2.ingest(&TurnRecord {
            user_id: "t".into(), agent_id: None, thread_id: None, channel: "c".into(),
            user_msg: "x".into(), assistant_msg: String::new(), conversation_id: None,
        }).await.unwrap(), 0);
    }

    // user_id_field — the provider sends the configured identity (with a UUID
    // fallback when the chosen field is blank), parsed leniently.
    #[tokio::test]
    async fn user_id_field_selects_the_sent_identity() {
        fn q() -> MemoryQuery {
            let mut q = MemoryQuery::new("uuid-123", "employer", 5);
            q.username = Some("tarek".into());
            q.email = Some("tarek@example.com".into());
            q
        }
        async fn sent(field: UserIdField, query: MemoryQuery) -> String {
            let mock = Arc::new(Mock {
                last: Mutex::new(None),
                get: std::collections::HashMap::new(),
                post: [("/search".to_string(), HttpResp { status: 200, body: json!({"state":"absent"}) })]
                    .into_iter().collect(),
            });
            let p = HttpMemoryProvider::new("ext-http", mock.clone(), true, MemoryLocality::Loopback, field);
            p.retrieve(&query).await.unwrap();
            let (_, _, body) = mock.last.lock().unwrap().clone().unwrap();
            body.unwrap()["user_id"].as_str().unwrap().to_string()
        }
        assert_eq!(sent(UserIdField::Id, q()).await, "uuid-123", "default sends the UUID");
        assert_eq!(sent(UserIdField::Username, q()).await, "tarek");
        assert_eq!(sent(UserIdField::Email, q()).await, "tarek@example.com");
        // Fallback: Email configured but absent → the UUID.
        let mut no_email = q(); no_email.email = None;
        assert_eq!(sent(UserIdField::Email, no_email).await, "uuid-123", "blank field falls back to UUID");
        // Parse is lenient + defaults to Id.
        assert_eq!(UserIdField::parse(Some("USERNAME")), UserIdField::Username);
        assert_eq!(UserIdField::parse(Some("email")), UserIdField::Email);
        assert_eq!(UserIdField::parse(Some("whatever")), UserIdField::Id);
        assert_eq!(UserIdField::parse(None), UserIdField::Id);
    }

    #[tokio::test]
    async fn identity_is_mirrored_under_the_configured_field_name() {
        // A service that reads identity from a field named `username` (not the
        // canonical `user_id`) must still receive the value. MIRA sends both.
        let mock = Arc::new(Mock {
            last: Mutex::new(None),
            get: std::collections::HashMap::new(),
            post: [("/search".to_string(), HttpResp { status: 200, body: json!({"state":"absent"}) })]
                .into_iter().collect(),
        });
        let p = HttpMemoryProvider::new("ext-http", mock.clone(), true, MemoryLocality::Loopback, UserIdField::Username);
        let mut q = MemoryQuery::new("uuid-1", "x", 5);
        q.username = Some("tarek".into());
        p.retrieve(&q).await.unwrap();
        let (_, _, body) = mock.last.lock().unwrap().clone().unwrap();
        let body = body.unwrap();
        assert_eq!(body["user_id"], "tarek", "SPI-canonical user_id carries the value");
        assert_eq!(body["username"], "tarek", "mirrored under the configured field name");
        // Default (Id) sends ONLY user_id, no mirror key.
        let p2 = HttpMemoryProvider::new("ext-http", mock.clone(), true, MemoryLocality::Loopback, UserIdField::Id);
        p2.retrieve(&MemoryQuery::new("uuid-1", "x", 5)).await.unwrap();
        let (_, _, body2) = mock.last.lock().unwrap().clone().unwrap();
        let body2 = body2.unwrap();
        assert_eq!(body2["user_id"], "uuid-1");
        assert!(body2.get("username").is_none(), "no mirror key for the default id field");
    }

    #[test]
    fn tls_ca_file_errors_are_graceful() {
        // No CA file → builds fine (bundled roots only).
        assert!(ReqwestHttpCaller::new("https://127.0.0.1:9", None, None).is_ok());
        // A missing CA file → a clear InvalidRequest at construction, not a panic
        // and not a silent skip. (reqwest defers actual PEM/cert validation to
        // connect time, so a malformed-but-present file surfaces later as an
        // Unavailable transport error, which is acceptable.)
        assert!(matches!(
            ReqwestHttpCaller::new("https://127.0.0.1:9", None, Some("/no/such/ca.pem")),
            Err(MemoryError::InvalidRequest(_))));
    }

    #[tokio::test]
    async fn http_read_only_passes_read_contract() {
        let (p, _) = provider(vec![("/search", 200, json!({ "state": "absent" }))], true);
        super::super::conformance::run_read_contract(&p).await.expect("http read contract");
    }

    #[tokio::test]
    async fn catalog_posts_identity_and_filters_and_passes_through() {
        // (catalog capability is gated on a probe now; the catalog() method itself
        // works regardless — that's what this test exercises.)
        let cat = json!({
            "state": "served",
            "catalog": {
                "scope": { "user_id": "tarek", "is_owner": true },
                "counts": { "subjects": 12, "facts": 40, "by_trust": { "verified": 3, "unverified": 5 } },
                "subject_detail": { "name": "Jordan Vale", "n_facts": 5 }
            }
        });
        let (p, mock) = provider(vec![("/catalog", 200, cat)], true);
        let mut params = CatalogParams::new("uuid-1");
        params.username = Some("tarek".into());
        params.subject = Some("Jordan Vale".into());
        params.prefix = Some("Jor".into());
        // user_id_field defaults to Id → sends the UUID.
        let res = p.catalog(&params).await.expect("catalog ok");
        assert_eq!(res.state, "served");
        assert_eq!(res.catalog["counts"]["by_trust"]["unverified"], 5);
        assert_eq!(res.catalog["subject_detail"]["n_facts"], 5);
        let (_, path, body) = mock.last.lock().unwrap().clone().unwrap();
        assert_eq!(path, "/catalog");
        let body = body.unwrap();
        assert_eq!(body["user_id"], "uuid-1");
        assert_eq!(body["subject"], "Jordan Vale");
        assert_eq!(body["prefix"], "Jor");
    }

    #[tokio::test]
    async fn catalog_404_reports_not_available() {
        let (p, _) = provider(vec![("/catalog", 404, json!({ "reason": "no such route" }))], true);
        assert!(matches!(
            p.catalog(&CatalogParams::new("tarek")).await,
            Err(MemoryError::InvalidRequest(_))), "404 → not available, not a 5xx error");
    }
}
