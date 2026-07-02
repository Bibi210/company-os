#!/usr/bin/env bash
# deploy-serve.sh — atomic promotion of MCP server binaries (RFC 18011bfc, volet B).
#
# Builds the requested crate(s) and promotes them into target/serve/ via an
# atomic rename, so the served binaries are NEVER a moving target for cargo:
# `make ci`, `cargo build`, `cargo test` operate on target/debug and can no
# longer touch what the proxy serves.
#
# Usage:
#   deploy-serve.sh              # promote all served binaries
#   deploy-serve.sh <crate>      # promote a single crate (used by the proxy
#                                #   bootstrap, RFC B4)
#
# Promotion sequence per binary (RFC B2, order is non-negotiable):
#   (1) cargo build -p <crate>                          (debug build)
#   (2) cp target/debug/<bin>  target/serve/.<bin>.tmp
#   (3) chmod +x               target/serve/.<bin>.tmp
#   (4) mv -f                  target/serve/.<bin>.tmp  target/serve/<bin>
# Source and destination both live under target/serve/ → same filesystem →
# POSIX rename is atomic. The proxy observes ONE clean rename event, never an
# absent or partial file.
#
# On build failure the script exits non-zero. It is the PROXY's job (not this
# script's) to not die on a failed bootstrap: the proxy enters waiting_binary.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SERVE_DIR="$REPO_ROOT/target/serve"
DEBUG_DIR="$REPO_ROOT/target/debug"

# target/ is inside BASH_SAFE_PATHS of the defense-in-depth hook, so writing
# under target/serve/ requires no write permit.
mkdir -p "$SERVE_DIR"

# Map of crate -> served binary basename. For this workspace the binary name
# equals the crate name for both MCP servers.
declare -a CRATES=(
  "companyos-orchestrator-server"
  "companyos-yaml-validator"
)

promote_one() {
  local crate="$1"
  local bin="$crate" # binary basename == crate name for these two crates
  local tmp="$SERVE_DIR/.${bin}.tmp"
  local dst="$SERVE_DIR/${bin}"

  echo "[deploy-serve] building ${crate} (debug)..."
  ( cd "$REPO_ROOT" && cargo build -p "$crate" )

  if [ ! -x "$DEBUG_DIR/$bin" ]; then
    echo "[deploy-serve] ERROR: expected built binary not found: $DEBUG_DIR/$bin" >&2
    return 1
  fi

  cp "$DEBUG_DIR/$bin" "$tmp"
  chmod +x "$tmp"
  mv -f "$tmp" "$dst"    # atomic rename within target/serve/
  echo "[deploy-serve] promoted ${bin} to ${dst}"
}

if [ "$#" -ge 1 ]; then
  # Single-crate mode (proxy bootstrap). Validate the argument against the map.
  requested="$1"
  found=0
  for c in "${CRATES[@]}"; do
    if [ "$c" = "$requested" ]; then found=1; fi
  done
  if [ "$found" -ne 1 ]; then
    echo "[deploy-serve] ERROR: unknown crate '$requested'. Known: ${CRATES[*]}" >&2
    exit 2
  fi
  promote_one "$requested"
else
  # Promote all served binaries.
  for c in "${CRATES[@]}"; do
    promote_one "$c"
  done
fi

echo "[deploy-serve] done."
