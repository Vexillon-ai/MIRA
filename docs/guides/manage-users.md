---
title: Manage users & access
description: Run MIRA for a household or team — add people, hand out invite links, open self-signup, and govern what each account can do.
sidebar:
  order: 8
---

A single MIRA instance can host many people, each with their own conversations,
memory, channels, and settings. As the **admin** you manage who has an account
and what they're allowed to do, all from the **Users** page (Settings → Users,
admin only).

This guide covers the everyday admin tasks: adding people, handing out invite
links, opening self-signup, setting roles, restricting capabilities, and
kicking a compromised account out of every session.

For *why* the model is shaped this way — isolation versus governance, the tool
policy layer, the secrets vault — see
[Security & multi-user](../concepts/security-and-multi-user.md). For directory
logins (Google, Entra, Keycloak, LDAP), see
[Single sign-on](single-sign-on.md).

## Add a user directly

The simplest path, good for one or two accounts:

1. Go to **Settings → Users**.
2. Click **Add user**, pick a **username**, set an initial **password**, and
   choose a **role** (see [Roles](#roles-admin-vs-user)).
3. Hand the credentials to the person; they sign in and can change their
   password.

For more than a couple of people, prefer invite links — you don't have to invent
or transmit passwords.

## Invite links

An **invite link** lets someone create their own account without you minting a
password. You pre-assign the role; they pick a username and password, and are
**signed in immediately** on redeeming it.

1. On **Settings → Users**, click **Invite** (or **Create invite link**).
2. Choose:
   - **Role** the new account will get (`user` or `admin`).
   - **Single-use** (the link works once) or **multi-use** (e.g. one link for a
     whole team).
   - An optional **expiry** — after which the link stops working.
3. Copy the link and send it however you like (chat, email).

When the invitee opens the link they land on the signup page, choose a username
and password, and are logged straight in with the role you set. Revoke an
outstanding invite from the Users page if you change your mind.

Invite links work **whether or not** open self-signup is enabled — they're the
default way to onboard people on an invite-only instance.

## Open self-signup (optional)

If you'd rather let people register themselves without an invite — say, an
internal team where anyone on staff should be able to join — turn on **open
self-signup**. It is **off by default**.

Enable it by setting `auth.signup.enabled` to `true` (Settings → Server, or in
`mira_config.json`). A few related settings shape it:

- **`auth.signup.require_approval`** (on by default) — new open signups land in a
  **pending** state instead of getting immediate access. They appear in a
  **pending-approval queue** on the Users page, where you **Approve** or
  **Reject** each one. Until approved, the person is told at login that their
  account is awaiting approval (not that their password is wrong). Set this to
  `false` to let approved-on-creation accounts sign in straight away.
- **`auth.signup.allowed_domains`** — restrict open signup to specific email
  domains (e.g. only `@example.com` addresses).
- **`auth.signup.default_role`** — the role open signups receive (`user` by
  default).

The signup page lives at **`/signup`**, and the login page links to it once
signup is enabled.

> Invite links bypass this queue — an invite you minted is already an approval.
> `require_approval` only governs **un-invited** open signups.

## Roles: admin vs user

Every account is either an **admin** or a **user**:

- **User** — a normal account. Sees and manages only their own conversations,
  memory, channels, and settings. Can't see other people's data or change
  operator/global settings.
- **Admin** — everything a user can do, plus the server-management surfaces: the
  Users page, operator/global settings (providers, channels, security, voice),
  the Plugins, Named Agents, and Workflows pages — and **admins bypass all
  capability restrictions** (below).

Keep the admin count small. Use invite links with the `user` role for everyone
who only needs their own assistant.

## Capability RBAC — govern what a user may do

Roles decide *whose data* a user sees. **Capability RBAC** decides *what a user
is allowed to do* — independently of their role. You attach a **capability
profile** to a **group** and/or to an individual **user**, restricting them on
four axes plus a budget:

- **Providers** — which LLM providers they may use.
- **Models** — which specific models.
- **Tools** — which agent tools (web search, code execution, image generation,
  MCP tools, …).
- **Channels** — which channel types they may add or enable.
- **Budget caps** — optional **per-task / per-session** spend limits.

Edit a profile from the **Users** or **Groups** admin page via the **shield
button** on a row.

The semantics are deliberately simple:

- **No restriction on an axis means allow-all.** An empty profile changes
  nothing, so existing installs are unaffected until you start restricting.
- **Grants are additive across groups.** A user gets the **union** of what all
  their groups allow — add someone to a group to *grant* them a model, tool, or
  channel.
- **Budget caps take the tightest value** across the profiles that apply.
- **Admins bypass everything.**

Enforcement is live across all four axes: the chat model picker hides disallowed
models (and an explicitly-selected disallowed provider/model is refused); the
per-turn tool set is intersected with the allow-list; **adding or enabling a
channel account a user isn't permitted to use is refused** (and disallowed
channels are hidden on their Channels page); and autonomous background-task
budgets are clamped to the user's cap.

**To make a fully restricted account** — for example a kid-safe profile — keep
that user only in groups that grant the intended subset (one safe model, web
search but no shell, no email channel), and grant nothing at the user level.

## Sign out everywhere

If an account is compromised, a role changes, or someone leaves, use **Sign out
everywhere** on their row in the Users page. This **revokes every refresh
token**, so the user is fully locked out within about 15 minutes — when their
short-lived access token expires. They'll have to sign in again from scratch
(and you may want to reset their password or disable the account too).

## Related

- [Single sign-on (OIDC & LDAP)](single-sign-on.md) — let people log in with
  Google, Microsoft Entra, Keycloak, Okta, or your directory.
- [Security & multi-user](../concepts/security-and-multi-user.md) — the model
  behind isolation, governance, and secrets.
- [Backup & restore](backup-and-restore.md) — the user/auth database is part of
  the backup bundle.
