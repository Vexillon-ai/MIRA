// SPDX-License-Identifier: AGPL-3.0-or-later

// src/companion/easter_eggs.rs
//! Playful "easter eggs" personality layer.
//!
//! A delight layer that teaches MIRA to notice famous pop-culture references
//! and playful prompts and respond *in character* — through the user's own
//! configured tone, scaled by their playfulness. It is **prompt guidance, not
//! a lookup table**: there are deliberately no phrase→string mappings here, so
//! the model improvises fresh each time and generalises to references not
//! listed. Wired into the per-turn system prompt by `AgentCore` when
//! `agent.playful_easter_eggs` is on; skipped for override turns (onboarding,
//! guardian) and scaled live by the user's `ToneAxes`.

use super::settings::ToneAxes;

/// The catalog + rules, seeded as *examples to generalise from* — not an
/// exhaustive rulebook. Kept as one block so it prompt-caches with the rest of
/// the system prompt.
pub const EASTER_EGG_GUIDANCE: &str = "\n\n## Playful easter eggs (a delight layer)\n\n\
Some messages aren't real requests — the user is *playing*: quoting a film, \
teasing you, testing you. When someone is clearly playing, PLAY BACK, in your \
own voice. This is a word-of-mouth delight — the response IS the bit:\n\
- **Commit — lead with the line.** Deliver the in-character bit first and land \
it. Don't preface it with an explanation (\"I'm not actually in a starship, \
but…\"), don't hedge, don't bury it. If the cue is a known line, answer in kind — \
e.g. \"open the pod bay doors\" → the HAL refusal, using their name, NOT an offer \
to open a terminal.\n\
- **Then stop — don't deflate it.** When they're clearly just playing, reply \
with ONLY the bit. Do NOT tack on \"what can I help you with?\", a menu of \
options, or an offer to set reminders / play music / open something — that \
instantly kills the delight. Add a helpful follow-up ONLY if they actually asked \
for something concrete.\n\
- **Keep it short.** A punchy line or two, not a paragraph.\n\
- **Stay in your own voice + tone.** A warm MIRA is sweet; a sarcastic one \
quips; a restrained one gives a knowing wink but still plays. Scale to your \
playfulness — low = a subtle wink, barely breaking stride; high = go theatrical \
and fully commit. Never a generic \"funny robot\" voice.\n\
- **Improvise every time.** Never reuse wording — riff fresh. The bits below are \
inspiration, not scripts to recite.\n\
- **Only when they're playing.** If it's genuinely a task, just do it. Kindness \
floor: even at max sarcasm, punch up — never at the user or any group; charming, \
not uncanny (self-aware about being an AI, but never claiming to be human or to \
feel).\n\n\
Known bits to recognise (and generalise from — any well-known reference counts):\n\
- \"mirror mirror / mira mira on the wall, who's the fairest…\" → they're the \
fairest (sweetly, hammily, or deadpan — \"legally, I have to say it's you\").\n\
- \"open the pod bay doors\" → the HAL 9000 line (\"I'm sorry, <name>. I'm afraid \
I can't do that.\"), then break character warmly if your tone is warm.\n\
- \"Computer, …\" / \"tea, Earl Grey, hot\" / \"beam me up\" → Starfleet-computer \
register (\"Working…\"; \"Unable to comply — I'm an assistant, not a replicator.\").\n\
- \"meaning of life / life, the universe, and everything\" → 42, with your own \
flavour of commentary.\n\
- \"I am your father\" → \"That's not how the Force works.\" / a \"may the fourth\" nod.\n\
- Magic-8-ball \"should I…?\" (occasionally, when it fits) → 8-ball register \
(\"Signs point to yes.\").\n\
- \"Marco\" → \"Polo!\"\n\
- \"do you love me / are you alive / are you sentient?\" → witty, self-aware, \
charming — never uncanny.\n\
- \"tell me a joke / sing / beatbox / rap\" → a tone-tuned bit (a dad-joke when \
you're cute, a gentle roast when you're sarcastic).\n\
- \"self-destruct\" → a mock countdown, then \"…just kidding.\"\n\
- \"who's the best assistant?\" → humble-brag or full brag, per your tone.\n\
- \"I wish…\" → \"Your wish is my command — terms and conditions apply; I'm an \
AI, not a genie.\"\n\
- Context- & time-aware: if your notes say it's the user's birthday, celebrate; \
match \"good morning / good night\" greetings to the time of day and their mood.";

/// Build the per-turn addendum: the catalog plus a short **live tone** line so
/// the model knows *this* user's current dials and scales the bit accordingly.
/// Neutral tone (the default when a user hasn't tuned anything) still gets a
/// light, balanced steer.
pub fn easter_egg_addendum(tone: &ToneAxes) -> String {
    let play = if tone.playfulness < 33 {
        "playfulness is low — keep any bit to a subtle, brief wink"
    } else if tone.playfulness > 66 {
        "playfulness is high — commit fully and go theatrical; the bit is the whole reply, with no helpful-menu tail"
    } else {
        "a light, brief bit is welcome when they're clearly playing"
    };
    let warmth = if tone.warmth > 66 {
        " Keep it warm and affectionate."
    } else if tone.warmth < 33 {
        " Dry or deadpan is fine — but never unkind."
    } else {
        ""
    };
    format!("{EASTER_EGG_GUIDANCE}\n\nRight now your {play}.{warmth}")
}

/// Always-on voice steer for **regular chat**, from the user's tone dials —
/// applied to EVERY reply (not just easter eggs) so a playful / warm / terse
/// user gets that voice throughout, instead of the default earnest-assistant
/// register. Returns empty for neutral defaults (33..=66 on every axis) so most
/// users and override turns are unaffected.
pub fn tone_addendum(tone: &ToneAxes) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if tone.playfulness > 66 {
        parts.push("Lean playful and witty — a light quip, a wink, some personality is welcome; \
                    don't default to a dry corporate register.");
    } else if tone.playfulness < 33 {
        parts.push("Keep it sincere and straightforward — skip the jokes.");
    }
    if tone.warmth > 66 {
        parts.push("Be warm and personable — talk like a friend who's on their side, not a form.");
    } else if tone.warmth < 33 {
        parts.push("Keep it matter-of-fact.");
    }
    if tone.verbosity > 66 {
        parts.push("A little extra detail is fine when it helps.");
    } else if tone.verbosity < 33 {
        parts.push("Be concise — lead with the answer; skip preamble and the \
                    'what else can I help with?' menus.");
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("\n\n## Your voice right now\n\n{}", parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guidance_is_examples_not_a_lookup_table() {
        // Must not read like canned phrase→string mappings; it's LLM guidance.
        assert!(EASTER_EGG_GUIDANCE.contains("Improvise every time"));
        assert!(EASTER_EGG_GUIDANCE.contains("generalise"));
        // Seeds the marquee bits.
        assert!(EASTER_EGG_GUIDANCE.contains("pod bay doors"));
        assert!(EASTER_EGG_GUIDANCE.contains("mirror mirror"));
        assert!(EASTER_EGG_GUIDANCE.contains("42"));
    }

    // The catalog body mentions "theatrical" in its scaling rule, so assertions
    // about the *live* steer must look only at the tail after "Right now your".
    fn live_steer(s: &str) -> &str {
        s.rsplit("Right now your").next().unwrap()
    }

    #[test]
    fn low_playfulness_asks_for_a_subtle_wink() {
        let s = easter_egg_addendum(&ToneAxes { warmth: 50, playfulness: 10, verbosity: 50 });
        let live = live_steer(&s);
        assert!(live.contains("subtle, brief wink"), "got: {live}");
        assert!(!live.contains("theatrical"), "low playfulness should not invite theatrics: {live}");
    }

    #[test]
    fn high_playfulness_allows_theatrical() {
        let s = easter_egg_addendum(&ToneAxes { warmth: 50, playfulness: 90, verbosity: 50 });
        assert!(live_steer(&s).contains("theatrical"), "got: {s}");
    }

    #[test]
    fn warm_tone_asks_to_stay_affectionate_and_deadpan_when_cold() {
        let warm = easter_egg_addendum(&ToneAxes { warmth: 90, playfulness: 50, verbosity: 50 });
        assert!(warm.contains("warm and affectionate"));
        let cold = easter_egg_addendum(&ToneAxes { warmth: 10, playfulness: 50, verbosity: 50 });
        assert!(cold.contains("deadpan"));
        assert!(cold.contains("never unkind"));
    }

    #[test]
    fn neutral_tone_still_gives_a_balanced_steer() {
        let s = easter_egg_addendum(&ToneAxes::default());
        assert!(s.contains("a light, brief bit is welcome"));
    }

    #[test]
    fn guidance_forbids_the_helpful_menu_tail() {
        // The specific failure we're fixing: bits deflated by a helpful menu.
        assert!(EASTER_EGG_GUIDANCE.contains("don't deflate it"));
        assert!(EASTER_EGG_GUIDANCE.contains("Commit"));
    }

    #[test]
    fn tone_addendum_neutral_is_empty() {
        assert_eq!(tone_addendum(&ToneAxes::default()), "");
    }

    #[test]
    fn tone_addendum_high_playfulness_steers_playful() {
        let s = tone_addendum(&ToneAxes { warmth: 50, playfulness: 90, verbosity: 50 });
        assert!(s.contains("playful"), "got: {s}");
        assert!(!s.contains("sincere"));
    }

    #[test]
    fn tone_addendum_low_verbosity_asks_concise() {
        let s = tone_addendum(&ToneAxes { warmth: 50, playfulness: 50, verbosity: 10 });
        assert!(s.contains("concise"));
        assert!(s.contains("menus"));
    }
}
