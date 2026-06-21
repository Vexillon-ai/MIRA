// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/companion.rs
//! HTTP API for companion-mode configuration.
//!
//! Two surface layers:
//!
//! - **Admin (`/api/admin/companion/groups/*`)** — companion-enable a
//! group, set its policy, manage per-member flags on behalf of any
//! user. Authorisation via the `AdminUser` extractor.
//! - **User (`/api/me/companion/group-membership/*`)** — flip your
//! OWN opt-in / channel preference / mute hours / daily-cap within
//! a group you're a member of. Authorisation via `AuthUser` —
//! never lets one user change another's flags.

use std::sync::Arc;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::agent::AgentCore;
use crate::auth::{AdminUser, AuthUser};
use crate::companion::groups::{
    CompanionGroupStore, GroupCompanionMember, GroupCompanionPolicy, SignalKind,
};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}

// Resolve the companion group store via the AgentCore handle. 503
// when companion isn't installed (channel-only / minimal builds).
fn group_store(
    agent: &AgentCore,
) -> Result<Arc<CompanionGroupStore>, (StatusCode, Json<serde_json::Value>)> {
    let sys = agent.companion().ok_or_else(|| err(
        StatusCode::SERVICE_UNAVAILABLE,
        "companion feature not enabled on this server",
    ))?;
    Ok(sys.groups_arc())
}

fn parse_signals(strs: &[String]) -> Vec<SignalKind> {
    strs.iter().filter_map(|s| SignalKind::parse(s)).collect()
}

// Resolve the companion dispatcher (fires check-ins/briefings) via the
// AgentCore handle. 503 when companion isn't wired.
fn dispatcher(
    agent: &AgentCore,
) -> Result<Arc<crate::companion::dispatcher::CompanionDispatcher>, (StatusCode, Json<serde_json::Value>)> {
    agent.companion_dispatcher().cloned().ok_or_else(|| err(
        StatusCode::SERVICE_UNAVAILABLE,
        "companion dispatcher not available on this server",
    ))
}

// Render a `DispatchOutcome` into a JSON body the test buttons can show.
fn outcome_json(o: &crate::companion::dispatcher::DispatchOutcome) -> serde_json::Value {
    use crate::companion::dispatcher::DispatchOutcome::*;
    match o {
        Sent { conversation_id, channel, chars } => serde_json::json!({
            "ok": true, "status": "sent",
            "channel": channel, "chars": chars, "conversation_id": conversation_id,
        }),
        SkippedNoChannel => serde_json::json!({
            "ok": false, "status": "skipped",
            "detail": "no deliverable channel resolved for your account",
        }),
        Failed(reason) => serde_json::json!({
            "ok": false, "status": "failed", "detail": reason,
        }),
    }
}

// POST /api/companion/checkin/test — fire a check-in to ME right now,
// bypassing the scheduler's policy gates, and report exactly what
// happened (delivered on which channel, skipped, or failed + reason).
// The whole point: make proactive delivery testable on demand instead of
// waiting for a scheduler window.
pub async fn test_checkin(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let d = dispatcher(&agent)?;
    let outcome = d.send_checkin(&me.id).await.map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("check-in dispatch: {e}"),
    ))?;
    Ok(Json(outcome_json(&outcome)))
}


// ── Response shapes ──────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct PolicyView {
    pub group_id: String,
    pub allowed_signals: Vec<String>,
    pub privacy_topics: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<GroupCompanionPolicy> for PolicyView {
    fn from(p: GroupCompanionPolicy) -> Self {
        Self {
            group_id: p.group_id,
            allowed_signals: p.allowed_signals.iter().map(|s| s.as_str().to_string()).collect(),
            privacy_topics: p.privacy_topics,
            created_at: p.created_at.timestamp_millis(),
            updated_at: p.updated_at.timestamp_millis(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct MemberView {
    pub group_id: String,
    pub user_id: String,
    pub contactable_for: Vec<String>,
    pub channel_preference: Vec<String>,
    pub mute_hours: Vec<(String, String)>,
    pub daily_message_cap: u32,
    pub opt_in: bool,
    pub joined_at: i64,
    pub updated_at: i64,
}

impl From<GroupCompanionMember> for MemberView {
    fn from(m: GroupCompanionMember) -> Self {
        Self {
            group_id: m.group_id,
            user_id: m.user_id,
            contactable_for: m.contactable_for.iter().map(|s| s.as_str().to_string()).collect(),
            channel_preference: m.channel_preference,
            mute_hours: m.mute_hours,
            daily_message_cap: m.daily_message_cap,
            opt_in: m.opt_in,
            joined_at: m.joined_at.timestamp_millis(),
            updated_at: m.updated_at.timestamp_millis(),
        }
    }
}

// ── Request shapes ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PutPolicyRequest {
    pub allowed_signals: Vec<String>,
    pub privacy_topics: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct PutMemberRequest {
    // Snake-case signal kinds the member wants to receive.
    pub contactable_for: Vec<String>,
    pub channel_preference: Vec<String>,
    pub mute_hours: Vec<(String, String)>,
    pub daily_message_cap: u32,
    pub opt_in: bool,
}

#[derive(Debug, Deserialize)]
pub struct UpdateMyMembershipRequest {
    pub contactable_for: Option<Vec<String>>,
    pub channel_preference: Option<Vec<String>>,
    pub mute_hours: Option<Vec<(String, String)>>,
    pub daily_message_cap: Option<u32>,
    pub opt_in: Option<bool>,
}

// ── Admin endpoints ──────────────────────────────────────────────────────────

// GET /api/admin/companion/groups — list every companion-enabled group.
pub async fn admin_list_groups(
    AdminUser(_me): AdminUser,
    Extension(agent): Extension<Arc<AgentCore>>,
) -> Result<Json<Vec<PolicyView>>, (StatusCode, Json<serde_json::Value>)> {
    let store = group_store(&agent)?;
    let pols = store.list_policies().map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("list_policies: {e}"),
    ))?;
    Ok(Json(pols.into_iter().map(PolicyView::from).collect()))
}

// GET /api/admin/companion/groups/{group_id} — policy + members.
#[derive(Debug, Serialize)]
pub struct GroupDetailView {
    pub policy: PolicyView,
    pub members: Vec<MemberView>,
}

pub async fn admin_get_group(
    AdminUser(_me): AdminUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Path(group_id): Path<String>,
) -> Result<Json<GroupDetailView>, (StatusCode, Json<serde_json::Value>)> {
    let store = group_store(&agent)?;
    let policy = store.get_policy(&group_id).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("get_policy: {e}"),
    ))?.ok_or_else(|| err(StatusCode::NOT_FOUND, format!("group '{group_id}' not companion-enabled")))?;
    let members = store.list_members(&group_id).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("list_members: {e}"),
    ))?;
    Ok(Json(GroupDetailView {
        policy: PolicyView::from(policy),
        members: members.into_iter().map(MemberView::from).collect(),
    }))
}

// PUT /api/admin/companion/groups/{group_id}/policy — create or
// update a group's companion policy. Creating a row at all is the
// "companion-enable this group" gesture.
pub async fn admin_put_policy(
    AdminUser(_me): AdminUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Path(group_id): Path<String>,
    Json(body): Json<PutPolicyRequest>,
) -> Result<Json<PolicyView>, (StatusCode, Json<serde_json::Value>)> {
    let store = group_store(&agent)?;
    let now = Utc::now();
    let prior = store.get_policy(&group_id).ok().flatten();
    let p = GroupCompanionPolicy {
        group_id: group_id.clone(),
        allowed_signals: parse_signals(&body.allowed_signals),
        privacy_topics: body.privacy_topics,
        created_at: prior.as_ref().map(|p| p.created_at).unwrap_or(now),
        updated_at: now,
    };
    store.upsert_policy(&p).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("upsert_policy: {e}"),
    ))?;
    Ok(Json(PolicyView::from(p)))
}

// DELETE /api/admin/companion/groups/{group_id} — remove the
// policy + every member row. The underlying auth-db `groups` row
// is untouched.
pub async fn admin_delete_group(
    AdminUser(_me): AdminUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Path(group_id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let store = group_store(&agent)?;
    store.delete_group(&group_id).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("delete_group: {e}"),
    ))?;
    Ok(StatusCode::NO_CONTENT)
}

// PUT /api/admin/companion/groups/{group_id}/members/{user_id} —
// upsert per-member flags. Admins can pre-opt-in a user (e.g. when
// a son adds himself to his father's family group).
pub async fn admin_put_member(
    AdminUser(_me): AdminUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Path((group_id, user_id)): Path<(String, String)>,
    Json(body): Json<PutMemberRequest>,
) -> Result<Json<MemberView>, (StatusCode, Json<serde_json::Value>)> {
    let store = group_store(&agent)?;
    // Require the group to be companion-enabled first — refusing to
    // upsert a member into a non-enabled group prevents orphan rows.
    if store.get_policy(&group_id).ok().flatten().is_none() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("group '{group_id}' is not companion-enabled — \
                     PUT its policy first"),
        ));
    }
    let now = Utc::now();
    let prior = store.get_member(&group_id, &user_id).ok().flatten();
    let m = GroupCompanionMember {
        group_id: group_id.clone(),
        user_id: user_id.clone(),
        contactable_for: parse_signals(&body.contactable_for),
        channel_preference: body.channel_preference,
        mute_hours: body.mute_hours,
        daily_message_cap: body.daily_message_cap,
        opt_in: body.opt_in,
        joined_at: prior.as_ref().map(|p| p.joined_at).unwrap_or(now),
        updated_at: now,
    };
    store.upsert_member(&m).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("upsert_member: {e}"),
    ))?;
    Ok(Json(MemberView::from(m)))
}

// DELETE /api/admin/companion/groups/{group_id}/members/{user_id}.
pub async fn admin_delete_member(
    AdminUser(_me): AdminUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Path((group_id, user_id)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let store = group_store(&agent)?;
    store.delete_member(&group_id, &user_id).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("delete_member: {e}"),
    ))?;
    Ok(StatusCode::NO_CONTENT)
}

// ── User endpoints ───────────────────────────────────────────────────────────

// GET /api/me/companion/groups — every companion-enabled group I'm
// a member of, with my flags.
pub async fn list_my_memberships(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
) -> Result<Json<Vec<MemberView>>, (StatusCode, Json<serde_json::Value>)> {
    let store = group_store(&agent)?;
    let group_ids = store.list_groups_for_user(&me.id).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("list_groups_for_user: {e}"),
    ))?;
    let mut out = Vec::with_capacity(group_ids.len());
    for gid in group_ids {
        if let Ok(Some(m)) = store.get_member(&gid, &me.id) {
            out.push(MemberView::from(m));
        }
    }
    Ok(Json(out))
}

// PATCH /api/me/companion/groups/{group_id} — update MY OWN flags
// within a group I'm in. Cannot touch other users' rows. Each
// `Some` field is applied; `None` is left untouched.
pub async fn update_my_membership(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Path(group_id): Path<String>,
    Json(body): Json<UpdateMyMembershipRequest>,
) -> Result<Json<MemberView>, (StatusCode, Json<serde_json::Value>)> {
    let store = group_store(&agent)?;
    let mut m = store.get_member(&group_id, &me.id).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("get_member: {e}"),
    ))?.ok_or_else(|| err(
        StatusCode::NOT_FOUND,
        format!("you are not a member of '{group_id}' or it's not companion-enabled"),
    ))?;
    if let Some(c) = body.contactable_for { m.contactable_for = parse_signals(&c); }
    if let Some(c) = body.channel_preference { m.channel_preference = c; }
    if let Some(mh) = body.mute_hours { m.mute_hours = mh; }
    if let Some(d) = body.daily_message_cap { m.daily_message_cap = d; }
    if let Some(o) = body.opt_in { m.opt_in = o; }
    m.updated_at = Utc::now();
    store.upsert_member(&m).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("upsert_member: {e}"),
    ))?;
    Ok(Json(MemberView::from(m)))
}

// ── Self-serve enable (setup wizard) ─────────────────────────────────────────
//
// POST /api/me/companion/enable — turn companion check-ins on for the CALLER
// with an optional safety contact, a per-user cadence cap, and a daily-briefing
// schedule, in one request. This is the HTTP equivalent of the chat-driven
// `companion_enable` (+ `companion_briefing_set`) flow, added so the web setup
// wizard can enable check-ins without sending the user through chat. It mirrors
// the tool's safety rule: a non-admin must name a safety contact; an admin may
// enable without one (the safety floor still audit-logs, just doesn't notify).

#[derive(Debug, Deserialize)]
pub struct EnableCompanionRequest {
    /// Another MIRA user to notify if the safety floor triggers. Optional for
    /// admins; required for non-admins. Empty string is treated as omitted.
    #[serde(default)]
    pub safety_contact_user_id: Option<String>,
    /// Per-user cap on proactive check-ins per local day (overrides the
    /// instance default). Omit to inherit the default.
    #[serde(default)]
    pub max_per_day: Option<u32>,
    /// Turn the daily briefing on as part of enabling.
    #[serde(default)]
    pub briefing_enabled: Option<bool>,
    /// Local hour (0..=23) the briefing fires at. Defaults to 7 on first enable.
    #[serde(default)]
    pub briefing_hour: Option<u8>,
}

pub async fn enable_companion(
    AuthUser(me):     AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Json(body):       Json<EnableCompanionRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let sys = agent.companion().ok_or_else(|| err(
        StatusCode::SERVICE_UNAVAILABLE,
        "companion feature not enabled on this server",
    ))?;

    // Normalise the safety contact: blank → None.
    let safety: Option<String> = body.safety_contact_user_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);

    // Mirror `companion_enable`: a non-admin must name a safety contact.
    if safety.is_none() && me.role != crate::auth::Role::Admin {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "a safety contact is required to enable check-ins",
        ));
    }
    if let Some(h) = body.briefing_hour {
        if h > 23 {
            return Err(err(StatusCode::BAD_REQUEST, "briefing_hour must be 0..=23"));
        }
    }

    // Enable: validates the contact, stamps setup, seeds the persona wiki.
    let mut s = sys.enable(&me.id, safety.as_deref()).map_err(|e| {
        use crate::companion::CompanionError::*;
        match e {
            SelfSafetyContact => err(StatusCode::BAD_REQUEST,
                "you can't be your own safety contact — pick someone else"),
            UnknownSafetyContact(u) => err(StatusCode::BAD_REQUEST,
                format!("'{u}' is not a known MIRA user")),
            Invalid(m) => err(StatusCode::BAD_REQUEST, m),
            other => err(StatusCode::INTERNAL_SERVER_ERROR, format!("enable: {other}")),
        }
    })?;

    // Apply the wizard's cadence + briefing choices in a single follow-up write.
    if let Some(mpd) = body.max_per_day      { s.cadence.max_per_day = Some(mpd); }
    if let Some(en)  = body.briefing_enabled { s.daily_briefing_enabled = en; }
    if let Some(h)   = body.briefing_hour    { s.daily_briefing_hour = h; }
    s.updated_at = Utc::now();
    sys.store().upsert(&s).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("save: {e}")))?;

    tracing::info!(
        user = %me.username,
        "companion enabled via setup wizard (briefing={}, hour={}, contact={:?})",
        s.daily_briefing_enabled, s.daily_briefing_hour, s.safety_contact_user_id,
    );

    Ok(Json(serde_json::json!({
        "companion_active":       s.is_active(Utc::now()),
        "enabled":                s.daily_briefing_enabled,
        "hour":                   s.daily_briefing_hour,
        "safety_contact_user_id": s.safety_contact_user_id,
        "max_per_day":            s.cadence.max_per_day,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::groups::CompanionGroupStore;
    use tempfile::tempdir;

    fn fresh_store() -> (tempfile::TempDir, Arc<CompanionGroupStore>) {
        let dir = tempdir().unwrap();
        let store = CompanionGroupStore::open(&dir.path().join("companion.db")).unwrap();
        (dir, Arc::new(store))
    }

    // The handlers themselves are integration-tested via curl;
    // unit tests here cover the View-shaping helpers + parse_signals
    // since they're the only non-trivial pure logic in this file.

    #[test]
    fn parse_signals_drops_garbage() {
        let out = parse_signals(&[
            "distress".into(),
            "garbage".into(),
            "missed_checkin".into(),
        ]);
        assert_eq!(out, vec![SignalKind::Distress, SignalKind::MissedCheckin]);
    }

    #[test]
    fn policy_view_round_trips() {
        let now = Utc::now();
        let p = GroupCompanionPolicy {
            group_id: "family".into(),
            allowed_signals: vec![SignalKind::Distress],
            privacy_topics: vec!["health".into()],
            created_at: now,
            updated_at: now,
        };
        let v = PolicyView::from(p.clone());
        assert_eq!(v.group_id, "family");
        assert_eq!(v.allowed_signals, vec!["distress"]);
        assert_eq!(v.privacy_topics, vec!["health"]);
    }

    #[test]
    fn member_view_carries_opt_in_and_caps() {
        let now = Utc::now();
        let m = GroupCompanionMember {
            group_id: "family".into(),
            user_id: "david".into(),
            contactable_for: vec![SignalKind::Distress, SignalKind::MissedCheckin],
            channel_preference: vec!["signal".into()],
            mute_hours: vec![("22:00".into(), "07:00".into())],
            daily_message_cap: 2,
            opt_in: true,
            joined_at: now,
            updated_at: now,
        };
        let v = MemberView::from(m);
        assert!(v.opt_in);
        assert_eq!(v.daily_message_cap, 2);
        assert_eq!(v.contactable_for, vec!["distress", "missed_checkin"]);
    }

    #[tokio::test]
    async fn admin_put_member_refuses_when_group_not_companion_enabled() {
        // Just a smoke check of the underlying store logic the
        // handler relies on — admin_put_member returns BAD_REQUEST
        // when get_policy is None.
        let (_dir, store) = fresh_store();
        assert!(store.get_policy("family").unwrap().is_none());
    }
}
