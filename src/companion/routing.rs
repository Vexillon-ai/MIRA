// SPDX-License-Identifier: AGPL-3.0-or-later

// src/companion/routing.rs
//! Cross-user routing gateway for the companion family bridge
//!.
//!
//! Pure function — given a signal, the group's policy, and the
//! group's member rows, return the list of users that should
//! actually receive a notice. All the filtering logic
//! (opt_in / contactable_for / mute_hours / daily_message_cap /
//! sender exclusion / Distress-bypass) lives here in one place,
//! easy to reason about, easy to test.
//!
//! The CALLER (the safety floor) is responsible for:
//! - Enumerating the user's companion-enabled groups.
//! - Loading the policy + members for each group.
//! - Tallying today's per-recipient delivery count (passed in via
//! `delivered_today_count`).
//! - Calling `route` per group.
//! - Actually delivering the notices and updating delivery counts.

use chrono::{DateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

use crate::companion::groups::{GroupCompanionMember, GroupCompanionPolicy, SignalKind};

// One recipient + the channel preference list to use for them.
// The caller picks the actual channel (first reachable in the
// list, falling back to web).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryTarget {
    pub user_id: String,
    pub channel_preference: Vec<String>,
}

// Why a candidate member was excluded — useful for tests + audit
// logs ("we considered notifying X, but…").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    // This member is the sender — never notify the source about
    // their own signal.
    Sender,
    // `opt_in = false`. The hard gate.
    NotOptedIn,
    // Signal kind not in member's `contactable_for` list.
    NotContactable,
    // Signal kind not in the group's `allowed_signals`.
    NotInGroupPolicy,
    // Member is in their mute window — skipped (Distress
    // overrides, so this only fires for non-Distress signals).
    MutedHours,
    // Member has hit their daily cap. (Distress bypasses.)
    DailyCapReached,
}

// Per-recipient input for the daily-cap check. The caller queries
// the safety_log for `kind=Distress|MissedCheckin|...` events
// delivered to `user_id` since the start of *their* local day and
// passes the count in here. Routing stays pure.
#[derive(Debug, Clone, Copy)]
pub struct DeliveryCount {
    pub today_local: u32,
}

// One row of the gateway's decision per candidate member. Tests
// (and the audit-log path in the safety floor) read this struct
// to verify routing was correct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingDecision {
    Deliver(DeliveryTarget),
    Skip { user_id: String, reason: SkipReason },
}

impl RoutingDecision {
    pub fn is_deliver(&self) -> bool { matches!(self, RoutingDecision::Deliver(_)) }
    pub fn user_id(&self) -> &str {
        match self {
            RoutingDecision::Deliver(t) => &t.user_id,
            RoutingDecision::Skip { user_id, .. } => user_id,
        }
    }
}

// Decide who in this group should receive a notice for `signal`.
// // - `sender_user_id` is excluded from delivery (never notify the
// user about their own signal).
// - `policy.allowed_signals` gates the entire group; if the signal
// isn't in the list, EVERY member's decision is
// `Skip { NotInGroupPolicy }`.
// - For each remaining member: enforce opt-in, contactable_for,
// mute_hours (Distress bypasses), daily_message_cap (Distress
// bypasses).
// - `now` + each member's `mute_hours` interpret in the member's
// tz; the caller passes `member_tz` per-member.
// // Returns a per-member decision so the caller can audit-log
// suppressions.
pub fn route(
    signal: SignalKind,
    sender_user_id: &str,
    policy: &GroupCompanionPolicy,
    members: &[GroupCompanionMember],
    member_tz: impl Fn(&str) -> Option<String>,
    delivered_today: impl Fn(&str) -> DeliveryCount,
    now: DateTime<Utc>,
) -> Vec<RoutingDecision> {
    let mut out = Vec::with_capacity(members.len());

    let signal_in_group_policy = policy.allowed_signals.contains(&signal);

    for m in members {
        if m.user_id == sender_user_id {
            out.push(RoutingDecision::Skip {
                user_id: m.user_id.clone(),
                reason: SkipReason::Sender,
            });
            continue;
        }
        if !signal_in_group_policy {
            out.push(RoutingDecision::Skip {
                user_id: m.user_id.clone(),
                reason: SkipReason::NotInGroupPolicy,
            });
            continue;
        }
        if !m.opt_in {
            out.push(RoutingDecision::Skip {
                user_id: m.user_id.clone(),
                reason: SkipReason::NotOptedIn,
            });
            continue;
        }
        if !m.contactable_for.contains(&signal) {
            out.push(RoutingDecision::Skip {
                user_id: m.user_id.clone(),
                reason: SkipReason::NotContactable,
            });
            continue;
        }

        // Distress is "always-deliver" — it bypasses mute_hours and
        // daily_cap. The whole point of the safety floor is to NOT
        // be silenceable on this signal class.
        if signal != SignalKind::Distress {
            // Mute hours
            let tz = member_tz(&m.user_id);
            if in_mute_window(&m.mute_hours, now, tz.as_deref()) {
                out.push(RoutingDecision::Skip {
                    user_id: m.user_id.clone(),
                    reason: SkipReason::MutedHours,
                });
                continue;
            }
            // Daily cap
            let count = delivered_today(&m.user_id);
            if count.today_local >= m.daily_message_cap {
                out.push(RoutingDecision::Skip {
                    user_id: m.user_id.clone(),
                    reason: SkipReason::DailyCapReached,
                });
                continue;
            }
        }

        out.push(RoutingDecision::Deliver(DeliveryTarget {
            user_id: m.user_id.clone(),
            channel_preference: m.channel_preference.clone(),
        }));
    }

    out
}

// Strip privacy-protected sentences from a notice body. Same
// dumb-but-effective approach as the wiki extractor's path filter:
// substring match per sentence (period-delimited), drop any
// sentence that contains a banned topic. Better to under-share
// than leak.
pub fn apply_privacy_filter(body: &str, privacy_topics: &[String]) -> String {
    if privacy_topics.is_empty() { return body.to_string(); }
    let lowered_topics: Vec<String> = privacy_topics.iter()
        .map(|t| t.to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if lowered_topics.is_empty() { return body.to_string(); }

    let sentences: Vec<&str> = body.split_inclusive(|c: char| c == '.' || c == '?' || c == '!').collect();
    let mut out = String::with_capacity(body.len());
    for s in sentences {
        let lower = s.to_lowercase();
        let contains_banned = lowered_topics.iter().any(|t| lower.contains(t));
        if !contains_banned {
            out.push_str(s);
        } else {
            // Replace banned sentence with a marker — the recipient
            // sees that *something* was withheld, which is honest
            // and lets them ask the source for detail if they need
            // to. Empty replacement would hide the existence of the
            // omission, which is worse.
            out.push_str(" [content withheld per group privacy settings] ");
        }
    }
    out
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn in_mute_window(
    mute_hours: &[(String, String)],
    now: DateTime<Utc>,
    tz_name: Option<&str>,
) -> bool {
    if mute_hours.is_empty() { return false; }
    let tz: Tz = tz_name
        .and_then(|s| s.parse::<Tz>().ok())
        .unwrap_or(chrono_tz::UTC);
    let local_now = tz.from_utc_datetime(&now.naive_utc()).time();

    for (start, end) in mute_hours {
        let Ok(s) = NaiveTime::parse_from_str(start, "%H:%M") else { continue; };
        let Ok(e) = NaiveTime::parse_from_str(end,   "%H:%M") else { continue; };
        let in_window = if s <= e {
            local_now >= s && local_now < e
        } else {
            // wraps midnight
            local_now >= s || local_now < e
        };
        if in_window { return true; }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(allowed: Vec<SignalKind>, privacy: Vec<&str>) -> GroupCompanionPolicy {
        let now = Utc::now();
        GroupCompanionPolicy {
            group_id: "family".into(),
            allowed_signals: allowed,
            privacy_topics: privacy.iter().map(|s| s.to_string()).collect(),
            created_at: now,
            updated_at: now,
        }
    }

    fn member(uid: &str, opt_in: bool, contactable: Vec<SignalKind>) -> GroupCompanionMember {
        let now = Utc::now();
        GroupCompanionMember {
            group_id: "family".into(),
            user_id: uid.into(),
            contactable_for: contactable,
            channel_preference: vec!["web".into()],
            mute_hours: vec![],
            daily_message_cap: 3,
            opt_in,
            joined_at: now,
            updated_at: now,
        }
    }

    fn no_tz(_: &str) -> Option<String> { None }
    fn no_count(_: &str) -> DeliveryCount { DeliveryCount { today_local: 0 } }

    // ── Basic happy path ─────────────────────────────────────────

    #[test]
    fn distress_routes_to_opted_in_members_excluding_sender() {
        let p = policy(vec![SignalKind::Distress], vec![]);
        let members = vec![
            member("alice", true, vec![SignalKind::Distress]),  // sender
            member("david", true, vec![SignalKind::Distress]),
            member("sarah", true, vec![SignalKind::Distress]),
        ];
        let decisions = route(
            SignalKind::Distress, "alice", &p, &members,
            no_tz, no_count, Utc::now(),
        );
        let delivered: Vec<&str> = decisions.iter()
            .filter_map(|d| match d {
                RoutingDecision::Deliver(t) => Some(t.user_id.as_str()),
                _ => None,
            }).collect();
        assert_eq!(delivered, vec!["david", "sarah"]);

        // Alice is in the list as Skip{Sender}.
        let alice_dec = decisions.iter().find(|d| d.user_id() == "alice").unwrap();
        assert!(matches!(alice_dec, RoutingDecision::Skip { reason: SkipReason::Sender, .. }));
    }

    // ── Gating ───────────────────────────────────────────────────

    #[test]
    fn not_opted_in_is_skipped() {
        let p = policy(vec![SignalKind::Distress], vec![]);
        let members = vec![member("david", false, vec![SignalKind::Distress])];
        let decisions = route(SignalKind::Distress, "alice", &p, &members,
                              no_tz, no_count, Utc::now());
        assert!(matches!(decisions[0],
            RoutingDecision::Skip { reason: SkipReason::NotOptedIn, .. }));
    }

    #[test]
    fn not_in_contactable_for_is_skipped() {
        let p = policy(vec![SignalKind::Distress, SignalKind::MissedCheckin], vec![]);
        // David opted in but only for MissedCheckin.
        let members = vec![member("david", true, vec![SignalKind::MissedCheckin])];
        let decisions = route(SignalKind::Distress, "alice", &p, &members,
                              no_tz, no_count, Utc::now());
        assert!(matches!(decisions[0],
            RoutingDecision::Skip { reason: SkipReason::NotContactable, .. }));
    }

    #[test]
    fn signal_not_in_group_policy_skips_all() {
        // Group policy doesn't include Distress at all.
        let p = policy(vec![SignalKind::MissedCheckin], vec![]);
        let members = vec![
            member("david", true, vec![SignalKind::Distress, SignalKind::MissedCheckin]),
            member("sarah", true, vec![SignalKind::Distress, SignalKind::MissedCheckin]),
        ];
        let decisions = route(SignalKind::Distress, "alice", &p, &members,
                              no_tz, no_count, Utc::now());
        // Both skip with NotInGroupPolicy.
        for d in &decisions {
            assert!(matches!(d,
                RoutingDecision::Skip { reason: SkipReason::NotInGroupPolicy, .. }));
        }
    }

    // ── Distress bypass ──────────────────────────────────────────

    #[test]
    fn distress_bypasses_mute_hours() {
        // Member's mute hours cover the current time.
        let mut david = member("david", true, vec![SignalKind::Distress]);
        // Mute all day everywhere — even matches UTC default.
        david.mute_hours = vec![("00:00".into(), "23:59".into())];
        let p = policy(vec![SignalKind::Distress], vec![]);
        let decisions = route(SignalKind::Distress, "alice", &p, &[david],
                              no_tz, no_count, Utc::now());
        assert!(decisions[0].is_deliver(), "Distress must bypass mute_hours");
    }

    #[test]
    fn missed_checkin_respects_mute_hours() {
        let mut david = member("david", true, vec![SignalKind::MissedCheckin]);
        david.mute_hours = vec![("00:00".into(), "23:59".into())];
        let p = policy(vec![SignalKind::MissedCheckin], vec![]);
        let decisions = route(SignalKind::MissedCheckin, "alice", &p, &[david],
                              no_tz, no_count, Utc::now());
        assert!(matches!(decisions[0],
            RoutingDecision::Skip { reason: SkipReason::MutedHours, .. }));
    }

    #[test]
    fn distress_bypasses_daily_cap() {
        let mut david = member("david", true, vec![SignalKind::Distress]);
        david.daily_message_cap = 1;
        let p = policy(vec![SignalKind::Distress], vec![]);
        let count = |_: &_| DeliveryCount { today_local: 5 }; // over cap
        let decisions = route(SignalKind::Distress, "alice", &p, &[david],
                              no_tz, count, Utc::now());
        assert!(decisions[0].is_deliver(),
            "Distress must bypass daily_message_cap");
    }

    #[test]
    fn missed_checkin_respects_daily_cap() {
        let mut david = member("david", true, vec![SignalKind::MissedCheckin]);
        david.daily_message_cap = 1;
        let p = policy(vec![SignalKind::MissedCheckin], vec![]);
        let count = |_: &_| DeliveryCount { today_local: 1 };
        let decisions = route(SignalKind::MissedCheckin, "alice", &p, &[david],
                              no_tz, count, Utc::now());
        assert!(matches!(decisions[0],
            RoutingDecision::Skip { reason: SkipReason::DailyCapReached, .. }));
    }

    // ── Privacy filter ───────────────────────────────────────────

    #[test]
    fn privacy_filter_strips_banned_sentence() {
        let body = "User said they fell at home. They mentioned blood pressure was high. Currently safe.";
        let out = apply_privacy_filter(body, &["health".into(), "blood pressure".into()]);
        assert!(out.contains("User said they fell at home."));
        assert!(!out.contains("blood pressure"));
        assert!(out.contains("[content withheld"));
        assert!(out.contains("Currently safe."));
    }

    #[test]
    fn privacy_filter_no_topics_is_passthrough() {
        let body = "Hi. Bye.";
        assert_eq!(apply_privacy_filter(body, &[]), body);
    }

    #[test]
    fn privacy_filter_is_case_insensitive() {
        let out = apply_privacy_filter(
            "BLOOD PRESSURE was high.",
            &["blood pressure".into()],
        );
        assert!(!out.contains("BLOOD"));
    }

    #[test]
    fn privacy_filter_ignores_empty_topic_strings() {
        let body = "Hi.";
        assert_eq!(apply_privacy_filter(body, &["".into(), "  ".into()]), body);
    }

    // ── Composition ──────────────────────────────────────────────

    #[test]
    fn routing_returns_per_member_decisions_with_diverse_outcomes() {
        let p = policy(vec![SignalKind::Distress, SignalKind::MissedCheckin], vec![]);
        let alice = member("alice", true, vec![SignalKind::Distress]); // sender
        let david = member("david", true, vec![SignalKind::Distress]);
        let mut sarah = member("sarah", true, vec![SignalKind::MissedCheckin]); // not distress
        // Bob isn't opted in.
        let bob = member("bob", false, vec![SignalKind::Distress]);
        // Carla is at her cap on Distress — but Distress bypasses; she
        // should still receive.
        let mut carla = member("carla", true, vec![SignalKind::Distress]);
        carla.daily_message_cap = 1;
        sarah.channel_preference = vec!["signal".into(), "web".into()];

        let count = |uid: &str| DeliveryCount {
            today_local: if uid == "carla" { 5 } else { 0 },
        };
        let decisions = route(
            SignalKind::Distress, "alice", &p,
            &[alice, david, sarah, bob, carla],
            no_tz, count, Utc::now(),
        );
        // alice → Skip(Sender)
        assert!(matches!(decisions[0],
            RoutingDecision::Skip { reason: SkipReason::Sender, .. }));
        // david → Deliver
        assert!(decisions[1].is_deliver());
        // sarah → Skip(NotContactable)
        assert!(matches!(decisions[2],
            RoutingDecision::Skip { reason: SkipReason::NotContactable, .. }));
        // bob → Skip(NotOptedIn)
        assert!(matches!(decisions[3],
            RoutingDecision::Skip { reason: SkipReason::NotOptedIn, .. }));
        // carla → Deliver (Distress bypasses cap)
        assert!(decisions[4].is_deliver());
    }
}
