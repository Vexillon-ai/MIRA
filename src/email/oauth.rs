// SPDX-License-Identifier: AGPL-3.0-or-later

// src/email/oauth.rs
//! OAuth 2.0 flow for the email channel (slice E4-1).
//!
//! Two providers — Google (Gmail) and Microsoft (Outlook / 365) —
//! share the same code-with-PKCE flow. The operator brings their
//! own client_ids at each provider (see design-docs/email-channel.md for
//! the registration steps); MIRA never stores or expects a client
//! secret since both providers support PKCE-only public-client
//! flows.
//!
//! Lifecycle:
//!   1. UI calls `POST /api/email/accounts/{id}/oauth/{provider}/start`
//!      → MIRA generates PKCE verifier + challenge, stores them
//!      keyed by a CSRF state token (10-min TTL), returns the
//!      provider's authorize URL.
//!   2. User's browser opens the URL, approves at the provider.
//!   3. Provider redirects to `/api/email/oauth/callback?code=…&state=…`.
//!      MIRA looks up the state, exchanges code+verifier for tokens
//!      via the provider's token endpoint, writes them onto the
//!      `email_accounts` row, marks `auth_mode = "oauth_<provider>"`.
//!   4. Subsequent IMAP/SMTP uses (slice E4-2) authenticate via
//!      XOAUTH2 with the stored `oauth_access_token`, calling
//!      `refresh_if_needed` here when the token is near expiry.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, ClientId, CsrfToken, EndpointNotSet, EndpointSet,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, RefreshToken,
    Scope, TokenResponse, TokenUrl,
};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::MiraError;
use crate::config::EmailOAuthConfig;
use crate::email::store::EmailAccountStore;

/// State entry held in the pending-flow map between `/start` and the
/// callback. TTL keeps stale entries from accumulating if the user
/// closes the tab.
struct PendingFlow {
    user_id:     String,
    account_id:  String,
    provider:    OAuthProvider,
    pkce_verifier: PkceCodeVerifier,
    created_at:  Instant,
}

const PENDING_TTL: Duration = Duration::from_secs(10 * 60);
const REFRESH_GUARD_SECS: i64 = 60; // refresh tokens this close to expiry

/// Identifier for the two providers MIRA supports today. Stored as
/// a string in the DB column `auth_mode` (`"oauth_google"` /
/// `"oauth_microsoft"`) so adding a third provider later is a
/// pure-additive change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OAuthProvider {
    Google,
    Microsoft,
}

impl OAuthProvider {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "google" | "oauth_google"       => Some(Self::Google),
            "microsoft" | "oauth_microsoft" => Some(Self::Microsoft),
            _ => None,
        }
    }
    pub fn auth_mode(&self) -> &'static str {
        match self {
            Self::Google    => "oauth_google",
            Self::Microsoft => "oauth_microsoft",
        }
    }
    pub fn slug(&self) -> &'static str {
        match self {
            Self::Google    => "google",
            Self::Microsoft => "microsoft",
        }
    }
    /// Authorization endpoint for this provider.
    fn auth_endpoint(&self) -> &'static str {
        match self {
            Self::Google    => "https://accounts.google.com/o/oauth2/v2/auth",
            Self::Microsoft => "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
        }
    }
    /// Token endpoint for this provider.
    fn token_endpoint(&self) -> &'static str {
        match self {
            Self::Google    => "https://oauth2.googleapis.com/token",
            Self::Microsoft => "https://login.microsoftonline.com/common/oauth2/v2.0/token",
        }
    }
    /// Scopes MIRA needs. Same shape for both: full mailbox access
    /// via IMAP + outbound via SMTP + a refresh token. Microsoft
    /// additionally needs `offline_access` to actually issue refresh
    /// tokens; Google issues them by default when the OAuth consent
    /// type is "Desktop app".
    fn scopes(&self) -> Vec<&'static str> {
        match self {
            Self::Google => vec![
                "https://mail.google.com/",
            ],
            Self::Microsoft => vec![
                "https://outlook.office.com/IMAP.AccessAsUser.All",
                "https://outlook.office.com/SMTP.Send",
                "offline_access",
            ],
        }
    }
    /// Default IMAP host + port for this provider — used when the
    /// account row has no explicit `imap_host`. Gmail and Outlook
    /// both expose well-known IMAP endpoints, and OAuth users
    /// rarely need to override them.
    pub fn imap_defaults(&self) -> (&'static str, u16) {
        match self {
            Self::Google    => ("imap.gmail.com",        993),
            Self::Microsoft => ("outlook.office365.com", 993),
        }
    }
    /// Same for SMTP. Both providers use port 465 + implicit TLS,
    /// matching the format our `smtp::send` already expects.
    pub fn smtp_defaults(&self) -> (&'static str, u16) {
        match self {
            Self::Google    => ("smtp.gmail.com",          465),
            Self::Microsoft => ("smtp.office365.com",      587),
        }
    }

    /// Per-provider `client_id` pulled from the live config.
    fn client_id_from<'c>(&self, cfg: &'c EmailOAuthConfig) -> Option<&'c str> {
        let raw = match self {
            Self::Google    => &cfg.google_client_id,
            Self::Microsoft => &cfg.microsoft_client_id,
        };
        if raw.is_empty() { None } else { Some(raw.as_str()) }
    }
}

/// Per-process map of state-token → pending flow. Single Mutex; the
/// map turns over slowly so contention is a non-issue.
pub struct OAuthStateStore {
    pending: Mutex<HashMap<String, PendingFlow>>,
}

impl OAuthStateStore {
    pub fn new() -> Self { Self { pending: Mutex::new(HashMap::new()) } }

    fn prune(&self) {
        let now = Instant::now();
        let mut map = self.pending.lock().unwrap();
        map.retain(|_, f| now.duration_since(f.created_at) <= PENDING_TTL);
    }
}

impl Default for OAuthStateStore {
    fn default() -> Self { Self::new() }
}

/// Result of `/start` — the authorize URL the UI redirects the
/// user's browser to. `state` is returned for debug only; the
/// caller doesn't need to send it back, the URL already carries it.
#[derive(Debug, Serialize)]
pub struct StartFlowResult {
    pub authorize_url: String,
    pub state:         String,
}

/// Build the authorize URL + record the pending flow. `user_id` is
/// the caller's id (so the callback verifies the account belongs to
/// them); `account_id` is the email_accounts row the tokens will be
/// written onto.
pub fn start_flow(
    state_store: &OAuthStateStore,
    cfg:         &EmailOAuthConfig,
    server_port: u16,
    provider:    OAuthProvider,
    user_id:     &str,
    account_id:  &str,
) -> Result<StartFlowResult, MiraError> {
    let client_id_str = provider.client_id_from(cfg).ok_or_else(|| MiraError::ConfigError(
        format!("email_oauth.{}_client_id not configured", provider.slug())
    ))?;

    let redirect_url = redirect_url(cfg, server_port)?;
    let (auth_url_obj, token_url_obj) = endpoints(provider)?;

    let client = BasicClient::new(ClientId::new(client_id_str.to_owned()))
        .set_auth_uri(auth_url_obj)
        .set_token_uri(token_url_obj)
        .set_redirect_uri(redirect_url);

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    let mut req = client.authorize_url(CsrfToken::new_random);
    for scope in provider.scopes() {
        req = req.add_scope(Scope::new(scope.to_string()));
    }
    let (authorize_url, csrf) = req
        .set_pkce_challenge(pkce_challenge)
        .url();

    // Microsoft needs `prompt=consent` to actually issue a refresh
    // token on subsequent sign-ins; without it, repeat connections
    // silently re-use the cached grant and `offline_access` may not
    // produce a refresh_token. Google's "Desktop app" type issues
    // refresh tokens reliably without an extra param.
    let mut authorize_url = authorize_url.to_string();
    if matches!(provider, OAuthProvider::Microsoft) {
        if !authorize_url.contains("prompt=") {
            authorize_url.push_str("&prompt=consent");
        }
    } else if matches!(provider, OAuthProvider::Google) {
        // Google needs access_type=offline + prompt=consent on the
        // FIRST grant to issue a refresh_token; without these the
        // crate's defaults give you an access_token only.
        if !authorize_url.contains("access_type=") {
            authorize_url.push_str("&access_type=offline");
        }
        if !authorize_url.contains("prompt=") {
            authorize_url.push_str("&prompt=consent");
        }
    }

    let state = csrf.secret().to_string();
    state_store.prune();
    state_store.pending.lock().unwrap().insert(state.clone(), PendingFlow {
        user_id:       user_id.to_owned(),
        account_id:    account_id.to_owned(),
        provider,
        pkce_verifier,
        created_at:    Instant::now(),
    });

    Ok(StartFlowResult { authorize_url, state })
}

/// Exchange a callback `code` + matching `state` for tokens, persist
/// them on the email account row. Returns the account_id we wrote
/// tokens onto so the handler can redirect back to the right /email
/// view. The pending-flow entry is consumed on success or expiry —
/// re-using a state token is a hard error.
pub async fn handle_callback(
    state_store: &OAuthStateStore,
    cfg:         &EmailOAuthConfig,
    server_port: u16,
    accounts:    &EmailAccountStore,
    code:        &str,
    state:       &str,
) -> Result<String, MiraError> {
    // Pop the pending flow (consume) — same state token can never
    // be exchanged twice. Surfaces tampering as a clear error rather
    // than a silent re-use.
    let flow = {
        state_store.prune();
        state_store.pending.lock().unwrap().remove(state)
    }.ok_or_else(|| MiraError::ConfigError(
        "oauth: unknown or expired state token".into()
    ))?;

    let client_id_str = flow.provider.client_id_from(cfg).ok_or_else(|| MiraError::ConfigError(
        format!("email_oauth.{}_client_id not configured", flow.provider.slug())
    ))?;
    let redirect_url = redirect_url(cfg, server_port)?;
    let (auth_url_obj, token_url_obj) = endpoints(flow.provider)?;

    let client = BasicClient::new(ClientId::new(client_id_str.to_owned()))
        .set_auth_uri(auth_url_obj)
        .set_token_uri(token_url_obj)
        .set_redirect_uri(redirect_url);

    let http = reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| MiraError::ProviderError(format!("oauth http client: {e}")))?;

    let token = client
        .exchange_code(oauth2::AuthorizationCode::new(code.to_owned()))
        .set_pkce_verifier(flow.pkce_verifier)
        .request_async(&http)
        .await
        .map_err(|e| MiraError::ProviderError(format!(
            "oauth token exchange ({}): {e}", flow.provider.slug()
        )))?;

    // Persist onto the account row + flip auth_mode so the IMAP
    // poller knows to use XOAUTH2.
    let expires_at_ms = expiry_ms(token.expires_in());
    // Defence-in-depth: confirm the account row still belongs to
    // the user who initiated the flow. The state-token map already
    // proves the same browser session started + finished the flow,
    // but a user delete or row reassignment between /start and
    // callback should surface as an error, not a silent token
    // write onto a row the user no longer owns.
    let row = accounts.get(&flow.account_id)
        .map_err(|e| MiraError::DatabaseError(format!("oauth callback load: {e}")))?
        .ok_or_else(|| MiraError::NotFound(format!(
            "oauth callback: account {} gone before exchange", flow.account_id
        )))?;
    if row.user_id != flow.user_id {
        return Err(MiraError::ConfigError(
            "oauth callback: account ownership changed mid-flow — refusing token write".into(),
        ));
    }

    persist_tokens(
        accounts,
        &flow.account_id,
        flow.provider,
        token.access_token().secret(),
        token.refresh_token().map(|r| r.secret().as_str()),
        expires_at_ms,
    )?;
    info!("oauth callback: persisted tokens for account {} (provider={})",
          flow.account_id, flow.provider.slug());

    Ok(flow.account_id)
}

/// Refresh the access token on `account` when the stored `oauth_expires_at`
/// is within `REFRESH_GUARD_SECS` of now (or already past). Returns
/// the still-or-newly-valid access token. Idempotent + safe to call
/// before every IMAP/SMTP authentication attempt.
pub async fn refresh_if_needed(
    cfg:      &EmailOAuthConfig,
    accounts: &EmailAccountStore,
    account_id: &str,
) -> Result<String, MiraError> {
    let row = accounts.get(account_id)
        .map_err(|e| MiraError::DatabaseError(format!("oauth refresh load: {e}")))?
        .ok_or_else(|| MiraError::NotFound(format!("oauth refresh: account {account_id} gone")))?;

    let provider = OAuthProvider::parse(&row.auth_mode).ok_or_else(|| MiraError::ConfigError(
        format!("account {account_id}: auth_mode={:?} is not an OAuth provider", row.auth_mode)
    ))?;

    let access = row.oauth_access_token.as_deref().unwrap_or("");
    let now_ms = now_ms();
    let needs_refresh = row.oauth_expires_at
        .map(|exp| exp - (REFRESH_GUARD_SECS * 1000) <= now_ms)
        .unwrap_or(true)
        || access.is_empty();
    if !needs_refresh {
        return Ok(access.to_owned());
    }

    let refresh = row.oauth_refresh_token.clone().ok_or_else(|| MiraError::ConfigError(
        format!("account {account_id}: refresh requested but no refresh_token stored — \
                 user must re-connect via the OAuth flow")
    ))?;

    let client_id_str = provider.client_id_from(cfg).ok_or_else(|| MiraError::ConfigError(
        format!("email_oauth.{}_client_id not configured", provider.slug())
    ))?;
    let (auth_url_obj, token_url_obj) = endpoints(provider)?;
    let client = BasicClient::new(ClientId::new(client_id_str.to_owned()))
        .set_auth_uri(auth_url_obj)
        .set_token_uri(token_url_obj);
    let http = reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| MiraError::ProviderError(format!("oauth http client: {e}")))?;

    let new_token = client
        .exchange_refresh_token(&RefreshToken::new(refresh.clone()))
        .request_async(&http)
        .await
        .map_err(|e| MiraError::ProviderError(format!(
            "oauth refresh ({}): {e}", provider.slug()
        )))?;

    let new_access = new_token.access_token().secret().to_owned();
    let expires_at_ms = expiry_ms(new_token.expires_in());
    // Microsoft sometimes rotates the refresh_token on refresh.
    // Persist the new one if returned; keep the old one otherwise.
    let new_refresh = new_token.refresh_token()
        .map(|r| r.secret().as_str())
        .unwrap_or(refresh.as_str());

    persist_tokens(
        accounts, account_id, provider,
        &new_access, Some(new_refresh), expires_at_ms,
    )?;
    Ok(new_access)
}

// ── Internals ────────────────────────────────────────────────────────────────

fn endpoints(
    p: OAuthProvider,
) -> Result<(AuthUrl, TokenUrl), MiraError> {
    let auth = AuthUrl::new(p.auth_endpoint().to_string())
        .map_err(|e| MiraError::ConfigError(format!("oauth auth url: {e}")))?;
    let token = TokenUrl::new(p.token_endpoint().to_string())
        .map_err(|e| MiraError::ConfigError(format!("oauth token url: {e}")))?;
    Ok((auth, token))
}

fn redirect_url(cfg: &EmailOAuthConfig, server_port: u16) -> Result<RedirectUrl, MiraError> {
    let base = if cfg.public_base_url.is_empty() {
        format!("http://127.0.0.1:{server_port}")
    } else {
        cfg.public_base_url.trim_end_matches('/').to_owned()
    };
    let url = format!("{base}/api/email/oauth/callback");
    RedirectUrl::new(url).map_err(|e| MiraError::ConfigError(format!("oauth redirect: {e}")))
}

fn persist_tokens(
    accounts:     &EmailAccountStore,
    account_id:   &str,
    provider:     OAuthProvider,
    access:       &str,
    refresh:      Option<&str>,
    expires_at_ms: Option<i64>,
) -> Result<(), MiraError> {
    // Round-trip via the row so we don't trample fields we don't
    // care about. The store doesn't have a direct "set OAuth tokens"
    // path yet; future cleanup if E4 grows more knobs.
    let row = accounts.get(account_id)?
        .ok_or_else(|| MiraError::NotFound(format!("persist_tokens: account {account_id} gone")))?;
    // Direct SQL would be faster but the store layer doesn't expose
    // it. Use a parameterised UPDATE via a tiny helper on the store
    // — added below.
    accounts.set_oauth_tokens(
        &row.id,
        provider.auth_mode(),
        access,
        refresh,
        expires_at_ms,
    )
}

fn expiry_ms(expires_in: Option<Duration>) -> Option<i64> {
    expires_in.map(|d| now_ms() + (d.as_millis() as i64))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// Silence false-positive unused-warning when the store's
// `set_oauth_tokens` isn't yet wired (chunk-by-chunk landing).
#[allow(dead_code)]
fn _types_in_use() {
    let _ = std::any::type_name::<EndpointSet>();
    let _ = std::any::type_name::<EndpointNotSet>();
}
