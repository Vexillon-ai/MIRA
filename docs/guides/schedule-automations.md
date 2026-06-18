---
title: Schedule automations
description: Make MIRA do things on a schedule or in response to events — reminders, daily summaries, and recurring tasks.
sidebar:
  order: 4
---

An **automation** is a standing instruction: MIRA does something on a schedule or
when an event happens, without you asking each time. A reminder every morning, a
weekly review, a summary delivered to your phone — all automations.

The easiest way to create one is to **just ask in chat**. This guide covers that,
plus how to view and cancel what you've set up.

## Create one by asking

Tell MIRA what to do and when, in plain language:

> Remind me to take a break every 2 hours on weekdays.

> Every morning at 8, summarise my calendar for the day and message me on
> Telegram.

> Every Sunday at 6pm, help me plan the week ahead.

MIRA turns that into a **cron-scheduled automation** — a recurring action with a
time spec it works out from your phrasing ("every morning at 8", "every Monday",
"every 2 hours"). It runs in your timezone and respects your quiet hours where
they apply.

You don't need to learn cron syntax. If you want precise control, the
**Automations** page (below) lets you pick presets or enter a custom schedule and
previews the next few fire times.

## Two kinds of automation

- **Cron-scheduled** — runs on a clock: "daily at 9", "every 2 hours", "every
  Sunday". This is what most reminders and recurring summaries become.
- **Event-triggered** — runs in response to something happening, such as an
  inbound webhook from another service (a calendar reminder, a home-automation
  event, a button on your phone). The action fires when the event arrives rather
  than on a clock.

Both end up doing the same kinds of thing: send you a message, run a prompt
through the agent, call a tool, or post to an outbound webhook.

## Deliver the result to a channel

An automation can deliver its result wherever you want to receive it. A morning
briefing can land in **Telegram**, a reminder can come by **Signal**, a weekly
review can arrive by **email**. Name the channel when you ask ("message me on
Signal…"), and make sure that channel is connected first — see
[Connect a channel](connect-a-channel.md).

This is the same machinery behind MIRA's [proactive check-ins and daily
briefing](proactive-checkins-and-briefing.md); those are automations MIRA ships
with, and the ones you create work the same way.

## View and cancel your automations

Open the **Automations** page from the sidebar to see everything you've set up:
its schedule, when it last ran, when it runs next, and its current status. From
there you can **run it now**, **pause**, **snooze**, or **delete** it. Each
automation also keeps a **run history** so you can confirm it fired and see what
it did.

You can manage them conversationally too:

> What automations do I have?

> Cancel the water-break reminder.

MIRA lists and cancels schedules through its automation tools, so you never have
to leave the chat if you don't want to.

## Recipes

- **Daily briefing on your phone** — *"Every weekday at 7:30, give me a briefing
  with my calendar and anything new in my wiki, and send it to Telegram."*
- **Recurring nudge** — *"Remind me to log my expenses every Friday at 5pm."*
- **Weekly review** — *"Every Sunday evening, review the past week's
  conversations and suggest three priorities for next week."*
- **One-off follow-up** — *"Check in with me about the dentist appointment
  tomorrow morning."*

## Next steps

- **[Proactive check-ins & briefing](proactive-checkins-and-briefing.md)** — the
  built-in automations and how to tune them.
- **[Connect a channel](connect-a-channel.md)** — so results can reach you off
  the web UI.
- **[Named agents & workflows](named-agents-and-workflows.md)** — for
  multi-step tasks you want MIRA to run end to end.
