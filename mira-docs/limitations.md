# MIRA limitations & known gaps

An honest list so MIRA can set expectations rather than over-promise.

## Voice
- **Kokoro TTS is English only** (American/British) and has no speech-rate control. Other languages use eSpeak or a cloud/OpenAI-compatible backend.
- Kokoro and GPU TTS (Chatterbox AMD Vulkan) shine on the right hardware; on plain CPU, expect Piper-quality or slower synthesis.
- **Real-time voice turns** (cancel/barge-in, spoken style, speculative turns) are driven by an external voice client over the chat API — MIRA provides the turn lifecycle, not a built-in wake-word/audio loop. Cancelling a turn while a tool is mid-execution drops that tool call mid-flight (for controlled holding, start the turn speculatively — side effects wait for the commit). The per-turn `cached_tokens` figure is reported only by the OpenAI-compatible provider; the lmstudio/ollama providers report it as 0.

## Proactive delivery
- Check-ins and briefings only fire when the gateway is **running** at the scheduled moment. If the host is asleep/restarting, a window can be missed (the briefing now catches up later the same day, but check-ins don't replay).
- Delivery depends on the channel being reachable and (for Telegram) MIRA having learned your chat id from a prior inbound message.

## MCP
- Tool results that are **video** have no standard MCP source today (video isn't an MCP content-block type); audio and images are fully supported.
- Adding/removing servers hot-reloads, but a slow server reconnect is done inline on save (a refinement to reconnect only the changed server is on the backlog).
- stdio MCP servers run with the gateway's environment. MIRA can provision pinned Node/uv runtimes into `~/.mira/deps` (on a consent prompt) and prepend them to PATH for the spawned server, so npx/uvx work even when they aren't on the service PATH (e.g. a Windows LocalSystem service that can't see a user-PATH Node); only a runtime MIRA doesn't manage still needs to be reachable itself.

## Memory & time
- Recall is **time-aware** — every turn carries the current date/time and each recalled fact is tagged with when it was recorded — so MIRA reasons about whether a fact is past/current/future rather than treating old facts as current. But it relies on the **model** doing that reasoning from the labels: memories are **not auto-expired or rewritten** when their time passes (a past appointment stays in memory, just marked with its age), and relative phrases ("tomorrow", "next Friday") are stored as-said rather than resolved to an absolute date. A weaker local model may still occasionally miss the reconciliation.
- Auto-capture is **off by default** — a fresh install records nothing to long-term memory until you set `memory.auto_extract.mode` to `heuristic` (local) or `llm`. On request ("remember that…") still works regardless.
- The **knowledge-graph memory** (`memory.graph`) and the **consolidation** passes (`memory.consolidation`) are **designed but not yet wired to a live per-turn writer** — enabling them currently has no effect outside the benchmark (nothing populates `kg_edges` during normal turns), so aggregation still resolves against topic-grouped top-k retrieval, not exact graph set-membership. The per-turn graph writer is pending.
- External memory **providers** support the `files` (in-process) and `http` (SPI) transports today. The `mcp` transport has an adapter and starter mapping files but **isn't wired into the provider builder yet**, so an `mcp`-transport provider is **skipped at startup** — use `http` to attach an external service in the meantime.
- Deleted memories are soft-deleted (hidden from recall immediately) and **permanently purged** after `memory.retention_days` (default 30); the purge runs at startup and then every few hours. Deleting a corrected fact removes its whole version chain. **Erase permanently now** on the Memory page hard-deletes on demand.
- A **correction** replaces a fact only when it's the same subject + attribute and that attribute is single-valued (e.g. favourite colour, city, employer); multi-valued attributes (languages, pets, siblings) always keep every value, and when MIRA can't be sure it keeps both rather than overwrite. Because the design errs on the side of **keeping both**, some real corrections aren't collapsed into one row: when the attribute label drifts (e.g. "vehicle" vs "current_vehicle" — mostly canonicalised, not always), an attribute MIRA can't classify defaults to keep-both, an **unnamed** relative ("my sister") isn't keyed so a later update about them is kept alongside, and a memory written **before v0.383** has no key so it can't be auto-corrected. Two people sharing a first name share a key. A correction written **only in a non-Latin script** (e.g. an Arabic-only "I moved to Doha") isn't yet recognised as being about you, so it's kept alongside the old fact rather than replacing it — add an English/Latin form to collapse it. Credentials (passwords, API keys, card numbers) are refused by the memory writer on every write path, not stored. "Remember to …" is treated as a reminder, not a stored fact.
- **Personal facts mostly live in memory, not the wiki.** The wiki no longer files person *dossiers* (`pages/people/*`), so a person's facts don't get a shadow page. But the wiki extractor can still fold a personal detail into a general notes page (e.g. `pages/notes/observations.md`), and those copies are **not** reached by memory delete/correct/export — so deleting a memory may not remove every wiki mention yet. Fully separating personal facts from the wiki is pending. (Pages created before v0.383 aren't auto-migrated — remove lingering `pages/people/*` by hand.)
- **Daily summaries are not updated when you delete or correct a memory.** A day's conversation rollup is a snapshot written at the time; deleting or superseding a fact later doesn't touch the summaries that already mention it. So a deleted fact can still resurface in recall through that day's summary (the same kind of shadow copy as the wiki note above), until the summary is purged by retention. Deleting the summary itself (on the Daily summaries tab) removes that copy.

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
- Single-server multi-user is mature — separate accounts/memory/settings, SSO/OIDC and LDAP/AD sign-in, self-service signup/invites, admin session revocation, and **four-axis capability RBAC** (which providers, models, tools, and channels each group/user may use, plus per-task/session budget caps). The remaining gap is a dedicated multi-tenant SaaS control plane (org billing, cross-instance federation), not per-user isolation or permissions.
