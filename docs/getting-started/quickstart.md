---
title: Quickstart
description: Finish first-run setup and have your first conversation with MIRA in a few minutes.
sidebar:
  order: 2
---

You've [installed MIRA](installation.md) and it's running at
**http://localhost:8080**. Let's finish setup and have your first conversation.

## 1. Sign in

Open **http://localhost:8080** in your browser. Sign in with the **admin
account** you created during `mira setup`.

> Didn't set one up, or installed manually? The first time MIRA starts with no
> users, it prints a one-time admin password to the console/log — use that, then
> change it from **Settings → Users**.

## 2. Complete onboarding

On first sign-in, MIRA runs a short **onboarding conversation**. It's not a form
— you just chat, and MIRA asks a few questions to start building its model of
you (your name, how you'd like to be addressed, your timezone, a little about
what you'll use it for).

Answer naturally. Everything you share becomes part of MIRA's
[memory and wiki](../concepts/overview.md#how-mira-is-different-from-a-chat-app),
so it can personalise replies later. You can skip anything you'd rather not
share, and change it all afterwards.

## 3. Have your first conversation

Once onboarding is done you're in the main chat. Try a few things:

- **Just talk.** Ask a question, ask for help with a task — it works like any
  chat assistant.
- **Ask what it can do:**
  > *"What can you do?"*

  MIRA answers from its built-in documentation — capabilities, settings, and
  how-tos are all in the binary.
- **Give it something to remember:**
  > *"Remember that my dog's name is Pixel and she's a border collie."*

  Later, ask *"what do you remember about me?"* and you'll see it stuck.

## 4. Choose your model (optional)

MIRA uses the provider you set up during install. If you configured more than
one model, you can switch per-conversation from the **model picker** at the
bottom of the chat — handy for sending a hard question to a stronger model and
everyday chat to a cheaper one.

To add or change providers, go to **Settings → Providers** (admin only).

## What's next

You now have a working MIRA you can chat with in the browser. The next step is
to let it reach you *outside* the browser, and to switch on the things that make
it proactive:

- **[Connect a channel](../guides/connect-a-channel.md)** — message MIRA on
  Telegram or Signal, or by email.
- **Turn on proactive check-ins and a daily briefing** — *Settings →
  Notifications*. MIRA will start conversations with you (quiet-hours aware), not
  just answer when asked.
- **Add tools via MCP** — the `/mcp` page has a catalog of external tool servers
  (browser automation, GitHub, filesystem, and more) you can connect in one
  click.

Want to understand how the pieces fit together first? Read
**[What is MIRA?](../concepts/overview.md)**.
