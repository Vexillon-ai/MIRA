---
title: HTTP Memory Provider SPI
description: The provider-neutral HTTP contract MIRA speaks to an external long-term-memory service — endpoints, the trust model, and the catalog surface — so any compliant service plugs in with no MIRA code change.
sidebar:
  order: 12
---

MIRA's long-term memory is a **pluggable seam** ([Pluggable memory
providers](./memory-providers.md)). Beyond the built-in store and a files store,
MIRA can fan recall across an **external HTTP memory service**.
This page is the **versioned, provider-neutral contract** that service speaks.
Any service matching this shape works with no change to MIRA — MIRA has no
knowledge of a specific vendor, its internal tiers, or its payload shapes.

**SPI version:** `1`. **Transport:** HTTP/JSON. **Auth:** a bearer token or an
`X-Api-Key` header read from a key file (trailing newline trimmed); an optional
custom CA (`tls_ca_file`) for a self-signed/private-CA certificate. A non-loopback
endpoint must be opted into with `allow_non_loopback: true`.

Configure it as a provider (see the [providers guide](./memory-providers.md#an-external-http-memory-service)):

```jsonc
{ "id": "home-memory", "enabled": true, "transport": "http",
  "base_url": "https://memory.example.com/v1",
  "api_key_file": "~/.config/mira/memory_api_key.txt",
  "allow_non_loopback": true, "read_only": true,
  "user_id_field": "username" }      // which identity MIRA sends (id | username | email)
```

## Identity

MIRA sends the asking user's identity **value** as `user_id` on **every** call.
Which account field supplies that value is `user_id_field` (`id` — MIRA's UUID,
the default — or `username` or `email`), falling back to the UUID when the chosen
field is blank. For `username`/`email`, MIRA **also mirrors** the same value under a
key of that name (`username` or `email`) in the request body — so a service that
reads identity from a named field rather than from `user_id` still receives it.
Read whichever you prefer; they carry the same value. The service is responsible for
**owner-scoping**: a caller only ever sees its own memory.

## Trust model

Every served fact carries a **generic trust level** — one of:

| level | meaning | how MIRA renders it |
|---|---|---|
| `verified` | the service attests the fact (it stands behind it) | **Established** — shown as reliable, with its source |
| `reported` | found/corroborated but not attested | **Found in a source** — attributed, not asserted |
| `unverified` | present in some material the service holds, unconfirmed | **Found in a source** — an attributed quote |

A fact may also carry an optional free-text `trust_label` (e.g. "found in source,
not verified") which MIRA shows verbatim beside the fact. MIRA never invents or
upgrades trust; it renders exactly what the level/label say. A service maps its own
internal tiers onto these three levels inside its own adapter — MIRA stays neutral.

## Endpoints

All paths are under the provider's `base_url`.

### `GET /capabilities`
`→ { "spi_version": "1", "read": bool, "write": bool, "ingest": bool,
     "forget": bool, "catalog": bool, "citations": bool,
     "trust_levels": ["verified","reported","unverified"] }`

Declares what the service supports; MIRA probes this once at boot and **gates
features on it** (the `catalog` browser + `memory_catalog` tool register only when
`catalog` is declared; a `verified` fact is honoured only when `citations` is on).
A flag may be given as a flat boolean (above) or implied by listing the endpoint
under `routes_live` / `verbs.{read,write}` (e.g. `/catalog` present ⇒ catalog) —
MIRA honours either. MIRA **re-probes on a timer**, so a service that was
unreachable at boot recovers its capabilities without a restart. It is
**fail-soft**: while unconfirmed, read and `citations` stay on (so recall works
and every SPI fact keeps its trust level), but `catalog` stays **off** until a
probe confirms it — MIRA won't offer the catalog browser for a store that may
not have one.

### `POST /search`
`{ "query": string, "user_id": string, "k": int, "consumer_model"?: string }`
→ `{ "state": "served" | "absent" | "withheld" | "unavailable",
     "reason"?: string,
     "facts": [ { "id"?: string, "content": string, "score"?: number,
                  "subject"?: string, "relation"?: string,
                  "trust": "verified" | "reported" | "unverified",
                  "trust_label"?: string,
                  "source"?: { "kind": string, "label": string, "uri"?: string },
                  "recorded_at"?: rfc3339 } ] }`

`served` returns facts; `absent` means nothing matched (a healthy "not recorded",
**never** read as "false"); `unavailable` means the store is down. `withheld` means
the source **has** related material but deliberately didn't return it for this query
(e.g. a near or ambiguous match) — MIRA renders it **with its `reason`** ("be more
specific") and never as "no such memory", so always include a human `reason`. Any
non-2xx HTTP status is treated as **unavailable**, never absent — so a bad key
(401/403) or wrong path (404) never reads to the user as "you have no such memory".
A service may also echo its own native state under `provider_meta` for debugging;
MIRA only acts on the generic `state`.

### `POST /add` · `POST /ingest` · `POST /forget`
`/add` `{ user_id, text, source }`, `/ingest` `{ user_id, texts, source }`,
`/forget` `{ user_id, item_id }`. Only called when the service is writable
(`read_only: false` and the capability is declared). A closed write-gate may answer
`404`, treated as a no-op.

### `GET /health`
`→ { "ready": bool }` (or `503` while warming).

### `GET|POST /catalog`
*What is held*, not a read of it — powering the per-user **Source catalog** browser
and the `memory_catalog` tool. Owner-scoped.
`{ user_id, subject?, prefix?|contains?, page?, per_page? }` →
`{ "state": …,
   "catalog": {
      "counts": { "subjects": int, "facts": int,
                  "by_trust": { "verified": int, "reported": int, "unverified": int } },
      "page": { "page": int, "per_page": int, "n_matching_subjects": int, "n_pages_subjects": int },
      "subjects": [ { "name": string, "aliases"?: [string], "label"?: string } ],
      "subject_detail"?: { "name": string, "state": string, "n_facts": int,
                           "facts": [ { "relation"?: string, "content": string,
                                        "trust": "verified"|"reported"|"unverified",
                                        "trust_label"?: string } ] } } }`

`prefix` and `contains` are mutually exclusive. A service that predates `/catalog`
answers `404`; MIRA then simply hides the catalog surface.

## Compatibility

The SPI is versioned (`spi_version`). MIRA treats a fact's `trust`/`trust_label`
and the catalog's `by_trust` as the contract; it does not read any vendor-specific
tier names or nested per-store shapes. A service evolving its internal model only
needs to keep mapping onto these generic levels.
