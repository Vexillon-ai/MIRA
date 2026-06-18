# MIRA documentation

> 📖 **Prefer a searchable site?** Read these docs at
> **[vexillon.ai/docs](https://vexillon.ai/docs)** — same content, with search
> and navigation. This folder is the source of truth; the site is built from it.

**MIRA** is a self-hosted personal AI agent. You run it on your own hardware; it
talks to whichever LLM provider you configure, reaches you on real channels
(web, Signal, Telegram, email, push…), remembers you, and acts through tools.

New here? Start with **[What is MIRA?](concepts/overview.md)**, then
**[Install MIRA](getting-started/installation.md)**.

## Documentation map

The docs are organised by what you're trying to do
([Diátaxis](https://diataxis.fr/)):

### 🚀 Getting started — *tutorials, hand-held*
- [Install MIRA](getting-started/installation.md) — get it running on your machine
- [Quickstart](getting-started/quickstart.md) — first-run setup and your first conversation

### 🔧 Guides — *how-to recipes for a specific task*
- [Connect a channel](guides/connect-a-channel.md) — reach MIRA on Telegram, Signal, email, and more
- [Proactive check-ins & daily briefing](guides/proactive-checkins-and-briefing.md) — let MIRA message you first
- [Voice replies & talking to MIRA](guides/voice-replies.md) — spoken replies and voice input
- [Schedule automations](guides/schedule-automations.md) — recurring and event-triggered actions
- [Add tools with MCP](guides/add-tools-with-mcp.md) — extend MIRA with external tool servers
- [Named agents & workflows](guides/named-agents-and-workflows.md) — reusable agents, chained into workflows
- [Generate images & video](guides/generate-images-and-video.md) — create images and short clips in chat
- [Manage users & access](guides/manage-users.md) — invites, roles, capability limits, sessions
- [Single sign-on (OIDC & LDAP)](guides/single-sign-on.md) — connect your identity provider
- [Back up & restore](guides/backup-and-restore.md) — protect and move your install

### 💡 Concepts — *how and why it works*
- [What is MIRA?](concepts/overview.md) — the big picture and how it differs from a chat app
- [Memory & the wiki](concepts/memory-and-wiki.md) — how MIRA remembers you
- [Channels](concepts/channels.md) — the model behind reaching you everywhere
- [Tools & MCP](concepts/tools-and-mcp.md) — how MIRA acts, and how to extend it
- [Agents & orchestration](concepts/agents-and-orchestration.md) — sub-agents, named agents, workflows
- [Security & multi-user](concepts/security-and-multi-user.md) — accounts, isolation, and access control

### 📑 Reference — *look up a precise fact*
- [Settings reference](reference/settings.md) — every configuration option
- [Command-line reference](reference/cli.md) — the `mira` CLI commands

## Contributing to the docs

These docs are written for end users and feed the Vexillon documentation site
directly. If you're adding or editing a page, read
**[CONTRIBUTING.md](CONTRIBUTING.md)** first — it covers the Diátaxis structure,
the required frontmatter, and the house style so the site build stays a clean
pass-through.

> Looking for *engineering* design docs (architecture, phase plans)? Those are
> internal and live in `design-docs/`, not here.
