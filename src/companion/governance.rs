// SPDX-License-Identifier: AGPL-3.0-or-later

// src/companion/governance.rs
//! Family governance model for the MIRA-Guardian care-net (Slice 1).
//!
//! Formalizes the roles that approval + escalation routing build on, **without
//! a new storage layer** — it reuses what already exists:
//!
//! - A **family** is an auth **group** (`groups`/`group_members`); companion
//!   policy is a sidecar on that same group identity (see `groups.rs`).
//! - A member's **care role** ([`CareRole`]: `Standard` / `Child` / `Elder`)
//!   says whether they are a *ward* (protected, watched over) or an ordinary
//!   adult.
//! - A ward's **guardian(s)** are the members responsible for them — today,
//!   their configured **safety contact**.
//!
//! This is the first slice of the two-layer family-governance program; it
//! deliberately builds the role +
//! guardian-resolution foundation (and the approval-routing mechanism in
//! `guardian_actions`) without yet introducing the per-category consent/
//! visibility policy engine or member-scoped protective actions.

use crate::companion::settings::{CareRole, CompanionStore};

/// Whether a care role denotes a *ward* — someone being watched over, whose
/// serious-risk signals and (future) protective actions route to a guardian.
pub fn is_ward(role: CareRole) -> bool {
    role.is_monitored()
}

/// The user ids responsible for `member_id` — its guardian(s), in priority
/// order: the primary **safety contact** first, then any **additional
/// guardians** (`CareNet::guardian_ids`). A ward can have more than
/// one guardian (e.g. both parents), all of whom a distress escalation reaches.
/// De-duplicated; empty when no guardian is configured.
pub fn guardians_of(store: &CompanionStore, member_id: &str) -> Vec<String> {
    let Some(s) = store.get(member_id).ok().flatten() else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    let mut push = |id: String| {
        if !id.is_empty() && !out.contains(&id) {
            out.push(id);
        }
    };
    if let Some(primary) = s.safety_contact_user_id {
        push(primary);
    }
    for g in s.care.guardian_ids {
        push(g);
    }
    out
}

/// The base-principle floor for **cross-member wellbeing visibility** (Slice 6): whether `requester_id` may see `ward_id`'s wellbeing summary.
///
/// Non-negotiable, applied to *everyone* (admins included):
/// - the person must be a **ward** (monitored role) — an ordinary adult's data
///   is never visible to their contact (adult-data-private), and
/// - the ward must have **consented** to the care arrangement
///   (transparency-to-the-watched).
///
/// On top of that floor: an **admin** may view (operator), or a **guardian** of
/// that ward (the configurable/role layer). No setting can widen past the floor.
pub fn may_view_wellbeing(
    requester_id:       &str,
    ward_id:            &str,
    requester_is_admin: bool,
    store:              &CompanionStore,
) -> bool {
    let Some(ward) = store.get(ward_id).ok().flatten() else { return false };
    // Base-principle floor — applies to admins too.
    if !is_ward(ward.care.role) || ward.care.consent_at.is_none() {
        return false;
    }
    requester_is_admin || guardians_of(store, ward_id).iter().any(|g| g == requester_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::settings::{CareNet, CareRole, CompanionSettings, CompanionStore};
    use chrono::Utc;
    use tempfile::tempdir;

    #[test]
    fn is_ward_tracks_monitored_roles() {
        assert!(!is_ward(CareRole::Standard));
        assert!(is_ward(CareRole::Child));
        assert!(is_ward(CareRole::Elder));
    }

    #[test]
    fn guardians_of_unions_primary_and_additional_deduped() {
        let dir = tempdir().unwrap();
        let store = CompanionStore::open(&dir.path().join("companion.db")).unwrap();

        // A ward (kid) with primary guardian "mum" + additional guardians
        // "dad" and (a duplicate of) "mum".
        let mut s = CompanionSettings::new("kid");
        s.safety_contact_user_id = Some("mum".into());
        s.care = CareNet {
            role: CareRole::Child,
            consent_at: None,
            guardian_ids: vec!["dad".into(), "mum".into()],
        };
        store.upsert(&s).unwrap();

        // Primary first, then additional, de-duplicated.
        assert_eq!(guardians_of(&store, "kid"), vec!["mum".to_string(), "dad".to_string()]);

        // Single-contact ward (no additional) still works.
        let mut solo = CompanionSettings::new("gran");
        solo.safety_contact_user_id = Some("son".into());
        store.upsert(&solo).unwrap();
        assert_eq!(guardians_of(&store, "gran"), vec!["son".to_string()]);

        // Unknown / unconfigured member → no guardians (never panics).
        assert!(guardians_of(&store, "nobody").is_empty());
    }

    // Upsert a ward `id` with primary guardian `primary`, `role`, and consent.
    fn seed_ward(store: &CompanionStore, id: &str, primary: &str, role: CareRole, consented: bool) {
        let mut s = CompanionSettings::new(id);
        s.safety_contact_user_id = Some(primary.into());
        s.care = CareNet {
            role,
            consent_at: if consented { Some(Utc::now()) } else { None },
            guardian_ids: vec![],
        };
        store.upsert(&s).unwrap();
    }

    #[test]
    fn wards_of_is_the_reverse_of_guardians_of() {
        let dir = tempdir().unwrap();
        let store = CompanionStore::open(&dir.path().join("companion.db")).unwrap();
        // kid: primary "mum", additional "dad".
        let mut kid = CompanionSettings::new("kid");
        kid.safety_contact_user_id = Some("mum".into());
        kid.care = CareNet { role: CareRole::Child, consent_at: None, guardian_ids: vec!["dad".into()] };
        store.upsert(&kid).unwrap();
        seed_ward(&store, "gran", "mum", CareRole::Elder, false);

        let mut mums = store.wards_of("mum").unwrap();
        mums.sort();
        assert_eq!(mums, vec!["gran".to_string(), "kid".to_string()]);
        assert_eq!(store.wards_of("dad").unwrap(), vec!["kid".to_string()]);
        assert!(store.wards_of("nobody").unwrap().is_empty());
    }

    #[test]
    fn may_view_wellbeing_enforces_the_floor_for_everyone() {
        let dir = tempdir().unwrap();
        let store = CompanionStore::open(&dir.path().join("companion.db")).unwrap();
        seed_ward(&store, "kid", "mum", CareRole::Child, true);   // ward, consented
        seed_ward(&store, "teen", "mum", CareRole::Child, false); // ward, NOT consented
        seed_ward(&store, "pal", "mum", CareRole::Standard, true); // adult peer, consented

        // Consented ward: their guardian yes; an admin yes; a stranger no.
        assert!(may_view_wellbeing("mum", "kid", false, &store));
        assert!(may_view_wellbeing("someadmin", "kid", true, &store));
        assert!(!may_view_wellbeing("stranger", "kid", false, &store));

        // Non-consented ward: floor blocks EVEN the guardian and an admin
        // (transparency-to-the-watched).
        assert!(!may_view_wellbeing("mum", "teen", false, &store));
        assert!(!may_view_wellbeing("someadmin", "teen", true, &store));

        // Adult peer (Standard): never visible, even to their contact / an admin
        // (adult-data-private).
        assert!(!may_view_wellbeing("mum", "pal", false, &store));
        assert!(!may_view_wellbeing("someadmin", "pal", true, &store));

        // Unknown member → false.
        assert!(!may_view_wellbeing("mum", "ghost", true, &store));
    }
}
