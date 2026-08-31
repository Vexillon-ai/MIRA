#!/usr/bin/env bash
# One-command Restricted Mode e2e: (re)build the test image from a host-built
# binary, bring up a FRESH stack, and run the Playwright suite.
#
# A fresh stack matters: the cap/capacity tests reason about per-instance state
# (active guests, the daily token counter), so the run must not inherit state
# from a previous run — hence --force-recreate every time.
#
# Usage:
#   e2e/run.sh                 # build + up + test
#   e2e/run.sh --no-build      # skip the cargo/web/image build (reuse images)
#   e2e/run.sh --down          # tear the stack down and exit
set -euo pipefail

cd "$(dirname "$0")/.."          # repo root
COMPOSE="docker compose -f e2e/docker-compose.test.yml"
BIN="${CARGO_TARGET_DIR:-target}/release/mira"

if [ "${1:-}" = "--down" ]; then $COMPOSE down -v; exit 0; fi

BUILD=1
if [ "${1:-}" = "--no-build" ]; then BUILD=0; shift; fi

if [ "$BUILD" = "1" ]; then
  echo "==> building release binary + web SPA"
  cargo build --release --bin mira
  ( cd web && npm run build )
  echo "==> staging artifacts"
  mkdir -p e2e/.artifacts
  cp "$BIN" e2e/.artifacts/mira
  rm -rf e2e/.artifacts/web && cp -r web/dist e2e/.artifacts/web
  echo "==> building images"
  docker build -q -f e2e/mock-llm/Dockerfile -t mira-mock:e2e e2e/mock-llm >/dev/null
  docker build -q -f e2e/Dockerfile.test     -t mira:e2e      . >/dev/null
fi

echo "==> bringing up a fresh stack"
$COMPOSE up -d --force-recreate

echo "==> waiting for all instances to be healthy"
for _ in $(seq 1 40); do
  h=$($COMPOSE ps --format '{{.Status}}' | grep -c healthy || true)
  [ "$h" -ge 6 ] && break
  sleep 3
done

echo "==> running Playwright"
cd e2e
[ -d node_modules ] || npm install
npx playwright install chromium >/dev/null 2>&1 || true
npx playwright test "$@"
