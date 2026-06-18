---
title: What is MIRA?
description: The big picture — what MIRA is, how it differs from a chat app, and how the system is shaped.
sidebar:
  order: 1
---

**MIRA** (Multi-tasking Intelligent Responsive Assistant) is a **self-hosted
personal AI agent**. You run it on your own machine; it talks to whichever LLM
provider you configure — Anthropic, OpenAI, OpenRouter, DeepSeek, Gemini, or a
local / OpenAI-compatible server. Your conversations, memory, and data stay on
your hardware.

If you've used ChatGPT or Claude, MIRA will feel familiar to chat with — but it
does several things a chat app doesn't.

## How MIRA is different from a chat app

**It reaches you on real channels.** MIRA isn't only a web tab. It can message
you on **Signal** and **Telegram**, send and receive **email**, post to
**Discord, Matrix, Slack, or WhatsApp**, and push **browser/phone
notifications**. It can also *start* conversations — a proactive check-in, a
morning briefing — not just answer when asked.

**It remembers.** MIRA keeps a growing **memory** of facts about you and a
personal **wiki** of longer notes, and uses both to personalise replies across
sessions and channels. Tell it something once and it carries forward.

**It acts, through tools.** Beyond chat, MIRA runs tools: web search and fetch,
sandboxed code execution, your calendar, PDF extraction, scheduled automations,
and — through **MCP (Model Context Protocol)** — any external tool server you
connect, from browser automation to GitHub to your own scripts.

**It speaks.** Built-in **text-to-speech** and **speech-to-text** mean replies
and check-ins can arrive as voice notes, and you can talk back.

**It serves more than one person.** A single MIRA instance can host several
people, each with their own account, memory, and settings, plus an admin who
manages the server.

## The shape of the system

MIRA runs as a single **gateway** process that hosts the agent, the web/HTTP
API, the channel pollers, and the schedulers. There's no database server or
message broker to stand up — it's one binary plus a data directory.

```
┌──────────────────────────────────────────────────────────────┐
│  Channels:  Web · Telegram · Signal · Email · Discord ·       │
│             Matrix · WhatsApp · Slack · Push · (your own)     │
└───────────────────────────┬──────────────────────────────────┘
                            ▼
┌──────────────────────────────────────────────────────────────┐
│  Gateway (one process)                                        │
│    Agent core ── multi-LLM router · Memory + Wiki ·           │
│                  Tools + MCP host · Automations · Auth         │
└───────────────────────────┬──────────────────────────────────┘
                            ▼
         Data directory (~/.mira/data): your DBs, wiki,
         artifacts, and voice models — all on your disk
```

**Settings come in two kinds:**

- **Operator / global settings** live in `mira_config.json` — providers,
  channels, security, voice, and so on. These are admin-managed and apply
  server-wide.
- **Per-user settings** belong to each person — their voice preferences,
  companion configuration, channel accounts, connected MCP servers, and
  profile.

**Your data lives in a data directory** (`~/.mira/data` by default): the
auth/user database, the memory database, the wiki, the companion database,
artifacts, and downloaded voice models. You can put it anywhere — a backed-up
volume or an external disk — by choosing it during `mira setup`, setting
`data_dir` in the config, or passing `--data-dir`. Backing up that one directory
backs up your whole MIRA.

## Asking MIRA about itself

MIRA ships with its own documentation **built into the binary**. You can ask it
things like *"what can you do?"*, *"how do I enable companion check-ins?"*, or
*"what does the `agent.max_tool_rounds` setting do?"* and it will answer from
that built-in reference. For the current *value* of a setting — and to change
one — MIRA respects your access level: you can see and change your own settings,
operator/global settings are admin-only, and secrets are never shown.

## Next steps

- **[Install MIRA](../getting-started/installation.md)** on your machine.
- **[Quickstart](../getting-started/quickstart.md)** — finish setup and have
  your first conversation.
- **[Connect a channel](../guides/connect-a-channel.md)** so MIRA can reach you
  outside the browser.
