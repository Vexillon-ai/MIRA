---
title: Back up & restore
description: Protect, move, or recover a MIRA install — back up the whole data directory and restore it safely.
sidebar:
  order: 10
---

Everything that makes a MIRA install *yours* lives in one place: the **data
directory** (`~/.mira/data` by default). Back up that directory and you've
backed up your whole MIRA — every conversation, memory, wiki page, automation,
channel account, and key. This guide covers the one-click backup, scheduled
snapshots, and how to restore safely on the same machine or a new one.

## What's in a backup

A MIRA backup bundles the **entire data directory plus the config file** into a
single `tar.gz`. That includes:

- **Databases** — history, memory (and the knowledge graph), automations,
  channel accounts, calendar, the wiki audit log, MCP servers, and email.
- **The wiki tree** — your per-user notes and the system wiki, as markdown on
  disk (see [Memory & the wiki](../concepts/memory-and-wiki.md)).
- **Artifacts and avatars** — generated images/video and uploaded files.
- **Voice models** — any downloaded TTS/STT models.
- **Keys and secrets** — the master key, the per-skill secrets vault, the Web
  Push (VAPID) keypair, and the config file *including provider API keys*.

Because the archive carries live credentials, treat it as sensitive — store it
somewhere private, and use encryption (below) if it leaves your machine.

## One-click backup

The simplest way to grab a backup is from the web UI:

1. Go to **Settings → Server**.
2. Choose **Download backup**. MIRA bundles the data directory and config into a
   versioned `tar.gz` and streams it to your browser.

That's a complete, portable snapshot. Keep a copy somewhere off the machine — an
external disk or a private cloud folder — so a disk failure can't take both the
install and its backup.

### Encrypt with a passphrase

For an archive that's safe to store off-machine, turn on **encryption** before
downloading and set a passphrase. MIRA encrypts the archive with AES-256-GCM
under a key derived from your passphrase (argon2id), so the file is useless
without it.

> The passphrase **never leaves your browser** and is never stored. If you lose
> it, the archive can't be recovered — write it down somewhere safe.

## Scheduled snapshots

Rather than remembering to click, you can have MIRA take backups for you. Enable
**scheduled backups** (the `backup.scheduled_*` settings — see the
[settings reference](../reference/settings.md)) and MIRA writes a snapshot to
`<data_dir>/backups/` on a configurable interval, keeping a configurable number
of recent archives (retention) and pruning older ones.

Scheduled snapshots live *inside* the data directory, so they protect against
accidental deletion and bad edits — but not against losing the disk itself. Pair
them with an occasional off-machine download or a backup of the whole volume.

## Restore

Restoring swaps the new archive in for your current data. MIRA does this in two
phases so a restore can't leave you with a half-written data directory:

1. **Stage.** Upload an archive under **Settings → Server → Restore** (or ask
   MIRA in chat — see below). MIRA writes a `.restore_pending` marker and
   triggers a clean restart.
2. **Swap on next boot.** When MIRA comes back up, it first archives your
   *current* data into `.pre_restore_backup/` (so a mistaken restore is itself
   reversible), then swaps the uploaded archive into place and finishes starting.

The service is briefly unavailable while it restarts — that's expected. When it
returns, you're running on the restored data.

### Version-compatibility guard

MIRA refuses a restore across a **major version** mismatch *before* it swaps
anything, so an archive from an incompatible release can't corrupt a newer
install. Restore into the same major version (ideally the same or a newer minor
version) it was taken from. If you're moving between machines, install a matching
[MIRA version](../getting-started/installation.md) first, then restore.

## Back up or restore from chat

MIRA exposes its backup tools to the agent, so you can just ask:

- *"back up my data"* → creates a backup (`backup_create`).
- *"what backups do I have?"* → lists available archives (`backup_list`).
- *"restore from yesterday's backup"* → stages a restore (`backup_restore`).

The destructive restore is **admin-gated** and requires an explicit confirmation
before it runs. For safety, restore-from-chat **refuses encrypted archives** — a
passphrase typed into a conversation would leak — so use **Settings → Server**
for any encrypted archive.

## Moving to a new machine

To migrate an install:

1. On the old machine, download a backup (encrypt it for the move).
2. [Install MIRA](../getting-started/installation.md) on the new machine, at a
   matching major version.
3. Restore the archive under **Settings → Server**.

Your accounts, memory, wiki, channels, and settings come across intact. You may
need to re-confirm anything tied to the host — for example, re-pointing a
channel webhook at the new address.

## Docker

If you run MIRA in Docker, the data directory is the mounted `./data` volume.
You can use the in-app backup exactly as above, but the simplest belt-and-braces
approach is to **back up the `./data` volume** at the host level (snapshot it, or
`tar` it while the container is stopped). Restoring is then just putting that
volume back and starting the container.

## See also

- [Settings reference](../reference/settings.md) — the `backup.*` keys and
  `data_dir`.
- [What is MIRA?](../concepts/overview.md) — why the data directory is the one
  thing to protect.
- [Command-line reference](../reference/cli.md) — `mira setup` / `mira install`
  and the `--data-dir` flag for choosing where state lives.
