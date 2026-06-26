---
title: Presence & proactive check-ins
description: Presence — MIRA's proactive companion mode. Make MIRA reach out first with periodic companion check-ins, a morning briefing, and a safety contact for missed check-ins.
sidebar:
  order: 2
---

Most assistants only answer when asked. MIRA can **start the conversation** —
a periodic check-in to see how you're doing, or a morning briefing built from
your calendar, wiki, and recent activity. This guide turns those on.

Both features are **per-user** and **opt-in**, and both need a connected
channel so MIRA has somewhere to reach you. If you haven't set one up yet, do
that first: see **[Connect a channel](connect-a-channel.md)**. Telegram is the
quickest to start with.

You **tune** companion behaviour on the **Settings → Presence** page (rhythm,
personality, what MIRA sends) — or just by asking MIRA in chat. **Enabling**
check-ins and the test buttons live under **Settings → Notifications**. (When
you finish onboarding, MIRA already configures Presence from your answers —
admins are switched on automatically; others enable once a safety contact is
set.)

## Turn on companion check-ins

Companion mode is what lets MIRA message you on its own — gentle, periodic
check-ins rather than replies to your prompts. It's designed to feel like a
friend who knows you, not an alarm clock: MIRA varies its timing, learns when
you tend to reply, and backs off when you're brief or busy.

1. Go to **Settings → Notifications** and **enable companion mode** for your
   account.
2. **Choose a preferred channel** for the check-ins (for example Telegram or
   Signal). This is where the proactive messages will arrive.
3. **Set your quiet hours** — the windows when MIRA must never message you (for
   example overnight, or during a regular nap). MIRA respects these absolutely;
   it won't reach out inside a quiet window no matter what.

That's enough to start. MIRA picks varied times within your allowed windows,
skips a check-in if you've just been chatting, and adjusts how often it reaches
out based on whether you tend to engage.

> Companion mode is usually set up **for** someone — an admin or family member
> configures it on the person's behalf — but you can also enable it for
> yourself. Either way it's strictly per-user: people without it enabled get
> normal MIRA behaviour.

### Tune the rhythm (Settings → Presence)

MIRA's timing is a **fuzzy band**, not a fixed schedule:

- **Messages per day** — a range like "1–4 a day". MIRA picks a count in that
  band each day and scatters them at **varied, non-round times** inside your
  contactable hours (each at least a minimum gap apart). *"Message me once or
  twice a day."*
- **Minimum gap** — the least time between two reach-outs (default 90 minutes).
- It **leans in** when you're engaging and **backs off** when you're brief or
  not replying — and **pauses after a few unanswered**, resuming the moment you
  reply, so MIRA never talks into the void.

Prefer predictable times instead? Switch the rhythm to **Scheduled** and give it
fixed times (e.g. 09:00 and 18:00). Everything is also adjustable in chat —
*"only check in in the mornings"*, *"leave more time between messages"*.

### Give MIRA personality (what it sends, and how)

On the Presence page you also shape *what* a reach-out is and *how it sounds*:

- **Message types** — toggle which kinds MIRA may send: a **check-in**, a
  **joke**, **"what I've been up to"** (it'll genuinely mention what its
  background agents did for you), a **follow-up** on something you recently
  discussed, a **share**, or a bit of **encouragement**. MIRA picks one per
  reach-out, biased by context.
- **Tone** — three sliders (warmth / playfulness / verbosity) with quick presets
  ("Warm & chatty", "Calm & concise", "Playful", "Professional"); for deeper
  voice, edit MIRA's **persona** wiki page.
- Or just say it: *"be funnier"*, *"stop the jokes"*, *"keep it short"*.

Each person's settings override the instance-wide defaults an admin sets in the
`companion` config block. The daily briefing is separate and isn't affected by
these limits.

## The care network

Presence can be more than friendly company: it can be a quiet **wellbeing net**
for someone you look out for — a child, or an older parent living
independently. If the person seems to be having a hard time, or simply goes
quiet, MIRA gives a trusted contact a gentle heads-up so a real human can check
in. You set this up on the **Settings → Presence** page under **Care network**.

It's built on four principles:

- **Never covert.** MIRA always tells the person, in plain language, that it's
  looking out for them and may give their contact a heads-up. The first time a
  care arrangement is active, MIRA works this disclosure naturally into a
  check-in. The "the person knows" acknowledgement on the Presence page records
  that this has happened.
- **Concern, not tattling.** Only genuine signals escalate — clear distress, or
  a run of unanswered check-ins. An ordinary off day doesn't trigger anything;
  MIRA just responds warmly. Repeat alerts for the same signal are suppressed.
- **A heads-up, not 911.** The contact gets a short, factual note — "you might
  want to check in" — so a person who knows them can follow up. MIRA escalates
  to that human; it does **not** call emergency services. In parallel, MIRA
  responds to the person warmly and surfaces crisis resources to *them*.
- **Minimal disclosure.** The contact sees a one- or two-sentence summary of the
  signal and the person's name — never the full conversation.

### Choose a care role

On the Presence page, pick who Presence is for:

- **Just me** — a companion for yourself; no one else is alerted (the default).
- **A child** — a guardian is alerted if their child seems to be struggling;
  MIRA keeps a gentle, age-aware tone.
- **An older adult** — a contact is alerted on silence or signs of distress — a
  light-touch wellbeing check for someone living more independently.

For a child or older-adult role, choose the **contact to alert** and confirm the
person has been told. How serious a signal is tunes the message: an *acute*
signal (mentions of self-harm, or acute physical symptoms) sends an urgent
heads-up and shows the person crisis resources prominently; a *concerning* one
(a low mood, loneliness) sends a softer "you might want to check in".

### Set a safety contact

When you configure check-ins, set a **safety contact**: the person MIRA notifies
if check-ins go unanswered (three in a row over 48 hours) or if a message reads
as distress. (Choosing a care role above sets this same contact.)

The safety contact is a hard prerequisite for ordinary user accounts — you
can't enable companion check-ins without one — because a feature that holds
daily conversations with a vulnerable person needs somewhere to turn when
something's wrong.

## Turn on the daily briefing

The daily briefing is a single morning summary, assembled from:

- your **calendar** — what's coming up today,
- recent **wiki** updates — notes you've added or changed,
- recent **activity** — automation runs and the like.

To enable it:

1. Under **Settings → Notifications**, **enable the daily briefing**.
2. **Set the hour** it should arrive (for example 7 for 7am). You can change it
   any time — or just ask MIRA: *"set my briefing hour to 7"*.

The briefing arrives on your connected channel. If your host was asleep or
restarting at the scheduled time, the briefing catches up later the same day.

## Test it right now

You don't have to wait for the next scheduled moment to confirm everything
works. Under **Settings → Notifications** there are two test buttons:

- **Send a check-in now** — fires a companion check-in immediately.
- **Send a briefing now** — builds and sends today's briefing immediately.

Use these to confirm the message actually reaches your chosen channel before
relying on the schedule.

## Troubleshooting

- **Nothing arrives.** Confirm you have a [connected channel](connect-a-channel.md)
  and that it's set as your preferred channel for notifications. On Telegram,
  MIRA can only message you proactively after you've **linked your chat**
  (Settings → My channels → Link Telegram, then send the `LINK-XXXX-XXXX` code to
  the bot) — that's how it captures your chat id.
- **A check-in never came.** Check-ins and briefings only fire while MIRA is
  **running** at the scheduled moment. If the host was asleep or restarting,
  that window can be missed — the briefing catches up later the same day, but
  check-ins don't replay.
- **I want to pause for a while.** Just tell MIRA — for example *"pause the
  check-ins for the weekend"*. It accepts the request without pushing back.

## Related

- [Connect a channel](connect-a-channel.md) — set up where MIRA reaches you.
- [Voice replies & talking to MIRA](voice-replies.md) — have check-ins and
  replies arrive as voice notes.
- [Channels](../concepts/channels.md) — how MIRA's channel model works.
