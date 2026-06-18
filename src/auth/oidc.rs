// SPDX-License-Identifier: AGPL-3.0-or-later

// src/auth/oidc.rs
//! SSO via OpenID Connect (Q2 #11). Generic, discovery-driven — one code path
//! covers Google / Microsoft Entra / Keycloak / Authentik / Okta.
//!
//! Flow: **Authorization Code + PKCE**, server-mediated. We never accept a
//! token from the browser; after the IdP redirects back with a `code`, MIRA
//! (server-side, over TLS) exchanges it for an access token and calls the
//! IdP's **userinfo** endpoint for the user's claims (sub / email / name).
//! That sidesteps ID-token/JWKS validation while staying secure — the access
//! token only ever talks to the IdP's own userinfo. The caller then maps the
//! claims to a MIRA user and mints MIRA's own session (see the handler).
//!
//! This module owns: discovery (`.well-known/openid-configuration`) with an
//! in-memory cache, a CSRF/PKCE state store, authorize-URL construction, the
//! code→userinfo exchange, and the pure provisioning *decision*. The DB writes
//! + session minting live in the HTTP handler so this stays testable.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use serde::Deserialize;

use crate::auth::models::Role;
use crate::config::{OidcConfig, OidcProvider};
use crate::MiraError;

/// State tokens older than this are pruned / rejected.
const STATE_TTL: Duration = Duration::from_secs(600);

// ── Discovery + claims wire types ───────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct DiscoveryDoc {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    userinfo_endpoint: Option<String>,
}

/// The user claims we read from the IdP's userinfo response.
#[derive(Debug, Clone, Deserialize)]
pub struct OidcClaims {
    /// IdP-stable subject id. Required.
    pub sub: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub email_verified: Option<bool>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub preferred_username: Option<String>,
    /// Canonical issuer (filled from discovery, not userinfo) — the value we
    /// bind identities against.
    #[serde(skip)]
    pub issuer: String,
}

struct PendingFlow {
    provider_id: String,
    pkce_verifier: PkceCodeVerifier,
    created_at: Instant,
}

// ── Service ─────────────────────────────────────────────────────────────────

pub struct OidcService {
    enabled: bool,
    providers: Vec<OidcProvider>,
    redirect_base: String,
    http: reqwest::Client,
    discovery: Mutex<HashMap<String, DiscoveryDoc>>,
    state: Mutex<HashMap<String, PendingFlow>>,
}

/// What to do with an authenticated IdP identity that we've looked up.
#[derive(Debug, PartialEq)]
pub enum ProvisionDecision {
    /// Identity already bound to a user — just issue a session.
    UseExisting,
    /// Email matches an existing user — link the identity, then issue.
    LinkExisting,
    /// No match; auto-provision a new account with this role.
    Create { role: Role },
    /// No match and provisioning not permitted.
    Reject(String),
}

impl OidcService {
    pub fn new(cfg: &OidcConfig, server_port: u16) -> Self {
        let redirect_base = if cfg.public_base_url.trim().is_empty() {
            format!("http://127.0.0.1:{server_port}")
        } else {
            cfg.public_base_url.trim_end_matches('/').to_owned()
        };
        let http = reqwest::ClientBuilder::new()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_default();
        Self {
            enabled: cfg.enabled,
            providers: cfg.providers.clone(),
            redirect_base,
            http,
            discovery: Mutex::new(HashMap::new()),
            state: Mutex::new(HashMap::new()),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled && !self.providers.is_empty()
    }

    /// `(id, display_name)` for each provider — drives the login buttons.
    pub fn provider_buttons(&self) -> Vec<(String, String)> {
        if !self.enabled {
            return Vec::new();
        }
        self.providers
            .iter()
            .filter(|p| !p.client_id.trim().is_empty() && !p.issuer.trim().is_empty())
            .map(|p| {
                let label = if p.display_name.trim().is_empty() { p.id.clone() } else { p.display_name.clone() };
                (p.id.clone(), label)
            })
            .collect()
    }

    fn provider(&self, id: &str) -> Option<&OidcProvider> {
        self.providers.iter().find(|p| p.id == id)
    }

    fn redirect_uri(&self) -> String {
        format!("{}/api/auth/oidc/callback", self.redirect_base)
    }

    /// Fetch + cache a provider's discovery document.
    async fn discover(&self, provider: &OidcProvider) -> Result<DiscoveryDoc, MiraError> {
        if let Some(d) = self.discovery.lock().unwrap().get(&provider.id) {
            return Ok(d.clone());
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            provider.issuer.trim_end_matches('/')
        );
        let doc: DiscoveryDoc = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| MiraError::ProviderError(format!("oidc discovery ({}): {e}", provider.id)))?
            .error_for_status()
            .map_err(|e| MiraError::ProviderError(format!("oidc discovery ({}): {e}", provider.id)))?
            .json()
            .await
            .map_err(|e| MiraError::ProviderError(format!("oidc discovery parse ({}): {e}", provider.id)))?;
        self.discovery.lock().unwrap().insert(provider.id.clone(), doc.clone());
        Ok(doc)
    }

    fn prune_state(&self) {
        let mut s = self.state.lock().unwrap();
        s.retain(|_, f| f.created_at.elapsed() < STATE_TTL);
    }

    /// Begin a login: resolve discovery, build the authorize URL (PKCE +
    /// state), stash the pending flow, and return the URL to redirect to.
    pub async fn begin(&self, provider_id: &str) -> Result<String, MiraError> {
        if !self.enabled {
            return Err(MiraError::ConfigError("OIDC is not enabled".into()));
        }
        let provider = self
            .provider(provider_id)
            .ok_or_else(|| MiraError::NotFound(format!("unknown OIDC provider: {provider_id}")))?;
        if provider.client_id.trim().is_empty() {
            return Err(MiraError::ConfigError(format!("OIDC provider {provider_id} missing client_id")));
        }
        let doc = self.discover(provider).await?;

        let client = BasicClient::new(ClientId::new(provider.client_id.clone()))
            .set_client_secret(ClientSecret::new(provider.client_secret.clone()))
            .set_auth_uri(AuthUrl::new(doc.authorization_endpoint.clone())
                .map_err(|e| MiraError::ConfigError(format!("oidc auth url: {e}")))?)
            .set_token_uri(TokenUrl::new(doc.token_endpoint.clone())
                .map_err(|e| MiraError::ConfigError(format!("oidc token url: {e}")))?)
            .set_redirect_uri(RedirectUrl::new(self.redirect_uri())
                .map_err(|e| MiraError::ConfigError(format!("oidc redirect url: {e}")))?);

        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let mut req = client.authorize_url(CsrfToken::new_random);
        for scope in provider.effective_scopes() {
            req = req.add_scope(Scope::new(scope));
        }
        let (authorize_url, csrf) = req.set_pkce_challenge(challenge).url();

        let state = csrf.secret().to_string();
        self.prune_state();
        self.state.lock().unwrap().insert(state, PendingFlow {
            provider_id: provider.id.clone(),
            pkce_verifier: verifier,
            created_at: Instant::now(),
        });

        Ok(authorize_url.to_string())
    }

    /// Complete a login: consume the state, exchange the code, and read the
    /// userinfo claims. Returns the provider id used + the validated claims
    /// (with canonical issuer).
    pub async fn complete(&self, code: &str, state: &str) -> Result<(String, OidcClaims), MiraError> {
        let flow = {
            self.prune_state();
            self.state.lock().unwrap().remove(state)
        }
        .ok_or_else(|| MiraError::Unauthorized)?;

        let provider = self
            .provider(&flow.provider_id)
            .ok_or_else(|| MiraError::ConfigError(format!("provider {} gone mid-flow", flow.provider_id)))?
            .clone();
        let doc = self.discover(&provider).await?;

        let client = BasicClient::new(ClientId::new(provider.client_id.clone()))
            .set_client_secret(ClientSecret::new(provider.client_secret.clone()))
            .set_auth_uri(AuthUrl::new(doc.authorization_endpoint.clone())
                .map_err(|e| MiraError::ConfigError(format!("oidc auth url: {e}")))?)
            .set_token_uri(TokenUrl::new(doc.token_endpoint.clone())
                .map_err(|e| MiraError::ConfigError(format!("oidc token url: {e}")))?)
            .set_redirect_uri(RedirectUrl::new(self.redirect_uri())
                .map_err(|e| MiraError::ConfigError(format!("oidc redirect url: {e}")))?);

        let token = client
            .exchange_code(AuthorizationCode::new(code.to_owned()))
            .set_pkce_verifier(flow.pkce_verifier)
            .request_async(&self.http)
            .await
            .map_err(|e| MiraError::ProviderError(format!("oidc token exchange ({}): {e}", provider.id)))?;

        let userinfo_url = doc
            .userinfo_endpoint
            .clone()
            .ok_or_else(|| MiraError::ProviderError(format!("oidc provider {} has no userinfo endpoint", provider.id)))?;

        let mut claims: OidcClaims = self
            .http
            .get(&userinfo_url)
            .bearer_auth(token.access_token().secret())
            .send()
            .await
            .map_err(|e| MiraError::ProviderError(format!("oidc userinfo ({}): {e}", provider.id)))?
            .error_for_status()
            .map_err(|e| MiraError::ProviderError(format!("oidc userinfo ({}): {e}", provider.id)))?
            .json()
            .await
            .map_err(|e| MiraError::ProviderError(format!("oidc userinfo parse ({}): {e}", provider.id)))?;
        claims.issuer = doc.issuer;

        if claims.sub.trim().is_empty() {
            return Err(MiraError::ProviderError("oidc userinfo missing `sub`".into()));
        }
        Ok((provider.id, claims))
    }

    /// Resolve the provisioning decision for a provider id after the handler's
    /// DB lookups (pure; see [`decide_provision`]).
    pub fn decide(
        &self,
        provider_id: &str,
        claims: &OidcClaims,
        matched_by_identity: bool,
        matched_by_email: bool,
    ) -> ProvisionDecision {
        match self.provider(provider_id) {
            Some(p) => decide_provision(p, claims, matched_by_identity, matched_by_email),
            None => ProvisionDecision::Reject(format!("unknown OIDC provider: {provider_id}")),
        }
    }
}

/// The provisioning rule, in one pure function (testable, no I/O):
/// 1. identity already bound → use it;
/// 2. else email matches an existing active user → link;
/// 3. else `auto_provision` + email present + domain allowed → create;
/// 4. else reject.
pub fn decide_provision(
    provider: &OidcProvider,
    claims: &OidcClaims,
    matched_by_identity: bool,
    matched_by_email: bool,
) -> ProvisionDecision {
    if matched_by_identity {
        return ProvisionDecision::UseExisting;
    }
    if matched_by_email {
        return ProvisionDecision::LinkExisting;
    }
    if !provider.auto_provision {
        return ProvisionDecision::Reject(
            "No MIRA account is linked to this identity. Ask an administrator to create or invite your account.".into(),
        );
    }
    let email = claims.email.as_deref().unwrap_or("").trim().to_lowercase();
    if email.is_empty() {
        return ProvisionDecision::Reject("The identity provider returned no email; cannot auto-provision.".into());
    }
    if !domain_allowed(&email, &provider.allowed_domains) {
        return ProvisionDecision::Reject(format!("Email domain not permitted for auto-provisioning ({email})."));
    }
    let role = if provider.default_role.trim().eq_ignore_ascii_case("admin") {
        Role::Admin
    } else {
        Role::User
    };
    ProvisionDecision::Create { role }
}

/// True if `email`'s domain is in `allowed` (empty `allowed` = any domain).
fn domain_allowed(email: &str, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    match email.rsplit_once('@') {
        Some((_, domain)) => allowed.iter().any(|d| d.trim().eq_ignore_ascii_case(domain)),
        None => false,
    }
}

/// Derive a safe, lowercase username seed from an SSO identity (used by
/// `LocalAuthService::create_sso_user`). Keeps `[a-z0-9._-]`, collapses the
/// rest, and never returns empty.
pub fn sanitize_username(seed: &str) -> String {
    let s: String = seed
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '.' })
        .collect();
    let s = s.trim_matches('.').to_string();
    if s.is_empty() { "user".into() } else { s }
}

/// Best username seed from claims: preferred_username → email local-part → sub.
pub fn username_seed(claims: &OidcClaims) -> String {
    if let Some(pu) = claims.preferred_username.as_deref().filter(|s| !s.trim().is_empty()) {
        return pu.to_string();
    }
    if let Some(email) = claims.email.as_deref() {
        if let Some((local, _)) = email.split_once('@') {
            if !local.is_empty() {
                return local.to_string();
            }
        }
    }
    claims.sub.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(auto: bool, domains: &[&str], role: &str) -> OidcProvider {
        OidcProvider {
            id: "test".into(),
            display_name: "Test".into(),
            issuer: "https://idp.example".into(),
            client_id: "cid".into(),
            client_secret: "sec".into(),
            scopes: vec![],
            auto_provision: auto,
            allowed_domains: domains.iter().map(|s| s.to_string()).collect(),
            default_role: role.into(),
        }
    }

    fn claims(email: Option<&str>) -> OidcClaims {
        OidcClaims {
            sub: "sub-1".into(),
            email: email.map(str::to_string),
            email_verified: Some(true),
            name: Some("A B".into()),
            preferred_username: None,
            issuer: "https://idp.example".into(),
        }
    }

    #[test]
    fn identity_match_uses_existing() {
        let d = decide_provision(&provider(false, &[], "user"), &claims(Some("a@x.com")), true, false);
        assert_eq!(d, ProvisionDecision::UseExisting);
    }

    #[test]
    fn email_match_links() {
        let d = decide_provision(&provider(false, &[], "user"), &claims(Some("a@x.com")), false, true);
        assert_eq!(d, ProvisionDecision::LinkExisting);
    }

    #[test]
    fn no_match_no_autoprovision_rejects() {
        let d = decide_provision(&provider(false, &[], "user"), &claims(Some("a@x.com")), false, false);
        assert!(matches!(d, ProvisionDecision::Reject(_)));
    }

    #[test]
    fn autoprovision_creates_with_role() {
        let d = decide_provision(&provider(true, &[], "admin"), &claims(Some("a@x.com")), false, false);
        assert_eq!(d, ProvisionDecision::Create { role: Role::Admin });
    }

    #[test]
    fn autoprovision_respects_domain_allowlist() {
        let p = provider(true, &["company.com"], "user");
        assert!(matches!(decide_provision(&p, &claims(Some("a@company.com")), false, false), ProvisionDecision::Create { .. }));
        assert!(matches!(decide_provision(&p, &claims(Some("a@gmail.com")), false, false), ProvisionDecision::Reject(_)));
    }

    #[test]
    fn autoprovision_requires_email() {
        let d = decide_provision(&provider(true, &[], "user"), &claims(None), false, false);
        assert!(matches!(d, ProvisionDecision::Reject(_)));
    }

    #[test]
    fn sanitize_and_seed() {
        assert_eq!(sanitize_username("Alice Smith!"), "alice.smith");
        assert_eq!(sanitize_username("  @@@  "), "user");
        let c = claims(Some("bob.jones@corp.com"));
        assert_eq!(username_seed(&c), "bob.jones");
    }

    #[test]
    fn domain_allowed_logic() {
        assert!(domain_allowed("a@x.com", &[]));
        assert!(domain_allowed("a@X.com", &["x.com".into()]));
        assert!(!domain_allowed("a@y.com", &["x.com".into()]));
    }

    /// Live check against Google's PUBLIC discovery doc (no secrets, no config
    /// touched). Confirms discovery + authorize-URL construction end-to-end.
    /// Network-dependent → ignored by default; run with:
    ///   cargo test --lib oidc::tests::begin_builds_google_url -- --ignored
    #[tokio::test]
    #[ignore]
    async fn begin_builds_google_url() {
        let cfg = OidcConfig {
            enabled: true,
            public_base_url: "https://mira.example".into(),
            providers: vec![OidcProvider {
                id: "google".into(),
                display_name: "Google".into(),
                issuer: "https://accounts.google.com".into(),
                client_id: "dummy-client-id".into(),
                client_secret: "dummy-secret".into(),
                scopes: vec![],
                auto_provision: false,
                allowed_domains: vec![],
                default_role: "user".into(),
            }],
        };
        let svc = OidcService::new(&cfg, 8082);
        let url = svc.begin("google").await.expect("begin");
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/"), "got {url}");
        assert!(url.contains("client_id=dummy-client-id"));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("redirect_uri=https%3A%2F%2Fmira.example%2Fapi%2Fauth%2Foidc%2Fcallback"));
        assert!(url.contains("scope=openid"));
    }
}
