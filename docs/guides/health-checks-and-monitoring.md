---
title: Health checks & monitoring
description: Point supervisors, load balancers, and uptime monitors at the right MIRA probe — /livez for liveness, /readyz for readiness — and understand the difference.
sidebar:
  order: 18
---

MIRA exposes two small, unauthenticated HTTP probes for automated monitoring.
They answer two **different** questions, and pointing your tooling at the right
one is the difference between a probe that catches real outages and one that
raises false alarms.

- **`GET /livez` — liveness.** "Is the process answering?" Returns `200 ok`
  immediately and touches **no** dependency — no LLM provider, no database, no
  subsystem. Because it never does dependency I/O, it stays responsive even when
  a dependency (say, a local inference server loading a model) is momentarily
  wedged.
- **`GET /readyz` — readiness.** "Is MIRA fit to serve a real request?" Returns
  `200` when the active model provider is reachable, `503` when it isn't. The
  verdict is **cached** (see below) and its JSON body reports the age of the
  cached check.

Both are public (no auth token needed) so a monitor or load balancer can reach
them without credentials. Neither returns any instance detail — an authenticated
`GET /api/status` carries the instance name, version, and counts for admins.

## Which probe should I use?

| Tool | Use | Why |
|------|-----|-----|
| **systemd / launchd / Windows service** watchdog | `/livez` | It should restart MIRA only when the *process* is dead, not when the provider is briefly slow. |
| **The liveness sentinel** (`mira guardian-watch`) | `/livez` (default) | It alarms on MIRA being *down*; a wedged provider is not MIRA being down. |
| **Docker `HEALTHCHECK`** | `/livez` | "Is the container serving?" — a plain `200` check. |
| **Load balancer / reverse proxy / Kubernetes readiness** | `/readyz` | Take an instance out of rotation when it can't actually serve, put it back when it can. |
| **Uptime monitor** (is the site up?) | `/livez` | You want to know the server is answering, not to page on a transient provider blip. |
| **Uptime monitor** (is it serving requests end-to-end?) | `/readyz` | Page when MIRA can't reach its model provider. |

Rule of thumb: **restart/alarm tooling → `/livez`; traffic-routing tooling →
`/readyz`.** If you only wire one thing, wire `/livez` — a liveness probe that
can block on a dependency is a self-inflicted outage, and `/livez` can't.

## Why `/readyz` is cached

`/readyz` round-trips the model provider to decide reachability. Without a cache,
every load balancer probing every few seconds would each trigger that round-trip
and could stampede the provider — and any caller whose timeout is shorter than
the provider's worst-case latency would flap between `200` and `503`.

So MIRA caches the readiness verdict for a short TTL
(`server.readiness_cache_ttl_secs`, default **30 seconds**) and refreshes it out
of band: probes are served the cached answer instantly, at most one provider
check is in flight per window, and no probe blocks on the provider. The
`cached_age_secs` field in the response tells you how old the served verdict is —
useful when a `503` is actually a stale cached miss rather than a live one. Set
the TTL to `0` to disable caching and check the provider inline on every probe.

## What about `/health`?

`/health` is the admin **System Health** *page* in the web UI — not a machine
endpoint. Point your browser there; point your monitoring at `/livez` or
`/readyz`.

> Upgrading from an older MIRA? `/health` used to be the machine probe. If you
> set a custom sentinel probe URL ending in `/health`, MIRA rewrites it to
> `/livez` automatically on the next start. Update any external monitors or
> `HEALTHCHECK`s you configured yourself.
