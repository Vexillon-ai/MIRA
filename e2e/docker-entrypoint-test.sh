#!/bin/sh
# Test-image entrypoint. Seeds a fresh, writable config from the read-only mount
# on every boot (so /data can stay ephemeral and each `up` starts clean), then
# starts the server. No interactive setup — the config is fully baked.
set -e

CFG_DIR="$HOME/.mira/config"
CFG="$CFG_DIR/mira_config.json"
SEED="${SEED_CONFIG:-/seed-config.json}"

mkdir -p "$CFG_DIR"
if [ -f "$SEED" ]; then
  cp "$SEED" "$CFG"
fi

# The guest baseline wiki (if any) is mounted read-only at /seed-wiki and named
# there by `restricted_mode.guest.seed_wiki_dir` (absolute) in the config; MIRA
# copies it into each guest at mint time.

exec mira --server --host 0.0.0.0 --port 8080
