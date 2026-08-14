---
title: Run long research automations
description: Set up a multi-cycle research task that searches, reads, and writes findings into your wiki — and make sure it actually finishes and records what it found.
sidebar:
  order: 17
---

A **research automation** is a standing prompt that runs on a schedule, gathers
material from the web, and writes what it learns into your **wiki** — a running
"state of the art" page that keeps improving on its own. It's one of the most
useful things MIRA can do unattended, but a long research turn has two limits you
need to set correctly, or it will **search a lot and never write anything**.

This guide covers those two limits and the review gate that decides where the
findings land.

## The shape of a research automation

Ask MIRA in chat, for example:

> "Every morning at 7am, research the latest developments in open-weight LLMs.
> Do a narrow slice each run, and **write your findings into my wiki before**
> you summarise them to me."

MIRA creates a schedule whose action is a **prompt** with web tools allowed. Each
run: search → read a few sources → append findings to a wiki page → tell you a
short summary.

## Limit 1 — the iteration budget (`max_iterations`)

A prompt turn runs a **tool loop**: search, read a result, search again, and
eventually call the **wiki-write** tool. Each of those is one *round*, and the
turn is capped at `max_iterations` rounds (default **10**). If the budget is too
low, the turn spends every round on `web_search`/`web_fetch`, gets forced into a
final prose reply, and the **wiki-write call never happens** — the single most
common cause of "it did the research but nothing showed up in my wiki".

For real research, give it room:

> "Set that research task's iteration budget to **24**."

A good rule of thumb: enough rounds for several search/read cycles **plus** the
write at the end — 20–30 is reasonable for a focused slice.

## Limit 2 — the action timeout (`max_action_secs`)

Every automation action has a wall-clock ceiling so a hung job can't run forever.
The default is **300 seconds (5 minutes)** — fine for a reminder, too short for
research. A multi-cycle research run legitimately needs **~15 minutes**, and if
it's cut off mid-run you'll see a failure like *"automation action exceeded its
300s wall-clock ceiling and was aborted"* — often *right before* the write.

Two ways to fix it:

- **Per task (recommended):** raise just this action's ceiling —
  > "Give that research task a **15-minute** timeout."

  MIRA sets `max_action_secs: 900` on the action. Other automations keep the
  default.
- **Globally (admin):** raise `automations.max_action_secs` in
  **Settings** (or `mira_config.json`) if most of your automations run long.

A long action no longer blocks anything else — MIRA runs each automation action
off its scheduling loop, so a 15-minute research run won't delay your other
schedules or the next tick.

## Where the findings land — the review gate

By default MIRA **doesn't silently rewrite your wiki**. The post-turn extractor's
proposals, and the model's own wiki writes, pass through a **review gate** you
control in **Settings → Wiki**:

- **Extraction mode** — `review` (queue for your approval), `auto` (apply
  immediately), or `off`.
- **Auto-apply threshold** — in review mode, apply **confident** extractions
  immediately and queue only the uncertain ones. New installs default this to
  `0.7`, so most solid research findings apply while doubtful ones wait.
- **Write mode** — the same choice for the model's own wiki-write tool.

If your research runs but the page looks empty, check the **review queue** first —
the findings may simply be waiting for your approval. If you'd rather they land
automatically, set the extraction/write mode to `auto`, or lower the auto-apply
threshold.

Because the wiki is **yours**, you can always edit or delete anything MIRA wrote
through the UI — the change still goes through the audit trail.

## A note on blocked writes

If you mark a page as **user-only** and the extractor keeps trying to write to it,
MIRA **stops re-proposing** that write after a few identical blocks (rather than
retrying it forever). The recent attempts stay visible in the wiki's op history
with their reason, so you can see what it wanted to add and where.

## Checklist

- [ ] Prompt tells MIRA to **write to the wiki before** summarising.
- [ ] **Iteration budget** raised (e.g. 24) so it has rounds to search *and* write.
- [ ] **Timeout** raised (e.g. 15 min) so it isn't cut off mid-run.
- [ ] **Review gate** set the way you want — queued for approval, or auto-applied.
