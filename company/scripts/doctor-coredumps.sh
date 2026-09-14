#!/usr/bin/env bash
# doctor-coredumps.sh — coredump surveillance for the companyos binaries
# (RFC 5bacb08a, D6).
#
# Two dumps of the served orchestrator went unnoticed on 2026-08-25 and six
# more followed in September, because nothing ever looked. This reports any
# core dump newer than a persisted baseline, plus the pre-unwind crash
# traces the servers write themselves.
#
# TWO MODES, and the difference matters:
#
#   --hook   invoked from the head of deploy-serve. INFORMATIONAL ONLY:
#            prints its report and ALWAYS exits 0, new dumps included, and
#            degrades gracefully when coredumpctl is missing or too old for
#            --json. deploy-serve runs under `set -euo pipefail` and is the
#            ONLY channel that puts a fix in service: a blocking doctor
#            would let the symptom block its own correction, and would
#            break `make setup` after a clone.
#
#   (none)   manual invocation, `make doctor`. The exit code MEANS
#            something: non-zero when dumps appeared since the baseline.
#
# The baseline is CAPTURED ON FIRST RUN, never hardcoded: the first draft
# of the design carved "20 dumps as of 2026-09-09" into the artifact and it
# was already false days later. The right semantics is "anything after the
# moment surveillance started".
#
# This script REPORTS, it never deletes. Purging dumps or traces is a human
# decision.

set -uo pipefail   # NOTE: no `-e`. A doctor must survive its own probes.

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BASELINE="$REPO_ROOT/company/data/coredump-baseline.json"
CRASHES_DIR="$REPO_ROOT/company/data/crashes"
MODE="${1:-manual}"

hook_mode() { [ "$MODE" = "--hook" ]; }

# In hook mode every exit is a success by construction.
finish() {
  local code="$1"
  if hook_mode; then exit 0; fi
  exit "$code"
}

say() { printf '[doctor] %s\n' "$*"; }

# --- Probe coredumpctl, degrade gracefully ---------------------------------
if ! command -v coredumpctl >/dev/null 2>&1; then
  say "coredumpctl not found: coredump surveillance unavailable on this machine."
  say "Crash traces written by the servers themselves are still listed below."
  list_traces_only=1
else
  list_traces_only=0
fi

DUMPS=""
if [ "$list_traces_only" -eq 0 ]; then
  # --json=short is available from systemd 249; older versions simply fail,
  # which is a degradation, not an error.
  DUMPS="$(coredumpctl list --json=short --no-pager 2>/dev/null)"
  if [ -z "$DUMPS" ] || ! printf '%s' "$DUMPS" | head -c 1 | grep -q '\['; then
    say "coredumpctl has no --json support (or listed nothing): surveillance degraded."
    list_traces_only=1
  fi
fi

# --- Current companyos dumps ----------------------------------------------
CURRENT=""
if [ "$list_traces_only" -eq 0 ]; then
  if command -v jq >/dev/null 2>&1; then
    CURRENT="$(printf '%s' "$DUMPS" \
      | jq -r '.[] | select(.exe != null) | select(.exe | test("companyos")) | "\(.time)|\(.pid)|\(.sig)|\(.exe)"' \
      2>/dev/null | sort)"
  else
    say "jq not found: falling back to the plain listing."
    CURRENT="$(coredumpctl list --no-pager 2>/dev/null | grep companyos | awk '{print $4"|"$1" "$2" "$3"|"$6"|"$8}' | sort)"
  fi
fi

# --- Baseline: capture on first run ---------------------------------------
if [ ! -f "$BASELINE" ]; then
  mkdir -p "$(dirname "$BASELINE")" 2>/dev/null
  printf '%s' "$CURRENT" > "$BASELINE" 2>/dev/null
  COUNT="$(printf '%s' "$CURRENT" | grep -c . )"
  say "Baseline captured: $COUNT companyos dump(s) known as of now ($BASELINE)."
  say "Anything appearing after this point will be reported."
  finish 0
fi

# --- Diff against the baseline --------------------------------------------
NEW=""
if [ "$list_traces_only" -eq 0 ]; then
  NEW="$(comm -13 <(sort "$BASELINE") <(printf '%s' "$CURRENT" | sort) 2>/dev/null)"
fi

NEW_COUNT="$(printf '%s' "$NEW" | grep -c . )"

# --- Crash traces written by the servers ----------------------------------
TRACE_COUNT=0
if [ -d "$CRASHES_DIR" ]; then
  TRACE_COUNT="$(find "$CRASHES_DIR" -maxdepth 1 -type f -name "*.txt" 2>/dev/null | wc -l)"
fi
if [ "$TRACE_COUNT" -gt 0 ]; then
  say "$TRACE_COUNT pre-unwind crash trace(s) under company/data/crashes/ (first-hand signal, independent of systemd):"
  find "$CRASHES_DIR" -maxdepth 1 -type f -name "*.txt" -printf '  %f\n' 2>/dev/null | sort | tail -10
fi

if [ "$NEW_COUNT" -eq 0 ]; then
  if [ "$list_traces_only" -eq 0 ]; then
    say "No new coredump since the baseline."
  fi
  finish 0
fi

say "$NEW_COUNT NEW coredump(s) since the baseline:"
printf '%s\n' "$NEW" | while IFS='|' read -r time pid sig exe; do
  [ -z "${pid:-}" ] && continue
  printf '  pid=%s signal=%s exe=%s\n' "$pid" "$sig" "$exe"
  printf '    investigate with: coredumpctl gdb %s\n' "$pid"
done
say "The baseline is NOT advanced automatically: a dump keeps being reported until a human decides."
finish 1
