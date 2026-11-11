---
title: Memory Provider SPI
description: The contract a pluggable memory provider implements to serve long-term memory to MIRA — verbs, scope keys, result shape, capability flags, MCP mapping, and conformance.
sidebar:
  order: 4
---

**Spec version: `0.1`.** This is a draft contract for a forthcoming feature; the memory
provider seam is not yet shipped. Semantics may change before `1.0`. Compatibility rule:
a provider declares the spec version it targets; MIRA accepts any provider whose major
version matches and whose minor version is ≤ MIRA's. Breaking changes bump the **minor**
within `0.x` (and the major from `1.0` on); additive changes keep the version. Every change
is recorded in the [changelog](#changelog) at the end.

A **memory provider** is a pluggable backend that gives MIRA long-term memory. The built-in
SQLite store is one provider; a provider can also be a local file store, an MCP memory server,
or a remote service. Providers plug into one trait seam, so switching or combining them needs
no code change.

---

## 1. What a provider does

Per conversational turn, scoped to one user, a provider answers:

1. **Recall** — "given this query, what do you remember that's relevant?" (`search`).
2. **Encode** — "here is a turn that just happened; store what's worth keeping" (`add` /
   `ingest_turn`), **only if the provider declares `write`**.

Providers are:

- **Capability-gated** — MIRA reads `capabilities()` once and never calls a verb the provider
  didn't advertise. A read-only provider advertises `write:false` and never receives an encode
  call; the UI shows a read-only badge.
- **Isolated** — every call carries a scope key with a required `user_id`; a provider must
  never return one user's memories to another.
- **Fail-soft** — a recall that errors or times out surfaces as *unavailable*, not *empty* —
  MIRA renders "the store could not be reached" rather than letting the model conclude the user
  has no such memory.

---

## 2. Verbs

Core (the minimum a useful provider implements):

| Verb | Gated by | Purpose |
|---|---|---|
| `health` | — | liveness + readiness (may report **warming** while stores preload) |
| `capabilities` | — | the flags in §5 |
| `search` | `read` | recall relevant facts for a query |
| `add` | `write` | store explicit items |
| `ingest_turn` | `write` + `ingest_conversation` | store a raw turn; provider extracts itself |
| `forget` | `forget` | delete by id or filter |
| `get` / `list` | `read` | fetch by id / enumerate a scope |
| `update` | `update` | mutate an existing item |

Optional (harness/admin):

| Verb | Gated by | Purpose |
|---|---|---|
| `consolidate` | `consolidate` | provider-side dedup / summarise / decay |
| `export` | `read` | portable JSON dump for one scope (§6) |
| `import` | `write` | load a portable JSON dump |

A provider implementing only `health` + `capabilities` + `search` with `read:true` and all
else `false` is a valid **read-only recall provider**.

---

## 3. Scope keys

Every data call carries:

```jsonc
{
  "user_id":   "string (REQUIRED) — opaque stable per-user id; the isolation boundary",
  "agent_id":  "string|null — which MIRA agent/persona",
  "thread_id": "string|null — conversation/thread",
  "namespace": "string|null — caller-chosen partition",
  "consumer_model": "string|null — SEARCH only: the model that will consume these facts"
}
```

A call without `user_id` is a protocol error. Isolation between two `user_id`s is mandatory
and is part of the conformance suite (§10).

**`consumer_model`** (optional, `search` only) lets a provider vary what it returns, or how it
labels it, by which model will read the facts (some stores certify models and label answers
served to an uncertified one). The provider reports the outcome in
`provider_meta` (e.g. `consumer_certified: true|false` plus its own label); **MIRA surfaces
the label, never strips it.** Consumer certification is kept **separate** from a fact's
`trust` (§5): trust is about the fact's own attestation, certification is about the reader.
Whether an uncertified consumer should downgrade a fact's presentation is the provider's call,
expressed through what it puts in `trust` / `provider_meta` — MIRA does not infer it.

---

## 4. Result shape

`search` returns one of three **statuses** — these are distinct and must never be conflated.
The distinction is load-bearing: a confident answer on an `absent` result (the model filling a
lookup miss from its own weights) is the single most important failure a memory system must
avoid, and MIRA cannot guard against it if "the store has nothing" looks like "the store is
down" or like a merely low-recall search.

```jsonc
{ "status": "ok",          "facts": [ MemoryFact, ... ], "provider_meta": { } }  // ≥1 relevant fact
{ "status": "absent",      "reason": "no_store_for_user" }    // up, returned nothing for THIS query (reason optional)
{ "status": "unavailable", "reason": "connection refused" }   // store could not be reached
```

- **`ok`** — one or more facts; render them. A provider MAY also return `ok` with a
  top-level `provider_meta` carrying a *served-and-withheld* count (some facts served, others
  held back by a gate) — the withheld ones are a count, never content.
- **`absent`** — the provider is up and returned nothing **for this query**. The default
  framing MIRA uses is **"not found for this query," never "false"**: the fact may be stored
  but missed by search, or the user may simply never have told MIRA, so the model must assert
  neither that it's false nor that it's definitively absent. Only a provider with
  `can_assert_complete` (§5) earns the stronger **"not in memory"** framing, because only it
  actually enumerated the scope. A provider that can't tell "nothing" from "low recall" may
  instead return `ok` with `facts: []`, which MIRA treats as a soft absent. An optional
  `reason` (e.g. `no_store_for_user`) is surfaced.
- **`unavailable`** — down / timeout / internal error / store-not-built; MIRA renders "the
  store could not be reached" and the model must not conclude the user has no such memory.

A **malformed or invalid request** (bad reference, missing `user_id`) is a client error (HTTP
400 for an HTTP transport), **never** `absent` and never `unavailable` — don't let an input
bug masquerade as "the user has no such memory."

A provider MAY attach further explicit states in `provider_meta` (e.g. `{ "state":
"AMBIGUOUS", "n_matching": 12 }` with no ids, `{ "state": "NOT_EXACT", "key": "...",
"reason": "..." }`, or `{ "state": "WITHHELD", "reason": "<gate code>" }`) — these cross as
**states, never as fact content**, and MIRA surfaces them rather than inventing content.

### MemoryFact

```jsonc
{
  "id":          "string|null — provider-stable id (needed for forget/update/get)",
  "content":     "string — the fact text MIRA injects",
  "score":       0.0,            // float|null — relevance; used for cross-provider merge
  "recorded_at": "RFC3339|null", // when the provider learned it → '(recorded 3 weeks ago)'
  "valid_at":    "RFC3339|null", // temporal providers: when the fact became true
  "source_id":   "string — the provider id (labelled block + UI pill)",
  "provenance":  "string|null — the provider's own citation, if it has one",
  "confidence":  0.0,            // float|null — provider's own confidence
  "speaker":     "user|assistant|system|rollup|external",
  "trust":       "verified|unverified|unknown",   // see §5
  "complete_set":false,          // honoured ONLY if capabilities.can_assert_complete (§5)
  "provider_meta": { }            // opaque JSON — MIRA passes it through UNTOUCHED
}
```

`provider_meta` is for anything MIRA doesn't model; MIRA stores and renders it opaquely and
never interprets it. Keep it small.

---

## 5. Capability flags & two earned claims

```jsonc
"capabilities": {
  "read": true, "write": false, "forget": false, "update": false, "supersede": false,
  "ingest_conversation": false,   // provider extracts facts from raw turns itself
  "temporal": false,              // valid_at is meaningful
  "graph": false,                 // relational/graph store
  "citations": false,             // provenance is populated
  "can_assert_complete": false    // may a result claim it is the COMPLETE set?
}
```

Two claims are **earned**, never assumed:

- **`complete_set`** on a fact is honoured only when `can_assert_complete` is `true` **and**
  the result has ≥1 fact. This drives MIRA's "use THIS list as the full set for counting /
  listing" behaviour. A provider that cannot truly enumerate its scope leaves
  `can_assert_complete:false`; MIRA then never claims completeness from it. (A store that
  advertised completeness while returning nothing would invite a confident wrong "zero".)

- **`trust`** per fact tiers how MIRA presents it:
  - `verified` → presented as established, with its `source_id`/`provenance` and date; MIRA
    still flags it if it looks stale or contradictory, but does not hedge on it. Use only for
    facts the provider can attest.
  - `unverified` / `unknown` → presented as MIRA's own notes: useful but possibly incomplete
    or outdated, reconciled against the current date, hedged/flagged when stale.

  Default is `unverified`. Emit `verified` only for genuinely attested facts. *(Open for a
  future rev: a middle tier such as `corroborated`, between `verified` and `unverified`, for
  a fact confirmed against an external source but not first-party attested — "verified" would
  overstate it, "unverified" undersells it. Until then, use `unverified` with a descriptive
  label in `provider_meta`.)*

MIRA **respects** capabilities: read-only providers never receive writes; unsupported verbs
surface a legible "this store does not support X" to the model, not a generic error.

---

## 6. Portable export/import format

For `export`/`import` and for fixtures. One JSON object per scope:

```jsonc
{
  "mxf_version": "0.1",
  "scope": { "user_id": "...", "namespace": null },
  "items": [
    { "id": "...", "content": "...", "recorded_at": "...", "valid_at": null,
      "speaker": "user", "trust": "unverified", "tags": [], "provider_meta": {} }
  ]
}
```

---

## 7. Transports & locality

A provider is reached in-process (a native impl of the trait), over **MCP** (stdio or
Streamable-HTTP, verbs mapped by a mapping file — §8), or over **HTTP** (a small REST/JSON
service exposing the verbs directly).

MIRA classifies each provider as `loopback` / `lan` / `remote` by its endpoint. A
non-loopback provider is **refused at startup** unless its config sets `allow_non_loopback:
true`, because a remote memory provider receives the user's query text and may return their
private facts. MIRA is local-first and will not call a remote provider by default.

**Readiness.** `health` may report a **warming** state while the provider preloads (e.g. a
store that loads its data at start). MIRA treats warming as unavailable *for that turn* (it
renders the unavailable block, never a false "no memory") and keeps polling; a provider should
declare its expected cold-start time so MIRA can budget the per-turn recall timeout
accordingly.

---

## 8. MCP mapping-file format

An MCP memory server exposes tools under server-specific names; MIRA maps them to SPI verbs
with a per-server mapping file (no guessing — an unmapped server is rejected):

```jsonc
{
  "server": "example",
  "verbs": {
    "search": { "tool": "search_memories", "args": { "query": "$query", "user_id": "$user_id", "limit": "$k" },
                "result": { "list_at": "results", "content_at": "memory", "id_at": "id", "score_at": "score" } },
    "add":    { "tool": "add_memory",    "args": { "data": "$content", "user_id": "$user_id" } },
    "forget": { "tool": "delete_memory", "args": { "memory_id": "$id" } }
  },
  "capabilities": { "read": true, "write": true, "forget": true }
}
```

`$query/$user_id/$k/$content/$id/$agent_id/$thread_id/$namespace` are substituted from the
call. `result` describes how to read facts from the tool response: `list_at` is the array
field, the `*_at` keys locate fields on each element. MIRA ships mappings for the MCP
reference server (`create_entities`/`add_observations`/`search_nodes`), Mem0 OSS
(`add_memory`/`search_memories`), and Graphiti/Zep (`add_memory`/`search_nodes`/
`search_memory_facts`).

---

## 9. Config block

```jsonc
"memory": {
  "providers": [
    { "id": "builtin", "enabled": true,  "mode": "harness" },
    { "id": "files",   "enabled": false, "transport": "in_process", "root": "~/.mira/data/memfiles" },
    { "id": "example-remote", "enabled": false, "transport": "http", "base_url": "https://memory.example.internal/v1",
      "api_key_file": "...", "read_only": true, "allow_non_loopback": false, "user_id_field": "username" }
  ]
}
```

- `mode` = `harness` (MIRA searches/injects + ingests around the turn), `agent` (the model
  calls `memory_search`/`memory_add`/`memory_forget`), or `both`.
- `capabilities` are discovered at boot from the service's `GET /capabilities` (fail-soft: a
  service without the endpoint keeps the optimistic defaults). `read_only` always wins over a
  declared write/forget — it can disable a flag, never enable one the service doesn't advertise.
- Writes go to the built-in store; an external provider is written to only when it is writable
  and capture is on. Reads merge across all enabled read providers.
- The `mcp` transport has an adapter + starter mappings but is **not yet wired** into the
  provider builder, so an `mcp` provider is skipped at startup — use `http` today.

Every existing `memory.*` setting continues to configure the `builtin` provider.

---

## 10. Conformance

A provider passes the MIRA conformance suite when: isolation holds between two `user_id`s;
capability flags are respected (no write to a read-only provider; unsupported verbs refuse
legibly); `search` returns a well-formed result or `unavailable` (never a panic or a hang past
the timeout); `complete_set` is only set when `can_assert_complete`; and, if `write`, an
`add` → `search` → `forget` round-trip behaves. The same suite judges the built-in, files,
MCP, and any third-party provider.

---

## Changelog

- **0.1** — initial draft contract.
  - *rev 2026-11-02* (additive, pre-adoption): `search` returns an explicit **status**
    (`ok` / `absent` / `unavailable`) instead of facts-or-unavailable, so "nothing in
    memory" (abstention: *not in memory*, not *false*) is distinct from "store down" and from
    low recall; optional **`consumer_model`** on `search` with `provider_meta.consumer_certified`
    + label passthrough, kept separate from per-item `trust`; `ambiguous`/`withheld` noted as
    `provider_meta` states that never become fact content.
  - *rev 2026-11-02b* (additive): `absent` framed as "not found for this query" by default,
    "not in memory" only under `can_assert_complete`; optional `reason` on `absent`; malformed
    request = client/400, never absent/unavailable; `ok` may carry a served-and-withheld count
    in `provider_meta`; `health` may report **warming** (treated as unavailable-this-turn,
    budget the timeout on cold-start); flagged a possible future `corroborated` trust tier.
