// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/calendar.rs
//! HTTP surface for the calendar module.
//!
//! * Event CRUD is scoped to the authenticated user's own events.
//! * Sync is a manual trigger for the configured external provider — admin
//!   only, since it touches external credentials.
//! * OAuth start / callback endpoints drive the Google + Outlook flows. They
//!   run per-user so each user can connect their own calendar.

use std::sync::Arc;

use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Redirect},
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::auth::{AdminUser, AuthUser, LocalAuthService};
use crate::auth::models::Role;
use crate::calendar::{
    CalendarEvent, CalendarStore, EventInput,
    caldav::CalDavSync,
    google::{GoogleSync, GOOGLE_AUTH, GOOGLE_SCOPES},
    outlook::{OutlookSync, MS_AUTH, MS_SCOPES},
    store::OAuthTokens,
    sync::CalendarSync,
};
use crate::config::MiraConfig;
use crate::MiraError;

// ── List / create events ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListEventsQuery {
    /// Earliest start time to include (ms since epoch).
    pub from:  Option<i64>,
    /// Latest start time to include (ms since epoch).
    pub to:    Option<i64>,
    pub limit: Option<i64>,
    /// Admin-only: view another user's calendar. Ignored for non-admin
    /// callers (they always see their own).
    pub user_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EventListResponse {
    pub events: Vec<CalendarEvent>,
}

pub async fn list_events(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<CalendarStore>>,
    Extension(auth):  Extension<Arc<LocalAuthService>>,
    Query(q):         Query<ListEventsQuery>,
) -> impl IntoResponse {
    let limit  = q.limit.unwrap_or(500).clamp(1, 2000);
    let target = resolve_target(&user, q.user_id.as_deref());
    // Shared owners the target sees: org-wide + each group they belong to.
    let mut shared_owners = vec![crate::calendar::store::SHARED_OWNER.to_string()];
    if let Ok(gids) = auth.list_user_group_ids(&target) {
        for g in gids { shared_owners.push(crate::calendar::store::group_owner(&g)); }
    }
    match store.list_events_scoped(&target, &shared_owners, q.from, q.to, limit) {
        Ok(events) => Json(EventListResponse { events }).into_response(),
        Err(e)     => err_response(e),
    }
}

pub async fn create_event(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<CalendarStore>>,
    Extension(auth):  Extension<Arc<LocalAuthService>>,
    Json(input):      Json<EventInput>,
) -> impl IntoResponse {
    if let Err(e) = validate_input(&input) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    let owner = match resolve_owner(&user, &auth, input.shared, input.group_id.as_deref()) {
        Ok(o)     => o,
        Err(resp) => return resp,
    };
    match store.create_event(&owner, &input) {
        Ok(ev) => (StatusCode::CREATED, Json(ev)).into_response(),
        Err(e) => err_response(e),
    }
}

/// Resolve the owner for a create/update/delete:
/// - `group_id` (admins only, must exist) → `grp:<id>` (group-scoped),
/// - else `shared` (admins only) → the org-wide owner,
/// - else the caller's own id.
/// Returns a ready 4xx response if a non-admin attempts a shared/group event or
/// the group is unknown.
fn resolve_owner(
    user:     &crate::auth::models::User,
    auth:     &LocalAuthService,
    shared:   bool,
    group_id: Option<&str>,
) -> Result<String, axum::response::Response> {
    let gid = group_id.map(str::trim).filter(|g| !g.is_empty());
    if (gid.is_some() || shared) && user.role != Role::Admin {
        return Err((StatusCode::FORBIDDEN,
            "Organization / group events are managed by admins only.").into_response());
    }
    if let Some(g) = gid {
        match auth.get_group(g) {
            Ok(Some(_)) => Ok(crate::calendar::store::group_owner(g)),
            Ok(None)    => Err((StatusCode::BAD_REQUEST, "no such group").into_response()),
            Err(e)      => Err(err_response(e)),
        }
    } else if shared {
        Ok(crate::calendar::store::SHARED_OWNER.to_string())
    } else {
        Ok(user.id.clone())
    }
}

#[derive(Debug, Deserialize)]
pub struct UserOverrideQuery {
    pub user_id: Option<String>,
}

pub async fn get_event(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<CalendarStore>>,
    Path(id):         Path<String>,
    Query(q):         Query<UserOverrideQuery>,
) -> impl IntoResponse {
    let target = resolve_target(&user, q.user_id.as_deref());
    match store.get_event(&target, &id) {
        Ok(Some(ev)) => Json(ev).into_response(),
        Ok(None)     => (StatusCode::NOT_FOUND, "event not found").into_response(),
        Err(e)       => err_response(e),
    }
}

pub async fn update_event(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<CalendarStore>>,
    Extension(auth):  Extension<Arc<LocalAuthService>>,
    Path(id):         Path<String>,
    Json(input):      Json<EventInput>,
) -> impl IntoResponse {
    if let Err(e) = validate_input(&input) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    let owner = match resolve_owner(&user, &auth, input.shared, input.group_id.as_deref()) {
        Ok(o)     => o,
        Err(resp) => return resp,
    };
    match store.update_event(&owner, &id, &input) {
        Ok(Some(ev)) => Json(ev).into_response(),
        Ok(None)     => (StatusCode::NOT_FOUND,
                         "native event not found — external events are read-only",
                        ).into_response(),
        Err(e)       => err_response(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct DeleteEventQuery {
    /// Set true to delete an org-wide shared event (admins only).
    #[serde(default)]
    pub shared: bool,
    /// Set to delete a group-scoped event (admins only).
    #[serde(default)]
    pub group_id: Option<String>,
}

pub async fn delete_event(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<CalendarStore>>,
    Extension(auth):  Extension<Arc<LocalAuthService>>,
    Path(id):         Path<String>,
    Query(q):         Query<DeleteEventQuery>,
) -> impl IntoResponse {
    let owner = match resolve_owner(&user, &auth, q.shared, q.group_id.as_deref()) {
        Ok(o)     => o,
        Err(resp) => return resp,
    };
    match store.delete_event(&owner, &id) {
        Ok(true)  => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND,
                      "native event not found — external events are read-only",
                     ).into_response(),
        Err(e)    => err_response(e),
    }
}

// ── Manual sync ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct SyncResponse {
    pub provider: String,
    pub pulled:   usize,
}

pub async fn trigger_sync(
    AdminUser(user):   AdminUser,
    Extension(store):  Extension<Arc<CalendarStore>>,
    Extension(config): Extension<Arc<MiraConfig>>,
) -> impl IntoResponse {
    let provider = config.calendar.sync_provider.clone();
    if provider == "none" {
        return (StatusCode::BAD_REQUEST,
                "calendar.sync_provider is 'none' — nothing to sync",
               ).into_response();
    }

    let result = match provider.as_str() {
        "caldav" => {
            // CalDAV is per-user now — sync the admin's OWN connected account.
            match store.get_caldav(&user.id) {
                Ok(Some(c)) => CalDavSync::new(c.url, c.username, c.password).sync_user(&user.id, &store).await,
                Ok(None)    => return (StatusCode::BAD_REQUEST,
                    "You haven't connected a CalDAV account — connect it from your Calendar page first.").into_response(),
                Err(e)      => return err_response(e),
            }
        }
        "google" => {
            let sync = GoogleSync::new(
                Arc::clone(&store),
                config.calendar.google.client_id.clone(),
                config.calendar.google.client_secret.clone(),
            );
            sync.sync_user(&user.id, &store).await
        }
        "outlook" => {
            let sync = OutlookSync::new(
                Arc::clone(&store),
                config.calendar.outlook.client_id.clone(),
                config.calendar.outlook.client_secret.clone(),
            );
            sync.sync_user(&user.id, &store).await
        }
        other => return (StatusCode::BAD_REQUEST,
                        format!("unsupported sync_provider '{}'", other),
                       ).into_response(),
    };

    match result {
        Ok(pulled) => Json(SyncResponse { provider, pulled }).into_response(),
        Err(e)     => err_response(e),
    }
}

// ── OAuth flow ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct OAuthStartQuery {
    pub provider: String,
}

#[derive(Debug, Serialize)]
pub struct OAuthStartResponse {
    pub authorize_url: String,
}

/// Kick off an OAuth authorisation flow. Returns the authorization URL the
/// browser should redirect to. Uses the user's id as the `state` parameter
/// so the callback can identify which user is connecting.
pub async fn oauth_start(
    AuthUser(user):    AuthUser,
    Extension(config): Extension<Arc<MiraConfig>>,
    Query(q):          Query<OAuthStartQuery>,
) -> impl IntoResponse {
    let (auth, scopes, client_id, redirect) = match q.provider.as_str() {
        "google" => (
            GOOGLE_AUTH,
            GOOGLE_SCOPES,
            &config.calendar.google.client_id,
            &config.calendar.google.redirect_uri,
        ),
        "outlook" => (
            MS_AUTH,
            MS_SCOPES,
            &config.calendar.outlook.client_id,
            &config.calendar.outlook.redirect_uri,
        ),
        other => return (StatusCode::BAD_REQUEST,
                        format!("unsupported OAuth provider '{}'", other),
                       ).into_response(),
    };
    if client_id.is_empty() {
        return (StatusCode::BAD_REQUEST,
                format!("calendar.{}.client_id is not configured", q.provider),
               ).into_response();
    }

    let url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code\
         &access_type=offline&prompt=consent&scope={}&state={}:{}",
        auth,
        urlencode(client_id),
        urlencode(redirect),
        urlencode(scopes),
        urlencode(&q.provider),
        urlencode(&user.id),
    );

    Json(OAuthStartResponse { authorize_url: url }).into_response()
}

#[derive(Debug, Deserialize)]
pub struct OAuthCallbackQuery {
    pub code:  Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

/// OAuth redirect endpoint. The authorization server returns here with a
/// `code` + the `state` we embedded on the start call. We exchange the code
/// for access/refresh tokens and persist them for the user.
pub async fn oauth_callback(
    Extension(store):  Extension<Arc<CalendarStore>>,
    Extension(config): Extension<Arc<MiraConfig>>,
    Query(q):          Query<OAuthCallbackQuery>,
) -> impl IntoResponse {
    if let Some(err) = q.error {
        return (StatusCode::BAD_REQUEST,
                format!("OAuth callback error: {}", err),
               ).into_response();
    }
    let Some(code) = q.code else {
        return (StatusCode::BAD_REQUEST, "missing ?code").into_response();
    };
    let Some(state) = q.state else {
        return (StatusCode::BAD_REQUEST, "missing ?state").into_response();
    };
    let Some((provider, user_id)) = state.split_once(':') else {
        return (StatusCode::BAD_REQUEST, "malformed ?state").into_response();
    };

    let (token_url, client_id, client_secret, redirect, scope) = match provider {
        "google" => (
            "https://oauth2.googleapis.com/token",
            &config.calendar.google.client_id,
            &config.calendar.google.client_secret,
            &config.calendar.google.redirect_uri,
            GOOGLE_SCOPES,
        ),
        "outlook" => (
            "https://login.microsoftonline.com/common/oauth2/v2.0/token",
            &config.calendar.outlook.client_id,
            &config.calendar.outlook.client_secret,
            &config.calendar.outlook.redirect_uri,
            MS_SCOPES,
        ),
        _ => return (StatusCode::BAD_REQUEST, "unsupported provider").into_response(),
    };

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c)  => c,
        Err(e) => return err_response(MiraError::ServerError(e.to_string())),
    };

    let resp = client.post(token_url)
        .form(&[
            ("code",          code.as_str()),
            ("client_id",     client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("redirect_uri",  redirect.as_str()),
            ("grant_type",    "authorization_code"),
            ("scope",         scope),
        ])
        .send()
        .await;

    let resp = match resp {
        Ok(r)  => r,
        Err(e) => return err_response(MiraError::ServerError(format!("token exchange: {}", e))),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body   = resp.text().await.unwrap_or_default();
        return (StatusCode::BAD_GATEWAY,
                format!("token exchange failed ({}): {}", status, body),
               ).into_response();
    }

    #[derive(Debug, Deserialize)]
    struct TokenResp {
        access_token:  String,
        #[serde(default)] refresh_token: Option<String>,
        #[serde(default)] expires_in:    Option<i64>,
        #[serde(default)] scope:         Option<String>,
    }
    let body: TokenResp = match resp.json().await {
        Ok(b)  => b,
        Err(e) => return err_response(MiraError::ServerError(format!("token body: {}", e))),
    };
    let now = chrono::Utc::now().timestamp_millis();
    let expires_at = body.expires_in.map(|s| now + s * 1000);

    let tokens = OAuthTokens {
        user_id:       user_id.to_string(),
        provider:      provider.to_string(),
        access_token:  body.access_token,
        refresh_token: body.refresh_token,
        expires_at,
        scope:         body.scope,
    };
    if let Err(e) = store.save_tokens(&tokens) {
        return err_response(e);
    }
    info!("calendar OAuth tokens stored for user={} provider={}", user_id, provider);

    // Send the user back to their Calendar page (open to all users — Settings is
    // admin-only), where the per-user connect panel shows the "connected" state.
    Redirect::to("/calendar?connected=1").into_response()
}

#[derive(Debug, Serialize)]
pub struct OAuthStatusResponse {
    /// This user's per-account connection state.
    pub google_connected:   bool,
    pub outlook_connected:  bool,
    pub caldav_connected:   bool,
    /// Whether the operator has set up that provider's OAuth app (client_id) at
    /// the instance level — i.e. whether a user can connect it at all.
    pub google_configured:  bool,
    pub outlook_configured: bool,
    /// The instance's active external provider ("none"|"caldav"|"google"|"outlook")
    /// and whether sync is on — so the per-user UI shows the relevant control.
    pub sync_provider:      String,
    pub sync_enabled:       bool,
}

pub async fn oauth_status(
    AuthUser(user):    AuthUser,
    Extension(store):  Extension<Arc<CalendarStore>>,
    Extension(config): Extension<Arc<MiraConfig>>,
) -> impl IntoResponse {
    let google  = store.get_tokens(&user.id, "google").map(|o| o.is_some()).unwrap_or(false);
    let outlook = store.get_tokens(&user.id, "outlook").map(|o| o.is_some()).unwrap_or(false);
    let caldav  = store.has_caldav(&user.id).unwrap_or(false);
    let c = &config.calendar;
    Json(OAuthStatusResponse {
        google_connected:   google,
        outlook_connected:  outlook,
        caldav_connected:   caldav,
        google_configured:  !c.google.client_id.is_empty(),
        outlook_configured: !c.outlook.client_id.is_empty(),
        sync_provider:      c.sync_provider.clone(),
        sync_enabled:       c.enabled && c.sync_provider != "none",
    }).into_response()
}

pub async fn oauth_disconnect(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<CalendarStore>>,
    Query(q):         Query<OAuthStartQuery>,
) -> impl IntoResponse {
    if !matches!(q.provider.as_str(), "google" | "outlook") {
        return (StatusCode::BAD_REQUEST, "provider must be google or outlook").into_response();
    }
    match store.delete_tokens(&user.id, &q.provider) {
        Ok(())  => StatusCode::NO_CONTENT.into_response(),
        Err(e)  => err_response(e),
    }
}

// ── Per-user CalDAV (Nextcloud etc.) ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CalDavConnectBody {
    pub url:      String,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct CalDavConnectResponse { pub synced: usize }

/// POST /api/calendar/caldav — connect (or update) THIS user's CalDAV account.
/// Per-user: unlike Google/Outlook there's no OAuth, so the user supplies their
/// own server URL + username + app-password. The credentials are validated by
/// running one sync immediately; they're only stored (password encrypted at
/// rest) if that succeeds — so we never persist creds that don't work.
pub async fn caldav_connect(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<CalendarStore>>,
    Json(body):       Json<CalDavConnectBody>,
) -> impl IntoResponse {
    let url      = body.url.trim().to_string();
    let username = body.username.trim().to_string();
    if url.is_empty() || username.is_empty() || body.password.is_empty() {
        return (StatusCode::BAD_REQUEST, "url, username and password are all required").into_response();
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return (StatusCode::BAD_REQUEST, "url must start with http:// or https://").into_response();
    }
    // Validate against the live server before storing anything.
    let sync = CalDavSync::new(url.clone(), username.clone(), body.password.clone());
    match sync.sync_user(&user.id, &store).await {
        Ok(n) => {
            if let Err(e) = store.save_caldav(&user.id, &url, &username, &body.password) {
                return err_response(e);
            }
            info!("calendar CalDAV connected for user={} ({} events)", user.id, n);
            (StatusCode::OK, Json(CalDavConnectResponse { synced: n })).into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            format!("Couldn't connect to that CalDAV account — check the URL, username and app-password. ({e})"),
        ).into_response(),
    }
}

/// POST /api/calendar/caldav/disconnect — remove THIS user's CalDAV account.
pub async fn caldav_disconnect(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<CalendarStore>>,
) -> impl IntoResponse {
    match store.delete_caldav(&user.id) {
        Ok(())  => StatusCode::NO_CONTENT.into_response(),
        Err(e)  => err_response(e),
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Pick the user whose calendar the request should target. Non-admin
/// callers are pinned to themselves regardless of `user_id`; admins may
/// view any user. This keeps the read-side admin "view-as" pattern in
/// one place instead of sprinkling it across handlers.
fn resolve_target(caller: &crate::auth::models::User, override_id: Option<&str>) -> String {
    match override_id {
        Some(id) if !id.is_empty() && caller.role == Role::Admin => id.to_string(),
        _ => caller.id.clone(),
    }
}

fn validate_input(input: &EventInput) -> Result<(), String> {
    if input.summary.trim().is_empty() {
        return Err("summary is required".into());
    }
    if input.ends_at < input.starts_at {
        return Err("ends_at must be >= starts_at".into());
    }
    Ok(())
}

fn err_response(e: MiraError) -> axum::response::Response {
    warn!("calendar handler error: {}", e);
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
}

/// Minimal URL-encoder for query values — we control the inputs (config
/// strings), so we only need to escape the reserved characters common in
/// scopes and redirect URIs. Avoids pulling a whole encoding crate for
/// five lines of code.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9'
            | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

