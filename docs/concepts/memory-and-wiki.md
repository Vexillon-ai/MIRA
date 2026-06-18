---
title: Memory & the wiki
description: How MIRA remembers you — automatic atomic-fact memory, semantic recall, the optional knowledge graph, and the curated wiki.
sidebar:
  order: 2
---

One of the things that sets MIRA apart from a plain chat app is that it
**remembers**. Tell it something once and it carries forward — across sessions,
across channels, and into its proactive messages. MIRA does this with two
complementary systems: an automatic **memory** of small facts about you, and a
curated **wiki** of longer notes. This page explains how each works and when
each is used.

## Two systems, on purpose

It helps to hold these apart from the start:

| | **Memory** | **Wiki** |
|---|---|---|
| What it holds | Atomic facts ("you have three cats") | Longer notes and pages (a project, a person, your preferences) |
| How it's written | **Automatically**, after conversations | **Curated** — you (or MIRA, with your review) write and edit it |
| Shape | Many tiny rows | Markdown pages with structure |
| Best for | Recalling specifics in passing | Knowledge you'll deliberately reference |

Memory is the fast, automatic layer; the wiki is the deliberate, hand-editable
one. They run in parallel, and a turn can draw on both.

## Memory: automatic atomic facts

After a conversation, a background **extractor** reads the exchange and writes
down **atomic facts** about you — single, self-contained statements like "prefers
to be briefed at 7am" or "is restoring a vintage bike". You don't have to do
anything; this happens on its own. (You can also tell MIRA to remember or forget
something explicitly — see below.)

There are two extractors: a cheap built-in **heuristic** one (pattern-based, no
model call) and a richer **LLM** one (a structured pass that's confidence-gated
and conflict-aware). The global `memory.auto_extract.mode` picks the default, and
`memory.auto_extract.llm_channels` lets you turn the LLM extractor on **per
channel** — for example, run the LLM extractor on Telegram while everything else
stays heuristic.

Each fact is **embedded** for semantic search and tagged with a coarse **topic**
(for example, all bike expenses, or every plant you own). When a later turn could
benefit from what MIRA knows, it retrieves relevant facts and folds them into the
reply — that's how MIRA personalises answers across sessions and channels without
you repeating yourself. Retrieval is **recency-aware**: semantic similarity is
blended with a freshness boost, so something you mentioned recently can surface
ahead of an older fact MIRA has simply recalled many times before.

The topic tag matters for a specific class of question. Rather than pulling only
the handful of *most similar* facts, MIRA retrieves the **whole topic together**,
so counting and totalling questions — *"how many plants do I have?"*, *"what have
I spent on the bike?"* — see the complete set instead of a lucky few. That
"retrieve the whole group" behaviour is what makes those questions reliable.

### The optional knowledge graph (experimental)

Flat fact-lists have a structural limit: for aggregation questions, the answer is
only as good as the set of facts that happened to be retrieved — miss one and you
undercount, pull in a stale one and you overcount. To push past that ceiling,
MIRA has an **experimental temporal knowledge-graph memory**, off by default.

When enabled, the extractor also stores facts as **typed, timestamped triples** —
an entity, a relation, and a value or another entity (for example, *bike →
spent → £40*, dated). Aggregation then resolves against the **exact set of
matching edges** rather than a fuzzy similarity search, which makes
counts and totals more precise. It's opt-in (`memory.graph.enabled`) and marked
experimental because building a clean graph from conversation is genuinely hard;
leave it off unless you specifically want sharper aggregation. The flat memory
above keeps working either way — the graph is **additive**, not a replacement.

## The wiki: curated, longer notes

Where memory is a scatter of atomic facts, the **wiki** is a small, organised
knowledge base of **longer-form pages** — markdown on disk, hand-editable, with a
tracked history. It's the place for things that deserve more than a one-line
fact: a project and its state, notes on a person, your standing preferences.

There are two wikis:

- **Your personal wiki** — per user, private to you. MIRA reads it to anchor a
  conversation in what it knows about you and your work.
- **The system wiki** — owned by admins, shared across the instance (MIRA's own
  identity and operational notes live here).

Two things make the wiki trustworthy rather than a black box:

- **It's curated, not silently accumulated.** MIRA can *propose* wiki updates
  from a conversation, but by default those go to a **review queue** for you to
  approve, edit, or reject before they land. You stay in control of what's
  written about you. The queue scales with you: approve or reject the whole
  batch at once, or set `wiki.auto_extract.auto_apply_above` so the
  high-confidence proposals apply automatically and only the uncertain ones
  wait for you. Each pending item shows the extractor's confidence.
- **Every change is audit-tracked.** Edits are recorded, so you can always see
  what changed and why — and you can open the wiki and edit any page yourself.

Because the wiki is plain markdown, it's fully **portable** — you can export it,
keep it under git, and read it without MIRA in the loop.

## Memory vs. wiki — which gets used when

On each turn MIRA draws on both, in a deliberate order: the wiki's narrative
knowledge anchors the agent first, then specific memory facts fill in the detail.
You don't choose between them — together they let MIRA answer *"what was that
project I mentioned?"* from the wiki and *"how many of X do I have?"* from
topic-grouped memory, in the same conversation.

## Managing it in plain language

You rarely need a settings page for any of this — just ask MIRA:

- *"What do you remember about me?"* — reviews your stored facts.
- *"Forget that I said X."* — removes a fact.
- *"Remember that I prefer tea to coffee."* — writes one on request.
- *"Add a wiki page about my kitchen-rebuild project."* — starts a curated note.
- *"Update my preferences page — I've switched to a standing desk."* — proposes
  an edit (which you can review).

You can also open the **Memory** and **Wiki** pages in the web UI to browse,
search, edit, and approve directly.

## See also

- [What is MIRA?](overview.md) — where memory and the wiki sit in the system.
- [Back up & restore](../guides/backup-and-restore.md) — your memory and wiki
  live in the data directory; backing it up protects both.
- [Proactive check-ins & briefing](../guides/proactive-checkins-and-briefing.md)
  — the daily briefing is built partly from recent wiki updates.
