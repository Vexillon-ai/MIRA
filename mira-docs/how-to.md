# How to do common things in MIRA

Practical recipes. Most can be done in the web UI or by asking MIRA in chat.

## Connect a channel
- **Telegram:** create a bot with @BotFather, copy the token, add it under Settings → Channels (or "add my telegram bot"). Message the bot once from your phone so MIRA learns your chat id (needed for proactive messages).
- **Signal:** register a number with signal-cli (operator setup), then add it as your Signal account.
- **Email:** add an email account (IMAP/SMTP, or Gmail/Outlook OAuth) under Settings → Email. Inbound mail from allowlisted senders becomes a conversation.

## Turn on proactive check-ins / daily briefing
- Enable companion mode and pick a preferred channel + quiet hours.
- Enable the daily briefing and set the hour. Test instantly with "Send a check-in now" / "Send a briefing now" in Settings → Notifications.

## Get replies as voice
- Set your per-channel voice preference to **always** (Telegram/Signal). MIRA will send a voice note alongside text. Web plays TTS in the browser.
- Pick a voice (e.g. Kokoro `af_heart`, `bf_emma`). Enable Kokoro for natural local speech.

## Add an external tool (MCP)
- Go to the `/mcp` page → **Browse catalog** → pick a server (e.g. Filesystem, GitHub, Puppeteer) → **Use** → fill any path/key → **Save**. It connects immediately (no restart). Tools appear to the agent as `mcp__<server>__<tool>`.
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
