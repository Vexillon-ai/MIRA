// SPDX-License-Identifier: AGPL-3.0-or-later

// src/notifications/fcm.rs
//! Firebase Cloud Messaging (HTTP v1) transport for the native mobile app.
//!
//! Opt-in (`notifications.fcm.enabled`). When on, the notification
//! dispatcher fans proactive events out to registered FCM device tokens in
//! addition to Web Push. Messages are **data-only** (no `notification`
//! block) so the app's own handler builds the local notification and can
//! route by `type`/`severity` — care/wellbeing → a high-priority,
//! non-collapsible channel.
//!
//! Auth is the standard Google service-account flow: we mint a short-lived
//! RS256 JWT signed with the service-account private key, exchange it at the
//! token endpoint for an OAuth2 access token (cached until ~1 min before
//! expiry), and send it as a Bearer token to the FCM v1 endpoint. The
//! service-account JSON is a secret — its path is redacted in the config API
//! and nothing in here logs the key or the device tokens.

use std::sync::Arc;
use std::time::Duration;

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::warn;

use crate::config::FcmConfig;
use crate::notifications::NotificationEnvelope;
use crate::MiraError;

const FCM_SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
const JWT_BEARER_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
/// How long FCM should hold a message for an offline device.
const MESSAGE_TTL: &str = "86400s"; // 24h

// ── Service-account JSON ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ServiceAccount {
    project_id:   String,
    client_email: String,
    private_key:  String,
    #[serde(default = "default_token_uri")]
    token_uri:    String,
}

fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".to_string()
}

// ── OAuth assertion + token-endpoint shapes ────────────────────────────────────

#[derive(Serialize)]
struct AssertionClaims {
    iss:   String,
    scope: String,
    aud:   String,
    iat:   i64,
    exp:   i64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in:   i64,
}

struct CachedToken {
    access_token: String,
    /// Unix seconds when the cached token should be considered stale.
    refresh_at:   i64,
}

// ── Send outcome ───────────────────────────────────────────────────────────────

/// Result of one FCM send. `Gone` means the token is no longer valid
/// (uninstalled app / rotated token) and the dispatcher should prune it.
pub enum FcmSendError {
    Gone,
    Other(String),
}

// ── Service ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct FcmService {
    project_id:   String,
    client_email: String,
    token_uri:    String,
    encoding_key: Arc<EncodingKey>,
    http:         reqwest::Client,
    token_cache:  Arc<Mutex<Option<CachedToken>>>,
}

impl FcmService {
    /// Build the service from config. Returns `Ok(None)` when FCM is
    /// disabled — callers store an `Option<FcmService>` and the dispatcher
    /// simply skips FCM rows when it's `None`. Returns `Err` only when FCM
    /// is *enabled* but misconfigured (missing path/project, unreadable or
    /// malformed JSON) so the operator gets a clear boot-time signal.
    pub fn open(cfg: &FcmConfig) -> Result<Option<Self>, MiraError> {
        if !cfg.enabled {
            return Ok(None);
        }
        let path = cfg.service_account_json_path.as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| MiraError::ConfigError(
                "notifications.fcm.enabled=true but service_account_json_path is unset".into()))?;
        let raw = std::fs::read_to_string(path)
            .map_err(|e| MiraError::ConfigError(format!("read FCM service account {path}: {e}")))?;
        let sa: ServiceAccount = serde_json::from_str(&raw)
            .map_err(|e| MiraError::ConfigError(format!("parse FCM service account JSON: {e}")))?;

        // Prefer the explicit config project_id; fall back to the JSON's.
        let project_id = cfg.project_id.clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| sa.project_id.clone());
        if project_id.trim().is_empty() {
            return Err(MiraError::ConfigError("FCM project_id is empty".into()));
        }

        let encoding_key = EncodingKey::from_rsa_pem(sa.private_key.as_bytes())
            .map_err(|e| MiraError::ConfigError(format!("FCM private key parse: {e}")))?;

        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| MiraError::ConfigError(format!("FCM http client: {e}")))?;

        Ok(Some(Self {
            project_id,
            client_email: sa.client_email,
            token_uri:    sa.token_uri,
            encoding_key: Arc::new(encoding_key),
            http,
            token_cache:  Arc::new(Mutex::new(None)),
        }))
    }

    /// Return a valid OAuth2 access token, minting + caching a new one when
    /// the cache is empty or within a minute of expiry.
    async fn access_token(&self) -> Result<String, MiraError> {
        let now = chrono::Utc::now().timestamp();
        {
            let cache = self.token_cache.lock().await;
            if let Some(tok) = cache.as_ref() {
                if now < tok.refresh_at {
                    return Ok(tok.access_token.clone());
                }
            }
        }

        // Mint the assertion JWT (1h validity) and exchange it.
        let claims = AssertionClaims {
            iss:   self.client_email.clone(),
            scope: FCM_SCOPE.to_string(),
            aud:   self.token_uri.clone(),
            iat:   now,
            exp:   now + 3600,
        };
        let assertion = encode(&Header::new(Algorithm::RS256), &claims, &self.encoding_key)
            .map_err(|e| MiraError::ConfigError(format!("FCM assertion sign: {e}")))?;

        let resp = self.http.post(&self.token_uri)
            .form(&[("grant_type", JWT_BEARER_GRANT), ("assertion", &assertion)])
            .send().await
            .map_err(|e| MiraError::ConfigError(format!("FCM token request: {}", e.without_url())))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(MiraError::ConfigError(format!("FCM token exchange HTTP {status}: {body}")));
        }
        let token: TokenResponse = resp.json().await
            .map_err(|e| MiraError::ConfigError(format!("FCM token decode: {e}")))?;

        let access = token.access_token.clone();
        {
            let mut cache = self.token_cache.lock().await;
            *cache = Some(CachedToken {
                access_token: token.access_token,
                // Refresh a minute before the real expiry.
                refresh_at:   now + token.expires_in.max(60) - 60,
            });
        }
        Ok(access)
    }

    /// Send one data-only message to a device token. Maps care/wellbeing
    /// (severity "high") to android `high` priority + a per-type collapse
    /// key so a burst of same-type events doesn't stack on the lock screen.
    pub async fn send(
        &self,
        token: &str,
        env:   &NotificationEnvelope,
    ) -> Result<(), FcmSendError> {
        let access = self.access_token().await
            .map_err(|e| FcmSendError::Other(format!("access token: {e}")))?;

        let priority = if env.severity == "high" { "high" } else { "normal" };

        // FCM data values MUST all be strings.
        let mut data = serde_json::Map::new();
        data.insert("type".into(),     env.r#type.clone().into());
        data.insert("category".into(), env.category.clone().into());
        data.insert("severity".into(), env.severity.clone().into());
        data.insert("title".into(),    env.title.clone().into());
        data.insert("body".into(),     env.body.clone().into());
        data.insert("sent_at".into(),  env.sent_at.to_string().into());
        if let Some(c) = &env.conversation_id { data.insert("conversation_id".into(), c.clone().into()); }
        if let Some(u) = &env.url            { data.insert("url".into(),            u.clone().into()); }

        let message = serde_json::json!({
            "message": {
                "token": token,
                "data":  data,
                "android": {
                    "priority":     priority,
                    "ttl":          MESSAGE_TTL,
                    "collapse_key":  env.r#type,
                }
            }
        });

        let url = format!(
            "https://fcm.googleapis.com/v1/projects/{}/messages:send",
            self.project_id
        );
        let resp = self.http.post(&url)
            .bearer_auth(&access)
            .json(&message)
            .send().await
            .map_err(|e| FcmSendError::Other(format!("send: {}", e.without_url())))?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        // A 404 (NOT_FOUND) or an UNREGISTERED error means the token is dead
        // — signal the dispatcher to prune it. Everything else is transient
        // or a config problem the operator should see logged.
        if status == reqwest::StatusCode::NOT_FOUND
            || body.contains("UNREGISTERED")
            || body.contains("NOT_FOUND")
        {
            return Err(FcmSendError::Gone);
        }
        warn!("fcm: send HTTP {status}");
        Err(FcmSendError::Other(format!("HTTP {status}: {body}")))
    }
}
