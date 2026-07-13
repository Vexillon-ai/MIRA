---
title: Keep conversations on-device (local-only failover)
description: MIRA is fail-closed by default — if your local model fails, it won't silently send your conversation to a cloud provider. Configure exactly which providers may be used as automatic fallbacks.
sidebar:
  order: 16
---

If you run MIRA on a **local model** (LM Studio, Ollama, or a self-hosted
OpenAI-compatible endpoint) for privacy, you don't want a crash or timeout to
quietly ship the conversation to a cloud API. MIRA is **fail-closed by
default**: its automatic failover only ever falls back to **local** providers.

## What "local-only" means

When your primary model fails mid-turn, MIRA tries the next provider in its
**failover chain**. By default that chain contains **only local providers** —
LM Studio, Ollama, and an OpenAI-compatible endpoint whose URL is a
loopback / private / `.local` address. Cloud providers (OpenAI, Anthropic,
Gemini, OpenRouter, DeepSeek, Groq, xAI, Moonshot) are **never** used as a
silent automatic fallback unless you explicitly add them.

If every local provider is down and no cloud fallback is configured, MIRA
returns a clear error — it does **not** reach for the cloud to "save" the turn.

Cloud providers stay fully usable for **explicit** choices: picking a cloud
model for a specific message still works. Only the *silent, automatic* fallback
is restricted.

## Configure the chain

**Settings → Providers → Automatic failover.**

- Your **primary** provider is pinned at the top.
- Below it, the **enabled fallbacks** in priority order — drag to reorder,
  remove with ✕.
- Local providers are enabled by default; **cloud providers are off**.
- Enabling a cloud provider shows an amber warning:
  *⚠️ conversations sent to {provider} (cloud)* — turning it on is a deliberate,
  eyes-open choice.

Under the hood this is the `failover_providers` setting: an ordered list of
provider slugs.

- **Unset (default)** → local-only (the safe default).
- **A list** → exactly those providers, in that order.
- **An empty list** → no automatic fallback at all (hard fail-closed — the
  primary or nothing).

## Behaviour change note

Before this feature, MIRA's automatic failover would walk through *every*
configured provider, including cloud ones. It is now **local-only by default**
for all installs. If you previously relied on automatic cloud failover, re-enable
those providers in **Settings → Providers → Automatic failover** (with the
privacy trade-off shown).

## Related

- [Security & multi-user](/concepts/security-and-multi-user/)
