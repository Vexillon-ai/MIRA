#!/usr/bin/env sh
# MIRA bootstrap installer.
#
# Usage (Unix / macOS):
#   curl -fsSL https://get.vexillon.ai/install.sh | sh
# Or for a specific version:
#   curl -fsSL https://get.vexillon.ai/install.sh | sh -s -- --version 0.146.0
#
# What it does:
#   1. Detects platform (Linux x86_64 / Linux aarch64 / macOS x86_64 /
#      macOS aarch64).
#   2. Downloads the matching release tarball from the configured release source.
#   3. Verifies the SHA-256 checksum.
#   4. Places the `mira` binary on PATH (under ~/.local/bin so it doesn't need
#      sudo; falls back to /usr/local/bin if --system).
#   5. Runs the guided `mira setup` wizard (admin account, LLM provider, security)
#      — interactive, or unattended from MIRA_SETUP_* env with --unattended.
#   6. Runs `mira install` (or `mira install --system`) to register the service.
#   7. Opens the browser to the web UI to finish voice + channels.

set -eu

# ── Defaults — override via env or flag ──────────────────────────────────────

# Release source — provider-aware. Defaults to GitHub (the public release
# source). Internal builds can target a GitLab generic-package-registry by
# setting MIRA_RELEASE_PROVIDER=gitlab and MIRA_RELEASE_BASE_URL to the GitLab
# project API base (e.g. https://gitlab.example.com/api/v4/projects/<id>).
# RELEASES_URL / DOWNLOAD_BASE_URL may also be overridden directly.
: "${MIRA_RELEASE_PROVIDER:=github}"
case "$MIRA_RELEASE_PROVIDER" in
  github|gh)
    : "${MIRA_RELEASE_BASE_URL:=https://github.com/Vexillon-ai/MIRA}"
    api_base=$(printf '%s' "$MIRA_RELEASE_BASE_URL" \
      | sed 's#^https://github\.com/#https://api.github.com/repos/#')
    : "${RELEASES_URL:=${api_base}/releases}"
    : "${DOWNLOAD_BASE_URL:=${MIRA_RELEASE_BASE_URL}/releases/download}"
    TAG_PREFIX=v        # GitHub addresses release assets under the v<version> tag
    ;;
  gitlab|gl)
    if [ -z "${MIRA_RELEASE_BASE_URL:-}" ]; then
      echo "MIRA_RELEASE_PROVIDER=gitlab requires MIRA_RELEASE_BASE_URL" >&2
      echo "  (the GitLab project API base, e.g. https://gitlab.example.com/api/v4/projects/<id>)" >&2
      exit 1
    fi
    : "${RELEASES_URL:=${MIRA_RELEASE_BASE_URL}/releases}"
    : "${DOWNLOAD_BASE_URL:=${MIRA_RELEASE_BASE_URL}/packages/generic/mira}"
    TAG_PREFIX=         # GitLab's generic-package path uses the bare version
    ;;
  *)
    echo "Unknown MIRA_RELEASE_PROVIDER: '$MIRA_RELEASE_PROVIDER' (use 'github' or 'gitlab')" >&2
    exit 1
    ;;
esac
: "${INSTALL_DIR:=$HOME/.local/bin}"
: "${SYSTEM_INSTALL:=}"        # set to "1" to install system-scope
: "${VERSION:=}"               # blank = latest; e.g. "0.146.0"
: "${NO_RUN_INSTALL:=}"        # set to "1" to skip `mira install`
: "${NO_OPEN_BROWSER:=}"       # set to "1" to skip opening the browser
: "${NO_SETUP:=}"              # set to "1" to skip the guided `mira setup` wizard
: "${SETUP_UNATTENDED:=}"      # set to "1" to run `mira setup --unattended` (reads MIRA_SETUP_* env)

# ── Argument parsing ─────────────────────────────────────────────────────────

while [ $# -gt 0 ]; do
  case "$1" in
    --system) SYSTEM_INSTALL=1; INSTALL_DIR=/usr/local/bin; shift ;;
    --version) VERSION="$2"; shift 2 ;;
    --version=*) VERSION="${1#*=}"; shift ;;
    --install-dir) INSTALL_DIR="$2"; shift 2 ;;
    --no-supervisor) NO_RUN_INSTALL=1; shift ;;
    --no-browser) NO_OPEN_BROWSER=1; shift ;;
    --no-setup) NO_SETUP=1; shift ;;
    --unattended) SETUP_UNATTENDED=1; shift ;;
    -h|--help)
      cat <<EOF
MIRA installer

Options:
  --system               Install system-scope (writes /etc/systemd/system/
                         mira.service, requires sudo, runs on boot)
  --version <X.Y.Z>      Install a specific release (default: latest)
  --install-dir <path>   Where to put the binary (default: ~/.local/bin)
  --no-supervisor        Don't run \`mira install\` after extracting
  --no-browser           Don't open the browser at the end
  --no-setup             Skip the guided first-run \`mira setup\` wizard
  --unattended           Run \`mira setup --unattended\` (configure from
                         MIRA_SETUP_* env vars — for CI / scripted installs)

Environment variables map to the same flags. The release source defaults to
GitHub; override with MIRA_RELEASE_PROVIDER=github|gitlab + MIRA_RELEASE_BASE_URL,
or set RELEASES_URL / DOWNLOAD_BASE_URL directly.
EOF
      exit 0
      ;;
    *)
      echo "unknown arg: $1" >&2
      exit 2
      ;;
  esac
done

# ── Platform detection ───────────────────────────────────────────────────────

uname_s=$(uname -s)
uname_m=$(uname -m)

case "$uname_s" in
  Linux)  os=unknown-linux-gnu ;;
  Darwin) os=apple-darwin ;;
  *)
    echo "Unsupported OS: $uname_s. Native install supports Linux + macOS." >&2
    echo "On Windows, use WSL2 or wait for the .msi installer (slice 11)." >&2
    exit 1
    ;;
esac

case "$uname_m" in
  x86_64|amd64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *)
    echo "Unsupported arch: $uname_m. Tarballs ship for x86_64 + aarch64." >&2
    exit 1
    ;;
esac

# Tarball naming follows the rust target triple:
# e.g. `mira-0.146.0-x86_64-unknown-linux-gnu.tar.gz`.
target="${arch}-${os}"
echo "→ detected target: $target"

# ── Resolve the release tarball URL ──────────────────────────────────────────

# The CI workflow names assets like: mira-<version>-<target>.tar.gz
# with a sibling SHA256SUMS file containing the per-asset digest.
need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "Missing required command: $1" >&2; exit 1;
  }
}
need curl
need tar
need sha256sum 2>/dev/null || need shasum  # macOS ships shasum, Linux sha256sum

# Pick latest version if not pinned. The releases API returns an array
# sorted newest-first; the first entry's tag_name is the latest. The grep
# tolerates optional whitespace after the colon so it works against both
# GitLab's compact JSON and GitHub's pretty-printed JSON (the public mirror).
if [ -z "$VERSION" ]; then
  echo "→ resolving latest release from $RELEASES_URL"
  VERSION=$(curl -fsSL "$RELEASES_URL" \
    | grep -o '"tag_name"[[:space:]]*:[[:space:]]*"[^"]*"' \
    | head -n1 \
    | sed 's/.*"\([^"]*\)"$/\1/' \
    | sed 's/^v//')
  if [ -z "$VERSION" ]; then
    echo "Couldn't resolve latest release from $RELEASES_URL." >&2
    echo "Pin a version with --version 0.X.Y or set RELEASES_URL." >&2
    exit 1
  fi
  echo "✓ latest is $VERSION"
fi

# Download URL pattern. GitHub serves release assets at
# <repo>/releases/download/v<version>/<file>; GitLab's generic package
# registry at <base>/packages/generic/mira/<version>/<file>. TAG_PREFIX
# (set per provider above) accounts for the v-prefix difference.
RELEASE_BASE="${DOWNLOAD_BASE_URL}/${TAG_PREFIX}${VERSION}"
ASSET="mira-${VERSION}-${target}.tar.gz"
ASSET_URL="${RELEASE_BASE}/${ASSET}"

# ── Download + verify + extract ──────────────────────────────────────────────

tmp=$(mktemp -d)
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT INT

echo "→ downloading $ASSET_URL"
curl -fsSL "$ASSET_URL" -o "$tmp/$ASSET"

# Verify checksum if SHA256SUMS is published alongside.
SUMS_URL="${RELEASE_BASE}/SHA256SUMS"
if curl -fsSL "$SUMS_URL" -o "$tmp/SHA256SUMS" 2>/dev/null; then
  expected=$(grep " $ASSET\$" "$tmp/SHA256SUMS" | awk '{print $1}')
  if [ -n "$expected" ]; then
    if command -v sha256sum >/dev/null 2>&1; then
      actual=$(sha256sum "$tmp/$ASSET" | awk '{print $1}')
    else
      actual=$(shasum -a 256 "$tmp/$ASSET" | awk '{print $1}')
    fi
    if [ "$expected" != "$actual" ]; then
      echo "Checksum mismatch:" >&2
      echo "  expected: $expected" >&2
      echo "  got:      $actual"   >&2
      exit 1
    fi
    echo "✓ checksum ok"
  else
    echo "  (no entry for $ASSET in SHA256SUMS — skipping verify)"
  fi
else
  echo "  (no SHA256SUMS published — skipping verify)"
fi

echo "→ extracting"
tar -xzf "$tmp/$ASSET" -C "$tmp"

# ── Install the binary ──────────────────────────────────────────────────────

mkdir -p "$INSTALL_DIR"
src_bin=$(find "$tmp" -maxdepth 3 -type f -name mira | head -n1)
if [ -z "$src_bin" ]; then
  echo "Tarball did not contain a `mira` binary." >&2
  exit 1
fi

if [ "${SYSTEM_INSTALL:-}" = "1" ] && [ ! -w "$INSTALL_DIR" ]; then
  echo "→ moving binary to $INSTALL_DIR (sudo)"
  sudo install -m 0755 "$src_bin" "$INSTALL_DIR/mira"
else
  echo "→ moving binary to $INSTALL_DIR"
  install -m 0755 "$src_bin" "$INSTALL_DIR/mira"
fi
echo "✓ installed $INSTALL_DIR/mira"

# ── Guided first-run setup ───────────────────────────────────────────────────
#
# The wizard writes a validated config + creates the admin BEFORE the service
# starts. It must run before `mira install`. Two modes:
#   - unattended (--unattended / SETUP_UNATTENDED=1): reads MIRA_SETUP_* env, no TTY.
#   - interactive: needs a terminal. Under `curl … | sh` the script's stdin is the
#     pipe, so we point the wizard at /dev/tty so its prompts reach the user.
# System-scope setup is more involved (root, /etc + /var/lib paths) — for now the
# wizard runs for user-scope; --system relies on the first-run admin + web UI.

run_setup=1
[ -n "${NO_SETUP:-}" ] && run_setup=
[ "${SYSTEM_INSTALL:-}" = "1" ] && [ -z "${SETUP_UNATTENDED:-}" ] && run_setup=

if [ -n "$run_setup" ]; then
  if [ -n "${SETUP_UNATTENDED:-}" ]; then
    echo "→ running: mira setup --unattended"
    "$INSTALL_DIR/mira" setup --unattended || echo "  (setup failed — run \`mira setup\` to configure)"
  elif [ -e /dev/tty ]; then
    echo "→ launching guided setup…"
    # </dev/tty so prompts work even when the script came in over a pipe.
    "$INSTALL_DIR/mira" setup </dev/tty || echo "  (setup skipped — run \`mira setup\` anytime to configure)"
  else
    echo "  (no terminal detected — skipping guided setup; run \`mira setup\` to configure)"
  fi
elif [ "${SYSTEM_INSTALL:-}" = "1" ]; then
  echo "  (system install — the first-run admin password prints to the journal;"
  echo "   or run \`sudo mira setup --config /etc/mira/mira_config.json\` to configure)"
fi

# ── Register with the supervisor ────────────────────────────────────────────

if [ -z "${NO_RUN_INSTALL:-}" ]; then
  if [ "${SYSTEM_INSTALL:-}" = "1" ]; then
    echo "→ running: sudo mira install --system"
    sudo "$INSTALL_DIR/mira" install --system
  else
    echo "→ running: mira install"
    "$INSTALL_DIR/mira" install
  fi
fi

# ── Open the web UI ──────────────────────────────────────────────────────────

# Use the port the wizard wrote (python3 if available; else default 8080).
PORT=8080
CFG="$HOME/.mira/config/mira_config.json"
if command -v python3 >/dev/null 2>&1 && [ -f "$CFG" ]; then
  PORT=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["server"]["port"])' "$CFG" 2>/dev/null || echo 8080)
fi
URL="http://localhost:${PORT}/"
if [ -z "${NO_OPEN_BROWSER:-}" ]; then
  if command -v xdg-open >/dev/null 2>&1; then
    (xdg-open "$URL" >/dev/null 2>&1 &)
  elif command -v open >/dev/null 2>&1; then
    (open "$URL" >/dev/null 2>&1 &)
  fi
fi

cat <<EOF

────────────────────────────────────────────────────
MIRA $VERSION installed.
  binary: $INSTALL_DIR/mira
  open:   $URL

Log in with the admin account from setup (or, if you skipped setup, the
first-run password printed above / in the journal). Finish voice + channels
from the web UI.

If you skipped --system, MIRA runs while you're logged in.
For a VPS / always-on setup re-run with --system.

  status:  mira status
  logs:    journalctl --user -u mira -f
  stop:    mira stop
  upgrade: mira upgrade --binary
  reconfig: mira setup
────────────────────────────────────────────────────
EOF
