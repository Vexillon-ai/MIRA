---
title: Health endpoints
description: The /livez liveness and /readyz readiness HTTP probes — paths, responses, auth, and the readiness cache. /health is the SPA System Health page, not a probe.
sidebar:
  order: 3
---

MIRA exposes two unauthenticated HTTP probes for supervisors, load balancers,
and uptime monitors. For guidance on which to point a given tool at, see
[Health checks & monitoring](/guides/health-checks-and-monitoring/).

## `GET /livez` — liveness

Is the process answering? Does **no** dependency I/O (no provider, DB, or
subsystem), so it stays responsive even while a dependency is wedged.

| | |
|---|---|
| **Auth** | None (public) |
| **`200`** | Body `ok` (`text/plain`). Always, whenever the server is serving. |
| **Other** | The endpoint has no failure branch — a connection refused / timeout (the server isn't up) is the only "down" signal. |

```
$ curl -i http://127.0.0.1:8087/livez
HTTP/1.1 200 OK
content-type: text/plain; charset=utf-8

ok
```

## `GET /readyz` — readiness

Is MIRA fit to serve? Reflects whether the active model provider is reachable.
The verdict is **cached** and refreshed out of band (see below).

| | |
|---|---|
| **Auth** | None (public) |
| **`200`** | JSON `{"status":"ready","cached_age_secs":<n>}` — provider reachable. |
| **`503`** | JSON `{"status":"unavailable","detail":"provider unreachable","cached_age_secs":<n>}` — provider not reachable. |

`cached_age_secs` is the age, in seconds, of the cached check the response was
served from (`0` on a freshly computed verdict).

```
$ curl -i http://127.0.0.1:8087/readyz
HTTP/1.1 200 OK
content-type: application/json

{"status":"ready","cached_age_secs":12}
```

### Readiness cache

`/readyz`'s verdict is cached for `server.readiness_cache_ttl_secs` (default
**30**; `0` disables caching and checks inline on every probe). Within the TTL,
probes are served the cached value with no provider round-trip; once stale, the
cached value is still served immediately while a single background refresh runs.
So however many callers probe, at most one provider round-trip is in flight per
window and no probe blocks on the provider.

## `/health` is not a probe

`/health` is the admin **System Health** page in the web UI, served by the SPA —
it is deliberately **not** a server route. Machine probes are `/livez` and
`/readyz`.

> **Changed in 0.339.0.** `/health` was previously the machine health probe (a
> single overloaded endpoint that also ran a provider round-trip). It was split
> into `/livez` (liveness) and `/readyz` (readiness), and `/health` was freed for
> the System Health page. A sentinel `guardian.process.probe_url` ending in
> `/health` is auto-migrated to `/livez` on startup; update any external monitors
> or container `HEALTHCHECK`s you configured yourself.
