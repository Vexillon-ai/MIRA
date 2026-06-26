// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/channel_links.rs
//
// R1+R2: HTTP surface for users to manage their channel identity links
// and to request one-time codes used by the self-serve linking flow.
//
// All endpoints sit under /api/me/channel-links and are scoped to the
// authenticated user — admins get the same endpoints, just with their
// own user_id. Cross-user link management is intentionally NOT exposed
// here; an admin who needs to fix another user's link does it directly
// in auth.db. We can add an /api/admin/channel-links surface later if
// there's a real operator need.

use std::sync::Arc;

use axum::{
    extract::{Json, Path},
    http::StatusCode,
    response::IntoResponse,
    Extension,
};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::auth::AuthUser;
use crate::channel_identity::{ChannelLink, ChannelLinkCode, IdentityStore, LinkCodeStore};
use crate::MiraError;

fn err_resp(e: MiraError) -> axum::response::Response {
    match e {
        MiraError::NotFound(m)    => (StatusCode::NOT_FOUND, m).into_response(),
        MiraError::Forbidden      => StatusCode::FORBIDDEN.into_response(),
        MiraError::Unauthorized   => StatusCode::UNAUTHORIZED.into_response(),
        MiraError::ConfigError(m) => (StatusCode::CONFLICT, m).into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// A CPP channel string is `external:<provider_kind>` where the suffix is a
/// non-empty slug (lowercase alnum / `-` / `_`). Link codes are keyed on
/// this full string, so we accept it here even though it isn't a bare
/// `ChannelKind`.
fn is_valid_external_channel(s: &str) -> bool {
    let Some(kind) = s.strip_prefix("external:") else { return false; };
    !kind.is_empty()
        && kind.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

// ── Wire types ─────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct LinkResponse {
    pub id:          String,
    pub channel:     String,
    pub external_id: String,
    pub created_at:  i64,
    pub verified_at: i64,
}
impl From<ChannelLink> for LinkResponse {
    fn from(l: ChannelLink) -> Self {
        LinkResponse {
            id: l.id, channel: l.channel, external_id: l.external_id,
            created_at: l.created_at, verified_at: l.verified_at,
        }
    }
}

#[derive(Deserialize)]
pub struct IssueCodeRequest {
    /// "signal" / "telegram" / "discord". We validate by ChannelKind so
    /// "twitter" gets a 400 instead of stored cruft.
    pub channel: String,
}

#[derive(Serialize)]
pub struct IssueCodeResponse {
    pub code:        String,
    pub channel:     String,
    pub expires_at:  i64,
    pub ttl_seconds: i64,
}
impl From<ChannelLinkCode> for IssueCodeResponse {
    fn from(c: ChannelLinkCode) -> Self {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64).unwrap_or(0);
        let ttl_seconds = ((c.expires_at - now_ms) / 1000).max(0);
        IssueCodeResponse {
            code: c.code, channel: c.channel, expires_at: c.expires_at, ttl_seconds,
        }
    }
}

// ── Handlers ───────────────────────────────────────────────────────────

/// `GET /api/me/channel-links` — list every link the caller owns.
pub async fn list_my_links(
    AuthUser(caller): AuthUser,
    Extension(store): Extension<Arc<IdentityStore>>,
) -> impl IntoResponse {
    match store.list_for_user(&caller.id) {
        Ok(links) => {
            // Hide internal per-bot ownership rows (channels like
            // `telegram:personal:<account>`) — those are how a Personal bot
            // verifies its owner's chat, not user-facing linked accounts. Real
            // channel names never contain ':'.
            let out: Vec<LinkResponse> = links.into_iter()
                .filter(|l| !l.channel.contains(':'))
                .map(Into::into)
                .collect();
            axum::Json(out).into_response()
        }
        Err(e) => err_resp(e),
    }
}

/// `DELETE /api/me/channel-links/{id}` — unlink one of the caller's
/// identities. Ownership is verified before deletion: a caller cannot
/// drop another user's link even by guessing the id.
pub async fn delete_my_link(
    AuthUser(caller): AuthUser,
    Extension(store): Extension<Arc<IdentityStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    let row = match store.get(&id) {
        Ok(Some(l)) => l,
        Ok(None)    => return StatusCode::NOT_FOUND.into_response(),
        Err(e)      => return err_resp(e),
    };
    if row.user_id != caller.id {
        return StatusCode::FORBIDDEN.into_response();
    }
    match store.unlink(&id) {
        Ok(true)  => {
            info!(user = %caller.username, link = %id, channel = %row.channel,
                  external = %row.external_id, "channel link removed");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e)    => err_resp(e),
    }
}

/// `POST /api/me/channel-links/codes` — issue a one-time link code for
/// the caller's intended channel. Replaces any pending code for the
/// same (user, channel) pair so the user can re-request without
/// accumulating stale codes.
pub async fn issue_link_code(
    AuthUser(caller): AuthUser,
    Extension(codes): Extension<Arc<LinkCodeStore>>,
    Json(req):        Json<IssueCodeRequest>,
) -> impl IntoResponse {
    // Validate the channel name. Two accepted shapes:
    //   * a bare ChannelKind slug ("telegram", "discord", …)
    //   * a CPP channel string "external:<provider_kind>" — these don't
    //     parse as a bare ChannelKind (the kind is `external`, but the
    //     stored channel string carries the provider suffix and that's
    //     what link codes are keyed on). We accept any `external:<kind>`
    //     with a non-empty, slug-shaped suffix.
    // Wrong channel → 400 (a client request error), distinct from the
    // 409 `err_resp` maps ConfigError to for the "already linked" case.
    use std::str::FromStr;
    let valid = crate::channel_accounts::ChannelKind::from_str(&req.channel).is_ok()
        || is_valid_external_channel(&req.channel);
    if !valid {
        return (
            StatusCode::BAD_REQUEST,
            format!("Unknown channel: {}", req.channel),
        ).into_response();
    }
    match codes.issue(&caller.id, &req.channel) {
        Ok(c) => {
            info!(user = %caller.username, channel = %req.channel,
                  "issued channel link code");
            (StatusCode::CREATED, axum::Json(IssueCodeResponse::from(c))).into_response()
        }
        Err(e) => err_resp(e),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────
//
// Full HTTP-level tests: a small axum Router wired with the same three
// extensions the live router layers (auth service + identity store +
// link-code store), all backed by one temp auth.db. Requests carry a
// real signed bearer token minted via `issue_token_pair`, so the
// AuthUser extractor runs end-to-end (token verify → user lookup →
// active check). This exercises auth extraction, the ownership guard,
// and every status-code branch the handlers return.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::local::LocalAuthService;
    use crate::auth::models::{NewUser, Role};
    use crate::auth::tokens::issue_token_pair;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::{delete, get, post};
    use axum::Router;
    use tower::ServiceExt; // for `oneshot`

    const SECRET: &str = "test-secret-for-channel-links";

    struct Harness {
        _dir:    tempfile::TempDir,
        router:  Router,
        auth:    Arc<LocalAuthService>,
    }

    fn harness() -> Harness {
        let dir  = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.db");

        // All three stores share the same auth.db (the FK on user_id
        // resolves against the `users` table the auth service seeds).
        let auth = Arc::new(
            LocalAuthService::new(&path, SECRET.to_owned(), 7).unwrap(),
        );
        let identity = Arc::new(IdentityStore::open(&path).unwrap());
        let codes    = Arc::new(LinkCodeStore::open(&path).unwrap());

        let router = Router::new()
            .route("/api/me/channel-links", get(list_my_links))
            .route("/api/me/channel-links/{id}", delete(delete_my_link))
            .route("/api/me/channel-links/codes", post(issue_link_code))
            .layer(Extension(identity))
            .layer(Extension(codes))
            .layer(Extension(Arc::clone(&auth)));

        Harness { _dir: dir, router, auth }
    }

    /// Create a user and return `(user_id, bearer_token)`.
    fn make_user(auth: &LocalAuthService, username: &str) -> (String, String) {
        let user = auth.create_user(NewUser {
            username:     username.to_owned(),
            display_name: None,
            email:        None,
            password:     "pw".to_owned(),
            role:         Role::User,
        }).unwrap();
        let pair = issue_token_pair(&user, SECRET).unwrap();
        (user.id, pair.access_token)
    }

    fn req(method: &str, uri: &str, token: Option<&str>, body: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            b = b.header("Authorization", format!("Bearer {t}"));
        }
        if body.is_some() {
            b = b.header("Content-Type", "application/json");
        }
        b.body(body.map(|s| Body::from(s.to_owned())).unwrap_or_else(Body::empty)).unwrap()
    }

    async fn body_string(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn issue_code_returns_201_with_valid_channel() {
        let h = harness();
        let (_uid, token) = make_user(&h.auth, "alice");
        let resp = h.router.clone().oneshot(
            req("POST", "/api/me/channel-links/codes", Some(&token), Some(r#"{"channel":"discord"}"#)),
        ).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = body_string(resp).await;
        assert!(body.contains("LINK-"), "expected a code, got: {body}");
        assert!(body.contains("\"channel\":\"discord\""));
    }

    #[tokio::test]
    async fn issue_code_rejects_unknown_channel_with_400() {
        // This is the regression guard for the 400-not-409 fix.
        let h = harness();
        let (_uid, token) = make_user(&h.auth, "alice");
        let resp = h.router.clone().oneshot(
            req("POST", "/api/me/channel-links/codes", Some(&token), Some(r#"{"channel":"myspace"}"#)),
        ).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn issue_code_accepts_external_provider_channel() {
        // CPP channels are `external:<provider_kind>` — link codes key on
        // the full string, so the issue endpoint must accept it.
        let h = harness();
        let (_uid, token) = make_user(&h.auth, "alice");
        let resp = h.router.clone().oneshot(
            req("POST", "/api/me/channel-links/codes", Some(&token),
                Some(r#"{"channel":"external:nctalk"}"#)),
        ).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[test]
    fn external_channel_validation() {
        assert!(is_valid_external_channel("external:nctalk"));
        assert!(is_valid_external_channel("external:irc_bridge"));
        assert!(is_valid_external_channel("external:my-provider"));
        assert!(!is_valid_external_channel("external:"));        // empty suffix
        assert!(!is_valid_external_channel("external"));         // no suffix
        assert!(!is_valid_external_channel("external:has space")); // bad char
        assert!(!is_valid_external_channel("external:a:b"));     // bad char
        assert!(!is_valid_external_channel("telegram"));        // not external
    }

    #[tokio::test]
    async fn endpoints_require_auth() {
        let h = harness();
        let resp = h.router.clone().oneshot(
            req("GET", "/api/me/channel-links", None, None),
        ).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn list_is_empty_then_reflects_a_link() {
        let h = harness();
        let (uid, token) = make_user(&h.auth, "alice");

        // Empty to start.
        let resp = h.router.clone().oneshot(
            req("GET", "/api/me/channel-links", Some(&token), None),
        ).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_string(resp).await, "[]");

        // Insert a link directly via the store, then list reflects it.
        let identity = IdentityStore::open(&h._dir.path().join("auth.db")).unwrap();
        identity.link(&uid, "discord", "discord-snowflake-1").unwrap();

        let resp = h.router.clone().oneshot(
            req("GET", "/api/me/channel-links", Some(&token), None),
        ).await.unwrap();
        let body = body_string(resp).await;
        assert!(body.contains("discord-snowflake-1"), "got: {body}");
        assert!(body.contains("\"channel\":\"discord\""));
    }

    #[tokio::test]
    async fn delete_others_link_is_forbidden_not_found_leak() {
        let h = harness();
        let (alice_id, _alice_tok) = make_user(&h.auth, "alice");
        let (_bob_id, bob_tok)     = make_user(&h.auth, "bob");

        // Alice owns a link.
        let identity = IdentityStore::open(&h._dir.path().join("auth.db")).unwrap();
        let link = identity.link(&alice_id, "discord", "snowflake-A").unwrap();

        // Bob tries to delete it → 403, and the link survives.
        let resp = h.router.clone().oneshot(
            req("DELETE", &format!("/api/me/channel-links/{}", link.id), Some(&bob_tok), None),
        ).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(identity.get(&link.id).unwrap().is_some(), "link must survive a forbidden delete");
    }

    #[tokio::test]
    async fn delete_own_link_returns_204_and_removes_it() {
        let h = harness();
        let (alice_id, alice_tok) = make_user(&h.auth, "alice");
        let identity = IdentityStore::open(&h._dir.path().join("auth.db")).unwrap();
        let link = identity.link(&alice_id, "telegram", "tg-123").unwrap();

        let resp = h.router.clone().oneshot(
            req("DELETE", &format!("/api/me/channel-links/{}", link.id), Some(&alice_tok), None),
        ).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert!(identity.get(&link.id).unwrap().is_none(), "link must be gone");
    }

    #[tokio::test]
    async fn delete_unknown_link_returns_404() {
        let h = harness();
        let (_uid, token) = make_user(&h.auth, "alice");
        let resp = h.router.clone().oneshot(
            req("DELETE", "/api/me/channel-links/does-not-exist", Some(&token), None),
        ).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
