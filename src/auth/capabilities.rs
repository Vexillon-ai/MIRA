// SPDX-License-Identifier: AGPL-3.0-or-later

// src/auth/capabilities.rs
//! Capability RBAC — per-user / per-group governance of what a user is
//! *allowed to do*, distinct from the existing per-user *isolation* (whose
//! data a user can see).
//!
//! A [`CapabilityProfile`] restricts, on four axes, which **providers**,
//! **models**, **tools**, and **channels** a user may use, plus optional
//! **budget caps**. Profiles attach to groups (via `groups.capabilities_json`)
//! and optionally to a single user (the `user_capabilities` table). The
//! effective profile for a user is the **merge** of their direct profile and
//! every group they belong to.
//!
//! ## Semantics (deliberately simple + backward-compatible)
//!
//! Each axis is `Option<Vec<String>>`:
//! - `None`  — this profile does not govern the axis (contributes nothing).
//! - `Some(set)` — this profile restricts the axis to `set`.
//!
//! **Grants are additive.** The effective allow-set for an axis is the
//! *union* of every `Some(set)` across the governing profiles; if no profile
//! sets the axis, it is **unrestricted** (default-allow). This preserves the
//! behaviour of existing installs (no profiles → nothing is restricted) and
//! gives the intuitive "add a user to a group to grant them a model/tool" UX.
//! The flip side: to *restrict* a user, make sure they are only in groups that
//! grant the intended subset (a "kid-safe" user belongs to the kid group
//! alone). Budget caps are the exception — they are **restrictions**, so the
//! **tightest (minimum) wins**.
//!
//! **Admins bypass everything** — [`AuthDb::effective_capabilities`] returns an
//! unrestricted profile for `Role::Admin`.
//!
//! Channels are carried as a data field now; enforcement of the channel axis
//! lands in a later slice (tools / models / budget are enforced first).

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::auth::models::{AuthDb, Role};
use crate::MiraError;

// ── Type ───────────────────────────────────────────────────────────────────────

/// One capability profile. Stored as JSON on a group or a user; also the shape
/// returned by the merge as a user's *effective* capabilities.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CapabilityProfile {
    /// Allowed provider ids (e.g. `openai`, `anthropic`, `lmstudio`).
    /// `None` = unrestricted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<Vec<String>>,
    /// Allowed model ids / aliases. `None` = unrestricted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    /// Allowed tool names. `None` = unrestricted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// Allowed channel ids. `None` = unrestricted. (Data only for now —
    /// enforcement is a later slice.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<String>>,
    /// Per-task budget cap (USD). `None` = no cap from this profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_task_budget_usd: Option<f64>,
    /// Per-session budget cap (USD). `None` = no cap from this profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_budget_usd: Option<f64>,
}

impl CapabilityProfile {
    /// A profile that restricts nothing — what admins and unrestricted users
    /// resolve to.
    pub fn unrestricted() -> Self {
        Self::default()
    }

    /// True if every axis is unrestricted and no budget cap is set.
    pub fn is_unrestricted(&self) -> bool {
        self.providers.is_none()
            && self.models.is_none()
            && self.tools.is_none()
            && self.channels.is_none()
            && self.max_task_budget_usd.is_none()
            && self.session_budget_usd.is_none()
    }

    fn axis_allows(axis: &Option<Vec<String>>, value: &str) -> bool {
        match axis {
            None => true,
            Some(allowed) => allowed.iter().any(|a| a == value),
        }
    }

    /// May this user use the given provider id?
    pub fn allows_provider(&self, provider: &str) -> bool {
        Self::axis_allows(&self.providers, provider)
    }

    /// May this user use the given model id / alias?
    pub fn allows_model(&self, model: &str) -> bool {
        Self::axis_allows(&self.models, model)
    }

    /// May this user use the given tool?
    pub fn allows_tool(&self, tool: &str) -> bool {
        Self::axis_allows(&self.tools, tool)
    }

    /// May this user use the given channel id?
    pub fn allows_channel(&self, channel: &str) -> bool {
        Self::axis_allows(&self.channels, channel)
    }

    /// Restrict a candidate tool list to what this profile allows. Returns
    /// `None` when the tool axis is unrestricted (so callers can keep the
    /// existing `Option<Vec<String>>` "None = no restriction" contract and
    /// only narrow when a restriction actually exists).
    pub fn filter_tools(&self, candidates: &[String]) -> Option<Vec<String>> {
        let allowed = self.tools.as_ref()?;
        Some(
            candidates
                .iter()
                .filter(|t| allowed.iter().any(|a| a == *t))
                .cloned()
                .collect(),
        )
    }

    /// Clamp a requested task budget to this profile's cap (no-op when unset).
    pub fn cap_task_budget(&self, requested: f64) -> f64 {
        match self.max_task_budget_usd {
            Some(cap) => requested.min(cap),
            None => requested,
        }
    }

    /// Merge a set of profiles into one effective profile. Allow-lists union
    /// (grants are additive); budget caps take the tightest (minimum). An axis
    /// that no input governs stays `None` (unrestricted).
    pub fn merge(profiles: &[CapabilityProfile]) -> CapabilityProfile {
        fn union_axis<'a>(
            profiles: &'a [CapabilityProfile],
            pick: impl Fn(&'a CapabilityProfile) -> &'a Option<Vec<String>>,
        ) -> Option<Vec<String>> {
            let mut governed = false;
            let mut acc: Vec<String> = Vec::new();
            for p in profiles {
                if let Some(set) = pick(p) {
                    governed = true;
                    for v in set {
                        if !acc.contains(v) {
                            acc.push(v.clone());
                        }
                    }
                }
            }
            governed.then_some(acc)
        }

        fn min_budget(
            profiles: &[CapabilityProfile],
            pick: impl Fn(&CapabilityProfile) -> Option<f64>,
        ) -> Option<f64> {
            profiles
                .iter()
                .filter_map(pick)
                .fold(None, |acc, v| Some(acc.map_or(v, |a: f64| a.min(v))))
        }

        CapabilityProfile {
            providers: union_axis(profiles, |p| &p.providers),
            models: union_axis(profiles, |p| &p.models),
            tools: union_axis(profiles, |p| &p.tools),
            channels: union_axis(profiles, |p| &p.channels),
            max_task_budget_usd: min_budget(profiles, |p| p.max_task_budget_usd),
            session_budget_usd: min_budget(profiles, |p| p.session_budget_usd),
        }
    }
}

// ── Storage (impl AuthDb extension) ─────────────────────────────────────────────

impl AuthDb {
    /// Read a group's capability profile (`None` when the group sets none).
    pub fn get_group_capabilities(
        &self,
        group_id: &str,
    ) -> Result<Option<CapabilityProfile>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let json: Option<String> = match conn.query_row(
            "SELECT capabilities_json FROM groups WHERE id = ?1",
            params![group_id],
            |r| r.get(0),
        ) {
            Ok(j) => j,
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                return Err(MiraError::NotFound(format!("Group not found: {group_id}")))
            }
            Err(e) => return Err(MiraError::DatabaseError(e.to_string())),
        };
        parse_profile(json)
    }

    /// Set (or clear, with `None`) a group's capability profile.
    pub fn set_group_capabilities(
        &self,
        group_id: &str,
        profile: Option<&CapabilityProfile>,
    ) -> Result<(), MiraError> {
        let json = serialize_profile(profile)?;
        let now = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        let rows = conn
            .execute(
                "UPDATE groups SET capabilities_json = ?1, updated_at = ?2 WHERE id = ?3",
                params![json, now, group_id],
            )
            .map_err(|e| MiraError::DatabaseError(format!("set_group_capabilities: {e}")))?;
        if rows == 0 {
            return Err(MiraError::NotFound(format!("Group not found: {group_id}")));
        }
        Ok(())
    }

    /// Read a user's direct capability profile (`None` when unset).
    pub fn get_user_capabilities(
        &self,
        user_id: &str,
    ) -> Result<Option<CapabilityProfile>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let json: Option<String> = match conn.query_row(
            "SELECT capabilities_json FROM user_capabilities WHERE user_id = ?1",
            params![user_id],
            |r| r.get(0),
        ) {
            Ok(j) => Some(j),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(MiraError::DatabaseError(e.to_string())),
        };
        parse_profile(json)
    }

    /// Set (or delete, with `None`) a user's direct capability profile.
    pub fn set_user_capabilities(
        &self,
        user_id: &str,
        profile: Option<&CapabilityProfile>,
    ) -> Result<(), MiraError> {
        let now = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        match profile {
            None => {
                conn.execute(
                    "DELETE FROM user_capabilities WHERE user_id = ?1",
                    params![user_id],
                )
                .map_err(|e| MiraError::DatabaseError(format!("set_user_capabilities: {e}")))?;
            }
            Some(p) => {
                let json = serde_json::to_string(p)
                    .map_err(|e| MiraError::DatabaseError(format!("serialize profile: {e}")))?;
                conn.execute(
                    "INSERT INTO user_capabilities (user_id, capabilities_json, updated_at)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(user_id) DO UPDATE SET
                         capabilities_json = excluded.capabilities_json,
                         updated_at = excluded.updated_at",
                    params![user_id, json, now],
                )
                .map_err(|e| MiraError::DatabaseError(format!("set_user_capabilities: {e}")))?;
            }
        }
        Ok(())
    }

    /// The merged, effective capability profile for a user. Admins always get
    /// an unrestricted profile. For regular users this is the union of their
    /// direct profile and every group they belong to (see module docs).
    pub fn effective_capabilities(
        &self,
        user_id: &str,
        role: &Role,
    ) -> Result<CapabilityProfile, MiraError> {
        if matches!(role, Role::Admin) {
            return Ok(CapabilityProfile::unrestricted());
        }

        let mut profiles: Vec<CapabilityProfile> = Vec::new();
        if let Some(direct) = self.get_user_capabilities(user_id)? {
            profiles.push(direct);
        }
        for gid in self.list_user_group_ids(user_id)? {
            if let Some(gp) = self.get_group_capabilities(&gid)? {
                profiles.push(gp);
            }
        }
        Ok(CapabilityProfile::merge(&profiles))
    }

    /// Effective capabilities when the caller only has a user id (looks up the
    /// role itself). Unknown users default to a non-admin `User` role.
    pub fn effective_capabilities_for(
        &self,
        user_id: &str,
    ) -> Result<CapabilityProfile, MiraError> {
        let role = self
            .find_by_id(user_id)?
            .map(|u| u.role)
            .unwrap_or(Role::User);
        self.effective_capabilities(user_id, &role)
    }
}

fn parse_profile(json: Option<String>) -> Result<Option<CapabilityProfile>, MiraError> {
    match json {
        None => Ok(None),
        Some(j) if j.trim().is_empty() => Ok(None),
        Some(j) => serde_json::from_str(&j)
            .map(Some)
            .map_err(|e| MiraError::DatabaseError(format!("parse capability profile: {e}"))),
    }
}

fn serialize_profile(profile: Option<&CapabilityProfile>) -> Result<Option<String>, MiraError> {
    match profile {
        None => Ok(None),
        Some(p) => serde_json::to_string(p)
            .map(Some)
            .map_err(|e| MiraError::DatabaseError(format!("serialize capability profile: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prof(models: Option<&[&str]>, tools: Option<&[&str]>, budget: Option<f64>) -> CapabilityProfile {
        CapabilityProfile {
            models: models.map(|m| m.iter().map(|s| s.to_string()).collect()),
            tools: tools.map(|t| t.iter().map(|s| s.to_string()).collect()),
            max_task_budget_usd: budget,
            ..Default::default()
        }
    }

    #[test]
    fn unrestricted_allows_everything() {
        let p = CapabilityProfile::unrestricted();
        assert!(p.is_unrestricted());
        assert!(p.allows_model("anything"));
        assert!(p.allows_tool("shell"));
        assert!(p.allows_provider("openai"));
        assert!(p.filter_tools(&["a".into(), "b".into()]).is_none());
        assert_eq!(p.cap_task_budget(5.0), 5.0);
    }

    #[test]
    fn restriction_gates_axis() {
        let p = prof(Some(&["gpt-4o-mini"]), Some(&["web_search"]), None);
        assert!(p.allows_model("gpt-4o-mini"));
        assert!(!p.allows_model("gpt-4o"));
        assert!(p.allows_tool("web_search"));
        assert!(!p.allows_tool("shell"));
        // providers axis untouched → unrestricted
        assert!(p.allows_provider("anthropic"));
    }

    #[test]
    fn filter_tools_intersects() {
        let p = prof(None, Some(&["web_search", "calendar"]), None);
        let got = p
            .filter_tools(&["web_search".into(), "shell".into(), "calendar".into()])
            .unwrap();
        assert_eq!(got, vec!["web_search".to_string(), "calendar".to_string()]);
    }

    #[test]
    fn merge_unions_grants() {
        // kid group grants only mini; "power" group grants gpt-4o.
        let kid = prof(Some(&["gpt-4o-mini"]), None, None);
        let power = prof(Some(&["gpt-4o"]), None, None);
        let m = CapabilityProfile::merge(&[kid, power]);
        assert!(m.allows_model("gpt-4o-mini"));
        assert!(m.allows_model("gpt-4o"));
        assert!(!m.allows_model("claude-opus")); // still gated to the union
    }

    #[test]
    fn merge_silent_axis_stays_unrestricted() {
        // Neither profile governs tools → tools unrestricted after merge.
        let a = prof(Some(&["m1"]), None, None);
        let b = prof(Some(&["m2"]), None, None);
        let m = CapabilityProfile::merge(&[a, b]);
        assert!(m.tools.is_none());
        assert!(m.allows_tool("shell"));
    }

    #[test]
    fn merge_budget_takes_minimum() {
        let a = prof(None, None, Some(2.0));
        let b = prof(None, None, Some(0.5));
        let c = prof(None, None, None);
        let m = CapabilityProfile::merge(&[a, b, c]);
        assert_eq!(m.max_task_budget_usd, Some(0.5));
        assert_eq!(m.cap_task_budget(5.0), 0.5);
    }

    #[test]
    fn merge_empty_is_unrestricted() {
        let m = CapabilityProfile::merge(&[]);
        assert!(m.is_unrestricted());
    }

    #[test]
    fn empty_allowlist_denies_all() {
        // An explicit empty set is a real "deny everything" restriction.
        let p = prof(Some(&[]), None, None);
        assert!(!p.allows_model("anything"));
    }

    // ── Storage round-trip ──────────────────────────────────────────────────

    use crate::auth::groups::NewGroup;
    use rusqlite::params;
    use tempfile::tempdir;

    fn open_db_with_user(user_id: &str) -> (tempfile::TempDir, AuthDb) {
        let dir = tempdir().unwrap();
        let db = AuthDb::open(&dir.path().join("auth.db")).unwrap();
        let conn = db.conn.lock().unwrap();
        let now = AuthDb::now_ms();
        conn.execute(
            "INSERT INTO users (id, username, password_hash, role, is_active, created_at, updated_at)
             VALUES (?1, ?1, 'x', 'user', 1, ?2, ?2)",
            params![user_id, now],
        )
        .unwrap();
        drop(conn);
        (dir, db)
    }

    #[test]
    fn group_and_user_caps_round_trip_and_merge() {
        let (_dir, db) = open_db_with_user("u1");

        // No profiles → unrestricted.
        let eff = db.effective_capabilities("u1", &Role::User).unwrap();
        assert!(eff.is_unrestricted());

        // Group restricts models to mini + caps budget at 0.50.
        let g = db.create_group(NewGroup { name: "kids".into(), description: None }, "u1").unwrap();
        db.set_group_capabilities("u1-noexist", None).unwrap_err(); // missing group errors
        db.set_group_capabilities(&g.id, Some(&prof(Some(&["gpt-4o-mini"]), None, Some(0.50)))).unwrap();
        db.add_member(&g.id, "u1", "u1").unwrap();

        // User direct profile additionally grants a tool restriction.
        db.set_user_capabilities("u1", Some(&prof(None, Some(&["web_search"]), None))).unwrap();

        let eff = db.effective_capabilities("u1", &Role::User).unwrap();
        assert!(eff.allows_model("gpt-4o-mini"));
        assert!(!eff.allows_model("gpt-4o"));
        assert!(eff.allows_tool("web_search"));
        assert!(!eff.allows_tool("shell"));
        assert_eq!(eff.max_task_budget_usd, Some(0.50));

        // Admins bypass everything regardless of stored profiles.
        let admin_eff = db.effective_capabilities("u1", &Role::Admin).unwrap();
        assert!(admin_eff.is_unrestricted());

        // Round-trip getters.
        assert!(db.get_group_capabilities(&g.id).unwrap().is_some());
        assert!(db.get_user_capabilities("u1").unwrap().is_some());

        // Clearing removes the restriction.
        db.set_user_capabilities("u1", None).unwrap();
        assert!(db.get_user_capabilities("u1").unwrap().is_none());
    }
}
