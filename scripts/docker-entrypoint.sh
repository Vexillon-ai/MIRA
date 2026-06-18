#!/bin/sh
# MIRA container entrypoint. Runs under tini (PID 1). Handles first-run
# onboarding, then execs the requested command.
#
# Onboarding in Docker (no interactive TTY on `docker compose up`):
#   - Interactive:  docker compose run --rm mira setup
#                   (writes config to the /data volume, then `up` as normal)
#   - Unattended:   set MIRA_SETUP_* env (see docker-compose.yml). On first boot
#                   with no config, the server start auto-runs `mira setup
#                   --unattended` from those vars before launching.
#   - Neither:      the server starts and auto-generates an admin password
#                   (printed to the logs) — the existing first-run behaviour.
set -e

CONFIG="${MIRA_CONFIG:-$HOME/.mira/config/mira_config.json}"

case "$1" in
  # Convenience: `docker compose run --rm mira setup` (and friends) → `mira …`,
  # so users don't have to type the binary name twice.
  setup|--version|--help|-h)
    exec mira "$@"
    ;;
  mira)
    # Default server start (`mira --server …`) with no config yet?
    if [ "$2" = "--server" ] && [ ! -f "$CONFIG" ]; then
      if [ -n "${MIRA_SETUP_PROVIDER:-}" ] && [ -n "${MIRA_SETUP_ADMIN_PASS:-}" ]; then
        echo "[entrypoint] first run + MIRA_SETUP_* present → mira setup --unattended"
        mira setup --unattended \
          || echo "[entrypoint] setup failed; starting anyway (a first-run admin will be auto-generated)"
      else
        echo "[entrypoint] first run: no config yet — the server will auto-generate an admin (see the log below)."
        echo "[entrypoint] To configure interactively instead: docker compose run --rm mira setup"
      fi
    fi
    exec "$@"
    ;;
  *)
    exec "$@"
    ;;
esac
