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
   chat: *"add my telegram bot"* and follow along.) The bot starts receiving
   straight away — no restart needed.
3. **Link your chat.** Because a bot is reachable by anyone who knows its
   @username, MIRA won't act as you until it has *verified* your chat. In
   **Settings → My channels → Link Telegram**, copy the one-time
   `LINK-XXXX-XXXX` code and send it to your bot. The bot replies *"✅ secured
   to your account"* — now you're talking to MIRA from your phone, and it has
   your **chat id** for proactive messages.

### How MIRA receives messages: polling (default) vs. webhook

Each Telegram account runs in one of two delivery modes (set when you add it,
changeable later by editing the account's config):

- **Polling** (**default**) — MIRA long-polls Telegram's `getUpdates`. It works
  **anywhere**: behind NAT, on `localhost`, with **no public URL, no
  port-forwarding, no reverse proxy, no TLS cert**. This is the right choice for
  a home or self-hosted install, and it's why a freshly added bot just works.
  Trade-off: one lightweight poll loop per account and a touch more latency than
  a push.
- **Webhook** — Telegram pushes updates to
  `https://your-host/webhook/telegram/<account-id>`. More efficient and instant,
  with no idle polling. Trade-off: it needs a **public HTTPS URL Telegram can
  reach** (a domain + reverse proxy + certificate), so it's for production
  deployments. MIRA authenticates the pushes with a secret-token header.

Most self-hosters should stay on **polling**. Switch to webhook only when you're
behind a public HTTPS endpoint and want to drop the poll loop.

### Routing mode: Personal vs. Shared vs. Guest-OK

When you add a Telegram (or Discord) account you choose a **routing mode** — who
each inbound message runs as. You can change it later in place from the account
row on the **Channel Accounts** page (it applies live; switching to Shared/Guest
shows a confirmation explaining the implications).

| Mode | Who it serves | Pros | Cons |
|---|---|---|---|
| **Personal** *(default)* | **Only the owner's verified chat** (you link once). Any other sender is ignored. | Simplest; private; secure-by-default (a stranger who finds the bot can't act as you). | One person only. |
| **Shared** | Any member who has **linked** their own MIRA account to the bot (each sends their own `LINK-XXXX-XXXX` code). Unlinked senders are turned away. | One bot for a whole **family/team**; admin creates it once, members never touch BotFather; **each member keeps their own context, memory, persona, and voice** — nothing is merged. | One bot identity/name for everyone; members must link before first use; the admin holds the token. |
| **Guest-OK** | Same as Shared, **plus** unlinked senders get a temporary **guest** session instead of being refused. | Open access — good for a public-facing or casual bot where you don't want to gate on linking. | Anyone who finds the bot can use it (as a guest); least private; guests share a generic identity. |

> **Security note.** Because a bot is reachable by anyone who knows its
> @username, **Personal** is locked to the owner's verified chat — it will not
> act as you for an unknown sender. For multiple people, use **Shared** (each
> member links with their own code) rather than handing out a Personal bot.

**Recommended:** a single **Shared** bot for a household — the admin creates one
bot, and each family member links their own account and gets their own private
MIRA behind it. Use **Personal** for your own solo bot, and **Guest-OK** only
when you deliberately want the bot open to anyone.

## Signal

Signal is end-to-end encrypted and great for a private, phone-native MIRA. It
runs on [`signal-cli`](https://github.com/AsamK/signal-cli), which MIRA drives
itself — it starts the `signal-cli ... daemon --http` process and talks to it
over a local HTTP API. It works on **Linux, macOS, and Windows**. It is **not**
a global on/off switch: you add a Signal *account* (a phone number), exactly
like adding a Telegram bot, and MIRA launches the daemon for that account.

**MIRA installs the runtime for you.** signal-cli is a Java application, but you
don't have to install Java or signal-cli by hand. The first time you add a
Signal account, MIRA fetches a pinned, checksum-verified copy of **signal-cli**
and (on platforms that need it) a **bundled Temurin JRE** into `~/.mira/deps/`,
then starts the daemon. On Linux x86_64 it uses signal-cli's self-contained
native build, so no JRE is downloaded. (This is a ~100 MB one-time download; on
Windows it includes the JRE, ~150 MB. Available since 0.277.0.)

The one thing MIRA can't do for you is **register the phone number** with Signal
— that's an interactive step (Signal sends a verification code / CAPTCHA):

1. **Register a number** with signal-cli on the host (the admin does this once,
   from a terminal). A dedicated number works best. *(If you'd rather use your
   own signal-cli install, point `channels.signal.cli_binary` at it — MIRA
   prefers an explicit path over the managed copy.)*
2. **Add your Signal account** under **Settings → Channels**, pointing at that
   number. MIRA installs the runtime if needed and starts the daemon — no
   restart. (Watch the channel status; the first start waits on the download.)
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

- **MIRA can't message me proactively on Telegram.** Make sure you've **linked
  your chat** (Settings → My channels → Link Telegram, then send the
  `LINK-XXXX-XXXX` code to the bot) — MIRA captures your chat id at link time.
- **The bot just answers my link code like a normal message.** The account is
  in **Personal** mode and your chat isn't linked yet, *or* it's a shared bot
  still set to Personal. Personal serves only the owner's linked chat; for a
  multi-user/family bot, set the account's routing mode to **Shared** so it
  honours link codes from everyone.
- **Nothing arrives.** Check that the channel account is **enabled** in Settings
  → Channels, and that your account is permitted to use that channel (an admin
  can restrict channels per user).
- **Email doesn't come through.** Confirm the sender is on the account's
  allowlist.
