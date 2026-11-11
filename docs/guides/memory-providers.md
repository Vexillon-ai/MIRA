---
title: Pluggable memory providers
description: How MIRA's long-term memory works, and how to add a second memory store — a plain-text files baseline or an external HTTP memory service — alongside the built-in one.
sidebar:
  order: 11
---

MIRA's long-term memory is a **pluggable seam**. The built-in SQLite store is one
provider; you can run a plain-text files store or an external HTTP memory service
alongside it, with no code change — just config. This guide covers the common
setups. The exact contract (for building your own provider) is the
[Memory Provider SPI](../reference/memory-provider-spec.md).

## Defaults — nothing to configure

Out of the box, memory is the built-in store, and **auto-capture is off**: MIRA
records nothing to long-term memory until you opt in — this holds across every
store, including a Files or external provider (harness ingest also stops while
capture is off). Turn it on with:

```jsonc
"memory": { "auto_extract": { "mode": "heuristic" } }   // local, no model call
// or "mode": "llm" for the richer, conflict-aware model-based extractor
```

You can also store/recall on demand in any chat — "remember that I prefer window
seats", "what car do I drive?", "forget that" — via the `memory_add`,
`memory_search`, and `memory_forget` tools, regardless of the auto-capture mode.

An **explicit "remember this"** request is always honoured and stored, bypassing
the category/confidence gates — so e.g. "remember my dog is Biscuit, a kelpie" is
kept even when the *Relationship* category is off for automatic capture.
**Automatic** capture, by contrast, respects your `allowed_categories` on **both**
the heuristic and LLM extractors, and never re-stores a fact it just recalled into
the turn nor resurrects one you recently deleted.

## Add a provider

Set `memory.providers`. An empty list (the default) means the built-in store
only; a populated list adds external providers the recall **router** fans across,
on top of the always-present built-in store. Writes go to the built-in store; an
external provider is written to only when it is writable and capture is on. Every
existing `memory.*` key keeps configuring the `builtin` provider.

### A plain-text files store

A simple, inspectable baseline — one note per file under a directory:

```jsonc
"memory": {
  "providers": [
    { "id": "builtin", "enabled": true },
    { "id": "files", "enabled": true, "transport": "in_process",
      "mode": "both", "root": "~/.mira/data/memfiles" }
  ]
}
```

### An external MCP memory server (planned — not yet wired)

The `mcp` transport and its **mapping files** (starter mappings for Mem0,
Graphiti, and the reference server ship in `config/memory-mappings/`) translate
MIRA's verbs to an MCP server's tool names. The adapter exists, but attaching a
live MCP client isn't wired into the provider builder yet, so an `mcp` provider is
**skipped at startup** today. Use the HTTP transport below to attach an external
memory service in the meantime.

### An external HTTP memory service

Point a provider at any HTTP memory service that speaks MIRA's provider-neutral
[Memory Provider SPI](./memory-provider-spi.md), with `transport: "http"`, a
`base_url`, and an `api_key_file` (sent as the `X-Api-Key` header, trailing
newline trimmed). A non-loopback endpoint needs `allow_non_loopback: true`.

```jsonc
{ "id": "home-memory", "enabled": true, "transport": "http",
  "base_url": "https://memory.example.com/v1",
  "api_key_file": "~/.config/mira/memory_api_key.txt",
  "allow_non_loopback": true, "read_only": true }
```

If the service uses a **self-signed or private-CA certificate**, name its PEM in
`tls_ca_file` — MIRA's HTTP client trusts a bundled root set, **not** your OS
trust store, so a cert installed with `update-ca-certificates` isn't trusted
automatically:

```jsonc
"tls_ca_file": "~/.config/mira/memory_ca.pem"
```

By default MIRA sends the turn's **account UUID** as `user_id` on each recall. If
your service is keyed on the **username** or **email** instead, set
`user_id_field` to `"username"` or `"email"` (default `"id"`); it falls back to
the UUID when the chosen field is blank:

```jsonc
"user_id_field": "username"
```

## How recall behaves

- Results from all enabled read providers are **merged** by relevance, each fact
  tagged with the provider it came from (shown as a pill under the reply).
- A provider that's **down** is reported explicitly ("store unavailable") — MIRA
  never reads a down store as "you have no such memory". A provider that's up but
  has nothing is "not recorded", **not** "false".
- Facts are presented in one of three tiers, by the [trust
  level](./memory-provider-spi.md#trust-model) the source assigns each one:
  - **Established** — the provider *attests* the fact (SPI trust `verified`). Shown as
    reliable, with its source.
  - **Found in a source (attributed, unverified)** — the provider returned the fact but
    does **not** attest it (SPI `reported` or `unverified`). Shown as the source's
    findings — attributed to it and quoted, never asserted as fact, and **never
    dropped**: a subject a source holds only at this level still surfaces (it won't read
    as "no memory"). Each carries the source's own trust label.
  - **Your own notes** — the built-in store's unverified recollections. Useful but maybe
    outdated; reconciled against the current date.

## Browsing what a source holds

Some sources expose a **catalog** — *what is held* about you, rather than a read of
it. When one is configured, a **Source catalog** panel appears on your **Memory**
page: counts by trust level, a filterable, paged list of the subjects on
file, and — click a subject — its held facts with their **trust labels** (an
unverified item shows as an attributed, quoted source passage). The agent also gets a
**`memory_catalog`** tool, so asking *"what do you know about X?"* can enumerate a
subject's holdings instead of only doing a similarity search.

Both are **per user and owner-scoped**: you only ever see your own holdings, and the
panel/tool appear only when a catalog-capable source is configured for you. (For the
HTTP transport this maps to the service's `GET|POST /catalog` endpoint.)

## Safety

- **Local-first.** A non-loopback (LAN/remote) provider is refused on startup
  unless you set `"allow_non_loopback": true` for it — a remote store receives
  your query text and may hold private facts, so it's opt-in.
- **Read-only** providers never receive writes (`"read_only": true`), and the UI
  shows which stores are read-only.
- Each user's memory is isolated; a provider only ever sees the asking user's
  scope.
- **Your data is yours.** The **Memory** page shows everything MIRA remembers about
  you across three tabs — **Saved**, **Daily summaries**, and **Connected sources** —
  with a quiet provenance line on each item (how it was learned, which channel, when)
  and machine tags hidden. You can delete any of your own memories (not just admins),
  clear everything at once (`DELETE /api/memory/mine`), and export the lot as JSON or
  Markdown (`GET /api/memory/export?format=md`). Editing a fact (supersede) keeps the
  original's tags and provenance, and **View history** (`GET /api/memory/{id}/history`)
  shows the full version chain.
