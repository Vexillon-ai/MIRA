# What MIRA is

MIRA (Multi-tasking Intelligent Responsive Assistant) is a **self-hosted personal AI agent**. You run it on your own machine; it talks to whichever LLM provider you configure (Anthropic, OpenAI, OpenRouter, DeepSeek, Gemini, or a local/OpenAI-compatible server). Your conversations, memory, and data stay on your hardware.

## How MIRA is different from a chat app

- **Reaches you on real channels.** MIRA isn't only a web tab — it can message you on **Signal** and **Telegram**, send and receive **email**, and push **browser/phone notifications**. It can start conversations with you (proactive check-ins, a daily briefing), not just answer when asked.
- **Remembers.** A growing **memory** of facts about you plus a **wiki** of longer notes, both used to personalise replies across sessions and channels.
- **Acts, via tools.** Beyond chat, MIRA runs tools: web search/fetch, code execution, calendar, PDF extraction, scheduled automations, and — through **MCP (Model Context Protocol)** — any external tool server you connect (browser automation, GitHub, databases, your own scripts…).
- **Speaks.** Built-in **text-to-speech** (Kokoro / Piper) and **speech-to-text** (whisper), so check-ins and replies can arrive as voice notes and you can talk back.
- **Multi-user.** One MIRA instance can serve several people with separate accounts, memory, and settings, plus an admin who manages the server.

## The shape of the system

- A single **gateway** process hosts the agent, the HTTP/web API, the channel pollers, and the schedulers.
- **Settings** come in two kinds:
  - **Operator/global settings** (`mira_config.json`) — providers, channels, security, TTS, etc. Admin-managed, server-wide.
  - **Per-user settings** — each person's own voice preferences, companion config, channel accounts, connected MCP servers, profile.
- Data lives under a **data directory** (`~/.mira/data` by default): the auth/user DB, memory DB, wiki, companion DB, artifacts, and TTS models. You can put it elsewhere — e.g. a backed-up volume or external disk — by choosing it during `mira setup`, setting `data_dir` in the config, or passing `--data-dir` / `MIRA_DATA_DIR`. `mira install` pins your choice into the service so it survives across restarts and supervisor accounts.

## Asking MIRA about itself

MIRA ships with this documentation built in. Ask things like *"what can you do?"*, *"how do I enable companion check-ins?"*, *"what does the `agent.max_tool_rounds` setting do?"* and MIRA will answer from these docs. For the current *value* of a setting (and to change it), MIRA respects your access level — you can see and change your own settings; operator/global settings are admin-only and secrets are never shown.
