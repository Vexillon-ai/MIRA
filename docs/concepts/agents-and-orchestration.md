---
title: Agents & orchestration
description: How MIRA's agent model works — the main loop, background sub-agents, named agents, workflows, and the fleet view that watches them.
sidebar:
  order: 5
---

At its core MIRA is an **agent**: a loop that takes your message, decides which
[tools](tools-and-mcp.md) to call, calls them, reads the results, and repeats
until it has an answer. Most of the time that's all you see — a conversation that
quietly uses tools. But for bigger jobs MIRA can spin up *more* agents, name and
reuse them, and chain them into pipelines. This page explains how those pieces
fit together.

## The main agent loop

Every conversation runs through one **main agent**. It holds your context — your
[memory and wiki](memory-and-wiki.md), the current channel, your settings — and
works the request by calling tools across several rounds until it's done. This is
the agent you talk to directly, and the one that decides when a task is big
enough to hand off.

## Background sub-agents

Some tasks are too long to run inside a single reply — a deep research dig, a
multi-file analysis, a job that takes minutes. For these, the main agent
**spawns a background sub-agent**: a worker that goes off and does the task on its
own, then **reports back** with a structured result. The sub-agent doesn't talk
to you directly; it works to a delimited brief and returns its findings to the
agent that launched it, which folds them into the conversation.

This keeps your chat responsive — MIRA can acknowledge the request, kick off the
worker, and come back when it's finished — and it's the foundation everything
below builds on.

## Named agents

A **named agent** is a *saved* agent profile: a persona (system prompt), a tool
allowlist, an optional model alias, and an optional budget, addressed by a
lowercase `@handle`. Where a background sub-agent is spun up ad hoc for one task,
a named agent is configured once and **reused** — invoked by name in chat
(*"ask `@researcher`…"*) or delegated to autonomously inside MIRA's automations
and proactive work.

Named agents make MIRA's behaviour predictable and composable: an `@researcher`
always has the same tools and model, so you (and workflows) can rely on it.
See [Named agents & workflows](../guides/named-agents-and-workflows.md) to build
one.

## Workflows: orchestration over a DAG

A **workflow** chains named agents and skills into a **DAG** — a directed graph
of steps with no cycles. Each step targets an agent or skill, carries a brief
that can interpolate the run input (`{{input}}`) and any upstream step's result
(`{{steps.<id>.output}}`), and declares which steps it depends on.

The **orchestrator** runs that graph in **waves**: steps with no outstanding
dependencies execute in **parallel**, each step starts as soon as its
dependencies finish, and outputs feed forward to the steps that need them. This
turns a vague "do these five things" into a precise pipeline where independent
work happens at once and dependent work waits.

Workflows add three controls that make long, autonomous runs trustworthy:

- **Continue-on-error** — a failing step skips its dependents but lets
  independent branches finish, instead of sinking the whole run.
- **Conditional `when` guards** — a step runs only if an upstream output matches,
  so the graph can branch on what it finds.
- **Human-in-the-loop checkpoints** — a step can require approval; the run
  **pauses** and waits for a person, and because the pause is persisted it
  **survives a restart** and resumes exactly where it stopped. This is how you
  gate a step that spends money or sends something irreversible.

Every run is persisted with per-step status and output, so a workflow is both an
automation *and* an audit trail.

## Watching the fleet: observability

Running many agents is only useful if you can see what they're doing. The
**Agents** page is a **live fleet view** over every active worker:

- **Per-agent progress** — what each worker is working on right now.
- **Cost and burn-rate** — fleet totals and per-worker spend, so a run that's
  burning budget is visible before it's expensive.
- **Typed failure reasons** — when a worker fails, it says *why* in a structured
  way, rather than vanishing.
- **A browsable artifact workspace** — each task's output files, viewable in
  place, so you can inspect what an agent actually produced.

Together with the persisted workflow run history, this gives MIRA an
observability story closer to a job scheduler than a chat app.

## Code-agent adapters

MIRA can also delegate to **external coding agents** as sub-agents. Through
adapters, a worker can wrap a tool like **Claude Code** or **opencode**, hand it
a project folder and a brief, let it work, and verify the result before reporting
back. This lets MIRA orchestrate real coding tasks using purpose-built tools
while keeping the same spawn-and-report-back model as any other sub-agent.

## See also

- [Named agents & workflows](../guides/named-agents-and-workflows.md) — the
  hands-on guide to building both.
- [Tools & MCP](tools-and-mcp.md) — the tools agents call and how to add more.
- [Schedule automations](../guides/schedule-automations.md) — run agents and
  workflows on a schedule.
