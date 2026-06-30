// SPDX-License-Identifier: AGPL-3.0-or-later

// src/notifications/web_push.rs
//! Web Push (VAPID) — Q1.2 of the post-research roadmap.
//!
//! Lets users opt in to browser/phone push notifications so companion
//! check-ins reach them without an open browser tab or the messenger
//! app. Uses [`web-push-native`] for the http-ece + VAPID work; the
//! actual HTTP POST goes through our shared `reqwest` client so we
//! stay on `rustls-tls` (no openssl dependency drag).
//!
//! Lifecycle:
//! 1. Server boot — `WebPushService::open` loads or generates a VAPID
//!    keypair (PEM under `data_dir/web_push_vapid.key`) and opens the
//!    subscriptions SQLite table.
//! 2. Client — fetches the public key from `GET /api/notifications/push/public-key`,
//!    calls `PushManager.subscribe()`, POSTs the resulting subscription
//!    to `POST /api/notifications/push/subscribe`.
//! 3. NotificationBus — `spawn_bus_forwarder` subscribes to the bus
//!    and fans out `ConversationUpdated` events that carry a user_id
//!    to every registered push subscription for that user.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::{engine::general_purpose, Engine as _};
use p256::PublicKey;
use rusqlite::{params, Connection};
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, warn};
// `jwt_simple` is re-exported from web-push-native so we don't have to
// take a direct dep on it (the crate's API depends on these exact
// version-pinned types — going through the re-export keeps us in lockstep).
use web_push_native::jwt_simple::algorithms::{ECDSAP256KeyPairLike, ES256KeyPair};
use web_push_native::{Auth, WebPushBuilder};

use crate::notifications::{NotificationBus, NotificationEnvelope};
use crate::MiraError;

/// File under `data_dir/` that stores the PEM-encoded VAPID keypair.
/// Generated on first boot and reused thereafter so existing browser
/// subscriptions don't get invalidated across restarts.
const VAPID_KEY_FILENAME: &str = "web_push_vapid.key";

/// `mailto:` or `https:` contact passed in the VAPID `sub` claim — push
/// gateways (Mozilla, Google) use this to reach out if our pushes are
/// causing trouble. Override via config in a later slice; today we just
/// pin a sensible default.
const VAPID_CONTACT: &str = "mailto:admin@mira.local";

// ── Service ──────────────────────────────────────────────────────────────────

/// The proactive-notification dispatcher. Despite the historical name, this
/// is the single transport-agnostic fan-out point: it owns the subscription
/// store and delivers each event to every registered subscription for a
/// user, branching by `kind` — Web Push (VAPID, built in) and, when
/// configured, Firebase Cloud Messaging for the native app. (We kept the
/// `WebPushService` name rather than introducing a parallel
/// `NotificationDispatcher` type so the existing Extension wiring and call
/// sites stay put — see design-docs/mobile-app-support.md.)
#[derive(Clone)]
pub struct WebPushService {
    keypair: Arc<ES256KeyPair>,
    store:   Arc<WebPushStore>,
    http:    reqwest::Client,
    /// Optional FCM transport — `None` when `notifications.fcm.enabled=false`.
    fcm:     Option<crate::notifications::fcm::FcmService>,
}

impl WebPushService {
    /// Open the service: load or mint a VAPID keypair, open the
    /// subscriptions store, and attach the optional FCM transport.
    /// Idempotent; safe to call from tests.
    pub fn open(
        data_dir: &Path,
        db_path:  &Path,
        fcm:      Option<crate::notifications::fcm::FcmService>,
    ) -> Result<Self, MiraError> {
        let keypair = load_or_create_keypair(&data_dir.join(VAPID_KEY_FILENAME))?;
        let store   = Arc::new(WebPushStore::open(db_path)?);
        let http    = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| MiraError::ConfigError(format!("web push http client: {e}")))?;
        Ok(Self { keypair: Arc::new(keypair), store, http, fcm })
    }

    /// The VAPID public key in uncompressed SEC1 form, base64url-no-pad
    /// encoded. The browser passes this as `applicationServerKey` to
    /// `PushManager.subscribe()`.
    pub fn vapid_public_key_b64url(&self) -> String {
        // ES256KeyPair → P256KeyPair → P256PublicKey carries
        // `to_bytes_uncompressed` (the SEC1 form the W3C Push API
        // expects as `applicationServerKey`).
        let bytes = self.keypair.key_pair().public_key().to_bytes_uncompressed();
        general_purpose::URL_SAFE_NO_PAD.encode(&bytes)
    }

    /// Persist a new subscription. Endpoint uniqueness is enforced at
    /// the DB layer — re-subscribing the same browser is idempotent
    /// and refreshes the `updated_at` timestamp.
    pub fn subscribe(
        &self,
        user_id:   &str,
        endpoint:  &str,
        p256dh:    &str,
        auth:      &str,
        user_agent: Option<&str>,
    ) -> Result<String, MiraError> {
        self.store.upsert(user_id, endpoint, p256dh, auth, user_agent)
    }

    /// Register (or refresh) an FCM device token for the native app. Tokens
    /// are deduped by value — re-registering the same token just refreshes
    /// its `platform` / `device_name` / `updated_at`.
    pub fn subscribe_fcm(
        &self,
        user_id:     &str,
        token:       &str,
        platform:    Option<&str>,
        device_name: Option<&str>,
    ) -> Result<String, MiraError> {
        self.store.upsert_fcm(user_id, token, platform, device_name)
    }

    /// Register (or refresh) a generic **HTTP push endpoint** (PUSH-NOTIFICATIONS
    /// Part C) — the mechanism-agnostic transport. `endpoint_url` is the
    /// FCM-relay `push_url` (push.vexillon.ai) or a UnifiedPush distributor URL;
    /// `auth_secret`, when present, is sent as `Authorization: Bearer …`. MIRA
    /// holds no Google credentials — it just POSTs the §4 envelope to this URL.
    /// Deduped by endpoint URL (re-registering the same URL refreshes the row).
    pub fn subscribe_http(
        &self,
        user_id:      &str,
        endpoint_url: &str,
        auth_secret:  Option<&str>,
        platform:     Option<&str>,
        device_name:  Option<&str>,
    ) -> Result<String, MiraError> {
        self.store.upsert_http(user_id, endpoint_url, auth_secret.unwrap_or(""), platform, device_name)
    }

    /// Whether the FCM transport is configured + enabled.
    pub fn fcm_enabled(&self) -> bool {
        self.fcm.is_some()
    }

    /// Remove a single subscription. Caller scopes by user_id so a user
    /// can only delete their own rows (admin scope handled at the HTTP
    /// layer).
    pub fn unsubscribe(&self, sub_id: &str, user_id: &str) -> Result<(), MiraError> {
        self.store.delete(sub_id, user_id)
    }

    /// List a user's subscriptions (for the Settings UI to show
    /// "registered devices" and let the user revoke individual ones).
    pub fn list_for_user(&self, user_id: &str) -> Result<Vec<PushSubscription>, MiraError> {
        self.store.list_for_user(user_id)
    }

    /// Fan out a push payload to every active subscription for
    /// `user_id`. Failures per-subscription are logged and swallowed;
    /// the only fatal error is "user has no auth contact." If a push
    /// gateway returns 404 or 410 we delete the dead subscription so
    /// the next call doesn't re-attempt it.
    pub async fn send_to_user(
        &self,
        user_id: &str,
        payload: &NotificationEnvelope,
    ) -> Result<u32, MiraError> {
        use crate::notifications::fcm::FcmSendError;

        let subs = self.store.list_for_user(user_id)?;
        if subs.is_empty() { return Ok(0); }
        let body = serde_json::to_vec(payload)
            .map_err(|e| MiraError::ConfigError(format!("payload serialise: {e}")))?;
        let mut delivered = 0u32;
        for sub in subs {
            match sub.kind.as_str() {
                // FCM device token (native app). `endpoint` holds the token.
                "fcm" => {
                    let Some(fcm) = &self.fcm else { continue }; // FCM disabled — skip.
                    match fcm.send(&sub.endpoint, payload).await {
                        Ok(()) => delivered += 1,
                        Err(FcmSendError::Gone) => {
                            debug!("fcm: token gone for sub {} — removing", sub.id);
                            let _ = self.store.delete(&sub.id, user_id);
                        }
                        Err(FcmSendError::Other(e)) => {
                            warn!("fcm: send to {} failed: {e}", sub.id);
                        }
                    }
                }
                // Generic HTTP push endpoint (FCM relay / UnifiedPush). POST the
                // envelope to the endpoint URL with an optional bearer secret;
                // a 410 means the endpoint is dead → prune (PUSH-NOTIFICATIONS
                // Part C). MIRA never touches Google/Firebase here.
                "http" => match self.send_http(&sub, &body).await {
                    Ok(()) => delivered += 1,
                    Err(WebPushSendError::Gone) => {
                        debug!("http push: endpoint gone (410) for sub {} — removing", sub.id);
                        let _ = self.store.delete(&sub.id, user_id);
                    }
                    Err(WebPushSendError::Other(e)) => {
                        warn!("http push: send to {} failed: {e}", sub.id);
                    }
                },
                // Web Push (VAPID) — the default for browser subscriptions.
                _ => match self.send_one(&sub, &body).await {
                    Ok(()) => delivered += 1,
                    Err(WebPushSendError::Gone) => {
                        debug!("web push: gateway 404/410 for sub {} — removing", sub.id);
                        let _ = self.store.delete(&sub.id, user_id);
                    }
                    Err(WebPushSendError::Other(e)) => {
                        warn!("web push: send to {} failed: {e}", sub.id);
                    }
                },
            }
        }
        Ok(delivered)
    }

    async fn send_one(
        &self,
        sub:  &PushSubscription,
        body: &[u8],
    ) -> Result<(), WebPushSendError> {
        let endpoint = sub.endpoint.parse::<http::Uri>()
            .map_err(|e| WebPushSendError::Other(format!("endpoint parse: {e}")))?;
        let pubkey_bytes = general_purpose::URL_SAFE_NO_PAD.decode(&sub.p256dh)
            .or_else(|_| general_purpose::STANDARD.decode(&sub.p256dh))
            .map_err(|e| WebPushSendError::Other(format!("p256dh decode: {e}")))?;
        let auth_bytes = general_purpose::URL_SAFE_NO_PAD.decode(&sub.auth)
            .or_else(|_| general_purpose::STANDARD.decode(&sub.auth))
            .map_err(|e| WebPushSendError::Other(format!("auth decode: {e}")))?;
        let ua_pub = PublicKey::from_sec1_bytes(&pubkey_bytes)
            .map_err(|e| WebPushSendError::Other(format!("p256dh parse: {e}")))?;
        if auth_bytes.len() != 16 {
            return Err(WebPushSendError::Other(format!(
                "auth must be 16 bytes, got {}", auth_bytes.len()
            )));
        }
        let ua_auth = Auth::clone_from_slice(&auth_bytes);

        let request = WebPushBuilder::new(endpoint.clone(), ua_pub, ua_auth)
            .with_vapid(self.keypair.as_ref(), VAPID_CONTACT)
            .build(body.to_vec())
            .map_err(|e| WebPushSendError::Other(format!("build: {e}")))?;

        // The web-push-native crate returns an `http::Request` — convert
        // to a reqwest call so we stay on the project's existing TLS
        // stack (rustls) rather than pulling in isahc / hyper-tls.
        let (parts, payload) = request.into_parts();
        let mut req = self.http.request(
            reqwest::Method::POST,
            parts.uri.to_string(),
        ).body(payload);
        for (name, value) in parts.headers.iter() {
            if let Ok(v) = value.to_str() {
                req = req.header(name.as_str(), v);
            }
        }
        let resp = req.send().await
            // `without_url()` keeps the FCM endpoint URL — which embeds the
            // browser's push-subscription token — out of the logged error.
            .map_err(|e| WebPushSendError::Other(format!("send: {}", e.without_url())))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
            return Err(WebPushSendError::Gone);
        }
        let body_txt = resp.text().await.unwrap_or_default();
        Err(WebPushSendError::Other(format!("HTTP {status}: {body_txt}")))
    }

    /// POST the envelope to a generic HTTP push endpoint (FCM relay /
    /// UnifiedPush). Sends `Authorization: Bearer <auth>` when the subscription
    /// carries a secret. Per the Part C contract, a **410 Gone** (and we also
    /// fold in 404) means the endpoint is dead → surface `Gone` so the caller
    /// prunes the subscription.
    async fn send_http(
        &self,
        sub:  &PushSubscription,
        body: &[u8],
    ) -> Result<(), WebPushSendError> {
        let mut req = self.http
            .request(reqwest::Method::POST, &sub.endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_vec());
        if !sub.auth.is_empty() {
            req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {}", sub.auth));
        }
        let resp = req.send().await
            // `without_url()` keeps the endpoint (which embeds the push_id) out
            // of logged errors.
            .map_err(|e| WebPushSendError::Other(format!("send: {}", e.without_url())))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        if status == reqwest::StatusCode::GONE || status == reqwest::StatusCode::NOT_FOUND {
            return Err(WebPushSendError::Gone);
        }
        let body_txt = resp.text().await.unwrap_or_default();
        Err(WebPushSendError::Other(format!(
            "HTTP {status}: {}", body_txt.chars().take(200).collect::<String>()
        )))
    }
}

enum WebPushSendError {
    Gone,
    Other(String),
}

// ── Bus forwarder ────────────────────────────────────────────────────────────

/// Spawn a tokio task that subscribes to the NotificationBus and forwards
/// each event to web-push subscribers. Today we forward only the
/// `ConversationUpdated` events that carry a non-empty user_id —
/// companion check-ins and inbound messages on non-web channels both
/// emit these. Tests can drop this task without losing functionality.
pub fn spawn_bus_forwarder(bus: Arc<NotificationBus>, service: WebPushService) {
    tokio::spawn(async move {
        let mut rx = bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(notif) => {
                    let Some(user_id) = notif.user_id.clone() else { continue };
                    let payload = notif.to_envelope();
                    if let Err(e) = service.send_to_user(&user_id, &payload).await {
                        warn!("web push: forwarder send_to_user failed: {e}");
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    warn!("web push: forwarder lagged by {n} events");
                }
                Err(RecvError::Closed) => {
                    info!("web push: forwarder exiting (bus closed)");
                    return;
                }
            }
        }
    });
}

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct PushSubscription {
    pub id:         String,
    pub user_id:    String,
    /// Web Push endpoint URL, or — for `kind == "fcm"` — the device token.
    pub endpoint:   String,
    pub p256dh:     String,
    pub auth:       String,
    pub user_agent: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Transport: `"webpush"` (default) or `"fcm"`.
    pub kind:        String,
    /// Native-client platform (e.g. "android"). FCM rows only.
    pub platform:    Option<String>,
    /// Native-client device label. FCM rows only.
    pub device_name: Option<String>,
}

// ── Store ────────────────────────────────────────────────────────────────────

struct WebPushStore {
    conn: Mutex<Connection>,
}

impl WebPushStore {
    fn open(path: &Path) -> Result<Self, MiraError> {
        let conn = Connection::open(path)
            .map_err(|e| MiraError::ConfigError(format!("web push db open: {e}")))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS web_push_subscriptions (
               id          TEXT PRIMARY KEY,
               user_id     TEXT NOT NULL,
               endpoint    TEXT NOT NULL UNIQUE,
               p256dh      TEXT NOT NULL,
               auth        TEXT NOT NULL,
               user_agent  TEXT,
               created_at  INTEGER NOT NULL,
               updated_at  INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_web_push_user
               ON web_push_subscriptions(user_id);"
        ).map_err(|e| MiraError::ConfigError(format!("web push schema: {e}")))?;

        // 0.282.0 — additive columns for the FCM transport. Existing rows
        // (browser Web Push) default to kind='webpush'. For FCM rows the
        // `endpoint` column holds the device token (reusing its UNIQUE
        // constraint for per-token dedup); p256dh/auth are empty. ALTER is
        // idempotent via error-swallow — SQLite has no IF NOT EXISTS here.
        for sql in [
            "ALTER TABLE web_push_subscriptions ADD COLUMN kind TEXT NOT NULL DEFAULT 'webpush'",
            "ALTER TABLE web_push_subscriptions ADD COLUMN platform TEXT",
            "ALTER TABLE web_push_subscriptions ADD COLUMN device_name TEXT",
        ] {
            let _ = conn.execute(sql, []);
        }
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn upsert(
        &self,
        user_id:   &str,
        endpoint:  &str,
        p256dh:    &str,
        auth:      &str,
        user_agent: Option<&str>,
    ) -> Result<String, MiraError> {
        let conn = self.conn.lock().expect("web push store poisoned");
        let now = chrono::Utc::now().timestamp_millis();
        // If the same endpoint is already registered, refresh the row
        // in-place so we keep one subscription per browser even when
        // p256dh/auth rotate (some browsers cycle these).
        let existing: Option<String> = conn.query_row(
            "SELECT id FROM web_push_subscriptions WHERE endpoint = ?1",
            params![endpoint],
            |r| r.get(0),
        ).ok();
        let id = match existing {
            Some(id) => {
                conn.execute(
                    "UPDATE web_push_subscriptions
                     SET user_id = ?2, p256dh = ?3, auth = ?4,
                         user_agent = ?5, updated_at = ?6
                     WHERE id = ?1",
                    params![id, user_id, p256dh, auth, user_agent, now],
                ).map_err(|e| MiraError::ConfigError(format!("web push update: {e}")))?;
                id
            }
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                conn.execute(
                    "INSERT INTO web_push_subscriptions
                       (id, user_id, endpoint, p256dh, auth, user_agent, created_at, updated_at, kind)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, 'webpush')",
                    params![id, user_id, endpoint, p256dh, auth, user_agent, now],
                ).map_err(|e| MiraError::ConfigError(format!("web push insert: {e}")))?;
                id
            }
        };
        Ok(id)
    }

    /// Upsert an FCM device-token subscription, deduped by token (stored in
    /// the `endpoint` column). p256dh/auth are unused for FCM rows — set to
    /// empty strings to satisfy NOT NULL.
    fn upsert_fcm(
        &self,
        user_id:     &str,
        token:       &str,
        platform:    Option<&str>,
        device_name: Option<&str>,
    ) -> Result<String, MiraError> {
        let conn = self.conn.lock().expect("web push store poisoned");
        let now = chrono::Utc::now().timestamp_millis();
        let existing: Option<String> = conn.query_row(
            "SELECT id FROM web_push_subscriptions WHERE endpoint = ?1",
            params![token],
            |r| r.get(0),
        ).ok();
        let id = match existing {
            Some(id) => {
                conn.execute(
                    "UPDATE web_push_subscriptions
                     SET user_id = ?2, kind = 'fcm', platform = ?3,
                         device_name = ?4, updated_at = ?5
                     WHERE id = ?1",
                    params![id, user_id, platform, device_name, now],
                ).map_err(|e| MiraError::ConfigError(format!("fcm update: {e}")))?;
                id
            }
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                conn.execute(
                    "INSERT INTO web_push_subscriptions
                       (id, user_id, endpoint, p256dh, auth, user_agent,
                        created_at, updated_at, kind, platform, device_name)
                     VALUES (?1, ?2, ?3, '', '', NULL, ?4, ?4, 'fcm', ?5, ?6)",
                    params![id, user_id, token, now, platform, device_name],
                ).map_err(|e| MiraError::ConfigError(format!("fcm insert: {e}")))?;
                id
            }
        };
        Ok(id)
    }

    /// Upsert a generic HTTP push-endpoint subscription (Part C), deduped by
    /// endpoint URL (stored in the `endpoint` column). The bearer secret lives
    /// in the `auth` column (empty when none); `p256dh` is unused.
    fn upsert_http(
        &self,
        user_id:      &str,
        endpoint_url: &str,
        auth_secret:  &str,
        platform:     Option<&str>,
        device_name:  Option<&str>,
    ) -> Result<String, MiraError> {
        let conn = self.conn.lock().expect("web push store poisoned");
        let now = chrono::Utc::now().timestamp_millis();
        let existing: Option<String> = conn.query_row(
            "SELECT id FROM web_push_subscriptions WHERE endpoint = ?1",
            params![endpoint_url],
            |r| r.get(0),
        ).ok();
        let id = match existing {
            Some(id) => {
                conn.execute(
                    "UPDATE web_push_subscriptions
                     SET user_id = ?2, kind = 'http', auth = ?3, platform = ?4,
                         device_name = ?5, updated_at = ?6
                     WHERE id = ?1",
                    params![id, user_id, auth_secret, platform, device_name, now],
                ).map_err(|e| MiraError::ConfigError(format!("http push update: {e}")))?;
                id
            }
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                conn.execute(
                    "INSERT INTO web_push_subscriptions
                       (id, user_id, endpoint, p256dh, auth, user_agent,
                        created_at, updated_at, kind, platform, device_name)
                     VALUES (?1, ?2, ?3, '', ?4, NULL, ?5, ?5, 'http', ?6, ?7)",
                    params![id, user_id, endpoint_url, auth_secret, now, platform, device_name],
                ).map_err(|e| MiraError::ConfigError(format!("http push insert: {e}")))?;
                id
            }
        };
        Ok(id)
    }

    fn delete(&self, sub_id: &str, user_id: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().expect("web push store poisoned");
        conn.execute(
            "DELETE FROM web_push_subscriptions WHERE id = ?1 AND user_id = ?2",
            params![sub_id, user_id],
        ).map_err(|e| MiraError::ConfigError(format!("web push delete: {e}")))?;
        Ok(())
    }

    fn list_for_user(&self, user_id: &str) -> Result<Vec<PushSubscription>, MiraError> {
        let conn = self.conn.lock().expect("web push store poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, user_id, endpoint, p256dh, auth, user_agent, created_at, updated_at,
                    kind, platform, device_name
             FROM web_push_subscriptions WHERE user_id = ?1
             ORDER BY updated_at DESC"
        ).map_err(|e| MiraError::ConfigError(format!("web push list prep: {e}")))?;
        let rows = stmt.query_map(params![user_id], |r| {
            Ok(PushSubscription {
                id:          r.get(0)?,
                user_id:     r.get(1)?,
                endpoint:    r.get(2)?,
                p256dh:      r.get(3)?,
                auth:        r.get(4)?,
                user_agent:  r.get(5)?,
                created_at:  r.get(6)?,
                updated_at:  r.get(7)?,
                kind:        r.get(8)?,
                platform:    r.get(9)?,
                device_name: r.get(10)?,
            })
        }).map_err(|e| MiraError::ConfigError(format!("web push list query: {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| MiraError::ConfigError(format!("web push row: {e}")))?);
        }
        Ok(out)
    }
}

// ── Keypair persistence ──────────────────────────────────────────────────────

/// Load the VAPID keypair from disk; mint a fresh one if the file
/// doesn't exist yet. Stored as raw 32-byte private scalar so we don't
/// drag in PEM encode/decode for jwt_simple's flavour.
fn load_or_create_keypair(path: &Path) -> Result<ES256KeyPair, MiraError> {
    if path.exists() {
        let raw = std::fs::read(path)
            .map_err(|e| MiraError::ConfigError(format!("vapid key read: {e}")))?;
        return ES256KeyPair::from_bytes(&raw)
            .map_err(|e| MiraError::ConfigError(format!("vapid key parse: {e}")));
    }
    info!("Minting fresh VAPID keypair at {}", path.display());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| MiraError::ConfigError(format!("vapid keypair dir: {e}")))?;
    }
    let kp = ES256KeyPair::generate();
    std::fs::write(path, kp.to_bytes())
        .map_err(|e| MiraError::ConfigError(format!("vapid key write: {e}")))?;
    // Restrict to owner-read on unix so the private key isn't
    // group/world-readable when we're sharing a data_dir.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(kp)
}

// ── Helpers re-exported for the HTTP layer ───────────────────────────────────

/// Convenience wrapper used by the HTTP layer when a single
/// `WebPushService` clone is the dependency we want to thread.
pub fn service_path(data_dir: &Path) -> PathBuf {
    data_dir.join("web_push.db")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn open_mints_a_keypair_and_returns_a_public_key() {
        let dir = tempdir().unwrap();
        let svc = WebPushService::open(dir.path(), &dir.path().join("wp.db"), None).unwrap();
        let pk = svc.vapid_public_key_b64url();
        // Uncompressed SEC1 P-256 = 65 bytes → 87 base64url chars.
        assert_eq!(pk.len(), 87, "VAPID public key should be 87 b64url chars");
        // Reopening reuses the same keypair (subscriptions don't churn).
        let svc2 = WebPushService::open(dir.path(), &dir.path().join("wp.db"), None).unwrap();
        assert_eq!(pk, svc2.vapid_public_key_b64url());
    }

    #[test]
    fn subscribe_is_idempotent_per_endpoint() {
        let dir = tempdir().unwrap();
        let svc = WebPushService::open(dir.path(), &dir.path().join("wp.db"), None).unwrap();
        let id1 = svc.subscribe("alice", "https://fcm/x", "pubkey", "auth123456789012", None).unwrap();
        let id2 = svc.subscribe("alice", "https://fcm/x", "pubkey", "auth123456789012", None).unwrap();
        assert_eq!(id1, id2, "same endpoint refreshes the row instead of creating a second");
        let list = svc.list_for_user("alice").unwrap();
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn unsubscribe_scoped_by_user() {
        let dir = tempdir().unwrap();
        let svc = WebPushService::open(dir.path(), &dir.path().join("wp.db"), None).unwrap();
        let id = svc.subscribe("alice", "https://fcm/y", "pk", "auth123456789012", None).unwrap();
        // Wrong user can't delete alice's row.
        svc.unsubscribe(&id, "mallory").unwrap();
        assert_eq!(svc.list_for_user("alice").unwrap().len(), 1);
        // Owner can.
        svc.unsubscribe(&id, "alice").unwrap();
        assert!(svc.list_for_user("alice").unwrap().is_empty());
    }

    // ── Part C — generic HTTP push endpoint ──

    #[test]
    fn http_subscribe_dedups_by_url_and_stores_secret() {
        let dir = tempdir().unwrap();
        let svc = WebPushService::open(dir.path(), &dir.path().join("wp.db"), None).unwrap();
        let url = "https://push.vexillon.ai/v1/p/abc";
        let id1 = svc.subscribe_http("alice", url, Some("sek"), Some("android"), Some("Pixel 8")).unwrap();
        let id2 = svc.subscribe_http("alice", url, Some("rotated"), Some("android"), Some("Pixel 8")).unwrap();
        assert_eq!(id1, id2, "same endpoint URL refreshes the row");
        let list = svc.list_for_user("alice").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].kind, "http");
        assert_eq!(list[0].endpoint, url);
        assert_eq!(list[0].auth, "rotated", "secret refreshed on re-subscribe");
        assert_eq!(list[0].device_name.as_deref(), Some("Pixel 8"));
    }

    /// Accept one connection, capture the raw request, reply with `status_line`.
    async fn oneshot_server(status_line: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/p/test");
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let resp = format!("HTTP/1.1 {status_line}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
            req
        });
        (url, handle)
    }

    fn sample_envelope() -> NotificationEnvelope {
        crate::notifications::Notification {
            kind:    crate::notifications::NotificationKind::ConversationUpdated,
            message: Some("hi".into()),
            channel: Some("web".into()),
            ..Default::default()
        }.to_envelope()
    }

    #[tokio::test]
    async fn http_dispatch_posts_envelope_with_bearer() {
        let (url, handle) = oneshot_server("200 OK").await;
        let dir = tempdir().unwrap();
        let svc = WebPushService::open(dir.path(), &dir.path().join("wp.db"), None).unwrap();
        svc.subscribe_http("alice", &url, Some("secret-xyz"), None, None).unwrap();

        let delivered = svc.send_to_user("alice", &sample_envelope()).await.unwrap();
        assert_eq!(delivered, 1);

        let req = handle.await.unwrap().to_lowercase();
        assert!(req.contains("authorization: bearer secret-xyz"), "missing bearer; req:\n{req}");
        assert!(req.contains("\"type\""), "envelope JSON should be in the body");
        // A success leaves the subscription in place.
        assert_eq!(svc.list_for_user("alice").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn http_dispatch_prunes_on_410() {
        let (url, handle) = oneshot_server("410 Gone").await;
        let dir = tempdir().unwrap();
        let svc = WebPushService::open(dir.path(), &dir.path().join("wp.db"), None).unwrap();
        svc.subscribe_http("bob", &url, None, None, None).unwrap();

        let delivered = svc.send_to_user("bob", &sample_envelope()).await.unwrap();
        assert_eq!(delivered, 0, "410 is not a delivery");
        let _ = handle.await;
        // Contract: 410 → prune the dead endpoint.
        assert!(svc.list_for_user("bob").unwrap().is_empty(), "410 should prune the subscription");
    }
}
