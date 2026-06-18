// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/onboarding.rs
//! Onboarding state / start / restart-group endpoints.
//!
//! The onboarding system prompt and tool set are wired on the chat turn
//! itself (see `handlers/chat.rs`). These endpoints own the lifecycle knobs
//! around that: telling the UI where a user stands, spawning the
//! onboarding conversation, and letting a user redo a single group without
//! wiping their whole profile.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Json, Query},
    http::StatusCode,
    response::IntoResponse,
    Extension,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::warn;

use crate::agent::AgentCore;
use crate::auth::{AuthUser, LocalAuthService, Role};
use crate::history::{HistoryStore, MessageRole, NewConversation, NewMessage};
use crate::onboarding::{profile_md_path, OnboardingSchema};
use crate::tools::onboarding::{finalize_onboarding, FinalizeError};
use crate::MiraError;

/// Canned opener the server writes into a fresh onboarding conversation so
/// the user lands on a friendly greeting instead of an empty chat waiting
/// for them to speak first. Deterministic text — no LLM inference at
/// conversation-creation time keeps `/start` fast and predictable, and the
/// onboarding system prompt still steers every follow-up turn.
const KICKOFF_OPENER: &str = "\
Hi! I'm really glad you're here. I'd love to learn a few things about you so \
I can be more useful — it's a short chat, and you can skip anything you'd \
rather not answer or pause at any time.

Let's start simple: what should I call you? Your full name if you'd like to \
share it, or just a first name or nickname — whichever feels right.";

/// Filesystem root for per-user data. Layered as an Extension so handlers
/// can resolve `{data_dir}/profiles/...` without plumbing the whole config.
#[derive(Debug, Clone)]
pub struct DataDir(pub Arc<PathBuf>);

// ── DTOs ──────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct TargetUserQuery {
    /// Admin-only: inspect or mutate another user's onboarding state.
    pub user_id: Option<String>,
}

/// Schema-level summary for one group. The UI uses this to label dots on
/// the progress strip and build the Settings revisit list.
#[derive(Serialize)]
pub struct GroupSummary {
    pub id:       String,
    pub label:    String,
    pub optional: bool,
}

#[derive(Serialize)]
pub struct OnboardingStateResponse {
    pub user_id:                 String,
    pub onboarded_at:            Option<i64>,
    pub active_conversation_id:  Option<String>,
    pub completed_groups:        Vec<String>,
    pub skipped_keys:            Vec<String>,
    pub remaining_groups:        Vec<String>,
    pub total_groups:            usize,
    /// All groups, in schema order. Lets the UI show labels without
    /// maintaining a duplicate table on the frontend.
    pub groups:                  Vec<GroupSummary>,
}

#[derive(Serialize)]
pub struct StartOnboardingResponse {
    pub conversation_id: String,
    /// `true` when this call reused an existing active conversation,
    /// `false` when a new one was created. Lets the UI decide whether to
    /// show a "picking up where you left off" hint.
    pub resumed: bool,
}

#[derive(Deserialize)]
pub struct RestartGroupRequest {
    pub group_id: String,
    /// Optional admin override, mirrors the query-param form but keeps
    /// the target user in the JSON body so curl/axios flows are simple.
    #[serde(default)]
    pub user_id:  Option<String>,
}

// ── Shared helpers ────────────────────────────────────────────────────────────

/// Resolve the target user id. Non-admins always act on themselves; admins
/// can address another user via `?user_id=` or a body field.
fn resolve_target<'a>(
    caller:       &'a crate::auth::User,
    requested:    Option<&'a str>,
) -> Result<&'a str, axum::response::Response> {
    match requested {
        Some(uid) if uid != caller.id => {
            if caller.role == Role::Admin {
                Ok(uid)
            } else {
                Err((StatusCode::FORBIDDEN, "Only admins can target another user").into_response())
            }
        }
        _ => Ok(caller.id.as_str()),
    }
}

fn err_response(e: MiraError) -> axum::response::Response {
    match e {
        MiraError::NotFound(msg) => (StatusCode::NOT_FOUND, msg).into_response(),
        _                        => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Decode `onboarding_progress` into a JSON object, tolerating null/absent.
fn load_progress(auth: &LocalAuthService, user_id: &str) -> Result<Value, MiraError> {
    let raw = auth.get_profile(user_id)?
        .and_then(|p| p.onboarding_progress)
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| json!({}));
    let obj = if raw.is_object() { raw } else { json!({}) };
    Ok(obj)
}

fn save_progress(auth: &LocalAuthService, user_id: &str, progress: &Value) -> Result<(), MiraError> {
    let s = serde_json::to_string(progress)
        .map_err(|e| MiraError::ConfigError(format!("progress JSON serialize: {}", e)))?;
    auth.set_onboarding_progress(user_id, &s)
}

fn string_array(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

// ── GET /api/onboarding/state ────────────────────────────────────────────────

pub async fn get_state(
    AuthUser(caller):  AuthUser,
    Extension(auth):   Extension<Arc<LocalAuthService>>,
    Extension(store):  Extension<Arc<HistoryStore>>,
    Query(q):          Query<TargetUserQuery>,
) -> axum::response::Response {
    let target = match resolve_target(&caller, q.user_id.as_deref()) {
        Ok(id)  => id.to_owned(),
        Err(r)  => return r,
    };

    // Schema drives the "what's left" computation. Load it once per request —
    // it's a small validated struct, and the alternative is a startup-time
    // extension we'd have to plumb through the router.
    let schema = match OnboardingSchema::bundled() {
        Ok(s)  => s,
        Err(e) => {
            warn!("onboarding state: schema failed to load: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "onboarding schema unavailable").into_response();
        }
    };

    let profile = match auth.get_profile(&target) {
        Ok(p)  => p,
        Err(e) => return err_response(e),
    };
    let progress = profile.as_ref()
        .and_then(|p| p.onboarding_progress.as_ref())
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or_else(|| json!({}));

    let completed = string_array(&progress, "completed_groups");
    let skipped   = string_array(&progress, "skipped_keys");

    // If the pointer references a conversation that was deleted, clear it
    // so the UI doesn't bounce the user into a 404. Swallow write errors
    // here — a stale pointer is harmless compared to a 500.
    let mut active = progress.get("active_conversation_id")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    if let Some(id) = active.as_deref() {
        match store.get_conversation(id) {
            Ok(Some(c)) if c.user_id == target && c.mode == "onboarding" => {}
            _ => {
                active = None;
                if let Some(mut p) = progress.as_object().cloned() {
                    p.remove("active_conversation_id");
                    let _ = save_progress(&auth, &target, &Value::Object(p));
                }
            }
        }
    }

    let remaining: Vec<String> = schema.groups.iter()
        .map(|g| g.id.clone())
        .filter(|id| !completed.contains(id))
        .collect();

    let groups = schema.groups.iter().map(|g| GroupSummary {
        id:       g.id.clone(),
        label:    g.label.clone(),
        optional: g.optional,
    }).collect();

    let resp = OnboardingStateResponse {
        user_id:                target,
        onboarded_at:           profile.as_ref().and_then(|p| p.onboarded_at),
        active_conversation_id: active,
        completed_groups:       completed,
        skipped_keys:           skipped,
        remaining_groups:       remaining,
        total_groups:           schema.groups.len(),
        groups,
    };
    axum::Json(resp).into_response()
}

// ── POST /api/onboarding/start ───────────────────────────────────────────────

/// Shared core of `start_onboarding`. Returns `(conversation_id, resumed)` so
/// both the HTTP handler and integration tests can exercise the same logic
/// without going through axum's extractors.
fn start_onboarding_core(
    auth:   &LocalAuthService,
    store:  &HistoryStore,
    target: &str,
) -> Result<(String, bool), MiraError> {
    let mut progress = load_progress(auth, target)?;

    // 1. Cheap path: a stored pointer that still resolves to a live,
    //    correctly-moded conversation owned by this user.
    if let Some(id) = progress.get("active_conversation_id").and_then(|v| v.as_str()) {
        if let Some(c) = store.get_conversation(id)? {
            if c.user_id == target && c.mode == "onboarding" {
                return Ok((id.to_owned(), true));
            }
        }
    }

    // 2. Pointer missing or stale. Look for any other onboarding conversation
    //    the user already owns (e.g. created before this endpoint existed).
    //    Reuse the most-recent one rather than piling up dead threads.
    let existing = store.list_conversations(target, Some("web"), 200, 0)?
        .into_iter()
        .find(|c| c.mode == "onboarding");

    let (conv_id, resumed) = if let Some(c) = existing {
        (c.id, true)
    } else {
        // 3. None exists — create one. Title is deliberately generic; the
        //    auto-titler runs on the chat endpoint, not here.
        let c = store.create_conversation(NewConversation {
            user_id:          target.to_owned(),
            channel:          "web".to_owned(),
            title:            Some("Getting to know you".to_owned()),
            model:            None,
            provider:         None,
            external_user_id: None,
            mode:             Some("onboarding".to_owned()),
        })?;

        // Seed a friendly opener so the user isn't greeted by an empty chat.
        // Best-effort — if this fails, the flow still works; the user just
        // needs to say hi first.
        if let Err(e) = store.add_message(NewMessage {
            conversation_id: c.id.clone(),
            role:            MessageRole::Assistant,
            content:         KICKOFF_OPENER.to_owned(),
            content_type:    "text".to_owned(),
            token_count:     None,
            model:           None,
            tool_calls:      None,
            metadata:        None,
        }) {
            warn!("start_onboarding: failed to seed opener for {}: {}", target, e);
        }

        (c.id, false)
    };

    // 4. Stamp the pointer. Best-effort — a failure here doesn't break the
    //    flow (next call will just re-resolve), but surface it so the user
    //    doesn't silently lose "resume" across logouts.
    if let Some(obj) = progress.as_object_mut() {
        obj.insert("active_conversation_id".to_owned(), Value::String(conv_id.clone()));
        obj.entry("completed_groups".to_owned()).or_insert_with(|| json!([]));
        obj.entry("skipped_keys".to_owned()).or_insert_with(|| json!([]));
    }
    if let Err(e) = save_progress(auth, target, &progress) {
        warn!("start_onboarding: failed to save progress pointer: {}", e);
    }

    Ok((conv_id, resumed))
}

pub async fn start_onboarding(
    AuthUser(caller):  AuthUser,
    Extension(auth):   Extension<Arc<LocalAuthService>>,
    Extension(store):  Extension<Arc<HistoryStore>>,
    Query(q):          Query<TargetUserQuery>,
) -> axum::response::Response {
    let target = match resolve_target(&caller, q.user_id.as_deref()) {
        Ok(id)  => id.to_owned(),
        Err(r)  => return r,
    };

    match start_onboarding_core(&auth, &store, &target) {
        Ok((conversation_id, resumed)) => {
            axum::Json(StartOnboardingResponse { conversation_id, resumed }).into_response()
        }
        Err(e) => err_response(e),
    }
}

// ── POST /api/onboarding/restart-group ───────────────────────────────────────

pub async fn restart_group(
    AuthUser(caller):  AuthUser,
    Extension(auth):   Extension<Arc<LocalAuthService>>,
    Json(req):         Json<RestartGroupRequest>,
) -> axum::response::Response {
    let target = match resolve_target(&caller, req.user_id.as_deref()) {
        Ok(id)  => id.to_owned(),
        Err(r)  => return r,
    };

    let schema = match OnboardingSchema::bundled() {
        Ok(s)  => s,
        Err(e) => {
            warn!("restart_group: schema failed to load: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "onboarding schema unavailable").into_response();
        }
    };

    let group = match schema.group(&req.group_id) {
        Some(g) => g,
        None    => return (StatusCode::BAD_REQUEST, format!("unknown group_id: {}", req.group_id)).into_response(),
    };
    let group_keys: std::collections::HashSet<&str> =
        group.questions.iter().map(|q| q.key.as_str()).collect();

    let mut progress = match load_progress(&auth, &target) {
        Ok(p)  => p,
        Err(e) => return err_response(e),
    };

    // Mutate in place. We keep existing user_profile values untouched —
    // restart is "let me revisit these questions," not "wipe what I said."
    // The LLM will see them in the prompt and offer to confirm or change.
    if let Some(obj) = progress.as_object_mut() {
        if let Some(arr) = obj.get_mut("completed_groups").and_then(|v| v.as_array_mut()) {
            arr.retain(|v| v.as_str() != Some(&req.group_id));
        }
        if let Some(arr) = obj.get_mut("skipped_keys").and_then(|v| v.as_array_mut()) {
            arr.retain(|v| v.as_str().map(|s| !group_keys.contains(s)).unwrap_or(true));
        }
    }

    if let Err(e) = save_progress(&auth, &target, &progress) {
        return err_response(e);
    }

    StatusCode::NO_CONTENT.into_response()
}

// ── POST /api/onboarding/reset ────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct ResetRequest {
    #[serde(default)]
    pub user_id: Option<String>,
}

/// Wipe the fields captured during onboarding so the user can start fresh.
/// Does NOT delete the onboarding conversation itself — the client calls
/// `/api/onboarding/start` afterwards to get a conversation to chat into.
///
/// Seed memories tagged `source="onboarding"` are also cleared so the
/// next run doesn't stack new seeds on top of stale ones.
pub async fn reset_onboarding(
    AuthUser(caller):    AuthUser,
    Extension(auth):     Extension<Arc<LocalAuthService>>,
    Extension(agent):    Extension<Arc<AgentCore>>,
    Extension(data_dir): Extension<DataDir>,
    Json(req):           Json<ResetRequest>,
) -> axum::response::Response {
    let target = match resolve_target(&caller, req.user_id.as_deref()) {
        Ok(id)  => id.to_owned(),
        Err(r)  => return r,
    };

    // 1. Clear DB-held profile columns (including progress + onboarded_at).
    if let Err(e) = auth.reset_onboarding_profile(&target) {
        return err_response(e);
    }

    // 2. Remove the profile.md file, if present. Missing file is fine —
    //    the user may never have reached a section that wrote one. Other
    //    IO errors are logged but don't fail the request; a leftover file
    //    is recoverable, a half-wiped state is harder to explain.
    let md = profile_md_path(data_dir.0.as_path(), &target);
    match std::fs::remove_file(&md) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("reset_onboarding: failed to remove {}: {}", md.display(), e),
    }

    // 3. Drop memory seeds tagged with `source="onboarding"` for this user.
    //    Log-and-continue: a leftover seed is surfaceable in the UI and the
    //    user can delete it manually, so a transient memory error shouldn't
    //    block the rest of the reset.
    match agent.memory.delete_by_source_detail("onboarding", &target).await {
        Ok(n) if n > 0 => tracing::info!("reset_onboarding: removed {} seed memories for {}", n, target),
        Ok(_)          => {}
        Err(e)         => warn!("reset_onboarding: seed memory cleanup failed for {}: {}", target, e),
    }

    StatusCode::NO_CONTENT.into_response()
}

// ── POST /api/onboarding/finalize ────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct FinalizeRequest {
    #[serde(default)]
    pub user_id: Option<String>,
}

#[derive(Serialize)]
pub struct FinalizeFailureResponse {
    /// Ids of required groups that still have no recorded activity or
    /// explicit completion. The UI can map these back to group labels.
    pub untouched_required_groups: Vec<String>,
}

/// User-invoked finalization backstop: the reasoning-distilled local models
/// routinely narrate "all done!" without ever firing `complete_onboarding`.
/// When the UI detects `remaining_groups` is empty but `onboarded_at` is
/// still null, it calls this endpoint to stamp the user as onboarded
/// server-side.
///
/// Returns:
/// - `204 No Content` on success (including idempotent no-op when already onboarded)
/// - `400 Bad Request` with `{ untouched_required_groups: [...] }` when the
///   silent-skip guard refuses
/// - `500 Internal Server Error` on storage failure
pub async fn finalize(
    AuthUser(caller): AuthUser,
    Extension(auth):  Extension<Arc<LocalAuthService>>,
    Json(req):        Json<FinalizeRequest>,
) -> axum::response::Response {
    let target = match resolve_target(&caller, req.user_id.as_deref()) {
        Ok(id)  => id.to_owned(),
        Err(r)  => return r,
    };

    let schema = match OnboardingSchema::bundled() {
        Ok(s)  => s,
        Err(e) => {
            warn!("finalize: schema failed to load: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "onboarding schema unavailable").into_response();
        }
    };

    match finalize_onboarding(&auth, &schema, &target, None) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(FinalizeError::UntouchedRequiredGroups(groups)) => {
            (StatusCode::BAD_REQUEST, axum::Json(FinalizeFailureResponse {
                untouched_required_groups: groups,
            })).into_response()
        }
        Err(FinalizeError::Storage(e)) => err_response(e),
    }
}

// ── POST /api/onboarding/post-complete-chat ──────────────────────────────────

#[derive(Serialize)]
pub struct PostCompleteChatResponse {
    pub conversation_id: String,
}

/// Pick the friendliest name we have, in priority order. Falls back to "there"
/// so the greeting still parses as a sentence ("Hi there!") when onboarding
/// finished with no recorded name.
fn friendly_name(p: &Option<crate::auth::UserProfile>, fallback: &str) -> String {
    p.as_ref()
        .and_then(|pp| pp.preferred_name.as_deref()
            .or(pp.nickname.as_deref())
            .or(pp.full_name.as_deref()))
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|| fallback.to_owned())
}

/// Build a warm opener that references details captured during onboarding.
/// Deterministic — no LLM round-trip — so the post-onboarding transition
/// stays fast and predictable.
fn personalized_opener(
    profile: &Option<crate::auth::UserProfile>,
    summary: Option<&str>,
) -> String {
    let name = friendly_name(profile, "there");

    let mut lines = vec![format!(
        "Hi {}, thanks for walking me through that — I've got a better sense of \
         you now and I've saved what you shared so I can use it in future chats.",
        name
    )];

    if let Some(p) = profile {
        let tz = p.timezone.as_deref().unwrap_or("").trim();
        if !tz.is_empty() {
            lines.push(format!("I've got you down in {} — I'll factor that into timing.", tz));
        }
    }
    if let Some(s) = summary.map(str::trim).filter(|s| !s.is_empty()) {
        lines.push(format!("And for context, I've noted: {}", s));
    }

    lines.push(String::new());
    lines.push(
        "So — what's on your mind today? Happy to dig into anything, or we can \
         just chat."
        .to_owned()
    );
    lines.join("\n")
}

/// Create a fresh (non-onboarding) chat conversation, seed it with a warm
/// personalized opener derived from the just-captured profile, and return
/// the new conversation id so the UI can navigate into it.
///
/// Refuses to run unless the user is actually onboarded — prevents a UI
/// bug or hostile client from short-circuiting the flow.
pub async fn post_complete_chat(
    AuthUser(caller):  AuthUser,
    Extension(auth):   Extension<Arc<LocalAuthService>>,
    Extension(store):  Extension<Arc<HistoryStore>>,
    Query(q):          Query<TargetUserQuery>,
) -> axum::response::Response {
    let target = match resolve_target(&caller, q.user_id.as_deref()) {
        Ok(id)  => id.to_owned(),
        Err(r)  => return r,
    };

    let profile = match auth.get_profile(&target) {
        Ok(p)  => p,
        Err(e) => return err_response(e),
    };

    // Guard: don't hand out a personalized opener if onboarding isn't done.
    if profile.as_ref().and_then(|p| p.onboarded_at).is_none() {
        return (StatusCode::BAD_REQUEST,
                "post-complete-chat called before onboarding finished").into_response();
    }

    let summary = profile.as_ref()
        .and_then(|p| p.onboarding_progress.as_deref())
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v.get("summary").and_then(|x| x.as_str().map(str::to_owned)));

    let opener = personalized_opener(&profile, summary.as_deref());

    let conv = match store.create_conversation(NewConversation {
        user_id:          target.clone(),
        channel:          "web".to_owned(),
        title:            Some("Fresh start".to_owned()),
        model:            None,
        provider:         None,
        external_user_id: None,
        mode:             Some("chat".to_owned()),
    }) {
        Ok(c)  => c,
        Err(e) => return err_response(e),
    };

    if let Err(e) = store.add_message(NewMessage {
        conversation_id: conv.id.clone(),
        role:            MessageRole::Assistant,
        content:         opener,
        content_type:    "text".to_owned(),
        token_count:     None,
        model:           None,
        tool_calls:      None,
        metadata:        None,
    }) {
        warn!("post_complete_chat: seed greeting failed for {}: {}", target, e);
    }

    axum::Json(PostCompleteChatResponse { conversation_id: conv.id }).into_response()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::User;
    use tempfile::TempDir;

    fn test_user(id: &str, role: Role) -> User {
        User {
            id:                id.to_owned(),
            username:          id.to_owned(),
            display_name:      None,
            email:             None,
            role,
            is_active:         true,
            created_at:        0,
            updated_at:        0,
            last_login:        None,
            phone:             None,
            preferred_contact: None,
            avatar:            None,
            voice_prefs:       None,
        }
    }

    fn setup() -> (TempDir, Arc<LocalAuthService>, Arc<HistoryStore>, String) {
        let dir  = TempDir::new().unwrap();
        let auth = Arc::new(
            LocalAuthService::new(
                &dir.path().join("auth.db"),
                "test-secret".to_owned(),
                7,
            ).unwrap()
        );
        let hist = Arc::new(HistoryStore::open(&dir.path().join("history.db")).unwrap());
        let user = auth.create_user(crate::auth::NewUser {
            username:      "u1".to_owned(),
            password:      "password1".to_owned(),
            display_name:  None,
            email:         None,
            role:          Role::User,
        }).unwrap();
        (dir, auth, hist, user.id)
    }

    #[test]
    fn resolve_target_forbids_cross_user_for_non_admin() {
        let caller = test_user("u1", Role::User);
        let err = resolve_target(&caller, Some("u2")).unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn resolve_target_allows_admin_cross_user() {
        let caller = test_user("admin", Role::Admin);
        let t = resolve_target(&caller, Some("u1")).unwrap();
        assert_eq!(t, "u1");
    }

    #[test]
    fn resolve_target_self_always_allowed() {
        let caller = test_user("u1", Role::User);
        let t = resolve_target(&caller, Some("u1")).unwrap();
        assert_eq!(t, "u1");
        let t = resolve_target(&caller, None).unwrap();
        assert_eq!(t, "u1");
    }

    #[test]
    fn load_progress_missing_returns_empty_object() {
        let (_d, auth, _h, user_id) = setup();
        let p = load_progress(&auth, &user_id).unwrap();
        assert!(p.is_object());
        assert_eq!(p.as_object().unwrap().len(), 0);
    }

    #[test]
    fn save_progress_round_trips_object() {
        let (_d, auth, _h, user_id) = setup();
        let p = json!({ "completed_groups": ["name"], "skipped_keys": [] });
        save_progress(&auth, &user_id, &p).unwrap();
        let loaded = load_progress(&auth, &user_id).unwrap();
        assert_eq!(loaded, p);
    }

    #[test]
    fn start_onboarding_is_idempotent_and_stamps_pointer() {
        let (_d, auth, hist, user_id) = setup();

        let (id1, resumed1) = start_onboarding_core(&auth, &hist, &user_id).unwrap();
        assert!(!resumed1, "first call should create a new conversation");

        let (id2, resumed2) = start_onboarding_core(&auth, &hist, &user_id).unwrap();
        assert_eq!(id1, id2, "second call must return the same conversation id");
        assert!(resumed2, "second call should flag resumed=true");

        // Pointer is stamped in the progress blob.
        let progress = load_progress(&auth, &user_id).unwrap();
        assert_eq!(
            progress.get("active_conversation_id").and_then(|v| v.as_str()),
            Some(id1.as_str())
        );

        // Conversation has mode=onboarding and belongs to the user.
        let c = hist.get_conversation(&id1).unwrap().unwrap();
        assert_eq!(c.mode, "onboarding");
        assert_eq!(c.user_id, user_id);
    }

    #[test]
    fn start_onboarding_seeds_opener_on_fresh_conversation() {
        let (_d, auth, hist, user_id) = setup();
        let (id, resumed) = start_onboarding_core(&auth, &hist, &user_id).unwrap();
        assert!(!resumed);

        let msgs = hist.get_messages(&id, 10, None).unwrap();
        assert_eq!(msgs.len(), 1, "fresh conversation must be seeded with one opener");
        assert_eq!(msgs[0].role, MessageRole::Assistant);
        assert!(msgs[0].content.contains("I'd love to learn"), "unexpected opener: {}", msgs[0].content);

        // Calling again should resume, not add a second opener.
        let (id2, resumed2) = start_onboarding_core(&auth, &hist, &user_id).unwrap();
        assert_eq!(id2, id);
        assert!(resumed2);
        let msgs_after = hist.get_messages(&id, 10, None).unwrap();
        assert_eq!(msgs_after.len(), 1, "resuming must not append another opener");
    }

    #[test]
    fn start_onboarding_creates_fresh_conv_when_pointer_is_stale() {
        let (_d, auth, hist, user_id) = setup();

        // Seed a bogus pointer that doesn't resolve to any conversation.
        save_progress(
            &auth,
            &user_id,
            &json!({
                "active_conversation_id": "does-not-exist",
                "completed_groups": [],
                "skipped_keys": [],
            }),
        ).unwrap();

        let (id, resumed) = start_onboarding_core(&auth, &hist, &user_id).unwrap();
        assert_ne!(id, "does-not-exist");
        assert!(!resumed, "stale pointer should not count as a resume");
    }

    #[test]
    fn personalized_opener_greets_by_name_and_timezone() {
        use crate::auth::UserProfile;
        let p = UserProfile {
            user_id:        "u".into(),
            preferred_name: Some("Alex".into()),
            timezone:       Some("Australia/Sydney".into()),
            ..Default::default()
        };
        let opener = personalized_opener(&Some(p), Some("staff eng, ships onboarding"));
        assert!(opener.contains("Alex"), "missing name: {}", opener);
        assert!(opener.contains("Australia/Sydney"), "missing timezone: {}", opener);
        assert!(opener.contains("staff eng"), "missing summary: {}", opener);
    }

    #[test]
    fn personalized_opener_falls_back_when_profile_empty() {
        let opener = personalized_opener(&None, None);
        assert!(opener.contains("there"), "expected fallback greeting, got: {}", opener);
    }

    #[test]
    fn restart_group_logic_clears_completed_and_skipped_for_group() {
        // Simulate the progress-mutation body of restart_group directly so
        // we don't need a live axum runtime to exercise the interesting logic.
        let schema = OnboardingSchema::bundled().unwrap();
        let group = schema.group("name").expect("'name' group must exist");
        let group_keys: std::collections::HashSet<&str> =
            group.questions.iter().map(|q| q.key.as_str()).collect();

        // A key belonging to the group, and one outside it.
        let inside  = *group_keys.iter().next().unwrap();
        let outside = "timezone";
        assert!(!group_keys.contains(outside));

        let mut progress = json!({
            "completed_groups": ["name", "location_time"],
            "skipped_keys":     [inside, outside],
        });
        if let Some(obj) = progress.as_object_mut() {
            if let Some(arr) = obj.get_mut("completed_groups").and_then(|v| v.as_array_mut()) {
                arr.retain(|v| v.as_str() != Some("name"));
            }
            if let Some(arr) = obj.get_mut("skipped_keys").and_then(|v| v.as_array_mut()) {
                arr.retain(|v| v.as_str().map(|s| !group_keys.contains(s)).unwrap_or(true));
            }
        }

        let completed = string_array(&progress, "completed_groups");
        let skipped   = string_array(&progress, "skipped_keys");
        assert_eq!(completed, vec!["location_time"]);
        assert_eq!(skipped,   vec![outside]);
    }
}
