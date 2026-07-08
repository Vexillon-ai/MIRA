---
title: Open apps & games MIRA builds
description: When MIRA's coding agent builds a game or web app, open it from a clickable link — how the per-app link works and how to reach it.
sidebar:
  order: 14
---

Ask MIRA to build something runnable — *"build me a Nokia-style Snake game"* —
and its coding agent produces a self-contained web app (an `index.html` with its
CSS/JS) saved on the server. MIRA then **serves it at its own link** so you can
actually play it.

## Opening a built app

Just ask in plain language:

> open the Snake game you built earlier

MIRA looks up the app (via its `list_web_apps` tool) and replies with a link
like:

```
http://019f3827-4387-7b11-a977-23ed0b39b79b.localhost:8087/
```

Click it and the game opens in your browser. MIRA **won't** claim it opened a
tab for you — it runs as a background service and can't reach your screen, so it
hands you the URL to click instead.

## Why the odd `<id>.localhost` address

Each app is served on **its own web origin** — `http://<task-id>.localhost:<port>/`
— separate from the MIRA app itself. That isolation is deliberate: a built app is
arbitrary HTML/JavaScript, and giving it a distinct origin means it **can't read
your MIRA login or call MIRA's API as you**. The task id in the address is a long,
unguessable value, so the link itself is the key.

`localhost` is the trick that makes this free: every major browser resolves
`anything.localhost` to your own machine automatically (no DNS, no extra port, no
firewall change). The catch is the flip side — it only works when **your browser
is on the same machine as MIRA**. Opening the link from your phone or another PC
won't reach it.

## Reaching it over the network (or WSL gateway IP)

The default `subdomain` mode only works when your browser can resolve
`<task-id>.localhost` to the machine MIRA runs on — i.e. the browser is on the
same box, or reaches MIRA via `localhost` (as WSL's built-in port forwarding
does). If instead you reach MIRA over the LAN or a **WSL gateway IP** (like
`http://192.0.2.10:<port>`), that subdomain won't resolve there.

For those setups, switch the **mode**:

- **`port`** — MIRA opens a *second* listener (default `server.port + 1`) and
  serves each app at `http://<host>:<apps-port>/a/<task-id>/`. Because it's a
  real port, it's reachable over any host that reaches MIRA — including a
  gateway/LAN IP. Set `server.web_apps.advertised_host` to the IP you use (e.g.
  `192.0.2.10`) so the link MIRA hands you points there.
- **`both`** — serve both ways; MIRA gives you the subdomain link with the port
  link as a backup.

The trade-off: `subdomain` gives each app its own origin (the strongest
isolation); `port` puts all apps on one shared origin. For a personal instance
that difference is minor — pick whichever *reaches* you.

## Settings

Serving is **on by default**. Configure it under `server.web_apps`:

| Setting | Default | Meaning |
| --- | --- | --- |
| `server.web_apps.enabled` | `true` | Serve built web apps at a per-app link. |
| `server.web_apps.mode` | `subdomain` | `subdomain`, `port`, or `both` (see above). |
| `server.web_apps.host_suffix` | `localhost` | Host suffix for the subdomain origin (`<task-id>.<suffix>`). |
| `server.web_apps.port` | `0` | Port-mode listener port (`0` = `server.port + 1`). |
| `server.web_apps.advertised_host` | *(derived)* | Host to put in port-mode links (a LAN/WSL-gateway IP). |

If serving is off, MIRA will still tell you the app was built and where it lives
on disk — it just won't give you a working link.

## Related

- [Named agents & workflows](/guides/named-agents-and-workflows/) — how MIRA
  delegates build tasks to a coding agent in the background.
