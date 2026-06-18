---
title: Channels
description: How MIRA reaches you and you reach MIRA — the per-user channel model, the supported set, shared bots, and how channels make MIRA proactive.
sidebar:
  order: 3
---

A **channel** is any way MIRA can reach you, and any way you can reach MIRA. The
web chat is one channel; your phone messenger is another; email is a third. This
page explains the model behind them — what a channel is, how channels are scoped,
and why they're what make MIRA more than a web tab.

For the step-by-step of connecting one, see
**[Connect a channel](../guides/connect-a-channel.md)**.

## What a channel is

Think of a channel as a **two-way pipe** between you and the agent. Each channel
carries messages in both directions:

- **Inbound** — a message you send (a Telegram message, an email, a Signal
  voice note) becomes a conversation turn the agent answers.
- **Outbound** — the agent's reply, or a message it starts on its own, goes
  back out through the same channel.

Because every channel is two-way, MIRA can hold a real conversation on whichever
one suits the moment — and can *begin* one, not just respond.

## Channels are per-user

Channels belong to **people**, not to the server as a whole. Each person on a
MIRA instance connects their own — their own Telegram bot, their own email
mailbox, their own Signal number — under **Settings → Channels**. Your channels
are yours; another user's messages never cross into your conversations.

This is the same boundary that applies to memory and settings: a single MIRA
instance serves several people, each with their own accounts, data, and
channels. (See [Security & multi-user](security-and-multi-user.md).) An admin
can additionally restrict which channels a given user is allowed to use.

## The supported channels

MIRA ships with a broad built-in set, plus an extension point for anything else:

- **Web chat** — the built-in UI at the server's address. Streaming replies,
  attachments, and in-browser voice playback.
- **Telegram** — a bot token; the easiest channel to start with. Two-way text
  and voice notes.
- **Signal** — end-to-end encrypted, via `signal-cli`. Two-way text and voice
  notes.
- **Email** — a first-class channel: inbound mail from allowed senders becomes a
  conversation, and MIRA can email you (handy for daily briefings).
- **Discord** — a per-user (or shared) bot routed to your Discord channels.
- **Matrix** — a bot against any homeserver (matrix.org or self-hosted).
- **WhatsApp** — via the Meta WhatsApp Business Cloud API.
- **Slack** — via the Slack Events API.
- **Push** — browser and phone Web Push notifications, even with no tab open.
- **Custom channels** — *any* other messaging system, through the **Channel
  Provider Protocol** (see below).

The exact setup differs per platform, but each is added the same way — under
**Settings → Channels**, with that platform's credentials.

## Custom channels: the Channel Provider Protocol

You're not limited to the built-in set. MIRA can talk to any messaging system
through an external **provider** — a small process that bridges MIRA to that
system over the **Channel Provider Protocol (CPP)**, a signed-HTTP contract best
described as **"MCP for channels."**

You add an external channel account, MIRA exposes a webhook and mints signing
secrets, and the provider relays messages in both directions. A custom channel
inherits everything a built-in one has — message history, identity routing,
proactive delivery, and even voice (when the provider supports audio). Providers
can be written in any language and shipped separately, so new channels arrive
without rebuilding MIRA.

## Shared bots and identity linking

By default each user runs their own bot. But an admin can instead run **one
shared bot** — a single Telegram, Discord, Matrix, WhatsApp, Slack, or custom
bot that serves **many users**, routing each inbound message to the right MIRA
account.

This works through **identity linking**: a user generates a one-time
`LINK-XXXX-XXXX` code in their channel settings and sends it to the shared bot,
which binds that platform identity to their MIRA account. From then on, the
shared bot knows who's talking and keeps everyone's conversations, memory, and
settings separate — one bot, many private agents.

## Channels make MIRA proactive

Channels are also what let MIRA reach out **first**. Once a channel is connected,
MIRA can start conversations rather than only answer them:

- **Companion check-ins** — periodic, quiet-hours-aware messages.
- **Daily briefing** — a morning summary from your calendar, wiki, and activity.
- **Automations** — scheduled or event-triggered messages MIRA sends you.

Each of these picks a channel to deliver on (your preferred one, with
fallbacks), which is why a proactive feature is only as useful as the channel
behind it. See
[Proactive check-ins & daily briefing](../guides/proactive-checkins-and-briefing.md).

## Next steps

- **[Connect a channel](../guides/connect-a-channel.md)** — the how-to.
- **[Voice replies & talking to MIRA](../guides/voice-replies.md)** — get spoken
  replies on a channel.
- **[Proactive check-ins & daily briefing](../guides/proactive-checkins-and-briefing.md)**
  — put your channels to work.
