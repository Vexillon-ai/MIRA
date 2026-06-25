---
title: Security & multi-user
description: How MIRA hosts many people safely — per-user isolation, capability governance, the tool policy and audit layer, the secrets vault, and the code-execution sandbox.
sidebar:
  order: 6
---

A single MIRA instance can serve a household or a team. This page explains the
model that makes that safe: how accounts are authenticated, how one person's
data is kept separate from another's, how an admin governs what each account may
do, and the layers that protect tools, secrets, and code execution.

The whole model rests on one distinction worth getting straight up front:
**isolation** is about *whose data you can see*; **governance** is about *what
you're allowed to do*. They're enforced separately, and you tune them
separately.

## Accounts and authentication

Every person has their own account. MIRA authenticates with **JWT sessions** and
stores passwords hashed with **argon2id** (a modern, memory-hard hash) — never
in plaintext.

A login issues two things: a **short-lived access token** (a JWT, used on each
request) and a longer-lived **refresh token** (an HTTP-only cookie used to mint
fresh access tokens). This pair is why revocation is quick but not instantaneous:
revoking a user's refresh tokens — [Sign out
everywhere](../guides/manage-users.md#sign-out-everywhere) — locks them out
within about 15 minutes, when their current access token expires.

Identity can also come from outside MIRA. **OIDC** single sign-on and **LDAP /
Active Directory** let people log in with an existing provider or directory
account; whichever way they authenticate, MIRA issues the *same* kind of session
afterwards. Both are off by default and **local login always works**, so an
identity-provider outage never locks everyone out. See
[Single sign-on](../guides/single-sign-on.md).

## Two roles

- **User** — a normal account. Operates entirely within their own data and
  settings.
- **Admin** — manages the server: users, operator/global settings, plugins,
  named agents, and workflows. Admins also **bypass capability restrictions**
  (below).

Keep the set of admins small.

## Isolation: whose data you can see

Each person on a MIRA instance gets their **own** of everything that's personal:

- **Conversations** — chat history is per-user; nobody sees anyone else's
  threads.
- **Memory and wiki** — each person has their own memory of facts and their own
  personal wiki notes. (There's also a shared *system* wiki, separate from
  personal notes.)
- **Channels** — each user connects their own Telegram bot, Signal number, email
  mailbox, and so on, under their own settings.
- **Settings** — voice preferences, companion/check-in configuration, connected
  MCP servers, and profile all belong to the individual.
- **Agents & audit** — the **Agents** dashboard and the **Audit** log are scoped
  to *you*: you see (and can interrupt/pause/resume) only the agents you started
  and their sub-agents, and only your own agents' audit entries. Admins see the
  whole fleet, including system-initiated agents, and the full audit log.

A user cannot read another user's conversations, memory, channels, agents, or
settings. **Operator/system views are admin-only:** the **Logs** stream (which
contains every user's messages and system internals), the **Sessions** list (and
session eviction), and the system-wide counts on the **Status** page are
restricted to admins; non-admins see only operational status (version, uptime,
supervisor). Operator/global settings (providers config, security, server-wide
channels) are admin-only too. This separation is the default and needs no
configuration — adding a user gives them a clean, private workspace.

## Governance: what you're allowed to do

Isolation keeps people *apart*; it doesn't decide *what they can do* with their
own workspace. That's **capability governance**, and it's enforced through
**capability RBAC**.

An admin attaches a **capability profile** to a group and/or an individual user.
A profile restricts, on four axes — **providers**, **models**, **tools**, and
**channels** — what that user may use, plus optional **per-task / per-session
budget caps**. The design is deliberately permissive-by-default:

- An axis with **no restriction is allow-all**, so existing installs are
  unaffected until you start restricting.
- Grants are **additive across a user's groups** — they get the union of what
  all their groups allow.
- Budget caps take the **tightest** applicable value.
- **Admins bypass all of it.**

Enforcement is live everywhere it matters: the model picker hides disallowed
models, the per-turn tool set is intersected with the allow-list, adding a
disallowed channel is refused, and autonomous background-task budgets are clamped
to the cap. This is how you build, say, a kid-safe account: put the user only in
groups that grant a safe model, a couple of tools, and nothing else. See
[Manage users & access](../guides/manage-users.md#capability-rbac--govern-what-a-user-may-do).

## The tool policy layer and audit

Beyond *which* tools a user may use, MIRA has a **tool policy layer** that
governs how tools run — gating sensitive or destructive actions and, where
configured, requiring confirmation before they execute. Some tools are
intentionally guarded: for example, the destructive restore action is
admin-gated and requires explicit confirmation, and the shell tool is opt-in and
intended only for trusted deployments.

Tool use is **auditable**: MIRA records per-tool activity, so an admin can see
what the agent did on a user's behalf. Capability RBAC decides *whether* a tool
is available; the policy layer and audit decide *how* it runs and leave a
record.

## The secrets vault

Skills and integrations often need credentials — an API key, a service password.
MIRA stores these in a **per-skill secrets vault** encrypted with
**AES-256-GCM**. Secrets are scoped to the skill that owns them rather than
pooled globally.

Two properties matter:

- **Encryption at rest.** Vault contents are encrypted on disk, not stored in
  plaintext.
- **Redaction on read.** Secrets configured in MIRA's config are **redacted when
  read back** — viewing settings (or asking MIRA about them) never reveals a
  stored secret. The agent can *use* a credential without *disclosing* it.

This is also why the in-chat backup/restore path refuses encrypted archives:
passphrases typed into a conversation would leak.

## Code-execution sandbox and runtime confinement

When the agent runs code, an **optional sandbox** confines that execution. On
Linux this isolates the process; treat the sandbox as a containment layer for
code execution rather than a guarantee against a determined attacker. (The
sandbox is Linux-only — it's a no-op elsewhere — and at present a pre-baked
rootfs isn't shipped, so sandboxed code can still see the host filesystem.
Enable code execution and shell only on deployments you trust accordingly.)

The same **deny-by-default** philosophy governs plugin components: a spawned
plugin reaches the network only if its manifest declares an egress allowlist,
and then only the hosts on that list — otherwise it runs offline and MIRA tells
you, rather than silently running unfiltered.

## Putting it together

A new account on a MIRA instance is, by default, **isolated** (its own private
workspace) and **ungoverned** (allow-all on every capability axis). From there
an admin tightens governance to taste — restricting models, tools, channels, or
budget — without ever touching another user's data. Identity can come from a
local password or from your SSO/LDAP provider, and a compromised session can be
revoked across the board in minutes.

## Related

- [Manage users & access](../guides/manage-users.md) — the admin tasks: add
  users, invite links, roles, capability RBAC, sign-out-everywhere.
- [Single sign-on (OIDC & LDAP)](../guides/single-sign-on.md) — directory and
  SSO login.
- [What is MIRA?](overview.md) — the big-picture shape of the system.
