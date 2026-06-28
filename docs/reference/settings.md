---
title: Settings reference
description: How MIRA's settings are organised, where they live, and how to view or change them.
sidebar:
  order: 1
---

MIRA's settings come in two tiers. This page explains the model and where to
find each option; the **complete, per-key reference is generated from the
configuration schema** (see [below](#the-complete-reference)).

## The two tiers

| Tier | Where it lives | Who can change it | Examples |
|------|----------------|-------------------|----------|
| **Operator / global** | `mira_config.json` | Admin only | providers, channels, security, voice backends, automations |
| **Per-user** | each account's own settings | the user (their own) | voice preferences, companion config, channel accounts, connected MCP servers, profile |

Operator settings are server-wide and admin-managed. Per-user settings belong to
each person and don't affect anyone else.

## Viewing and changing settings

You have three ways to work with settings:

- **The web UI** — *Settings*. Your own settings are always editable here;
  operator/global settings appear for admins.
- **Ask MIRA in chat** — e.g. *"set my briefing hour to 7"* (your own), or, as an
  admin, *"enable the daily briefing server-wide"*. MIRA confirms global changes,
  keeps secrets hidden, and protects security/provider/proxy keys.
- **Edit `mira_config.json` directly** (operator settings) — then restart the
  service. Most settings also apply live on save through the UI/API.

> **Secrets are never shown.** API keys and other secret values are redacted on
> read everywhere — the UI, the API, and chat.

## Applying changes

- **Most config applies live** when saved through the UI or API.
- **MCP servers hot-reload** — connecting or removing one takes effect with no
  restart.
- A few **server-level features** (e.g. reasoning auto-routing) take effect after
  a service restart.

## Reasoning visibility & control

Some models "think" before answering — reasoning models (e.g. gpt-oss, the qwen3
family) emit a chain-of-thought before their reply.

- **See it.** When a model streams reasoning, MIRA shows it as a collapsible
  **"Thinking"** block above the answer — expand it to follow the model's
  working, or leave it folded away.
- **Suppress it with `/no_think`.** Reasoning can be slow and can burn through the
  tool-loop token budget (a model that keeps thinking may stall before it acts),
  so you can turn it off two ways:
  - **Globally** — *Settings → Providers* (admin), applied to every request.
  - **Per conversation** — a toggle in the chat view, just for that thread.

  Both take effect across chat, channels, and tool loops.

## The complete reference

Every configuration key — its type, default, and description — is **generated
from the single source of truth**, `config/mira_config.schema.json`. Don't rely
on a hand-written list; consult the generated reference, which is always
matched to the schema:

- **In the app:** ask MIRA *"what does the `<setting>` setting do?"* — it answers
  from the built-in settings reference.
- **On the docs site:** the full per-key table is published under this page,
  generated from the schema.

If you're editing the docs, **never hand-write the per-key table** — it must be
generated from `config/mira_config.schema.json` so it can't drift. See
[CONTRIBUTING](../CONTRIBUTING.md#source-of-truth-boundaries-avoid-drift).
