---
title: Connect a channel
description: Reach MIRA outside the browser — set up Telegram, Signal, or email so it can message you (and you it).
sidebar:
  order: 1
---

A **channel** is any way MIRA can reach you and you can reach MIRA. The web UI is
one channel; this guide adds the others so MIRA can message you on your phone —
and, once you turn on proactive features, start conversations with you.

Channels are **per-user**: each person on a MIRA instance connects their own.
You manage them under **Settings → Channels**.

## Telegram (easiest to start with)

Telegram only needs a bot token — no phone-number registration — so it's the
quickest channel to set up.

1. **Create a bot.** In Telegram, open a chat with
   [@BotFather](https://t.me/BotFather) and send `/newbot`. Follow the prompts to
   name your bot; BotFather replies with a **token** that looks like
   `123456:ABC-DEF...`.
2. **Add it to MIRA.** Go to **Settings → Channels → My channels**, add a
   **Telegram** account, and paste the token. (You can also just ask MIRA in
   chat: *"add my telegram bot"* and follow along.)
3. **Message the bot once.** Open your new bot in Telegram and send it any
   message. This lets MIRA learn your **chat id**, which it needs before it can
   message you proactively.

That's it — reply to the bot and you're talking to MIRA from your phone.

> Adding the account turns the Telegram channel on automatically, and MIRA
> receives messages by **polling** Telegram — no public URL, port-forwarding, or
> reverse proxy needed, so it works on a home/localhost install behind NAT. (For
> a public deployment you can switch the account to **webhook** mode.)

## Signal

Signal is end-to-end encrypted and great for a private, phone-native MIRA, but
it needs a phone number registered with `signal-cli` (an operator/admin step).

1. **Register a number** with `signal-cli` on the server (the admin does this
   once). A dedicated number works best.
2. **Add your Signal account** under **Settings → Channels**, pointing at that
   number.
3. Message MIRA from your phone to confirm the link.

> Signal's setup is heavier than Telegram's because of the number registration.
> If you just want to try a phone channel quickly, start with Telegram.

## Email

MIRA can treat email as a conversation: inbound mail from people you allow
becomes a chat, and MIRA can email you (handy for daily briefings).

1. Go to **Settings → Email**.
2. Add an email account — either **IMAP/SMTP** with your mailbox details, or
   **Gmail / Outlook via OAuth** (sign in, no app password needed).
3. Set who's allowed to start conversations by email (an allowlist), so random
   inbound mail doesn't reach the agent.

Mail from an allowlisted sender now lands as a conversation; replies go back by
email.

## Other channels

MIRA also supports **Discord, Matrix, Slack, and WhatsApp**, plus **browser/phone
push notifications** and **custom channels** via the Channel Provider Protocol.
Each is added the same way — under **Settings → Channels** — with that platform's
credentials. Shared-bot setups (one bot routed to many users) are supported too.

## Get replies as voice notes

Once a phone channel is connected, you can have MIRA reply with **voice notes**
instead of (or alongside) text:

- Set your per-channel voice preference to **always** for Telegram or Signal.
- Pick a voice — for natural local speech, enable **Kokoro** and choose a voice
  like `af_heart` or `bf_emma`.

In the web UI, replies play as speech in the browser.

## Make MIRA proactive

Connecting a channel is what makes MIRA's proactive features useful. With a
channel in place, turn on:

- **Companion check-ins** — periodic, quiet-hours-aware messages.
- **Daily briefing** — a morning summary built from your calendar, wiki, and
  recent activity.

Both live under **Settings → Notifications**, where you can also fire a test
check-in or briefing immediately to confirm delivery.

## Troubleshooting

- **MIRA can't message me proactively on Telegram.** Make sure you've sent the
  bot at least one message — MIRA needs to capture your chat id first.
- **Nothing arrives.** Check that the channel account is **enabled** in Settings
  → Channels, and that your account is permitted to use that channel (an admin
  can restrict channels per user).
- **Email doesn't come through.** Confirm the sender is on the account's
  allowlist.
