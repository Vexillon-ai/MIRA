---
title: Updating & rolling back
description: Check for new MIRA releases, upgrade in place on Linux/macOS/Windows (or pull a new image on Docker), and roll back safely if an update misbehaves.
sidebar:
  order: 12
---

MIRA keeps itself current and can recover from a bad update. Checking for
updates is **on by default** and is **check-only** — MIRA compares your version
against the latest release and never downloads or installs anything on its own.
Upgrading is always a deliberate action (a button, or a command).

## Check for updates

Go to **Settings → Updates** (admin only). You'll see your current version, the
latest available, and when it was last checked. Controls:

- **Automatically check for updates** — on by default. A single lightweight
  request against the release provider. Turn it off to stop MIRA contacting the
  release host at all.
- **Check frequency** — how often the background check refreshes: **Daily**,
  **Weekly**, or **Monthly**.
- **Check now** — checks immediately, regardless of the frequency.

The same "a new version is available" signal also appears as a slim banner for
admins.

> Prefer the terminal? `mira upgrade` does the same end-to-end, and you can
> point the check at a different fork with `server.update_check.source_url`.

## Upgrade

When a newer version is available, how you apply it depends on how MIRA runs:

| Install | How to update |
| --- | --- |
| **Linux** (systemd / WSL), **macOS** (launchd), **Windows** (service) | **Upgrade now** in Settings, or `mira upgrade`. MIRA downloads the signed release for your platform, **verifies its signature**, swaps the binary, and restarts the service. |
| **Docker** | MIRA can't rebuild its own image. Pull the new tag and recreate the container, e.g. `docker compose pull && docker compose up -d`. Settings shows this guidance instead of an Upgrade button. |
| **Bare `mira --server`** (no service) | MIRA can download + swap the binary but can't restart itself — run `mira upgrade` from a terminal, then restart MIRA. |

Every download is checked against MIRA's embedded signing key before anything is
swapped, so a tampered or mismatched archive is refused and your running install
is left untouched.

## Roll back

Every upgrade first snapshots the **previous binary and config**, so you can
undo one that misbehaves.

- **Settings → Updates → Roll back** restores the previous binary + config and
  restarts.
- From a terminal: `mira rollback` (most recent snapshot), `mira rollback --list`
  to see what's available, or `mira rollback --version 0.292.3` for a specific
  one. The CLI works even if the new build won't start — run it from an admin
  terminal.

### When a rollback needs a backup instead

Rolling the *binary* back is easy; rolling back *data* is not, because upgrades
migrate your databases forward. If you roll back far enough that an older binary
can't safely read the migrated data, MIRA **refuses to start** with a clear
message rather than risk corrupting anything. In that case, restore the
pre-upgrade **backup** (`mira backup list` / `mira backup restore`, or the
Backups page) — see [Backup & restore](backup-and-restore.md).

## See also

- [Settings reference](../reference/settings.md) — the `server.update_check`
  options.
- [Backup & restore](backup-and-restore.md) — full point-in-time snapshots for
  the data-migration case.
