---
title: Named agents & workflows
description: Save reusable agent profiles, invoke them by name, and chain them into multi-step workflows.
sidebar:
  order: 6
---

A **named agent** is a reusable agent profile you set up once and call by name —
say, an `@researcher` that has web tools and a research model, or an `@editor`
with a strict house-style persona. A **workflow** chains several of those agents
(and [skills](../concepts/agents-and-orchestration.md)) into a multi-step
pipeline that runs on its own.

This guide shows how to create both. For the bigger picture of how MIRA's agents
fit together, see [Agents & orchestration](../concepts/agents-and-orchestration.md).

## Create a named agent

Named agents are **admin-managed** and live server-wide. You build them on the
**Named Agents** admin page.

1. Go to **Admin → Named Agents** and add an agent.
2. Give it a lowercase **`@handle`** — this is how you'll address it (e.g.
   `@researcher`). Keep it short and memorable.
3. Fill in the profile:
   - **Persona / system prompt** — who this agent is and how it should work.
   - **Tool allowlist** — exactly which tools it may use. An `@researcher`
     might get web search and fetch; a writing agent might get none. Narrowing
     the toolset keeps the agent focused and safe.
   - **Model alias** (optional) — pin the agent to a specific model, e.g. a
     cheaper model for summarising or a stronger one for analysis.
   - **Budget** (optional) — a spend cap for a single run, so a runaway agent
     stops itself.
4. Save. The agent is now reusable anywhere on the instance.

## Use a named agent

Once saved, an agent can be invoked two ways:

- **By name, in chat.** Mention its handle — *"ask `@researcher` to find the
  latest on X"* — and MIRA delegates that turn to the agent's persona, tools,
  and model.
- **Autonomously.** MIRA can delegate to a named agent on its own — inside an
  [automation](schedule-automations.md) or a proactive task — without you asking
  each time. Set the agent up once and it becomes part of MIRA's toolkit.

Running agents show up on the **Agents** page (the live fleet view), where you
can watch per-agent progress, spend, and any artifact files the agent produced.

## Chain agents into a workflow

A **workflow** is a saved pipeline that runs several agents/skills in order,
passing outputs forward. You build one on the **Workflows** admin page.

Each workflow is a set of **steps**, and each step:

- **targets** a named agent or a skill;
- carries a **brief** — the instruction for that step. The brief can
  interpolate the run input as `{{input}}` and any earlier step's result as
  `{{steps.<id>.output}}`, so a later step works on what an earlier one produced;
- **declares its dependencies** — which steps must finish first.

Those dependencies form a **DAG** (a graph with no cycles). The orchestrator
runs it in **waves**: steps with no outstanding dependencies run in parallel, and
each step starts the moment its dependencies finish. So a "gather" step can fan
out to three independent research steps at once, then a final "write-up" step
waits for all three and combines their outputs.

### Make steps resilient or conditional

In the step editor you can mark a step as:

- **Continue-on-error** — if this step fails, MIRA skips the steps that depend on
  it but lets independent branches finish, rather than failing the whole run.
- **Conditional (`when` guard)** — the step runs only if an upstream output
  matches a condition you set. Use this to branch: e.g. only run the "escalate"
  step when an earlier check reports a problem.
- **Requires approval (human-in-the-loop)** — the run **pauses** before this step
  and waits for a person to approve or reject it. The pause is saved to disk, so
  it **survives a restart**; approving resumes from exactly where it left off.
  Use this to gate a step that sends an email or spends real money.

You can also set a per-step **budget**, the same as for a standalone agent.

## Run a workflow

There are two ways to start a run:

- **From the UI.** On the Workflows page, open the workflow, enter an **input**,
  and run it. The page shows **run history** with live per-step status and each
  step's output as it lands.
- **Conversationally.** Just ask MIRA — *"run the weekly brief"*. It kicks off
  the workflow, returns immediately, and pings you when the run completes.

Every run is **persisted** with per-step status and output, so you can open an
old run later and see exactly what each step did — useful when a result looks
off and you want to find which step went wrong.

## See also

- [Agents & orchestration](../concepts/agents-and-orchestration.md) — how the
  agent model, sub-agents, named agents, and workflows fit together.
- [Schedule automations](schedule-automations.md) — run a workflow or agent on a
  cron schedule.
- [Add tools with MCP](add-tools-with-mcp.md) — extend the toolset an agent can
  be granted.
