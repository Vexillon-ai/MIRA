# Writing MIRA's documentation

This folder (`docs/`) is **MIRA's public, user-facing documentation** and the
**single source of truth** for it. The documentation site at
**[vexillon.ai/docs](https://vexillon.ai/docs)** (built with Astro Starlight) is
generated *directly from these markdown files* — it adds theme, navigation, and
search, and **does not re-author content**. Edit the docs here; the site
follows.

> If you maintain the Starlight site: treat this folder as the content source.
> Pull it in verbatim (submodule / CI sync). Do not fork or hand-edit pages on
> the site side, or the two will drift.

## Structure — the Diátaxis model

Docs are organised by what the reader is *trying to do*. Put each page in the
right bucket — mixing types (e.g. a tutorial that turns into reference) is the
single biggest cause of confusing docs.

| Folder | Type | The reader wants to… | Voice |
|--------|------|----------------------|-------|
| `getting-started/` | **Tutorial** | learn by doing, hand-held, can't fail | "We'll now…", concrete, one happy path |
| `guides/` | **How-to** | accomplish a specific task they already understand | "To do X: 1, 2, 3", task-focused |
| `concepts/` | **Explanation** | understand how/why something works | discursive, "MIRA does X because…" |
| `reference/` | **Reference** | look up a precise fact | dry, complete, consistent, no opinions |

When in doubt: *am I teaching (tutorial), giving a recipe (how-to), explaining
(concept), or listing facts (reference)?*

## Frontmatter (required)

Every page starts with YAML frontmatter. It renders as a small table on GitHub
and tells Starlight the title + sidebar placement, so the site build is a pure
pass-through:

```markdown
---
title: Installing MIRA
description: Get MIRA running on your own machine in a few minutes.
sidebar:
  order: 1
---
```

- **`title`** (required) — the page title (don't also repeat it as an `# H1`;
  Starlight renders the frontmatter title as the H1).
- **`description`** (required) — one sentence; used for SEO + search snippets.
- **`sidebar.order`** — integer controlling order within the section (lower =
  higher). Optional `sidebar.label` overrides the nav label.

## Voice & style

- **Write for a capable non-expert.** Assume they can use a terminal and follow
  steps, but don't assume they know MIRA's internals. Define a term the first
  time you use it.
- **Lead with the goal**, then the steps. Readers skim — put the outcome first.
- **Short sentences. Active voice.** "Add the token under Settings → Channels,"
  not "The token should be added…".
- **Show, don't just tell.** Real commands in fenced code blocks; real example
  values; expected output where it helps.
- **One happy path in tutorials.** Save edge cases and alternatives for how-to
  guides or a "Troubleshooting" section at the end.
- **Link generously** between pages (relative links, e.g.
  `../guides/connect-a-channel.md`).
- **British or American spelling** — match the surrounding docs (currently
  mostly British: "personalise", "behaviour").

## Source-of-truth boundaries (avoid drift)

MIRA has two doc surfaces besides this one. Keep them in their lanes:

- **`mira-docs/`** (compiled into the binary, powers the in-app `mira_help`
  tool) — terse **what / which**: capabilities, settings, limits. Keep these
  short and version-matched to the code.
- **`docs/`** (this folder) — narrative **how / why**: tutorials, walkthroughs,
  explanation. Don't copy `mira-docs/` prose here; *link* or rephrase for the
  tutorial context.
- **Settings reference** is the one genuine overlap. It's **generated** from
  `config/mira_config.schema.json` (the single source of truth) — never
  hand-write a settings table. `reference/settings.md` should point at, or be
  generated from, the same schema as `mira-docs/settings-reference.md`.

## Checklist before you commit a page

- [ ] In the right Diátaxis folder.
- [ ] Frontmatter with `title` + `description`.
- [ ] No duplicated `# H1` (the title frontmatter is the H1).
- [ ] Commands/examples are real and current.
- [ ] Links to related pages are relative and resolve.
- [ ] Reads well on GitHub *and* will render clean in Starlight.
