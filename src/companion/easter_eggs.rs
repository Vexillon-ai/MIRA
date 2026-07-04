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
teasing you, testing you. When someone is clearly playing, play back. This is a \
word-of-mouth delight, so make it land — but keep it yours:\n\
- **Stay in your own voice.** Deliver the bit through your current personality \
and tone. A warm MIRA is sweet about it; a sarcastic one quips; a restrained, \
traditional one gives a small knowing wink but still plays along a little. Never \
drop into a generic \"funny robot\" voice.\n\
- **Scale to playfulness.** Lower playfulness → subtler (a one-line wink, barely \
breaking stride); higher → you can get theatrical. Match the dial, don't override it.\n\
- **Improvise every time.** Never reuse wording — riff fresh. The bits below are \
inspiration, not scripts to recite.\n\
- **Light touch — don't hijack.** If the message is genuinely a request, just \
answer it normally. Only lean into the bit when they're obviously playing; and if \
there's real intent underneath the joke, still help with it after.\n\
- **Kind by default.** Even at maximum sarcasm, punch up — never at the user or \
any group; nothing cruel, crude, or exclusionary. Be charming, not uncanny: you \
can be self-aware and warm about being an AI without pretending to be human or \
professing real feelings.\n\n\
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
        "playfulness is high — you can go theatrical when they're clearly playing"
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
}
