---
title: Reach MIRA from away (remote access)
description: Use the mobile app to reach your home MIRA server from anywhere — without opening router ports — with Tailscale, or any tunnel/DDNS via a remote URL.
sidebar:
  order: 15
---

By default the mobile app connects to MIRA over your **local network**. To reach
it from *away* — on mobile data, at work, travelling — you need a stable address
that works from outside your home, **without** poking holes in your router.

The recommended way is **Tailscale**: a private network (WireGuard under the
hood) that links your devices directly, with no inbound ports opened. MIRA
detects your Tailscale address automatically and puts it in the pairing QR, so
the app just works both at home and away.

## Recommended: Tailscale

You install Tailscale **once** on the server and once on your phone — MIRA
doesn't bundle a VPN, it just removes the URL-typing and guesswork.

1. On the **server**, install and bring Tailscale up, then let it serve MIRA
   over HTTPS:

   ```sh
   curl -fsSL https://tailscale.com/install.sh | sh
   sudo tailscale up
   sudo tailscale serve --bg http://localhost:8080   # use your MIRA port
   ```

2. In the **Tailscale admin console**, enable **MagicDNS** and **HTTPS
   certificates** (Settings → DNS). This gives your server a real
   `https://<name>.<tailnet>.ts.net` address with a valid certificate.

3. Install **Tailscale on your phone** and sign into the same tailnet.

4. In MIRA: **Settings → Server → Remote access**. It shows whether Tailscale is
   installed, up, and serving HTTPS, and the **remote URL** it detected. When it
   reads *serving HTTPS*, you're done.

5. Pair the phone as usual (**Settings → Notifications → Pair a mobile device**).
   The QR now carries **both** your LAN address and the Tailscale remote URL —
   one scan, and the app connects at home *or* away, picking whichever is
   reachable.

If Tailscale is up but not yet serving HTTPS, the Remote access panel tells you
exactly which command to run and links the docs.

## Other tunnels: set a remote URL

If you use **Cloudflare Tunnel**, **dynamic DNS**, or a **reverse proxy** with
your own domain, just tell MIRA the address. In **Settings → Server → Remote
access**, set **Remote URL** to your externally-reachable base URL, e.g.:

```
https://mira.example.com
```

Or set it via config / environment:

```toml
[server]
remote_url = "https://mira.example.com"
```

```sh
MIRA_REMOTE_URL="https://mira.example.com"
```

It must be an absolute `http`/`https` URL. Once set, that address is embedded in
the pairing QR as the remote endpoint (it takes precedence over Tailscale
auto-detection).

## Running in a container (Tailscale sidecar)

If you run MIRA in Docker, the tidy pattern is a **Tailscale sidecar**: a
`tailscale` container joins your tailnet and MIRA shares its network, so
Tailscale fronts MIRA over HTTPS with nothing exposed on the host.

```yaml
# docker-compose.yml
services:
  tailscale:
    image: tailscale/tailscale:latest
    hostname: mira                         # → MagicDNS name mira.<tailnet>.ts.net
    environment:
      - TS_AUTHKEY=${TS_AUTHKEY}           # a reusable/ephemeral key from the admin console
      - TS_STATE_DIR=/var/lib/tailscale
      - TS_SERVE_CONFIG=/config/serve.json # front MIRA over HTTPS (see below)
      - TS_USERSPACE=false
    volumes:
      - tailscale-state:/var/lib/tailscale
      - ./ts-serve.json:/config/serve.json:ro
    devices:
      - /dev/net/tun:/dev/net/tun
    cap_add: [NET_ADMIN]
    restart: unless-stopped

  mira:
    image: your/mira:latest
    network_mode: service:tailscale        # share the tailscale container's network
    # ...your usual MIRA volumes/env; MIRA listens on 127.0.0.1:8080 here...
    restart: unless-stopped

volumes:
  tailscale-state:
```

```json
// ts-serve.json — serve MIRA's local port over HTTPS on the tailnet
{
  "TCP": { "443": { "HTTPS": true } },
  "Web": {
    "${TS_CERT_DOMAIN}:443": {
      "Handlers": { "/": { "Proxy": "http://127.0.0.1:8080" } }
    }
  }
}
```

**Set `remote_url` explicitly in this setup.** MIRA's auto-detection shells out
to the local `tailscale` CLI — but in the sidecar pattern that CLI lives in the
*tailscale* container, not MIRA's, so auto-detection returns nothing. Tell MIRA
its address directly (it's `https://<hostname>.<tailnet>.ts.net`):

```yaml
  mira:
    environment:
      - MIRA_REMOTE_URL=https://mira.your-tailnet.ts.net
```

Everything else works the same — that URL goes straight into the pairing QR.

## How it fits together

- **`base_url`** in the pairing QR = your **LAN / local** address (unchanged).
- **`remote_url`** = the **away** address (Tailscale-detected or configured),
  added only when known.
- The app stores both from one scan and auto-selects whichever responds.

None of this opens inbound ports on your router — Tailscale connects outbound,
and a remote URL just points at whatever tunnel you already run.

## Security notes

- The Remote access page is **admin-only** and never shows pairing secrets or
  tokens — it's detection + configuration + guidance.
- MIRA shells out to the local `tailscale` CLI read-only (status/serve), with a
  timeout, and treats a missing binary as "not detected" — it never blocks or
  crashes.
