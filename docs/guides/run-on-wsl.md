---
title: Run MIRA on WSL
description: Reach Windows-host services (LM Studio, a TTS server, SearXNG) from MIRA running inside WSL2 — set up the windows-host alias and let MIRA auto-detect misrouted URLs.
sidebar:
  order: 11
---

If you run MIRA **inside WSL2** and your AI services (LM Studio, a local TTS
server, SearXNG, …) run on the **Windows host**, there's one networking gotcha
to know about — and MIRA handles most of it for you.

## The gotcha: don't use the Windows LAN IP

WSL2's default networking mode is **NAT**. A NAT guest **cannot reach its own
Windows host's LAN IP** (e.g. `192.168.1.50`) — the connection just times out.
So if you point MIRA at `http://192.168.1.50:1234`, every call fails and MIRA
silently falls back to a slower provider.

The host *is* reachable, but only via the **WSL gateway IP** — and that IP
**changes every time WSL or Windows restarts**, so you can't just hardcode it.

## The fix: the `windows-host` alias

MIRA installs a tiny boot hook that maps a **stable hostname**, `windows-host`,
to whatever the gateway IP currently is — refreshed on every boot. Point your
service URLs at `http://windows-host:PORT` once and they keep working across
reboots.

Set it up (one time, needs root):

```bash
sudo mira wsl-host-alias-install
```

This is also done automatically when you run `sudo mira helper-install`. Check
it any time:

```bash
mira helper-status     # shows "WSL host-alias: installed" + the current mapping
```

Then use the hostname in your settings — for example LM Studio:

```
http://windows-host:1234/v1
```

## MIRA catches mistakes for you

You don't have to remember this. When MIRA starts inside WSL it **checks your
configured service URLs**: if one points at an address it can't reach but
`windows-host` *would* work, it tells you — with a notification and a banner in
**Settings** offering a **one-click fix** that swaps the URL to `windows-host`.

It only ever suggests the swap when it has **proven** it helps (the current
address fails *and* `windows-host` on the same port succeeds), so a URL pointing
at a genuinely different machine on your LAN is left alone. Nothing is changed
without your click.

## Want `localhost` to just work instead?

If you'd rather not use the alias at all, switch WSL to **mirrored networking**
(Windows-side, stable across reboots). On Windows, add to
`C:\Users\<you>\.wslconfig`:

```ini
[wsl2]
networkingMode=mirrored
```

Then run `wsl --shutdown` and restart WSL. In mirrored mode `http://localhost:1234`
reaches the host directly, and the alias isn't needed.
