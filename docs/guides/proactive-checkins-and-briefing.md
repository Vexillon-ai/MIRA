---
title: Proactive check-ins & daily briefing
description: Make MIRA reach out first — periodic companion check-ins, a morning briefing, and a safety contact for missed check-ins.
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

### Set a safety contact

Companion mode is often used as a wellbeing or caregiver feature — for example,
keeping a parent who lives alone in low-stakes daily contact. For that reason
it can escalate to a designated human.

When you configure check-ins, set a **safety contact**: a person MIRA notifies
if check-ins go unanswered (three in a row over 48 hours) or if a message reads
as distress. The escalation is short and factual — a heads-up so a real person
can follow up. MIRA escalates to that human; it does **not** call emergency
services on your behalf.

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
