# MIRA limitations & known gaps

An honest list so MIRA can set expectations rather than over-promise.

## Voice
- **Kokoro TTS is English only** (American/British) and has no speech-rate control. Other languages use eSpeak or a cloud/OpenAI-compatible backend.
- Kokoro and GPU TTS (Chatterbox AMD Vulkan) shine on the right hardware; on plain CPU, expect Piper-quality or slower synthesis.

## Proactive delivery
- Check-ins and briefings only fire when the gateway is **running** at the scheduled moment. If the host is asleep/restarting, a window can be missed (the briefing now catches up later the same day, but check-ins don't replay).
- Delivery depends on the channel being reachable and (for Telegram) MIRA having learned your chat id from a prior inbound message.

## MCP
- Tool results that are **video** have no standard MCP source today (video isn't an MCP content-block type); audio and images are fully supported.
- Adding/removing servers hot-reloads, but a slow server reconnect is done inline on save (a refinement to reconnect only the changed server is on the backlog).
- stdio MCP servers run with the gateway's environment; their runtimes (npx/uvx) must be reachable on the service PATH.

## Web / browsing
- Built-in web tools (`web_fetch`, `url_preview`) are **read-only**. To *act* in a browser (click, fill, log in) you need the Puppeteer MCP server.

## Settings via chat (this feature)
- MIRA can read/explain settings and change **your own**; admins can change global settings with confirmation. **Secrets are never readable.** Protected keys (security, providers, proxy) are steered to the UI rather than changed by chat.
- Hand-written feature docs can lag the code between releases; the **settings reference is generated from the schema** so it stays accurate.

## Calendar
- External sync is **one-way (read-only mirror)** — MIRA pulls external events in; it doesn't push native events back out. External events can't be edited from MIRA.
- The instance syncs **one external provider** at a time (`calendar.sync_provider`); each user connects their own account to that provider. CalDAV requires an **app password** (not your login password).
- **Org / per-group events are admin-managed** — only admins create/edit/delete them; group events are visible to a group's members (set by an admin). There are no per-group managers yet.
- The **agenda overlay** shows each automation's *next* fire (not every future occurrence) and the daily briefing; it's read-only.

## Platform
- On **WSL2 (NAT mode)**, MIRA can't reach the Windows host by its LAN IP — only via the `windows-host` alias (set up by `sudo mira wsl-host-alias-install`) or by switching WSL to mirrored networking. MIRA detects misrouted Windows-host URLs and offers a one-click fix, but the alias setup itself needs root (one time).
- Code execution (`code_run`) is sandboxed on **all platforms** via the WASM/WASI backend (Wasmtime + bundled WASI CPython); the Linux namespace+seccomp backend is the higher-fidelity default where a rootfs is installed. Caveats: the **scientific Python** backend (Pyodide-on-Node, opt-in via `sandbox.pyodide.enabled`) runs the user code in wasm but the **Node host process is privileged** — a weaker boundary than wasmtime, so it's for semi-trusted code. The Linux namespace backend without a provisioned rootfs still shares the host filesystem (run `mira sandbox install python`, or use the WASM backend, for full FS isolation).
- Native plugin **egress filtering** needs the privileged helper (one-time `sudo mira helper-install`). Without it, a native plugin that declares an egress allowlist runs with **no network at all** (fail-closed), not filtered. The container-tier fallback proxy is **HTTP/S-only** — non-HTTP egress through it is denied.
- Email webhook ingest does not verify per-provider HMAC signatures (the path secret is the authenticator); Mailgun multipart routes are unsupported (use the URL-encoded forward route).

## Multi-user
- Single-server multi-user works (separate accounts/memory/settings), but team/RBAC polish (SSO, fine-grained permissions) is basic compared to dedicated multi-tenant tools.
