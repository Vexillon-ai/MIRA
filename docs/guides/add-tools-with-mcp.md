---
title: Add tools with MCP
description: Extend MIRA with external tool servers — browser automation, GitHub, databases, your own scripts — through the Model Context Protocol.
sidebar:
  order: 5
---

MIRA can use **external tool servers** through **MCP (the Model Context
Protocol)** — an open standard for exposing tools to an AI agent. Connect an MCP
server and its tools become MIRA's tools: it can drive a browser, query GitHub,
read and write files, talk to a Postgres database, or run a script you wrote — no
rebuild, no restart.

This guide shows how to add one. For the bigger picture of how MIRA's tools work,
see [Tools & MCP](../concepts/tools-and-mcp.md).

## What MCP gives you

An MCP server is a small program that publishes a set of tools. MIRA acts as an
MCP **host**: it connects to the server, lists its tools, and offers them to the
agent during a turn. Popular servers cover:

- **Browser automation** (Puppeteer) — navigate, click, fill forms, screenshot.
- **GitHub** — read issues and pull requests, search code, open issues.
- **Filesystem** — read and write files in a directory you choose.
- **Databases** (Postgres and others) — run queries against a connection you
  provide.
- **Your own scripts** — wrap any tool you like as an MCP server in any language.

Servers are **per-user**: each person on a MIRA instance connects their own, with
their own paths and keys. What you connect, only you (and the agent acting for
you) can use.

## Add a server from the catalogue

The quickest path is the built-in catalogue of recommended servers.

1. Open the **`/mcp`** page (it's in the sidebar as **MCP servers**).
2. Click **Browse catalogue** and pick a server — for example **Filesystem**,
   **GitHub**, or **Puppeteer**.
3. Click **Use**. MIRA shows any settings the server needs.
4. **Fill in any path or key** the server asks for — a directory to expose, an
   API token, a database URL. Leave the rest at their defaults.
5. Click **Save**.

That's it. The server **connects immediately — no restart**. Its tools are
available to the agent on your very next message.

> **Runtimes are installed for you.** Most catalogue servers run via `npx`
> (Node) or `uvx` (Python). If that runtime isn't on the host yet, MIRA asks
> first — *"This MCP server needs Node.js (~55 MB) — install it now?"* — and on
> your approval downloads a pinned, checksum-verified copy into `~/.mira/deps/`
> and connects the server. You don't need to install Node or Python yourself,
> on any platform (Linux, macOS, or Windows).

> The catalogue is **admin-curated**. An admin adds, edits, enables, or disables
> entries, so the list you see is the set your instance's operator trusts. You
> can also add a server that isn't in the catalogue by giving MIRA the command
> (for a stdio server) or URL (for a Streamable-HTTP server) directly.

## How tools show up

Once a server is connected, its tools appear to the agent named
`mcp__<server>__<tool>` — for example `mcp__github__search_issues` or
`mcp__puppeteer__screenshot`. The double-underscore prefix keeps names from
different servers from colliding.

Servers **hot-reload**: add, change, or remove one and MIRA picks up the new tool
set without a restart. Tools that return **images, audio, or video** render
inline in chat — a screenshot or a generated chart shows up right in the
conversation.

## Example — let MIRA drive a browser

Say you want MIRA to open a web page and grab a screenshot.

1. On the **`/mcp`** page, **Browse catalogue** and choose **Puppeteer**.
2. Click **Use**, then **Save**. (Puppeteer needs no key — it bundles its own
   headless browser.)
3. Back in chat, ask MIRA to do something browser-shaped:

   > Open example.com, accept any cookie banner, and screenshot the page.

MIRA now has `navigate`, `click`, `fill`, and `screenshot` tools. It navigates,
clicks, fills forms, and captures the page — and the **screenshot renders inline**
in your conversation. From here you can ask it to log in to a dashboard, scrape a
table, or fill a form, all in plain language.

## Troubleshooting

- **The server isn't in the catalogue.** Ask an admin to add it, or connect it
  directly by giving MIRA the server's launch command or HTTP URL.
- **A tool needs a key I didn't set.** Reopen the server on the `/mcp` page and
  fill in the missing field — changes apply on save, no restart.
- **A stdio server won't start.** For `npx`/`uvx` servers MIRA manages the
  runtime itself (it prompts to install Node/uv and runs them from
  `~/.mira/deps/`), so these work out of the box. A *custom* stdio server whose
  command MIRA doesn't manage still needs its command + runtime reachable — if
  MIRA runs as a background service it uses the **machine/system PATH**, not your
  user PATH, so install that runtime system-wide or give the full path in the
  command. Flag it to your admin if unsure.

## Next steps

- **[Tools & MCP](../concepts/tools-and-mcp.md)** — how MIRA's tool model and
  capability tiers work.
- **[Named agents & workflows](named-agents-and-workflows.md)** — give a saved
  agent a tight tool allowlist that includes your MCP tools.
- **[Schedule automations](schedule-automations.md)** — have MIRA run an
  MCP-backed task on a schedule.
