// SPDX-License-Identifier: AGPL-3.0-or-later

// src/companion/chitchat.rs
//! Per-turn intent classifier — is this user message a *task* (set
//! a reminder, what's the weather, find me a recipe) or a *chat*
//! (hi, bit cold today, did you watch the test)?
//!
//! For companion-enabled users, a `Chat` classification tells the
//! pre-hook to append a "casual conversation mode" addendum to the
//! system prompt so the model responds conversationally instead of
//! jumping to tools.
//!
//! v1 (this slice) uses heuristics only — cheap, deterministic, no
//! extra LLM call per turn. The classifier is conservative: when in
//! doubt it returns `Task` (the default chat behaviour), which means
//! a missed `Chat` produces normal MIRA behaviour rather than missed
//! tool execution. A future slice can add an LLM tie-break for
//! ambiguous cases if the heuristic proves too brittle.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    /// Casual conversation — bias the model toward conversational
    /// response. No tool calls expected; keep it warm + brief.
    Chat,
    /// Task / question / instruction — normal MIRA behaviour, tools
    /// allowed, structured response expected.
    Task,
}

impl Intent {
    pub fn is_chat(self) -> bool { matches!(self, Intent::Chat) }
}

/// Classify a user message. The result is advisory — the model is
/// always free to respond however it sees fit. The classification
/// only changes which addendum (if any) the pre-hook appends.
pub fn classify(user_message: &str) -> Intent {
    let trimmed = user_message.trim();
    if trimmed.is_empty() { return Intent::Task; }

    // Universal Task signals — these always win, even when there's a
    // greeting opener. A question mark anywhere, an imperative at the
    // start (or right after a greeting), or a strong task phrase
    // ("send signal", "remind me", ...) override conversational tone.
    let rest_after_greeting = strip_leading_greeting(trimmed);
    let has_task_signal = contains_question_mark(trimmed)
        || starts_imperative(trimmed)
        || starts_imperative(rest_after_greeting)
        || contains_strong_task_signal(trimmed);
    if has_task_signal {
        return Intent::Task;
    }

    // Greeting opener with no task signal → Chat.
    if has_greeting_opener(trimmed) {
        return Intent::Chat;
    }

    // Short ambient remark → Chat. "Bit cold today", "not much
    // going on", "yeah ok". Strong-task-signal check above already
    // caught the brief commands.
    let word_count = trimmed.split_whitespace().count();
    if word_count <= SHORT_MESSAGE_WORDS {
        return Intent::Chat;
    }

    // Default — Task. Errs toward normal MIRA behaviour so a
    // miss-classified Chat just looks like regular conversation.
    Intent::Task
}

// ── Heuristic primitives ─────────────────────────────────────────────────────

const SHORT_MESSAGE_WORDS: usize = 6;

const GREETING_OPENERS: &[&str] = &[
    // English
    "hi", "hello", "hey", "morning", "afternoon", "evening", "yo",
    "g'day", "gday", "howdy", "sup",
    // Common compound openers
    "good morning", "good afternoon", "good evening", "good night",
    "how are you", "how's it going", "hows it going", "how you doing",
    "hows things",
];

/// Imperative verbs that almost always indicate a task / tool call.
/// Conservative list — anything not here just falls through to the
/// length-based heuristic.
const IMPERATIVE_VERB_OPENERS: &[&str] = &[
    "add", "set", "create", "schedule", "send", "search", "find",
    "look", "lookup", "show", "list", "open", "close", "delete",
    "remove", "remind", "reminders", "reminder",
    "tell me", "give me",
    // Tools likely to be name-checked
    "summarise", "summarize", "translate",
];

/// Strong tokens that contradict a "short ambient remark" reading
/// even when the message is brief. Lets a 4-word command like
/// "send signal to david" still classify as Task.
const STRONG_TASK_SIGNALS: &[&str] = &[
    "send signal", "send telegram", "send email",
    "remind me", "schedule a",
];

/// Strip a leading greeting + any trailing punctuation/whitespace
/// so the rest of the message can be checked for task signals.
/// E.g. `"hey set a reminder"` → `"set a reminder"`.
fn strip_leading_greeting(s: &str) -> &str {
    let lower = s.to_ascii_lowercase();
    let lower_trim = lower.trim_start_matches(|c: char| !c.is_alphabetic());
    let offset = s.len() - lower_trim.len();
    for g in GREETING_OPENERS {
        if let Some(rest_lower) = lower_trim.strip_prefix(g) {
            let boundary_ok = rest_lower.is_empty()
                || rest_lower.starts_with(|c: char| !c.is_alphanumeric());
            if !boundary_ok { continue; }
            // Compute the byte offset in the original string. Since
            // ASCII case-folding preserves byte positions for the
            // greeting words (all ASCII), this is safe.
            let cut = offset + g.len();
            let after = &s[cut..];
            return after.trim_start_matches(|c: char| !c.is_alphanumeric());
        }
    }
    s
}

fn has_greeting_opener(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    let lower = lower.trim_start_matches(|c: char| !c.is_alphabetic());
    GREETING_OPENERS.iter().any(|g| {
        // Match as whole-prefix only (so "highlight" doesn't match "hi").
        if let Some(rest) = lower.strip_prefix(g) {
            rest.is_empty() || rest.starts_with(|c: char| !c.is_alphanumeric())
        } else {
            false
        }
    })
}

fn contains_question_mark(s: &str) -> bool {
    s.contains('?')
}

fn starts_imperative(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    IMPERATIVE_VERB_OPENERS.iter().any(|v| {
        if let Some(rest) = lower.strip_prefix(v) {
            rest.is_empty() || rest.starts_with(|c: char| !c.is_alphanumeric())
        } else {
            false
        }
    })
}

fn contains_strong_task_signal(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    STRONG_TASK_SIGNALS.iter().any(|sig| lower.contains(sig))
}

/// System-prompt addendum injected when the message classifies as
/// `Chat` AND companion mode is active for the user. Kept short —
/// the wiki persona doc (`pages/companion/style.md`) is the
/// long-form guidance; this is just a turn-level nudge.
pub fn casual_mode_addendum() -> &'static str {
    "\n\n## Conversation mode\n\
     This message reads as casual conversation, not a task. Respond \
     conversationally — share something, ask a gentle follow-up, let \
     the conversation breathe. Don't jump to tool calls. Follow the \
     style notes in your wiki pages/companion/style.md (already \
     loaded above)."
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(s: &str) {
        assert_eq!(classify(s), Intent::Chat, "expected Chat for: {s:?}");
    }
    fn task(s: &str) {
        assert_eq!(classify(s), Intent::Task, "expected Task for: {s:?}");
    }

    // ── Chat cases ────────────────────────────────────────────────

    #[test]
    fn bare_greetings_are_chat() {
        chat("hi");
        chat("hello");
        chat("hey");
        chat("morning");
        chat("good morning");
        chat("g'day");
    }

    #[test]
    fn casual_remarks_are_chat() {
        chat("bit cold today");
        chat("not much going on");
        chat("just thinking about Rose");
        chat("watched the test last night");
        chat("yeah ok");
    }

    #[test]
    fn one_word_replies_are_chat() {
        chat("ok");
        chat("yeah");
        chat("nope");
        chat("alright");
    }

    // ── Task cases ────────────────────────────────────────────────

    #[test]
    fn questions_are_task() {
        task("what's the weather tomorrow?");
        task("hi can you set a reminder?");
        task("how are you doing today even?"); // Q-mark wins over greeting
    }

    #[test]
    fn imperative_openers_are_task() {
        task("set a reminder for 3pm");
        task("add fish curry to my recipes");
        task("find me a good cricket podcast");
        task("send a message to David");
        task("schedule a check-in at 10am");
    }

    #[test]
    fn long_messages_are_task() {
        task("here's a list of things I want to remember for my doctor's appointment next Tuesday morning before noon");
    }

    #[test]
    fn empty_input_falls_back_to_task() {
        task("");
        task("   \t  ");
    }

    #[test]
    fn short_message_with_task_signal_stays_task() {
        // 4 words but "send signal" is a strong task signal.
        task("send signal to david");
        task("remind me to call mum");
    }

    // ── Boundary cases ────────────────────────────────────────────

    #[test]
    fn greeting_followed_by_question_is_task() {
        task("hi how's the weather?");
        task("hello can you add a recipe?");
    }

    #[test]
    fn greeting_followed_by_imperative_is_task() {
        task("hey set a reminder");
        task("morning add this to my recipes");
    }

    #[test]
    fn greeting_followed_by_casual_chat_is_chat() {
        // No imperative, no question — still Chat.
        chat("hi just thinking about you");
        chat("hey not much");
    }

    #[test]
    fn substring_does_not_trigger_greeting_match() {
        // "highlight" shouldn't match "hi" as a greeting opener.
        // It IS short enough to trigger the length heuristic →
        // Chat. Adjust the test to verify the GREETING path
        // specifically doesn't fire (we approximate by checking the
        // helper directly).
        assert!(!has_greeting_opener("highlight that for me"));
    }

    #[test]
    fn imperative_match_is_word_boundary() {
        // "address" starts with "add" but shouldn't match the
        // imperative.
        assert!(!starts_imperative("address the package to David"));
        // But "add" alone or "add this" should:
        assert!(starts_imperative("add this"));
        assert!(starts_imperative("add"));
    }

    #[test]
    fn case_insensitive() {
        chat("HI");
        chat("Morning");
        task("WHAT'S THE WEATHER?");
        task("SET A REMINDER");
    }

    #[test]
    fn leading_punctuation_ok() {
        chat("…hi");
        chat("- hello");
    }

    #[test]
    fn intent_is_chat_helper() {
        assert!(Intent::Chat.is_chat());
        assert!(!Intent::Task.is_chat());
    }

    #[test]
    fn casual_mode_addendum_is_non_empty_and_mentions_style_md() {
        let s = casual_mode_addendum();
        assert!(!s.is_empty());
        assert!(s.contains("style.md"));
    }
}
