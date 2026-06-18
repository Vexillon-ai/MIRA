---
title: Calendar
description: Use MIRA's built-in calendar, connect your own Google / Outlook / CalDAV account, and (as an admin) create organisation-wide or per-team events.
sidebar:
  order: 12
---

MIRA has a **built-in calendar** that works on its own — no external service
required. You can connect your *own* external calendar on top of it, and admins
can publish events to the whole organisation or to a specific team.

## The built-in calendar

The **Calendar** page (left nav) is a month grid of your events. Create one by
clicking a day, or just ask MIRA in chat — *"add lunch with Dana on Friday at
1pm."* MIRA can list, create, update, and delete events as part of a
conversation. Everything is stored locally and is **per-user** — your events are
yours.

You'll also see MIRA's own **planned actions** overlaid read-only: upcoming
automation runs and your daily briefing, so the proactive things MIRA does for
you are visible where you already look.

## Connect your own external calendar

Each user connects **their own** account — there's no shared calendar login.
First an admin picks the provider once (see below); then, from **your** Calendar
page:

- **Google / Outlook (Microsoft 365):** click **Connect** and authorise in the
  pop-up. MIRA stores a per-user token and mirrors your events in.
- **CalDAV (Nextcloud, Fastmail, iCloud, Radicale, …):** enter your server URL,
  username, and an **app password** (not your login password — most providers
  issue app passwords in their security settings). Your password is **encrypted
  at rest**. MIRA validates it by syncing once before saving, so a wrong
  credential is rejected up front.

External events mirror in **read-only** — MIRA shows them but doesn't change your
external calendar.

### Running MIRA in WSL?

If MIRA runs in WSL and your CalDAV server is on the Windows host, use the
`windows-host` alias in the URL — see [Run MIRA on WSL](/guides/run-on-wsl/).

## Admin: set up the provider (one time)

In **Settings → Calendar**:

1. Toggle the calendar **on** (it is by default).
2. Pick the **Sync provider** (None / CalDAV / Google / Outlook).
3. For Google/Outlook, register an OAuth app with your provider and paste the
   **client ID / secret** + the redirect URI shown. (CalDAV needs nothing here —
   it's fully per-user.)

That's the whole instance setup. Each user then connects their own account from
their Calendar page — there's nothing per-user to enter in Settings.

## Admin: organisation & team events

When creating an event, admins get a **Visibility** picker:

- **Just me** — a normal personal event.
- **Everyone** — an **organisation event** every user on the server sees
  (company holidays, all-hands).
- **Group: <name>** — scoped to an **RBAC group** so only that group's members
  see it (team standups, on-call rotations).

Organisation and group events show with a distinct style and a people icon, and
are **read-only for non-admins**. Manage group membership under **Groups**.
