---
title: Safely expose MIRA (Restricted Mode)
description: Run MIRA as a guest, kiosk, or public "try it" instance — a fail-closed profile that denies dangerous capabilities, caps spend, and hands out sandboxed throwaway sessions.
sidebar:
  order: 19
---

By default MIRA is a *personal* assistant: it can run tools, message you on your
channels, look out for the people in your care, and reach out on its own. That is
exactly what you want on a private server behind a VPN — and exactly what you do
**not** want on an instance strangers can reach.

**Restricted Mode** is a fail-closed profile that locks an instance down to a safe
showcase set, so you can expose it deliberately: a public "try it" demo, a kiosk in
a waiting room, a shared guest instance — or simply to shrink the blast radius of a
[prompt-injection](../concepts/security-and-multi-user.md) attack. Enforcement is
**server-side**, at the policy layer: the model cannot talk its way past it, and no
user, UI, API, or prompt path can widen it at runtime.

> **Restricted Mode is defence-in-depth, not a replacement for a private
> network.** For your *own* remote access, a VPN or tunnel is still the right tool —
> see [Reach MIRA from away](remote-access.md). Reach for Restricted Mode when the
> instance is *meant* to be used by people who aren't you.

## When to use it

- **A public demo / "try it" page** — anonymous visitors chat with MIRA without an
  account (see [Guest sessions](#offer-anonymous-guest-sessions)).
- **A kiosk or shared guest instance** — a device in a shared space where anyone
  can walk up and use it.
- **Reducing prompt-injection blast radius** — even for a semi-trusted instance,
  denying shell/code/outbound-send means a malicious web page or document the agent
  reads can't turn into real-world actions.

## Turn it on

Set a profile in your `mira_config.json` and restart. The one built-in profile is
`hardened`:

```jsonc
"restricted_mode": {
  "profile": "hardened"
}
```

That's it. With `hardened` active, MIRA **allows** chat and reasoning, per-user
memory, and the wiki — and **denies**, at the policy layer:

- shell and code execution, and filesystem writes;
- **real outbound messages** on any channel (email / SMS / Signal / Telegram /
  WhatsApp / …);
- **care-network escalations** to real people;
- home-automation actuation and autonomous guardian actions;
- arbitrary web fetch / outbound HTTP;
- calendar and other external writes, and MCP tools that reach external systems.

Anything not explicitly allowed is denied, so a newly-added tool is denied by
default rather than newly exposed. An unrecognised profile name is a **fatal
startup error** — MIRA refuses to boot rather than fall back to "unrestricted".

The profile is read **once at startup and held immutably**. To change or turn it
off, edit the config and **restart** — there is deliberately no runtime switch.

## Add resource & cost caps

An exposed instance should bound its own spend and load. Add caps under
`restricted_mode.caps` — every one defaults to `0` = unlimited, so you opt into
exactly what you want:

```jsonc
"restricted_mode": {
  "profile": "hardened",
  "caps": {
    "messages_per_min":        10,        // per user, rolling 60s
    "max_tokens_per_turn":     512,       // hard output cap per reply
    "context_budget_tokens":   4000,      // how much history the model sees
    "max_concurrent_sessions": 20,        // simultaneous in-flight turns, whole instance
    "daily_token_ceiling":     2000000    // total tokens/day, whole instance
  }
}
```

When a cap is hit, MIRA replies with a brief, friendly message ("you're going a
little too fast", "busy — try again in a moment") instead of a raw error or
unbounded spend. `max_tokens_per_turn` is also applied to the model's own
`max_tokens`, so a turn can't be coaxed into a runaway generation. The full list of
knobs is in the [settings reference](../reference/settings.md).

## Offer anonymous guest sessions

For a true "try it" experience — no signup — turn on **guest sessions**. Each guest
gets a short-lived, **sandboxed** account with its own isolated memory, wiki, and
chat history, seeded from an optional baseline and **completely wiped** on a TTL
(and by a periodic reset, and on every restart — guests never persist).

```jsonc
"restricted_mode": {
  "profile": "hardened",
  "guest": {
    "enabled":          true,
    "session_ttl_secs": 1800,             // 30 minutes
    "max_active":       50,               // cap concurrent guests
    "seed_wiki_dir":    "guest-seed"      // baseline wiki copied into each guest
  }
}
```

A client starts a session by POSTing to the public mint endpoint:

```sh
curl -X POST https://your-host/api/auth/guest
```

On success it returns `201` with the server's name, a **scoped bearer token**, its
expiry, and the guest user — the same envelope the mobile "Try now" buttons use:

```jsonc
{
  "server":           { "name": "MIRA demo", "base_url": "https://your-host" },
  "access_token":     "…",
  "token_type":       "Bearer",
  "expires_at_ms":    1737000000000,
  "session_ttl_secs": 1800,
  "user":             { "id": "…", "username": "guest_…" }
}
```

> **Guest sessions are fail-closed.** A guest is minted **only** when a restriction
> profile is *also* active. If you enable guests without setting
> `restricted_mode.profile`, minting stays **disabled** (the endpoint returns
> `403`) and MIRA logs a warning at startup — an anonymous session is never handed
> out on an unrestricted instance. The endpoint also returns `503` when you're at
> the `max_active` cap.

## Pick the model backend

Restricted Mode is backend-agnostic. Point `primary_provider` at whatever you like
— a **capped cloud model** or a **local, OpenAI-compatible endpoint** — with no
code change. The per-turn caps above are enforced inside MIRA regardless of the
backend, so a local model and a cloud model honour the same limits.

## Run it in a container

Restricted Mode is just config, so a container image needs no special build — mount
or bake a `mira_config.json` with `restricted_mode` set, and MIRA comes up
restricted. Because the profile is fixed at startup, a container restart is how you
change it (which fits immutable-image deployment nicely).

## Monitor uptime

An exposed instance should be watched. MIRA ships two unauthenticated probes:

- **`GET /livez`** — liveness; is the process answering? (no dependency I/O)
- **`GET /readyz`** — readiness; are its dependencies healthy? (cached)

Point your load balancer / uptime monitor at these. See
[Health checks & monitoring](health-checks-and-monitoring.md) and the
[health-endpoint reference](../reference/health-endpoints.md).

## Harden the network (egress)

Restricted Mode denies the *application-level* paths to the outside world, but
defence-in-depth means also restricting egress at the **network** layer, so a
future gap can't reach out:

- **Allow only the egress you actually need** — typically your model provider's
  endpoint (or nothing, for a fully local model), and the channels you deliberately
  use. Deny the rest at the firewall / container network policy.
- **Block link-local and cloud-metadata addresses** (e.g. `169.254.169.254`) — MIRA's
  HTTP guard already refuses these, but a network-level block is a good backstop
  against SSRF.
- **Don't run the standalone Guardian watchdog on a public restricted instance.**
  It is a *separate* process that sends its own alerts and is not governed by the
  in-process profile.
- Terminate TLS and rate-limit at your reverse proxy as usual; keep MIRA itself
  bound to localhost behind it.

## What Restricted Mode does *not* do

- It is **not** a private-access solution. For reaching your own instance remotely,
  use a VPN/tunnel — see [Reach MIRA from away](remote-access.md).
- It does **not** change per-user isolation or roles; it *narrows what any session
  may do*. Admins on a restricted instance are restricted too — the profile is
  instance-wide.

## Related

- [Reach MIRA from away](remote-access.md) — the preferred way to use your *own*
  instance remotely.
- [Security & multi-user](../concepts/security-and-multi-user.md) — the auth,
  roles, and policy model Restricted Mode builds on.
- [Health checks & monitoring](health-checks-and-monitoring.md) — wiring up the
  `/livez` and `/readyz` probes.
- [Settings reference](../reference/settings.md) — every `restricted_mode` knob.
