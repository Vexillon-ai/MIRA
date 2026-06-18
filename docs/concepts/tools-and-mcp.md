---
title: Tools & MCP
description: How MIRA acts in the world — built-in tools, capability tiers, the audit trail, and connecting any MCP server.
sidebar:
  order: 4
---

A chat app produces text. MIRA **acts**: it searches the web, runs code, reads
your calendar, generates an image, or drives a browser — by calling **tools**. A
tool is a capability the agent can invoke mid-conversation to get information or
change something, then continue with the result. This page explains MIRA's tool
model: what's built in, how tools are tiered for safety, how they're audited, and
how MCP lets you add more.

## Built-in tools

Every MIRA instance ships with a core set of tools the agent can reach for:

- **Web search & fetch** — search the web, and fetch or preview a URL.
- **Sandboxed code execution** — run code in an isolated environment to compute,
  transform data, or test an idea.
- **Calendar** — create, list, update, and delete events.
- **PDF extraction** — pull text out of a PDF you share.
- **Summarise & recall** — condense a long conversation, and recall earlier
  history and stored memory.
- **Image & video generation** — turn a prompt into an image or a short video
  clip, rendered inline in chat. (These need an image/video provider key
  configured; see [Generate images & video](../guides/generate-images-and-video.md).)
- **Background sub-agents** — spawn a worker to grind through a longer task while
  the conversation continues.

There's also **date/time/timezone** help, **conversation summarisation**, and the
**automation tools** that let the agent schedule its own follow-ups (see
[Schedule automations](../guides/schedule-automations.md)). The agent picks the
right tool for a turn; you rarely call one by name.

## Capability tiers — why they matter

Not all tools carry the same risk. Reading a web page is harmless; writing to your
filesystem or running a shell command is not. MIRA classifies every tool by the
**capability** it needs, so the operator can reason about — and restrict — what
the agent can do:

- **Pure** — computes from its inputs, touches nothing external (date maths,
  summarising text).
- **Network** — reaches out to the internet (web search, fetch).
- **Filesystem** — reads or writes files.
- **Code** — runs code in a sandbox.
- **System** — touches the host more deeply (e.g. an opt-in shell, enabled only
  in trusted deployments).

These tiers feed MIRA's **tool policy layer**. An admin can restrict, per user or
group, exactly which tools are allowed — so a kid-safe account might get web
search and calendar but never code execution or the shell. The tier is the
vocabulary that makes those rules legible: you grant capabilities, not just
individual tool names. See
[Security & multi-user](security-and-multi-user.md) for how that's enforced.

## The audit trail

Because tools *do* things, MIRA records what they did. Every tool call is part of
the conversation's trace — you can see, per message, which tools ran, with what
inputs, and what they returned (the "tool/thinking traces" in the web UI).
Automations keep their own **run history** too: each scheduled or triggered action
logs its outcome, duration, and an output snippet. Nothing the agent does on your
behalf is invisible.

## MCP host mode — adding any tool

MIRA's built-in tools are a starting point, not a ceiling. Through **MCP (the
Model Context Protocol)**, MIRA can connect to **external tool servers** and
surface their tools as its own. An MCP server is a small program that publishes
tools; MIRA acts as the **host** that connects to it.

Connect one — browser automation, GitHub, a database, your own script — and its
tools appear to the agent named `mcp__<server>__<tool>`, alongside the built-ins,
hot-reloaded with no restart. From the agent's point of view there's no
difference between a built-in tool and an MCP one; from yours, it means MIRA's
abilities are open-ended. Servers are **per-user**, and an **admin-curated
catalogue** makes the common ones a single click.

The same capability tiers and audit trail apply to MCP tools, so adding a server
doesn't bypass the safety model — it extends it.

See **[Add tools with MCP](../guides/add-tools-with-mcp.md)** for the step-by-step.

## Skills & plugins — how capabilities are packaged

Related tools are grouped into **skills** — a named bundle (companion mode,
memory, the wiki, the calendar, research, summarise, and so on) that the agent
draws on. The Skills page lists what's available, and each skill can be
**enabled or disabled per user** — a kid-safe account might keep the calendar
but turn research off.

Skills come in two tiers:

- **System skills** — the built-in capabilities that ship inside MIRA. Their
  tools (and any background service or UI) are part of the binary, so they're
  **enabled/disabled, never uninstalled** — the Skills page shows a **System**
  badge and no remove button, and marks them **verified ("MIRA built-in")**
  because the binary itself is their trust anchor.
- **Installable extensions** — optional, self-contained add-ons packaged as a
  **`.mirapkg`**: an MCP server, an external channel bridge, or a third-party
  skill. An admin installs and removes these, and MIRA verifies a package's
  signature against a trusted publisher key before trusting it. This is the
  install/uninstall lane — distinct from the always-present system skills.

The rule of thumb: built-in capabilities are managed by **enabling/disabling**;
optional ones are managed by **installing/removing**.

## How tools fit the bigger picture

Tools are how a single turn reaches beyond text. When a task needs many tools,
several steps, or a dedicated persona, MIRA composes them into
**[agents and orchestration](agents-and-orchestration.md)** — named agents with
their own tool allowlists, and workflows that chain them. Tools are the verbs;
agents and workflows are the sentences.

## Next steps

- **[Add tools with MCP](../guides/add-tools-with-mcp.md)** — connect your first
  external server.
- **[Agents & orchestration](agents-and-orchestration.md)** — compose tools into
  reusable agents and multi-step workflows.
- **[Security & multi-user](security-and-multi-user.md)** — how tool policy and
  capability limits are enforced.
