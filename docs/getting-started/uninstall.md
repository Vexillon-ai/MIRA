---
title: Uninstall MIRA
description: Remove MIRA completely — the background service, the binary, and your data directory.
sidebar:
  order: 9
---

MIRA is self-contained — one binary, one background service, and one data
directory — so removing it completely is three steps:

```bash
mira uninstall            # disable & remove the background service
rm "$(command -v mira)"   # remove the binary from your PATH
rm -rf ~/.mira            # remove all data: config, databases, logs, secrets
```

1. **`mira uninstall`** disables and removes the service unit, so the OS no
   longer runs or restarts MIRA. It's the inverse of `mira install`, and leaves
   the binary and your data in place.
2. **Remove the binary.** `command -v mira` prints its location — for a
   one-line-installer setup that's `~/.local/bin/mira`. Delete that file.
3. **Remove your data.** Everything MIRA stored lives under `~/.mira` by default
   — config, the databases, logs, and the encrypted secrets vault. Deleting it
   removes MIRA's last trace.

> **The data step erases your conversations, memories, and secrets, and can't be
> undone.** Back up `~/.mira` first if you might want any of it later. If you
> chose a custom location with `--data-dir`, remove that folder instead — see
> [Choosing where your data lives](installation.md#choosing-where-your-data-lives).

> **System-wide installs** (`mira install --system` on Linux) need `sudo` for
> the uninstall and binary steps, and the service is removed from the system
> scope rather than your user scope.

## Docker

With Docker the container is the service and the mounted `./data` directory is
your whole install:

```bash
docker compose down        # stop & remove the container
rm -rf ./data              # erase config, databases, logs, secrets
```

Leave `./data` in place if you only mean to stop MIRA for now — bring it back
any time with `docker compose up -d`.

## Just pausing, not removing?

To stop MIRA without uninstalling, use the service controls instead —
`mira stop`, then `mira start` to resume (or `mira restart`). Your data and the
service registration stay untouched.
