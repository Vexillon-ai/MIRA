// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/http_policy.rs
//! Shared HTTP policy for Tier 2 web tools.
//!
//! Every outbound request made by a Tier 2 tool (web_fetch, url_preview,
//! any search backend that talks to a remote endpoint) goes through this
//! layer. The layer owns:
//!
//! - **SSRF guard**: blocks loopback, private, link-local, cloud metadata,
//! and IPv6-equivalent ranges. DNS rebinding is defeated by resolving
//! once, checking IPs, and pinning the chosen IP into the request.
//! - **Rate limits**: per-user + per-(user, domain) token buckets.
//! - **Size / time caps**: raw body ≤ N bytes, request timeout ≤ T secs.
//! - **Redirect handling**: manual, each hop revalidated.
//! - **Schemes**: only `http` / `https`; everything else rejected.
//!
//! The only escape hatch is a single user-configured SearXNG host+port
//! (to support home-server setups). That one host bypasses the IP-range
//! block but still gets every other check (scheme, size, time, rate).
//!
//! See `design-docs/phase7-tier2-web-tools.md` for the full security model.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use reqwest::redirect::Policy as RedirectPolicy;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::net::lookup_host;
use tracing::{debug, warn};
use url::Url;

// ── Public types ─────────────────────────────────────────────────────────────

// Configured bounds + policy state. Construct once in the gateway builder
// and `Arc::clone` into every Tier 2 tool.
#[derive(Clone)]
pub struct HttpPolicy {
    inner: Arc<HttpPolicyInner>,
}

struct HttpPolicyInner {
    user_agent:      String,
    max_body_bytes:  u64,
    request_timeout: Duration,
    max_redirects:   usize,
    denylist:        HashSet<String>,
    allowlist:       Option<HashSet<String>>, // None = default-allow mode
    // The one host:port where SSRF range-blocks are bypassed. Used only to
    // support a user-run SearXNG on their LAN.
    searxng_exception: Option<(String, u16)>,
    // Wrapped in `Arc` so `with_policy_engine` can hand back a fresh
    // `HttpPolicyInner` that shares the same rate-limit state. Without
    // this, attaching the engine after construction would reset the
    // per-user buckets — a subtle DoS amplification.
    rate:            Arc<RateLimiter>,
    // when set, calls made via the `*_with_context` methods
    // emit `NetworkEgress` events and may be denied. Calls made via the
    // legacy context-free `get` / `get_for_search` methods bypass the
    // engine (callers that don't yet have agent context can still issue
    // requests, but they sit outside the policy decision audit trail).
    policy_engine: Option<Arc<dyn crate::policy::PolicyEngine>>,
}

// Configuration input to [`HttpPolicy::new`]. Mirrors `[security.http]` +
// per-tool caps in `MiraConfig`.
#[derive(Debug, Clone)]
pub struct HttpPolicyConfig {
    pub user_agent:         String,
    pub max_body_bytes:     u64,
    pub request_timeout:    Duration,
    pub max_redirects:      usize,
    pub denylist:           Vec<String>,
    pub allowlist:          Vec<String>,
    pub allowlist_only:     bool,
    pub searxng_exception:  Option<(String, u16)>,
    pub rate_user_per_min:  u32,
    pub rate_user_per_hour: u32,
    pub rate_user_per_domain_per_min: u32,
    pub rate_search_per_min: u32,
}

impl Default for HttpPolicyConfig {
    fn default() -> Self {
        Self {
            user_agent:         format!("MIRA/{}", env!("CARGO_PKG_VERSION")),
            max_body_bytes:     5 * 1024 * 1024,
            request_timeout:    Duration::from_secs(30),
            max_redirects:      5,
            denylist:           vec![],
            allowlist:          vec![],
            allowlist_only:     false,
            searxng_exception:  None,
            rate_user_per_min:  60,
            rate_user_per_hour: 600,
            rate_user_per_domain_per_min: 10,
            rate_search_per_min: 30,
        }
    }
}

// Distinguishes between the fetch-rate and search-rate buckets. Callers
// shouldn't need to think about `RequestPurpose` directly — `get` defaults
// to `Fetch`; search backends go through `get_for_search` which handles
// the purpose + optional auth headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestPurpose {
    Fetch,
    Search,
}

// Typed result returned from `HttpPolicy::get`. Tools convert this into
// their own shapes (readable text, OG tags, search JSON, etc.).
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub final_url:    String,
    pub status:       u16,
    pub content_type: Option<String>,
    pub body:         Vec<u8>,
    pub truncated:    bool,
    // Set to the `Location` header when the status is 3xx and the header
    // was present. `HttpPolicy::get` uses this to drive the manual redirect
    // loop; tools that call `do_one_hop` directly (none today) could use it
    // to introspect redirects without a full follow.
    pub redirect_to:  Option<String>,
}

// Typed policy errors. Tools surface these with a stable string prefix so
// the model can recognise and reason about them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PolicyError {
    BlockedHost        { reason: String, host: String },
    BlockedScheme      { scheme: String },
    DenylistedDomain   { host: String },
    AllowlistOnly      { host: String },
    RateLimited        { retry_after_ms: u64, scope: String },
    TooLarge           { limit: u64, observed: u64 },
    Timeout,
    TooManyRedirects,
    Http               { status: u16, url: String },
    DnsResolution      { host: String, detail: String },
    InvalidUrl         (String),
    Transport          (String),
    // the configured policy engine returned `Deny` for the
    // `NetworkEgress` event. `rule` is the engine's stable rule id
    // (e.g. `"network-allowlist"`); `reason` is the human-readable
    // detail surfaced to the model.
    PolicyDenied       { rule: String, reason: String },
}

// Caller-supplied context for a policy-aware request. `user_id` drives
// the existing rate-limit bucket; `agent_id` + `skill_id` (when known)
// flow into the `NetworkEgress` event the engine evaluates.
// // Tools that don't yet have agent context can keep using the bare
// `get` / `get_for_search` methods — they take only `user_id` and
// skip the engine consult. As Phase C/D work plumbs the agent
// identity through, more call sites move to the `*_with_context`
// variants and the engine sees more events.
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub user_id:  String,
    pub agent_id: Option<crate::agent::instance::AgentId>,
    pub skill_id: Option<String>,
}

impl RequestContext {
    // Convenience for the common "I have a user_id, no agent" case
    // equivalent to the legacy `get(url, user_id)` semantics, but
    // callable through the new context-aware path so tests can
    // exercise the engine seam without faking an AgentId.
    pub fn user_only(user_id: impl Into<String>) -> Self {
        Self { user_id: user_id.into(), agent_id: None, skill_id: None }
    }
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyError::BlockedHost { reason, host }  => write!(f, "blocked host {}: {}", host, reason),
            PolicyError::BlockedScheme { scheme }      => write!(f, "blocked scheme: {}", scheme),
            PolicyError::DenylistedDomain { host }     => write!(f, "denylisted domain: {}", host),
            PolicyError::AllowlistOnly { host }        => write!(f, "not in allowlist: {}", host),
            PolicyError::RateLimited { scope, retry_after_ms } =>
                write!(f, "rate limited ({}), retry after {}ms", scope, retry_after_ms),
            PolicyError::TooLarge { limit, observed }  => write!(f, "body too large: {} > {} bytes", observed, limit),
            PolicyError::Timeout                        => write!(f, "request timed out"),
            PolicyError::TooManyRedirects               => write!(f, "too many redirects"),
            PolicyError::Http { status, url }           => write!(f, "http {}: {}", status, url),
            PolicyError::DnsResolution { host, detail } => write!(f, "dns resolution for {}: {}", host, detail),
            PolicyError::InvalidUrl(s)                  => write!(f, "invalid url: {}", s),
            PolicyError::Transport(s)                   => write!(f, "transport: {}", s),
            PolicyError::PolicyDenied { rule, reason }  => write!(f, "policy/{} denied: {}", rule, reason),
        }
    }
}

impl std::error::Error for PolicyError {}

// ── Construction ─────────────────────────────────────────────────────────────

impl HttpPolicy {
    pub fn new(cfg: HttpPolicyConfig) -> Self {
        let allowlist = if cfg.allowlist_only {
            Some(cfg.allowlist.into_iter().map(|s| s.to_lowercase()).collect())
        } else {
            None
        };
        let denylist = cfg.denylist.into_iter().map(|s| s.to_lowercase()).collect();

        let rate = RateLimiter::new(
            cfg.rate_user_per_min,
            cfg.rate_user_per_hour,
            cfg.rate_user_per_domain_per_min,
            cfg.rate_search_per_min,
        );

        Self {
            inner: Arc::new(HttpPolicyInner {
                user_agent:        cfg.user_agent,
                max_body_bytes:    cfg.max_body_bytes,
                request_timeout:   cfg.request_timeout,
                max_redirects:     cfg.max_redirects,
                denylist,
                allowlist,
                searxng_exception: cfg.searxng_exception,
                rate:              Arc::new(rate),
                policy_engine:     None,
            }),
        }
    }

    // attach a policy engine. Calls made through the
    // `*_with_context` methods will emit `NetworkEgress` events and
    // refuse on `Deny`. The new `HttpPolicy` shares the same
    // rate-limit state as the original (rate buckets are wrapped in
    // `Arc`) so attaching an engine doesn't accidentally reset user
    // quotas.
    pub fn with_policy_engine(self, engine: Arc<dyn crate::policy::PolicyEngine>) -> Self {
        let inner = self.inner;
        Self {
            inner: Arc::new(HttpPolicyInner {
                user_agent:        inner.user_agent.clone(),
                max_body_bytes:    inner.max_body_bytes,
                request_timeout:   inner.request_timeout,
                max_redirects:     inner.max_redirects,
                denylist:          inner.denylist.clone(),
                allowlist:         inner.allowlist.clone(),
                searxng_exception: inner.searxng_exception.clone(),
                rate:              Arc::clone(&inner.rate),
                policy_engine:     Some(engine),
            }),
        }
    }

    pub fn user_agent(&self) -> &str { &self.inner.user_agent }

    // Returns true iff a policy engine has been wired in. Used by
    // tests + by tools that want to know whether to bother
    // constructing a `RequestContext`.
    pub fn has_policy_engine(&self) -> bool {
        self.inner.policy_engine.is_some()
    }

    // ── Core request flow ────────────────────────────────────────────────────

    // GET `url`, resolving and SSRF-checking every hop. Rate-limits under
    // the caller's `user_id` using the fetch bucket (web_fetch, url_preview).
    pub async fn get(&self, url: &str, user_id: &str) -> Result<HttpResponse, PolicyError> {
        let host_for_rate = Url::parse(url).ok().and_then(|u| u.host_str().map(|s| s.to_owned()));
        self.inner.rate.check_fetch(user_id, host_for_rate.as_deref().unwrap_or(""))?;
        self.follow(url, &[]).await
    }

    // GET `url` for a search backend — uses the separate search rate bucket
    // (no per-domain throttle) and supports custom request headers (the
    // primary use case is Brave's `X-Subscription-Token`).
    pub async fn get_for_search(
        &self,
        url:     &str,
        user_id: &str,
        headers: &[(&str, &str)],
    ) -> Result<HttpResponse, PolicyError> {
        self.inner.rate.check_search(user_id)?;
        self.follow(url, headers).await
    }

    // context-aware variant of [`Self::get`]. Consults
    // the wired-in [`crate::policy::PolicyEngine`] (if any) before
    // hitting the rate limiter / SSRF guard. Falls through to the
    // same redirect-following machinery on Allow.
    //     // Tools that have an `_agent_id` / `_skill_id` injected into
    // their args (the standard pattern from the agent's tool loop)
    // build a `RequestContext` and call this. Tools without that
    // context stay on `get`.
    pub async fn get_with_context(
        &self,
        url: &str,
        ctx: &RequestContext,
    ) -> Result<HttpResponse, PolicyError> {
        self.evaluate_engine(url, ctx).await?;
        self.get(url, &ctx.user_id).await
    }

    // Search-bucket variant of [`Self::get_with_context`].
    pub async fn get_for_search_with_context(
        &self,
        url:     &str,
        ctx:     &RequestContext,
        headers: &[(&str, &str)],
    ) -> Result<HttpResponse, PolicyError> {
        self.evaluate_engine(url, ctx).await?;
        self.get_for_search(url, &ctx.user_id, headers).await
    }

    // Build a `NetworkEgress` event for the engine and translate any
    // `Deny` into [`PolicyError::PolicyDenied`]. Returns `Ok(())` when
    // no engine is wired or when the engine allows. Bare-URL parse
    // failures here surface as `PolicyError::InvalidUrl` so the
    // caller doesn't get a confusing "policy denied" for what's
    // actually a bad URL.
    async fn evaluate_engine(
        &self,
        url: &str,
        ctx: &RequestContext,
    ) -> Result<(), PolicyError> {
        let Some(engine) = &self.inner.policy_engine else { return Ok(()); };
        // We need an agent_id to attribute the NetworkEgress to;
        // without one the engine can't apply per-Skill rules
        // sensibly. Skip the consult in that case — the existing
        // SSRF / denylist / rate-limit checks still run.
        let Some(agent_id) = ctx.agent_id else { return Ok(()); };

        let parsed = url::Url::parse(url)
            .map_err(|_| PolicyError::InvalidUrl(url.to_owned()))?;
        let host   = parsed.host_str().unwrap_or("").to_owned();
        let scheme = parsed.scheme().to_owned();

        let event = crate::policy::PolicyEvent::NetworkEgress {
            agent_id,
            skill_id: ctx.skill_id.clone(),
            url: url.to_owned(),
            host,
            scheme,
            direction: crate::policy::NetworkEgressDirection::Outbound,
        };
        match engine.evaluate(&event).await {
            crate::policy::PolicyDecision::Allow => Ok(()),
            crate::policy::PolicyDecision::Deny { rule, reason } => {
                Err(PolicyError::PolicyDenied { rule, reason })
            }
        }
    }

    // Shared redirect-loop used by both `get` and `get_for_search`. Does
    // *not* consult the rate limiter — callers own that choice.
    async fn follow(
        &self,
        url:     &str,
        headers: &[(&str, &str)],
    ) -> Result<HttpResponse, PolicyError> {
        let mut current = url.to_owned();
        for hop in 0..=self.inner.max_redirects {
            let resp = self.do_one_hop(&current, headers).await?;
            if let Some(location) = resp.redirect_to.as_deref() {
                if hop == self.inner.max_redirects {
                    return Err(PolicyError::TooManyRedirects);
                }
                let next = resolve_redirect(&current, location)
                    .ok_or_else(|| PolicyError::InvalidUrl(format!("bad redirect target: {}", location)))?;
                debug!("http_policy: redirect {} -> {}", current, next);
                current = next;
                continue;
            }
            return Ok(resp);
        }
        Err(PolicyError::TooManyRedirects)
    }

    async fn do_one_hop(
        &self,
        url:     &str,
        headers: &[(&str, &str)],
    ) -> Result<HttpResponse, PolicyError> {
        let parsed = Url::parse(url).map_err(|_| PolicyError::InvalidUrl(url.to_owned()))?;

        // 1. Scheme gate.
        match parsed.scheme() {
            "http" | "https" => {}
            other            => return Err(PolicyError::BlockedScheme { scheme: other.to_owned() }),
        }

        // 2. Host extraction.
        let host = parsed.host_str()
            .ok_or_else(|| PolicyError::InvalidUrl(format!("no host in {}", url)))?
            .to_lowercase();
        let port = parsed.port_or_known_default()
            .ok_or_else(|| PolicyError::InvalidUrl(format!("unknown default port for {}", parsed.scheme())))?;

        // 3. Denylist (domain).
        if self.inner.denylist.iter().any(|d| host_matches(&host, d)) {
            return Err(PolicyError::DenylistedDomain { host: host.clone() });
        }

        // 4. Allowlist-only mode.
        if let Some(allow) = &self.inner.allowlist {
            if !allow.iter().any(|d| host_matches(&host, d)) {
                return Err(PolicyError::AllowlistOnly { host: host.clone() });
            }
        }

        // 5. Resolve host → IPs.
        let resolve_target = format!("{}:{}", host, port);
        let addrs: Vec<SocketAddr> = match lookup_host(&resolve_target).await {
            Ok(iter) => iter.collect(),
            Err(e)   => return Err(PolicyError::DnsResolution {
                host:   host.clone(),
                detail: e.to_string(),
            }),
        };

        let searxng_bypass = matches!(
            &self.inner.searxng_exception,
            Some((h, p)) if h.eq_ignore_ascii_case(&host) && *p == port
        );

        // 6. Pick the first IP that passes the SSRF guard.
        let chosen: Option<SocketAddr> = addrs.into_iter().find(|addr| {
            if searxng_bypass { true } else { !is_ip_blocked(addr.ip()) }
        });

        let chosen = chosen.ok_or_else(|| PolicyError::BlockedHost {
            reason: if searxng_bypass { "no usable address".into() }
                    else { "all resolved ips are in blocked range".into() },
            host:   host.clone(),
        })?;

        // 7. Build a one-shot client with the pinned IP.
        let client = build_client(&self.inner.user_agent, self.inner.request_timeout, &host, chosen)?;

        // 8. Issue the request and stream the body with the size cap applied.
        let mut req = client.get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                if e.is_timeout() { return Err(PolicyError::Timeout); }
                return Err(PolicyError::Transport(e.to_string()));
            }
        };

        let status       = resp.status();
        let final_url    = resp.url().to_string();
        let content_type = resp.headers().get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_owned());
        let redirect_to  = if status.is_redirection() {
            resp.headers().get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_owned())
        } else {
            None
        };

        let (body, truncated) = read_body_capped(resp, self.inner.max_body_bytes).await?;

        Ok(HttpResponse {
            final_url,
            status: status.as_u16(),
            content_type,
            body,
            truncated,
            redirect_to,
        })
    }
}

// ── Reqwest client builder ───────────────────────────────────────────────────

fn build_client(
    user_agent:    &str,
    timeout:       Duration,
    host:          &str,
    pinned:        SocketAddr,
) -> Result<Client, PolicyError> {
    Client::builder()
        .user_agent(user_agent)
        .timeout(timeout)
.redirect(RedirectPolicy::none())      // we handle redirects manually
.resolve(host, pinned)                 // DNS-rebinding defense
.https_only(false)                     // we permit http; the scheme check already gates it
        .build()
        .map_err(|e| PolicyError::Transport(format!("client build: {}", e)))
}

// ── Body streaming with cap ──────────────────────────────────────────────────

async fn read_body_capped(
    resp:  reqwest::Response,
    limit: u64,
) -> Result<(Vec<u8>, bool), PolicyError> {
    use futures_util::StreamExt;

    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut total: u64 = 0;
    let mut truncated = false;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| PolicyError::Transport(format!("body stream: {}", e)))?;
        total += chunk.len() as u64;
        if total > limit {
            // Keep whatever we had below the cap but mark it truncated.
            let room = (limit as usize).saturating_sub(buf.len());
            if room > 0 {
                buf.extend_from_slice(&chunk[..room.min(chunk.len())]);
            }
            truncated = true;
            break;
        }
        buf.extend_from_slice(&chunk);
    }

    Ok((buf, truncated))
}

// ── Redirect helpers ─────────────────────────────────────────────────────────

// Resolve a `Location` header against a base URL.
fn resolve_redirect(base: &str, loc: &str) -> Option<String> {
    let b = Url::parse(base).ok()?;
    b.join(loc).ok().map(|u| u.to_string())
}

// ── SSRF classifier ──────────────────────────────────────────────────────────

// Returns true if the given IP is inside a range we refuse to reach.
fn is_ip_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_ipv4_blocked(v4),
        IpAddr::V6(v6) => is_ipv6_blocked(v6),
    }
}

fn is_ipv4_blocked(ip: Ipv4Addr) -> bool {
    if ip.is_loopback()       { return true; }  // 127.0.0.0/8
    if ip.is_private()        { return true; }  // 10/8, 172.16/12, 192.168/16
    if ip.is_link_local()     { return true; }  // 169.254/16 — cloud metadata
    if ip.is_multicast()      { return true; }  // 224/4
    if ip.is_broadcast()      { return true; }
    if ip.is_unspecified()    { return true; }  // 0.0.0.0
    if ip.is_documentation()  { return true; }  // TEST-NET ranges

    let octets = ip.octets();

    // CGNAT / RFC 6598 — 100.64.0.0/10
    if octets[0] == 100 && (octets[1] & 0b1100_0000) == 0b0100_0000 { return true; }

    // Reserved / future — 240/4 (minus broadcast which is handled above)
    if octets[0] >= 240 { return true; }

    false
}

fn is_ipv6_blocked(ip: Ipv6Addr) -> bool {
    if ip.is_loopback()    { return true; }
    if ip.is_unspecified() { return true; }
    if ip.is_multicast()   { return true; }

    // IPv4-mapped (::ffff:a.b.c.d) and IPv4-compatible (::a.b.c.d, deprecated)
    // dispatch to the IPv4 checker for the embedded address.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_ipv4_blocked(v4);
    }
    let segments = ip.segments();
    if segments[0..6] == [0, 0, 0, 0, 0, 0] && segments[6] != 0 {
        // ::a.b.c.d (deprecated IPv4-compatible) — check the last 32 bits.
        let v4 = Ipv4Addr::new(
            (segments[6] >> 8) as u8, (segments[6] & 0xff) as u8,
            (segments[7] >> 8) as u8, (segments[7] & 0xff) as u8,
        );
        return is_ipv4_blocked(v4);
    }

    // Link-local fe80::/10
    if (segments[0] & 0xffc0) == 0xfe80 { return true; }

    // Unique-local fc00::/7
    if (segments[0] & 0xfe00) == 0xfc00 { return true; }

    // Deprecated site-local fec0::/10 — defensive.
    if (segments[0] & 0xffc0) == 0xfec0 { return true; }

    false
}

// ── Domain match (denylist / allowlist) ──────────────────────────────────────

// `host_matches("api.example.com", "example.com")` → true.
// Exact-match OR suffix-match at a dot boundary. Never matches across
// dots in the middle of a label.
fn host_matches(host: &str, pattern: &str) -> bool {
    let host = host.trim_end_matches('.').to_lowercase();
    let pat  = pattern.trim_start_matches('.').trim_end_matches('.').to_lowercase();
    if host == pat { return true; }
    if host.ends_with(&format!(".{}", pat)) { return true; }
    false
}

// ── Rate limiter (token bucket) ──────────────────────────────────────────────

struct RateLimiter {
    per_user:          Mutex<HashMap<String, Bucket>>,
    per_user_domain:   DashMap<(String, String), Bucket>,
    user_per_min:      u32,
    user_per_hour:     u32,
    user_per_domain_per_min: u32,
    search_per_min:    u32,
}

impl RateLimiter {
    fn new(
        user_per_min:            u32,
        user_per_hour:           u32,
        user_per_domain_per_min: u32,
        search_per_min:          u32,
    ) -> Self {
        Self {
            per_user:                Mutex::new(HashMap::new()),
            per_user_domain:         DashMap::new(),
            user_per_min,
            user_per_hour,
            user_per_domain_per_min,
            search_per_min,
        }
    }

    // Fetch-path rate check. web_fetch + url_preview use this.
    fn check_fetch(&self, user_id: &str, host: &str) -> Result<(), PolicyError> {
        let now = Instant::now();

        // Per-user short + long buckets (shared with search-agnostic flows).
        {
            let mut map = self.per_user.lock().unwrap();
            let key = format!("{}:min", user_id);
            let bucket = map.entry(key).or_insert_with(|| Bucket::new(60));
            if !bucket.try_consume(self.user_per_min, now) {
                warn!("rate_limit: user={} exceeded {}/min", user_id, self.user_per_min);
                return Err(PolicyError::RateLimited {
                    retry_after_ms: bucket.retry_after_ms(now),
                    scope:          "user_per_minute".into(),
                });
            }
            let key_h = format!("{}:hour", user_id);
            let bucket_h = map.entry(key_h).or_insert_with(|| Bucket::new(3600));
            if !bucket_h.try_consume(self.user_per_hour, now) {
                warn!("rate_limit: user={} exceeded {}/hour", user_id, self.user_per_hour);
                return Err(PolicyError::RateLimited {
                    retry_after_ms: bucket_h.retry_after_ms(now),
                    scope:          "user_per_hour".into(),
                });
            }
        }

        // Per (user, domain) minute bucket — skip if host blank (unresolvable URL).
        if !host.is_empty() {
            let key = (user_id.to_owned(), host.to_owned());
            let mut bucket = self.per_user_domain.entry(key).or_insert_with(|| Bucket::new(60));
            if !bucket.try_consume(self.user_per_domain_per_min, now) {
                warn!("rate_limit: user={} host={} exceeded {}/min", user_id, host, self.user_per_domain_per_min);
                return Err(PolicyError::RateLimited {
                    retry_after_ms: bucket.retry_after_ms(now),
                    scope:          "user_domain_per_minute".into(),
                });
            }
        }

        Ok(())
    }

    // Search-path rate check. A separate bucket so cheap searches don't
    // eat into the fetch budget (and vice-versa). No per-domain cap —
    // searches are globally limited, and the tool level picks the backend.
    fn check_search(&self, user_id: &str) -> Result<(), PolicyError> {
        let now = Instant::now();
        let mut map = self.per_user.lock().unwrap();
        let key = format!("{}:search_min", user_id);
        let bucket = map.entry(key).or_insert_with(|| Bucket::new(60));
        if !bucket.try_consume(self.search_per_min, now) {
            warn!("rate_limit: user={} exceeded search {}/min", user_id, self.search_per_min);
            return Err(PolicyError::RateLimited {
                retry_after_ms: bucket.retry_after_ms(now),
                scope:          "search_per_minute".into(),
            });
        }
        Ok(())
    }
}

struct Bucket {
    window_secs: u64,
    count:       u32,
    window_start: Instant,
}

impl Bucket {
    fn new(window_secs: u64) -> Self {
        Self { window_secs, count: 0, window_start: Instant::now() }
    }

    fn try_consume(&mut self, limit: u32, now: Instant) -> bool {
        if now.duration_since(self.window_start).as_secs() >= self.window_secs {
            self.window_start = now;
            self.count = 0;
        }
        if self.count >= limit { return false; }
        self.count += 1;
        true
    }

    fn retry_after_ms(&self, now: Instant) -> u64 {
        let elapsed = now.duration_since(self.window_start).as_millis() as u64;
        let window_ms = self.window_secs * 1000;
        window_ms.saturating_sub(elapsed)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    // ── SSRF classifier ─────────────────────────────────────────────────────

    fn v4(s: &str) -> IpAddr { IpAddr::V4(Ipv4Addr::from_str(s).unwrap()) }
    fn v6(s: &str) -> IpAddr { IpAddr::V6(Ipv6Addr::from_str(s).unwrap()) }

    #[test]
    fn ipv4_blocks_loopback_and_private_and_link_local() {
        for ip in ["127.0.0.1", "127.1.2.3", "10.0.0.1", "172.16.0.1", "192.168.1.1", "169.254.169.254"] {
            assert!(is_ip_blocked(v4(ip)), "{} should be blocked", ip);
        }
    }

    #[test]
    fn ipv4_blocks_cgnat() {
        assert!(is_ip_blocked(v4("100.64.0.1")));
        assert!(is_ip_blocked(v4("100.127.255.255")));
        // 100.63 is outside RFC6598; 100.128 is past it.
        assert!(!is_ip_blocked(v4("100.63.0.1")));
        assert!(!is_ip_blocked(v4("100.128.0.1")));
    }

    #[test]
    fn ipv4_blocks_multicast_broadcast_reserved() {
        assert!(is_ip_blocked(v4("224.0.0.1")));
        assert!(is_ip_blocked(v4("255.255.255.255")));
        assert!(is_ip_blocked(v4("240.0.0.1")));
        assert!(is_ip_blocked(v4("0.0.0.0")));
    }

    #[test]
    fn ipv4_allows_public() {
        for ip in ["8.8.8.8", "1.1.1.1", "142.250.80.46", "99.99.99.99"] {
            assert!(!is_ip_blocked(v4(ip)), "{} should be allowed", ip);
        }
    }

    #[test]
    fn ipv6_blocks_loopback_and_link_local_and_ula() {
        assert!(is_ip_blocked(v6("::1")));
        assert!(is_ip_blocked(v6("fe80::1")));
        assert!(is_ip_blocked(v6("fc00::1")));
        assert!(is_ip_blocked(v6("fd12:3456::1")));
    }

    #[test]
    fn ipv6_blocks_mapped_ipv4_metadata() {
        // ::ffff:169.254.169.254 — classic AWS metadata IP via v6 mapping.
        assert!(is_ip_blocked(v6("::ffff:a9fe:a9fe")));
    }

    #[test]
    fn ipv6_allows_public() {
        assert!(!is_ip_blocked(v6("2606:4700:4700::1111"))); // Cloudflare
        assert!(!is_ip_blocked(v6("2001:4860:4860::8888"))); // Google
    }

    // ── Domain match ────────────────────────────────────────────────────────

    #[test]
    fn host_matches_exact_and_suffix_only() {
        assert!(host_matches("example.com", "example.com"));
        assert!(host_matches("api.example.com", "example.com"));
        assert!(host_matches("a.b.example.com", "example.com"));

        // Not a suffix at a label boundary:
        assert!(!host_matches("notexample.com", "example.com"));
        assert!(!host_matches("example.com.evil", "example.com"));
    }

    #[test]
    fn host_matches_is_case_insensitive_and_strips_trailing_dot() {
        assert!(host_matches("Example.com.", "example.com"));
        assert!(host_matches("API.Example.COM", "example.com"));
    }

    // ── Rate limiter ────────────────────────────────────────────────────────

    #[test]
    fn rate_limiter_per_user_enforces_minute_cap() {
        let r = RateLimiter::new(3, 100, 100, 100);
        assert!(r.check_fetch("u1", "a.com").is_ok());
        assert!(r.check_fetch("u1", "a.com").is_ok());
        assert!(r.check_fetch("u1", "a.com").is_ok());
        let err = r.check_fetch("u1", "a.com").unwrap_err();
        assert!(matches!(err, PolicyError::RateLimited { .. }));
    }

    #[test]
    fn rate_limiter_per_user_domain_enforces_separately() {
        let r = RateLimiter::new(100, 100, 2, 100);
        assert!(r.check_fetch("u1", "a.com").is_ok());
        assert!(r.check_fetch("u1", "a.com").is_ok());
        // Third on a.com blocked; but b.com still has budget.
        assert!(r.check_fetch("u1", "a.com").is_err());
        assert!(r.check_fetch("u1", "b.com").is_ok());
    }

    #[test]
    fn rate_limiter_resets_after_window() {
        let r = RateLimiter::new(1, 100, 100, 100);
        assert!(r.check_fetch("u1", "a.com").is_ok());
        assert!(r.check_fetch("u1", "a.com").is_err());
        // Pretend the minute rolled over.
        {
            let mut map = r.per_user.lock().unwrap();
            let b = map.get_mut("u1:min").unwrap();
            b.window_start = Instant::now() - Duration::from_secs(120);
        }
        assert!(r.check_fetch("u1", "a.com").is_ok());
    }

    #[test]
    fn rate_limiter_search_bucket_is_independent() {
        let r = RateLimiter::new(100, 100, 100, 2);
        assert!(r.check_search("u1").is_ok());
        assert!(r.check_search("u1").is_ok());
        // Third search blocked; fetch budget is untouched.
        assert!(r.check_search("u1").is_err());
        assert!(r.check_fetch("u1", "a.com").is_ok());
    }

    // ── Redirect helpers ────────────────────────────────────────────────────

    #[test]
    fn resolve_redirect_handles_relative_and_absolute() {
        let base = "https://example.com/a/b";
        assert_eq!(
            resolve_redirect(base, "/c").unwrap(),
            "https://example.com/c"
        );
        assert_eq!(
            resolve_redirect(base, "https://other.com/x").unwrap(),
            "https://other.com/x"
        );
    }

    // ── Live request against a throw-away local server ──────────────────────

    // Spin up a throw-away HTTP server on 127.0.0.1:port (any free port).
    // Returns (base_url, shutdown_handle).
    async fn spawn_echo_server() -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr     = listener.local_addr().unwrap();
        let url      = format!("http://127.0.0.1:{}", addr.port());
        let h = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let body = "hello from test server\n";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body,
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        (url, h)
    }

    #[tokio::test]
    async fn policy_blocks_loopback_by_default() {
        let (url, _h) = spawn_echo_server().await;
        let p = HttpPolicy::new(HttpPolicyConfig::default());
        let err = p.get(&url, "u1").await.unwrap_err();
        assert!(matches!(err, PolicyError::BlockedHost { .. }),
            "expected BlockedHost, got {:?}", err);
    }

    #[tokio::test]
    async fn policy_allows_loopback_for_searxng_exception() {
        let (url, _h) = spawn_echo_server().await;
        let port = Url::parse(&url).unwrap().port().unwrap();
        let p = HttpPolicy::new(HttpPolicyConfig {
            searxng_exception: Some(("127.0.0.1".into(), port)),
            ..Default::default()
        });
        let resp = p.get(&url, "u1").await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(String::from_utf8_lossy(&resp.body).contains("hello from test server"));
    }

    #[tokio::test]
    async fn policy_rejects_non_http_schemes() {
        let p = HttpPolicy::new(HttpPolicyConfig::default());
        let err = p.get("file:///etc/passwd", "u1").await.unwrap_err();
        assert!(matches!(err, PolicyError::BlockedScheme { .. }));
    }

    #[tokio::test]
    async fn policy_rejects_invalid_url() {
        let p = HttpPolicy::new(HttpPolicyConfig::default());
        let err = p.get("not-a-url", "u1").await.unwrap_err();
        assert!(matches!(err, PolicyError::BlockedScheme { .. } | PolicyError::InvalidUrl(_)));
    }

    // ── engine seam ───────────────────────────────────

    use crate::policy::{
        AllowAllEngine, DenyAllEngine, NetworkEgressDirection,
        PolicyDecision, PolicyEngine, PolicyEvent,
    };
    use crate::agent::instance::AgentId;
    use std::sync::Mutex as StdMutex;

    // Engine that records every event it sees + replies via a closure.
    struct RecordingEngine {
        seen:   StdMutex<Vec<PolicyEvent>>,
        decide: Box<dyn Fn(&PolicyEvent) -> PolicyDecision + Send + Sync>,
    }
    #[async_trait::async_trait]
    impl PolicyEngine for RecordingEngine {
        async fn evaluate(&self, event: &PolicyEvent) -> PolicyDecision {
            self.seen.lock().unwrap().push(event.clone());
            (self.decide)(event)
        }
    }

    #[test]
    fn has_policy_engine_reflects_with_policy_engine_call() {
        let p = HttpPolicy::new(HttpPolicyConfig::default());
        assert!(!p.has_policy_engine());
        let p = p.with_policy_engine(Arc::new(AllowAllEngine));
        assert!(p.has_policy_engine());
    }

    #[tokio::test]
    async fn engine_consult_skipped_when_context_has_no_agent_id() {
        // No agent_id in context → engine never consulted (the
        // existing rate-limit / SSRF guards still run, of course).
        let engine = Arc::new(RecordingEngine {
            seen: StdMutex::new(Vec::new()),
            decide: Box::new(|_| PolicyDecision::Allow),
        });
        let p = HttpPolicy::new(HttpPolicyConfig::default())
            .with_policy_engine(engine.clone());
        // Deliberately point at a blocked scheme so we exit early —
        // we just want to assert evaluate_engine() doesn't fire when
        // agent_id is absent. The bare-URL parse happens regardless,
        // but the engine consult is gated on agent_id.
        let ctx = RequestContext::user_only("u1");
        let _ = p.get_with_context("file:///etc/passwd", &ctx).await;
        assert!(engine.seen.lock().unwrap().is_empty(),
            "engine consulted even though agent_id was None");
    }

    #[tokio::test]
    async fn engine_receives_network_egress_event_when_agent_id_present() {
        let engine = Arc::new(RecordingEngine {
            seen: StdMutex::new(Vec::new()),
            decide: Box::new(|_| PolicyDecision::Allow),
        });
        let p = HttpPolicy::new(HttpPolicyConfig::default())
            .with_policy_engine(engine.clone());
        let ctx = RequestContext {
            user_id:  "u1".into(),
            agent_id: Some(AgentId::new()),
            skill_id: Some("com.example.x".into()),
        };
        // Use a blocked-scheme URL so we don't actually attempt a
        // network call; the engine consult happens before scheme
        // validation in get_with_context (it runs BEFORE get()).
        let _ = p.get_with_context("file:///x", &ctx).await;

        let seen = engine.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "expected exactly one event");
        match &seen[0] {
            PolicyEvent::NetworkEgress { skill_id, url, host, scheme, direction, .. } => {
                assert_eq!(skill_id.as_deref(),  Some("com.example.x"));
                assert_eq!(url,                  "file:///x");
                assert_eq!(host,                 "");        // file:// has no host
                assert_eq!(scheme,               "file");
                assert_eq!(*direction, NetworkEgressDirection::Outbound);
            }
            other => panic!("expected NetworkEgress, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn engine_deny_short_circuits_to_policy_denied_error() {
        let p = HttpPolicy::new(HttpPolicyConfig::default())
            .with_policy_engine(Arc::new(DenyAllEngine::new("blocked for tests")));
        let ctx = RequestContext {
            user_id:  "u1".into(),
            agent_id: Some(AgentId::new()),
            skill_id: None,
        };
        // Even a perfectly-valid URL is denied — the engine decision
        // is checked before the request ever leaves the process.
        let err = p.get_with_context("https://example.com/", &ctx).await.unwrap_err();
        match err {
            PolicyError::PolicyDenied { rule, reason } => {
                assert_eq!(rule, "test/deny-all");
                assert_eq!(reason, "blocked for tests");
            }
            other => panic!("expected PolicyDenied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn engine_deny_for_search_uses_same_error_shape() {
        let p = HttpPolicy::new(HttpPolicyConfig::default())
            .with_policy_engine(Arc::new(DenyAllEngine::new("nope")));
        let ctx = RequestContext {
            user_id:  "u1".into(),
            agent_id: Some(AgentId::new()),
            skill_id: Some("com.test.search".into()),
        };
        let err = p.get_for_search_with_context("https://example.com/", &ctx, &[])
            .await.unwrap_err();
        assert!(matches!(err, PolicyError::PolicyDenied { .. }));
    }

    #[tokio::test]
    async fn no_engine_attached_means_get_with_context_falls_through_to_get() {
        // Without `with_policy_engine` the new API behaves exactly
        // like the legacy `get` — get_with_context should hit the
        // same scheme-block path.
        let p = HttpPolicy::new(HttpPolicyConfig::default());
        let ctx = RequestContext {
            user_id:  "u1".into(),
            agent_id: Some(AgentId::new()),
            skill_id: None,
        };
        let err = p.get_with_context("file:///etc/passwd", &ctx).await.unwrap_err();
        assert!(matches!(err, PolicyError::BlockedScheme { .. }));
    }

    #[tokio::test]
    async fn allow_engine_falls_through_to_normal_request_machinery() {
        // Allow engine + bad URL → we still see the normal blocked-
        // scheme error, proving the engine consult was a no-op
        // and the request continued through the existing pipeline.
        let p = HttpPolicy::new(HttpPolicyConfig::default())
            .with_policy_engine(Arc::new(AllowAllEngine));
        let ctx = RequestContext {
            user_id:  "u1".into(),
            agent_id: Some(AgentId::new()),
            skill_id: None,
        };
        let err = p.get_with_context("file:///etc/passwd", &ctx).await.unwrap_err();
        assert!(matches!(err, PolicyError::BlockedScheme { .. }));
    }

    #[tokio::test]
    async fn rate_limit_state_is_preserved_across_with_policy_engine() {
        // Critical correctness test: attaching the engine MUST NOT
        // reset the rate-limit buckets (otherwise admins could DoS
        // themselves by toggling policy on/off).
        let mut cfg = HttpPolicyConfig::default();
        cfg.rate_user_per_min = 1; // tight cap so the second call trips it
        let p1 = HttpPolicy::new(cfg);

        // Burn the only token. Can use a URL we know will be parsed
        // fine but blocked early — rate check fires first.
        let _ = p1.get("https://example.com/", "u1").await;

        // Attach engine, get a new HttpPolicy. Same user should still
        // be over budget.
        let p2 = p1.with_policy_engine(Arc::new(AllowAllEngine));
        let err = p2.get("https://example.com/", "u1").await.unwrap_err();
        assert!(matches!(err, PolicyError::RateLimited { .. }),
            "rate state lost across with_policy_engine; got {err:?}");
    }
}
