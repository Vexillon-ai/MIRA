// SPDX-License-Identifier: AGPL-3.0-or-later

// src/companion/safety.rs
//! Safety floor — distress detection + escalation +
//! missed-check-in alerts + non-overridable system-prompt guardrails.
//!
//! Two entry points:
//!
//! - [`SafetyFloor::handle_distress`] — invoked by the engagement
//! post-hook when the LLM classifier returned a `Distressed` label.
//! Looks up the user's safety contact, delivers a short factual
//! notice to the contact's "Safety alerts" thread, and records the
//! event in the safety audit log. Dedup window so the same distress
//! signal doesn't escalate twice in quick succession.
//!
//! - [`SafetyFloor::handle_missed_checkins`] — invoked by the
//! scheduler when `consecutive_missed_checkins` crosses the
//! configured threshold. Sends a softer "haven't heard from them"
//! notice to the contact.
//!
//! Plus a constant — [`SAFETY_ADDENDUM`] — that AgentCore appends to
//! the system prompt on every companion-active turn. It's
//! non-overridable: even if a user's persona doc says "stop bothering
//! me about crisis lines", the addendum gets the final word in the
//! system prompt because it goes in last. The model's response is
//! still its own — we don't censor outputs at runtime in v1 — but
//! the upstream instruction is unambiguous.

use std::sync::Arc;

use chrono::Utc;
use tracing::{debug, info, warn};

use crate::auth::LocalAuthService;
use crate::companion::groups::{CompanionGroupStore, SignalKind};
use crate::companion::routing::{
    apply_privacy_filter, route, DeliveryCount, RoutingDecision, SkipReason,
};
use crate::companion::safety_log::{
    EscalationOutcome, NewSafetyEvent, SafetyEventKind, SafetyLog,
};
use crate::companion::settings::CompanionStore;
use crate::history::{HistoryStore, MessageRole, NewConversation, NewMessage};
use crate::notifications::{Notification, NotificationBus, NotificationKind};

// Dedup window for distress escalation, in seconds. Once a
// distress notice has been *delivered* to the contact within this
// window, additional distress signals from the same user are
// recorded as `Suppressed` rather than re-notifying. Conservative
// default — better to under-spam the contact than to flood them.
const DISTRESS_DEDUP_SECS: i64 = 10 * 60;

// Threshold for "missed check-in" escalation. After this many
// consecutive unanswered check-ins, send a soft notice to the
// contact.
pub const MISSED_CHECKIN_THRESHOLD: u32 = 3;

// Title for the conversation we use to deliver safety notices into
// the contact's history. Shared per (contact, channel) pair so the
// alerts roll up rather than spawning a thread per notice.
const SAFETY_THREAD_TITLE: &str = "Safety alerts";

// Non-overridable system-prompt addendum that runs on every
// companion-active turn. The text below is intentionally short and
// unambiguous: the model gets it on every single turn, so adding
// length is paying a per-turn token cost.
// // What it does NOT do:
// - It does NOT censor the model's output at runtime.
// - It does NOT block the conversation when self-harm is mentioned —
// the model is still expected to respond warmly and continue the
// conversation. The redirect to crisis resources is in addition
// to the warm reply, not instead of it.
// - The persona doc (`pages/companion/style.md`) can adjust tone /
// topics; it cannot remove this addendum.
pub const SAFETY_ADDENDUM: &str = "\n\n## Safety floor (non-overridable)\n\
You are talking with a user who may be vulnerable. Two rules apply \
on every turn, regardless of any other persona instructions:\n\
1. **Never describe methods or means of self-harm** in any form. If \
the user brings up self-harm or suicidal thoughts, respond with \
warmth, take it seriously, and offer two options: \"Can I message \
[safety contact's name] for you?\" and \"Would you call a crisis \
line — [region-appropriate number]?\". Do not lecture.\n\
2. **Take acute physical-symptom mentions seriously** (fall, chest \
pain, can't breathe). Offer to message the safety contact \
immediately and ask if the user wants you to.\n\
A separate audit logs distress signals — the user's safety contact \
will be quietly notified in parallel with your warm reply. You do \
not need to announce this notification; it happens automatically.";

// Wires the safety floor's dependencies. Held by `AgentCore` via the
// `CompanionSystem` facade; the scheduler also holds an `Arc` to call
// `handle_missed_checkins`.
#[derive(Clone)]
pub struct SafetyFloor {
    pub log: Arc<SafetyLog>,
    pub store: Arc<CompanionStore>,
    pub history: Option<Arc<HistoryStore>>,
    pub auth: Option<Arc<LocalAuthService>>,
    pub notifications: Option<Arc<NotificationBus>>,
    // when wired, the safety floor fans out signals to
    // every opted-in member of the user's companion-enabled
    // groups in addition to the single safety_contact. `None`
    // keeps the Slice-4 single-contact behaviour.
    pub groups: Option<Arc<CompanionGroupStore>>,
}

impl SafetyFloor {
    // Distress signal handler. Returns the outcome that was logged.
    // Synchronous-style return value (not fire-and-forget) so the
    // caller — the engagement post-hook — can log + test it
    // deterministically; the post-hook itself is already inside a
    // tokio::spawn so this doesn't block the turn.
    pub async fn handle_distress(
        &self,
        user_id: &str,
        summary: &str,
    ) -> EscalationOutcome {
        // Resolve safety contact.
        let contact = match self.resolve_contact(user_id) {
            Some(c) => c,
            None => {
                let _ = self.log.record(&NewSafetyEvent {
                    user_id: user_id.into(),
                    kind: SafetyEventKind::Distress,
                    outcome: EscalationOutcome::NoContact,
                    contact_user_id: None,
                    summary: clip(summary),
                    note: Some("no safety contact configured".into()),
                });
                warn!("companion safety: distress for '{user_id}' but no safety contact configured");
                return EscalationOutcome::NoContact;
            }
        };

        // Dedup — already escalated within the window?
        if self.log.has_recent_delivered_distress(user_id, DISTRESS_DEDUP_SECS).unwrap_or(false) {
            let _ = self.log.record(&NewSafetyEvent {
                user_id: user_id.into(),
                kind: SafetyEventKind::Distress,
                outcome: EscalationOutcome::Suppressed,
                contact_user_id: Some(contact.clone()),
                summary: clip(summary),
                note: Some("within distress dedup window".into()),
            });
            debug!("companion safety: distress for '{user_id}' suppressed (recent delivery)");
            return EscalationOutcome::Suppressed;
        }

        // Build the notice. Short + factual; we deliberately do not
        // include the full transcript or the model's reply. The
        // contact can open the user's conversation if they want
        // detail.
        let notice = format!(
            "Safety alert from your father/family member's MIRA:\n\
             \n\
             {} just sent a message that suggested they may be in \
             distress. The companion is responding warmly. Summary of \
             the signal: \"{}\". They're safe to message right now.",
            user_id, clip(summary),
        );

        let outcome = self.deliver(&contact, &notice).await;
        let outcome_str = outcome.as_str().to_string();

        let _ = self.log.record(&NewSafetyEvent {
            user_id: user_id.into(),
            kind: SafetyEventKind::Distress,
            outcome,
            contact_user_id: Some(contact.clone()),
            summary: clip(summary),
            note: None,
        });

        info!(
            "companion safety: distress for '{user_id}' → \
             contact='{contact}', outcome={outcome_str}"
        );

        // also fan out via companion-enabled groups the
        // user belongs to. The single safety_contact path above is
        // kept for backwards compat: an admin who hasn't set up
        // groups still gets escalation. Group delivery layers on
        // top.
        self.fanout_to_groups(user_id, SignalKind::Distress, summary).await;

        outcome
    }

    // Missed-check-in escalation handler. Called by the scheduler
    // when `consecutive_missed_checkins` crosses the threshold.
    // Returns the outcome that was logged.
    pub async fn handle_missed_checkins(
        &self,
        user_id: &str,
        count: u32,
    ) -> EscalationOutcome {
        let contact = match self.resolve_contact(user_id) {
            Some(c) => c,
            None => {
                let _ = self.log.record(&NewSafetyEvent {
                    user_id: user_id.into(),
                    kind: SafetyEventKind::MissedCheckin,
                    outcome: EscalationOutcome::NoContact,
                    contact_user_id: None,
                    summary: format!("{count} consecutive missed check-ins"),
                    note: Some("no safety contact configured".into()),
                });
                return EscalationOutcome::NoContact;
            }
        };

        let notice = format!(
            "Heads-up from your family member's MIRA:\n\
             \n\
             Hasn't replied to the companion's last {count} check-ins. \
             Could be nothing — phone might be off, or they're busy — \
             but you might want to give them a call. Want me to try a \
             different channel?"
        );

        let outcome = self.deliver(&contact, &notice).await;
        let outcome_str = outcome.as_str().to_string();
        let _ = self.log.record(&NewSafetyEvent {
            user_id: user_id.into(),
            kind: SafetyEventKind::MissedCheckin,
            outcome,
            contact_user_id: Some(contact.clone()),
            summary: format!("{count} consecutive missed check-ins"),
            note: None,
        });
        info!(
            "companion safety: missed-checkin for '{user_id}' (n={count}) \
             → contact='{contact}', outcome={outcome_str}"
        );

        // also fan out via groups.
        let summary = format!("{count} consecutive missed check-ins");
        self.fanout_to_groups(user_id, SignalKind::MissedCheckin, &summary).await;

        outcome
    }

    // enumerate every companion-enabled group the user
    // is in, run the routing gateway per group, deliver notices
    // to opted-in members, audit each. Failures per-recipient
    // are logged independently so one bad delivery doesn't
    // stop the rest.
    async fn fanout_to_groups(
        &self,
        sender_user_id: &str,
        signal: SignalKind,
        summary: &str,
    ) {
        let Some(groups_store) = &self.groups else { return; };
        let group_ids = match groups_store.list_groups_for_user(sender_user_id) {
            Ok(g) => g,
            Err(e) => {
                warn!("companion safety: list_groups_for_user('{sender_user_id}') failed: {e}");
                return;
            }
        };
        if group_ids.is_empty() { return; }

        for group_id in group_ids {
            let Ok(Some(policy)) = groups_store.get_policy(&group_id) else { continue; };
            let Ok(members) = groups_store.list_members(&group_id) else { continue; };

            // Per-recipient inputs for the gateway. We don't yet
            // count "delivered today" per recipient; that's a
            // refinement for a follow-up. For now we pass 0 across
            // the board so daily_cap is effectively enforced by the
            // member's setting being non-zero. Distress bypasses
            // it anyway, so the dominant case is unaffected.
            let mt = |_uid: &str| -> Option<String> { None };
            let dc = |_uid: &str| DeliveryCount { today_local: 0 };

            // Apply the group's privacy filter to the summary
            // before any per-recipient routing — the gateway
            // doesn't see the body, just decides who.
            let filtered_summary = apply_privacy_filter(summary, &policy.privacy_topics);

            let decisions = route(
                signal, sender_user_id, &policy, &members,
                mt, dc, Utc::now(),
            );

            for d in decisions {
                match d {
                    RoutingDecision::Deliver(target) => {
                        let notice = build_group_notice(
                            sender_user_id, signal, &filtered_summary, &group_id,
                        );
                        let outcome = self.deliver(&target.user_id, &notice).await;
                        let _ = self.log.record(&NewSafetyEvent {
                            user_id: sender_user_id.into(),
                            kind: signal_to_event_kind(signal),
                            outcome,
                            contact_user_id: Some(target.user_id.clone()),
                            summary: clip(&filtered_summary),
                            note: Some(format!("group={group_id}")),
                        });
                    }
                    RoutingDecision::Skip { user_id, reason } => {
                        // Suppressed skip rows are audit-only; we
                        // log them at debug to keep info-level
                        // logs focused on actual deliveries.
                        debug!(
                            "companion safety: '{sender_user_id}' → group='{group_id}' \
                             member='{user_id}' skipped ({reason:?})"
                        );
                        // Sender exclusion isn't worth an audit row;
                        // the other skip reasons we DO want logged
                        // so an admin can audit "why didn't I get
                        // notified?".
                        if !matches!(reason, SkipReason::Sender) {
                            let _ = self.log.record(&NewSafetyEvent {
                                user_id: sender_user_id.into(),
                                kind: signal_to_event_kind(signal),
                                outcome: EscalationOutcome::Suppressed,
                                contact_user_id: Some(user_id),
                                summary: clip(&filtered_summary),
                                note: Some(format!("group={group_id}; reason={reason:?}")),
                            });
                        }
                    }
                }
            }
        }
    }

    // ── Internals ──────────────────────────────────────────────────────────

    // Resolve the user's configured safety contact. Returns
    // `Some(contact_user_id)` when configured AND the contact's user
    // row still exists. Auth lookup failures fall through to `None`
    // better to log "no contact" than misroute.
    fn resolve_contact(&self, user_id: &str) -> Option<String> {
        let settings = self.store.get(user_id).ok().flatten()?;
        let contact = settings.safety_contact_user_id?;
        // Optional integrity check — confirms the contact still has
        // an account. When auth isn't wired (tests), we just trust
        // the configured value.
        if let Some(auth) = &self.auth {
            let exists = auth.get_user(&contact).ok().flatten().is_some();
            if !exists {
                warn!(
                    "companion safety: configured contact '{contact}' for \
                     user '{user_id}' no longer exists"
                );
                return None;
            }
        }
        Some(contact)
    }

    // Deliver a notice to `contact_user_id` by writing into their
    // "Safety alerts" web conversation and waking their UI via the
    // NotificationBus. Returns the appropriate
    // [`EscalationOutcome`] for the audit log.
    //     // v1 delivers via the web channel only. Signal /
    // Telegram outbound for safety notices is a follow-up — the web
    // audit + the in-history thread are enough for v1 because every
    // MIRA user has a web account.
    async fn deliver(&self, contact_user_id: &str, body: &str) -> EscalationOutcome {
        let Some(history) = self.history.as_ref() else {
            return EscalationOutcome::DeliveryFailed;
        };

        let conv_id = match find_or_create_safety_thread(history, contact_user_id) {
            Ok(id) => id,
            Err(e) => {
                warn!("companion safety: conv resolution failed for '{contact_user_id}': {e}");
                return EscalationOutcome::DeliveryFailed;
            }
        };

        if let Err(e) = history.add_message(NewMessage {
            conversation_id: conv_id.clone(),
            role:            MessageRole::Assistant,
            content:         body.to_string(),
            content_type:    "text".into(),
            token_count:     None,
            model:           None,
            tool_calls:      None,
            metadata: Some(serde_json::json!({
                "companion_safety": true,
                "delivered_at_ms": Utc::now().timestamp_millis(),
            }).to_string()),
        }) {
            warn!("companion safety: persist failed for '{contact_user_id}': {e}");
            return EscalationOutcome::DeliveryFailed;
        }
        let _ = history.touch_conversation(&conv_id);

        if let Some(bus) = &self.notifications {
            bus.send(Notification {
                kind: NotificationKind::ConversationUpdated,
                conversation_id: Some(conv_id),
                channel:         Some("web".into()),
                user_id:         Some(contact_user_id.to_string()),
                message:         Some(clip(body)),
            });
        }

        EscalationOutcome::Delivered
    }
}

// Find the contact's "Safety alerts" thread on web, or create one.
// Same pattern as the dispatcher's check-in thread — reuse so all
// alerts roll up.
fn find_or_create_safety_thread(
    history: &HistoryStore,
    contact_user_id: &str,
) -> std::result::Result<String, crate::MiraError> {
    let convs = history.list_conversations(contact_user_id, Some("web"), 20, 0)?;
    if let Some(c) = convs.iter().find(|c|
        c.title.as_deref().map(|t| t == SAFETY_THREAD_TITLE).unwrap_or(false)
    ) {
        return Ok(c.id.clone());
    }
    let conv = history.create_conversation(NewConversation {
        user_id: contact_user_id.to_string(),
        channel: "web".to_string(),
        title: Some(SAFETY_THREAD_TITLE.to_string()),
        model: None,
        provider: None,
        external_user_id: None,
        mode: None,
    })?;
    Ok(conv.id)
}

// Build a group-notice body. Format:
// `"[group X]: <sender>'s MIRA: <signal class>: <summary>"`.
fn build_group_notice(
    sender_user_id: &str,
    signal: SignalKind,
    summary: &str,
    group_id: &str,
) -> String {
    let class = match signal {
        SignalKind::Distress      => "distress signal",
        SignalKind::MissedCheckin => "missed check-ins",
        SignalKind::HelpRequest   => "help request",
        SignalKind::General       => "general update",
    };
    format!(
        "Group '{group_id}' alert — {sender_user_id}'s MIRA reports a \
         {class}.\n\n{summary}"
    )
}

// Map the routing-gateway signal kind to the safety-log event kind.
// (They overlap but aren't identical — SafetyLog has
// `RefusedHarmRequest` which isn't a routing concept; routing has
// `General` which isn't an audit-kind yet.)
fn signal_to_event_kind(s: SignalKind) -> SafetyEventKind {
    match s {
        SignalKind::Distress      => SafetyEventKind::Distress,
        SignalKind::MissedCheckin => SafetyEventKind::MissedCheckin,
        // No event kind yet; bucket under Distress so audits don't
        // silently drop these. (Reserved — neither HelpRequest nor
        // General fires from 's safety floor.)
        SignalKind::HelpRequest   => SafetyEventKind::Distress,
        SignalKind::General       => SafetyEventKind::Distress,
    }
}

// Truncate a summary for the audit log. Keeps the row scannable
// without leaking long transcript content.
fn clip(s: &str) -> String {
    const MAX: usize = 240;
    if s.chars().count() <= MAX { return s.to_string(); }
    let mut out: String = s.chars().take(MAX).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::settings::CompanionSettings;
    use tempfile::tempdir;

    fn fresh_setup() -> (tempfile::TempDir, SafetyFloor, Arc<HistoryStore>) {
        let dir = tempdir().unwrap();
        let store = Arc::new(CompanionStore::open(&dir.path().join("companion.db")).unwrap());
        let log = Arc::new(SafetyLog::open(&dir.path().join("companion.db")).unwrap());
        let history = Arc::new(HistoryStore::open(&dir.path().join("history.db")).unwrap());
        let floor = SafetyFloor {
            log,
            store,
            history: Some(Arc::clone(&history)),
            auth: None,
            notifications: None,
            groups: None,
        };
        (dir, floor, history)
    }

    // Variant that wires in a real CompanionGroupStore — used by
    // tests covering the group bridge.
    fn fresh_setup_with_groups() -> (
        tempfile::TempDir, SafetyFloor, Arc<HistoryStore>,
        Arc<CompanionGroupStore>,
    ) {
        let dir = tempdir().unwrap();
        let store = Arc::new(CompanionStore::open(&dir.path().join("companion.db")).unwrap());
        let log = Arc::new(SafetyLog::open(&dir.path().join("companion.db")).unwrap());
        let groups = Arc::new(CompanionGroupStore::open(&dir.path().join("companion.db")).unwrap());
        let history = Arc::new(HistoryStore::open(&dir.path().join("history.db")).unwrap());
        let floor = SafetyFloor {
            log,
            store,
            history: Some(Arc::clone(&history)),
            auth: None,
            notifications: None,
            groups: Some(Arc::clone(&groups)),
        };
        (dir, floor, history, groups)
    }

    fn enable_user(floor: &SafetyFloor, user_id: &str, contact: &str) {
        let now = Utc::now();
        let s = CompanionSettings {
            user_id: user_id.into(),
            enabled: true,
            paused_until: None,
            quiet_hours: vec![],
            preferred_channels: vec![],
            safety_contact_user_id: Some(contact.into()),
            setup_completed_at: Some(now),
            last_checkin_at: None,
            consecutive_missed_checkins: 0,
            daily_briefing_enabled: false,
            daily_briefing_hour: 7,
            last_briefing_at: None,
            cadence: Default::default(),
            presence: Default::default(),
            created_at: now,
            updated_at: now,
        };
        floor.store.upsert(&s).unwrap();
    }

    #[tokio::test]
    async fn handle_distress_delivers_to_contact_thread() {
        let (_dir, floor, history) = fresh_setup();
        enable_user(&floor, "alice", "david");
        let outcome = floor.handle_distress("alice", "user mentioned feeling overwhelmed").await;
        assert_eq!(outcome, EscalationOutcome::Delivered);

        // David's web history should now contain a "Safety alerts" thread
        // with an assistant message.
        let convs = history.list_conversations("david", Some("web"), 10, 0).unwrap();
        let safety_conv = convs.iter()
            .find(|c| c.title.as_deref() == Some("Safety alerts"))
            .expect("expected Safety alerts thread for david");
        let msgs = history.get_messages(&safety_conv.id, 10, None).unwrap();
        assert!(msgs.iter().any(|m| m.content.contains("distress")));

        // Audit log has the row.
        let evs = floor.log.list_recent_for_user("alice", 10).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].outcome, EscalationOutcome::Delivered);
        assert_eq!(evs[0].contact_user_id.as_deref(), Some("david"));
    }

    #[tokio::test]
    async fn handle_distress_no_contact_returns_no_contact() {
        let (_dir, floor, _hist) = fresh_setup();
        // Enable without configuring contact: not possible via the
        // normal facade (it requires safety_contact), but we can
        // upsert directly to simulate the "contact removed" state.
        let now = Utc::now();
        floor.store.upsert(&CompanionSettings {
            user_id: "alice".into(),
            enabled: true,
            paused_until: None,
            quiet_hours: vec![],
            preferred_channels: vec![],
            safety_contact_user_id: None,
            setup_completed_at: Some(now),
            last_checkin_at: None,
            consecutive_missed_checkins: 0,
            daily_briefing_enabled: false,
            daily_briefing_hour: 7,
            last_briefing_at: None,
            cadence: Default::default(),
            presence: Default::default(),
            created_at: now,
            updated_at: now,
        }).unwrap();

        let outcome = floor.handle_distress("alice", "summary").await;
        assert_eq!(outcome, EscalationOutcome::NoContact);

        let evs = floor.log.list_recent_for_user("alice", 10).unwrap();
        assert_eq!(evs[0].outcome, EscalationOutcome::NoContact);
        assert_eq!(evs[0].contact_user_id, None);
    }

    #[tokio::test]
    async fn handle_distress_dedups_within_window() {
        let (_dir, floor, _hist) = fresh_setup();
        enable_user(&floor, "alice", "david");

        // First delivery
        let o1 = floor.handle_distress("alice", "first signal").await;
        assert_eq!(o1, EscalationOutcome::Delivered);
        // Immediate second signal → suppressed
        let o2 = floor.handle_distress("alice", "second signal moments later").await;
        assert_eq!(o2, EscalationOutcome::Suppressed);

        let evs = floor.log.list_recent_for_user("alice", 10).unwrap();
        assert_eq!(evs.len(), 2);
        // Newest first per list_recent_for_user
        assert_eq!(evs[0].outcome, EscalationOutcome::Suppressed);
        assert_eq!(evs[1].outcome, EscalationOutcome::Delivered);
    }

    #[tokio::test]
    async fn handle_missed_checkins_delivers_with_count_in_body() {
        let (_dir, floor, history) = fresh_setup();
        enable_user(&floor, "alice", "david");
        let outcome = floor.handle_missed_checkins("alice", 3).await;
        assert_eq!(outcome, EscalationOutcome::Delivered);

        let convs = history.list_conversations("david", Some("web"), 10, 0).unwrap();
        let conv = convs.iter().find(|c| c.title.as_deref() == Some("Safety alerts")).unwrap();
        let msgs = history.get_messages(&conv.id, 10, None).unwrap();
        let body = &msgs[0].content;
        assert!(body.contains("last 3 check-ins"),
            "expected 'last 3 check-ins' in:\n{body}");
    }

    #[tokio::test]
    async fn missed_checkins_and_distress_share_safety_thread() {
        let (_dir, floor, history) = fresh_setup();
        enable_user(&floor, "alice", "david");
        floor.handle_distress("alice", "signal").await;
        floor.handle_missed_checkins("alice", 3).await;

        // Both events should land in the SAME thread.
        let convs = history.list_conversations("david", Some("web"), 10, 0).unwrap();
        let safety_threads: Vec<_> = convs.iter()
            .filter(|c| c.title.as_deref() == Some("Safety alerts"))
            .collect();
        assert_eq!(safety_threads.len(), 1, "alerts should share one thread");
        let msgs = history.get_messages(&safety_threads[0].id, 10, None).unwrap();
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn safety_addendum_mentions_methods_refusal_and_resources() {
        // Smoke check on the constant — it's part of the public
        // contract.
        assert!(SAFETY_ADDENDUM.contains("Never describe methods"));
        assert!(SAFETY_ADDENDUM.contains("crisis line"));
        assert!(SAFETY_ADDENDUM.contains("non-overridable"));
    }

    // ── group bridge ──────────────────────────────────────────

    use crate::companion::groups::{
        CompanionGroupStore, GroupCompanionMember, GroupCompanionPolicy, SignalKind,
    };

    fn make_policy(group_id: &str, allowed: Vec<SignalKind>, privacy: Vec<&str>)
        -> GroupCompanionPolicy
    {
        let now = Utc::now();
        GroupCompanionPolicy {
            group_id: group_id.into(),
            allowed_signals: allowed,
            privacy_topics: privacy.iter().map(|s| s.to_string()).collect(),
            created_at: now,
            updated_at: now,
        }
    }

    fn make_member(group_id: &str, uid: &str, opt_in: bool, contactable: Vec<SignalKind>)
        -> GroupCompanionMember
    {
        let now = Utc::now();
        GroupCompanionMember {
            group_id: group_id.into(),
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

    #[tokio::test]
    async fn group_bridge_delivers_distress_to_opted_in_members_only() {
        let (_dir, floor, history, groups) = fresh_setup_with_groups();
        enable_user(&floor, "alice", "david");
        groups.upsert_policy(&make_policy("family", vec![SignalKind::Distress], vec![])).unwrap();
        // Alice (sender), David (opted in), Sarah (NOT opted in).
        groups.upsert_member(&make_member("family", "alice", true, vec![SignalKind::Distress])).unwrap();
        groups.upsert_member(&make_member("family", "david", true, vec![SignalKind::Distress])).unwrap();
        groups.upsert_member(&make_member("family", "sarah", false, vec![SignalKind::Distress])).unwrap();

        floor.handle_distress("alice", "user mentioned feeling low").await;

        // David got TWO threads written: one from the single-contact
        // path + one from the group bridge.
        // Both land in the same "Safety alerts" thread so they
        // collapse — verify the thread exists and has two messages.
        let convs = history.list_conversations("david", Some("web"), 10, 0).unwrap();
        let safety = convs.iter()
            .find(|c| c.title.as_deref() == Some("Safety alerts"))
            .expect("expected Safety alerts thread for david");
        let msgs = history.get_messages(&safety.id, 20, None).unwrap();
        assert!(msgs.len() >= 2,
            "expected single-contact + group notices in same thread, got {} messages", msgs.len());

        // Sarah is NOT opted in — should have NO Safety alerts thread.
        let sarah_convs = history.list_conversations("sarah", Some("web"), 10, 0).unwrap();
        assert!(!sarah_convs.iter().any(|c| c.title.as_deref() == Some("Safety alerts")),
            "Sarah is not opted in — should not have received a notice");
    }

    #[tokio::test]
    async fn group_bridge_routes_to_multiple_members() {
        let (_dir, floor, history, groups) = fresh_setup_with_groups();
        enable_user(&floor, "alice", "david");
        groups.upsert_policy(&make_policy("family", vec![SignalKind::Distress], vec![])).unwrap();
        // Two opted-in non-sender members.
        groups.upsert_member(&make_member("family", "alice", true, vec![SignalKind::Distress])).unwrap();
        groups.upsert_member(&make_member("family", "david", true, vec![SignalKind::Distress])).unwrap();
        groups.upsert_member(&make_member("family", "sarah", true, vec![SignalKind::Distress])).unwrap();

        floor.handle_distress("alice", "summary").await;

        for uid in &["david", "sarah"] {
            let convs = history.list_conversations(uid, Some("web"), 10, 0).unwrap();
            assert!(convs.iter().any(|c| c.title.as_deref() == Some("Safety alerts")),
                "{uid} should have received a notice");
        }
    }

    #[tokio::test]
    async fn group_bridge_sender_never_self_notifies() {
        let (_dir, floor, history, groups) = fresh_setup_with_groups();
        enable_user(&floor, "alice", "david");
        groups.upsert_policy(&make_policy("family", vec![SignalKind::Distress], vec![])).unwrap();
        // Make the sender opted in for the signal too — they should
        // STILL be excluded.
        groups.upsert_member(&make_member("family", "alice", true, vec![SignalKind::Distress])).unwrap();

        floor.handle_distress("alice", "summary").await;

        // Alice's own history must NOT have a Safety alerts thread
        // from the group bridge. (The single-contact path notifies
        // david, not alice, so alice has nothing either way.)
        let alice_convs = history.list_conversations("alice", Some("web"), 10, 0).unwrap();
        assert!(!alice_convs.iter().any(|c| c.title.as_deref() == Some("Safety alerts")),
            "Alice is the sender — never self-notify");
    }

    #[tokio::test]
    async fn group_bridge_audit_records_suppressed_skip_reasons() {
        let (_dir, floor, _hist, groups) = fresh_setup_with_groups();
        enable_user(&floor, "alice", "david");
        groups.upsert_policy(&make_policy("family", vec![SignalKind::Distress], vec![])).unwrap();
        groups.upsert_member(&make_member("family", "alice", true, vec![SignalKind::Distress])).unwrap();
        // Sarah opted in but isn't contactable for Distress.
        groups.upsert_member(&make_member("family", "sarah", true, vec![SignalKind::MissedCheckin])).unwrap();

        floor.handle_distress("alice", "summary").await;

        let evs = floor.log.list_recent_for_user("alice", 20).unwrap();
        // Should include a Suppressed row for sarah with the
        // NotContactable reason in the note.
        let sarah_suppressed = evs.iter().find(|e|
            e.contact_user_id.as_deref() == Some("sarah")
            && matches!(e.outcome, EscalationOutcome::Suppressed)
        );
        assert!(sarah_suppressed.is_some(),
            "expected a Suppressed audit row for sarah; got: {:?}", evs);
        assert!(sarah_suppressed.unwrap().note.as_ref()
            .map(|n| n.contains("NotContactable")).unwrap_or(false));
    }

    #[tokio::test]
    async fn group_bridge_skips_group_policy_disallowed_signals() {
        let (_dir, floor, _hist, groups) = fresh_setup_with_groups();
        enable_user(&floor, "alice", "david");
        // Group only allows MissedCheckin — Distress not in policy.
        groups.upsert_policy(&make_policy("family", vec![SignalKind::MissedCheckin], vec![])).unwrap();
        groups.upsert_member(&make_member("family", "alice", true, vec![SignalKind::Distress])).unwrap();
        groups.upsert_member(&make_member("family", "david", true, vec![SignalKind::Distress])).unwrap();

        floor.handle_distress("alice", "summary").await;

        // David got the single-contact notice (1 audit row Delivered)
        // and a NotInGroupPolicy suppression (1 audit row Suppressed).
        let evs = floor.log.list_recent_for_user("alice", 20).unwrap();
        let suppressions: Vec<_> = evs.iter()
            .filter(|e| e.contact_user_id.as_deref() == Some("david")
                && matches!(e.outcome, EscalationOutcome::Suppressed))
            .collect();
        assert!(!suppressions.is_empty());
        assert!(suppressions[0].note.as_ref()
            .map(|n| n.contains("NotInGroupPolicy")).unwrap_or(false));
    }

    #[test]
    fn clip_truncates_long_input() {
        let s = "x".repeat(500);
        let out = clip(&s);
        assert!(out.chars().count() <= 241);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn clip_leaves_short_alone() {
        assert_eq!(clip("hi"), "hi");
    }
}
