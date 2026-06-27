# How to do common things in MIRA

Practical recipes. Most can be done in the web UI or by asking MIRA in chat.

## Connect a channel
- **Telegram:** create a bot with @BotFather (`/newbot`), copy the token, add it under Settings → Channels → My channels (or "add my telegram bot"). The bot starts receiving immediately — no restart. Then **link your chat**: Settings → My channels → Link Telegram, and send the `LINK-XXXX-XXXX` code to the bot. (Linking is what captures your chat id for proactive messages.) See "Telegram: delivery mode and routing mode" below for polling vs webhook and Personal/Shared/Guest.
- **Signal:** register a number (the one interactive step — signal-cli sends a verification code), then add it as your Signal **account** (a phone number — not just the global toggle). MIRA **auto-installs** the runtime on first add: a pinned, checksum-verified signal-cli + a bundled Temurin JRE into `~/.mira/deps/` (Linux-x86_64 uses the self-contained native build, no JRE). It then starts the `signal-cli daemon --http` process for that account itself. Works on Linux, macOS, and **Windows**. To use your own signal-cli instead, point `channels.signal.cli_binary` at it (an explicit path overrides the managed copy). One-time ~100 MB download (~150 MB on Windows). (0.277.0+.)
- **Email:** add an email account (IMAP/SMTP, or Gmail/Outlook OAuth) under Settings → Email. Inbound mail from allowlisted senders becomes a conversation.

## Telegram: delivery mode and routing mode
Two independent settings on a Telegram account:

**Delivery mode — how MIRA receives messages:**
- **Polling (default):** MIRA long-polls Telegram's `getUpdates`. Works anywhere — behind NAT, on localhost, no public URL / port-forward / reverse proxy / TLS. The right choice for self-hosted/home installs. Cost: one poll loop per account.
- **Webhook:** Telegram pushes updates to `https://<host>/webhook/telegram/<account-id>` (authenticated by a secret-token header). Efficient and instant, but needs a public HTTPS URL Telegram can reach (domain + reverse proxy + cert) — for production deployments.

**Routing mode — who each inbound message runs as** (change it in place from the account row; it applies live):
- **Personal (default):** serves **only the owner's verified chat**. You link your own chat once (send a LINK code); any other sender is ignored. Linking is **per-bot**, so the same phone can own a personal bot under one MIRA account *and* be a different user on another bot (e.g. a shared family bot under a second account) — claiming a personal bot doesn't disturb your other links. *Pro:* simplest, private, secure-by-default (a stranger who finds the bot can't act as you). *Con:* one person only.
- **Shared:** one bot for several people. An admin creates the bot once; each member **keeps their own MIRA account** and links by sending their own LINK code. *Pro:* family/team bot, members never touch BotFather, each keeps their own context/memory/persona/voice. *Con:* one bot identity for all; members must link first; admin holds the token.
- **Guest-OK:** like Shared, but unlinked senders get a temporary **guest** session. *Pro:* open access. *Con:* anyone who finds the bot can use it; least private.

> Recommended for a household: a single **Shared** bot. Use **Personal** for a solo bot; **Guest-OK** only when you want it open to anyone.

## Turn on & tune proactive check-ins (Presence)
- **Enable:** finish onboarding (admins are auto-enabled; others enable once a safety contact is set), or use the setup wizard's Check-ins step, or ask MIRA in chat. Enabling needs a safety contact for non-admins.
- **Tune on the Presence page** (Settings → Presence): rhythm — Fuzzy band (1–N times/day at varied times) or Scheduled fixed times; tone sliders (warmth/playfulness/verbosity) + presets; which message types MIRA may send (check-in / joke / "what I've been up to" / follow-up / share / encouragement); whether it may mention what its agents did for you; daily briefing on/off + hour.
- **Or just tell MIRA in chat:** "message me less", "be funnier", "stop the jokes", "only check in in the mornings", "pause till Monday" — it updates the same settings.
- Test instantly with "Send a check-in now" / "Send a briefing now" in Settings → Notifications.

## Set up a care network (look out for a child or older adult)
- On **Settings → Presence → Care network**, pick a **care role**: *Just me* (default, no one alerted), *A child* (a guardian is alerted; gentle age-aware tone), or *An older adult* (a contact is alerted on silence or distress).
- For a child/older-adult role, choose the **contact to alert** and tick that **the person knows**. If you don't, MIRA discloses the arrangement to the person itself (woven naturally into a check-in) before it would ever reach out to the contact — it's never covert.
- What escalates: genuine distress, or three unanswered check-ins over ~48h. An *acute* signal (self-harm, acute physical symptoms) sends an urgent heads-up and shows the person crisis resources; a *concerning* one (low mood) sends a softer nudge. The contact only ever sees a one-line summary — never the conversation. MIRA gives a human a heads-up; it does not call emergency services.

## Get replies as voice
- Set your per-channel voice preference to **always** (Telegram/Signal). MIRA will send a voice note alongside text. Web plays TTS in the browser.
- Pick a voice (e.g. Kokoro `af_heart`, `bf_emma`). Enable Kokoro for natural local speech.

## Add an external tool (MCP)
- Go to the `/mcp` page → **Browse catalog** → pick a server (e.g. Filesystem, GitHub, Puppeteer) → **Use** → fill any path/key → **Save**. It connects immediately (no restart). Tools appear to the agent as `mcp__<server>__<tool>`.
- **Runtimes are handled for you.** Most catalog servers run via `npx` (Node) or `uvx` (Python/uv). If that runtime isn't installed, MIRA **asks permission** ("This MCP server needs Node.js (~55 MB) — install it now?") and, on approval, downloads a pinned, checksum-verified copy into `~/.mira/deps/` and connects the server. Works on Linux, macOS, and **Windows** — including a Windows service running as LocalSystem, which can't see a user-only Node/uv install. No manual Node/Python setup. (0.280.0+.)
- Admins can curate the catalog (add/edit/enable/disable entries).

## Make MIRA act in a browser
- Add the **Puppeteer** MCP server from the catalog. Then ask MIRA to navigate, click, fill forms, or screenshot a page. Screenshots render inline.

## Schedule something
- Ask MIRA to "remind me / check X every morning at 8" — it creates an automation (cron). View/cancel your schedules via the automations tools or the Automations page.

## Manage memory & notes
- MIRA writes memories automatically. Ask "what do you remember about me?" or "forget X". Use the wiki for longer notes ("add a wiki page about my project").

## Change a setting
- Your own: ask MIRA ("set my briefing hour to 7") or use the UI.
- Server-wide (admin): use Settings, or ask MIRA (it confirms global changes; secrets stay hidden; security/provider/proxy keys are protected).

## Control model "thinking" (reasoning)
- **See it:** when a reasoning model (gpt-oss, the qwen3 family, …) thinks, its chain-of-thought appears as a collapsed **Thinking** block above the reply — click to expand. (Reasoning streams over LM Studio's `reasoning` channel; MIRA captures it automatically.)
- **Suppress it:** reasoning models can burn the per-round token budget thinking before they act, which stalls tool loops. Turn on **Disable model reasoning (`/no_think`)** on **Settings → Providers** (applies everywhere — chat, channels, tool loops), or use the **Thinking / No-think** toggle in the chat window to override it for one conversation.
## Use the calendar
- The built-in calendar works on its own — no external service. Create events from the **Calendar** page or just ask MIRA ("add lunch with Dana Friday 1pm").
- **Connect your own external calendar** (each user, from their Calendar page): Google/Outlook via "Connect", or CalDAV (e.g. Nextcloud) by entering your server URL + username + an **app password**. An admin sets the provider once in Settings → Calendar first.
- **Org / team events (admins):** when creating an event, use the **Visibility** picker — "Everyone" for an organisation event all users see, or "Group: <name>" to scope it to an RBAC group's members.
- MIRA also overlays its own upcoming actions (automation runs, your daily briefing) on the calendar, read-only.

## Run on WSL with Windows-host services
- If MIRA runs in **WSL2** and your services (LM Studio, TTS, SearXNG) run on the **Windows host**, do NOT use the Windows LAN IP — a WSL2 NAT guest can't reach it. Use the `windows-host` alias instead: `http://windows-host:1234/v1`.
- Set it up once (root): `sudo mira wsl-host-alias-install` (also done by `sudo mira helper-install`). It maps `windows-host` to the WSL gateway IP and refreshes it on every boot. Check with `mira helper-status`.
- MIRA auto-detects URLs pointed at an unreachable Windows-host IP and offers a **one-click fix** (Settings banner) to swap them to `windows-host`. Alternative durable fix: WSL **mirrored networking** (`networkingMode=mirrored` in `.wslconfig`) so `localhost` works.

## Deploy / apply changes
- Most config applies live. Rust/binary changes need a rebuild + `systemctl --user restart mira`. Web UI changes need the bundle synced. MCP server changes hot-reload with no restart.

## Auto-route hard turns to a stronger model (admin)
- In `mira_config.json`, set `agent.reasoning`: `{ "enabled": true, "provider": "<a configured provider whose model is your strong one>", "effort": "medium" }`.
- MIRA then routes hard turns (code, math, long/multi-step prompts) to that provider and raises its reasoning effort; everything else uses the default. Tune the trip point with `min_chars`. Needs a rebuild + restart (it's a server feature).

## Benchmark MIRA's memory (LongMemEval)
- Download a LongMemEval dataset (e.g. `longmemeval_s.json`) — not bundled (not redistributable).
- Inspect only, no API spend: `mira bench memory --dataset path/to/longmemeval_s.json --dry-run`
- Smoke run (default 20 questions): `mira bench memory --dataset path/to/longmemeval_s.json`
- Full run + JSON report: `mira bench memory --dataset path/to/longmemeval_s.json --all --out results.json`
- Options: `--limit N`, `--question-type <type>` (the dataset is grouped by type, so use this to sample a specific category, e.g. `single-session-user`), `--answer-provider <id>`, `--judge-provider <id>`. Replays each conversation (with session dates) through MIRA's real memory + wiki pipeline; uses your configured providers (free if local, e.g. lmstudio).
