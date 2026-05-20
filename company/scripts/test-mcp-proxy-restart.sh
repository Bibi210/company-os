#!/usr/bin/env bash
# test-mcp-proxy-restart.sh
#
# Integration test for company/plugins/mcp-proxy.mjs (RFC d9399863).
#
# Scenarios:
#   Test 1 — nominal restart: send tools/list while touching the binary,
#            verify response arrives within timeout. Repeat 5 times.
#   Test 2 — circuit breaker: spawn proxy against a crashing binary,
#            verify proxy exits within 15s with non-zero code.
#
# Output: TAP-like ("ok N - <desc>" / "not ok N - <desc>"), exit 0 on success.
#
# The proxy is spawned in a subprocess; the proxy currently serving the
# opencode session is not touched.

set -euo pipefail

# --- Configuration ---
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PROXY="$REPO_ROOT/company/plugins/mcp-proxy.mjs"
TARGET_BINARY_REL="target/debug/companyos-yaml-validator"
TARGET_BINARY="$REPO_ROOT/$TARGET_BINARY_REL"

TMPDIR="$(mktemp -d -t mcp-proxy-test-XXXXXX)"
trap 'rm -rf "$TMPDIR"' EXIT

PASS=0
FAIL=0
TEST_NUM=0

tap_ok()    { TEST_NUM=$((TEST_NUM+1)); echo "ok $TEST_NUM - $1"; PASS=$((PASS+1)); }
tap_fail()  { TEST_NUM=$((TEST_NUM+1)); echo "not ok $TEST_NUM - $1"; FAIL=$((FAIL+1)); }
log()       { echo "# $*" >&2; }

# --- Pre-checks ---
log "Pre-check: node available"
command -v node >/dev/null || { tap_fail "node not found in PATH"; exit 1; }

log "Pre-check: proxy file exists"
[ -f "$PROXY" ] || { tap_fail "proxy not found at $PROXY"; exit 1; }

log "Pre-check: target binary available ($TARGET_BINARY_REL)"
if [ ! -x "$TARGET_BINARY" ]; then
  log "Target binary missing, building companyos-yaml-validator..."
  (cd "$REPO_ROOT" && cargo build -p companyos-yaml-validator) \
    || { tap_fail "cargo build failed"; exit 1; }
fi
[ -x "$TARGET_BINARY" ] || { tap_fail "target binary still missing after build"; exit 1; }

# --- Test 1: nominal restart loop ---
log "=== Test 1: nominal restart loop ==="

# We copy the binary into TMPDIR so we can touch it without affecting
# anything else. The proxy watches the path we give it on argv.
WORK_BINARY="$TMPDIR/binary"
cp "$TARGET_BINARY" "$WORK_BINARY"
chmod +x "$WORK_BINARY"

# The bidirectional dance with the proxy is delicate from pure bash, so
# we generate a small Node driver in TMPDIR and run it. The driver
# spawns the proxy, talks NDJSON JSON-RPC over its stdin/stdout, and
# reports TAP-style results on its own stdout.

NODE_DRIVER_NOMINAL="$TMPDIR/driver-nominal.mjs"
cat > "$NODE_DRIVER_NOMINAL" <<'NODE_EOF'
// Driver for nominal restart test.
// Args: <proxy-path> <binary-path>
import { spawn } from "node:child_process";
import { StringDecoder } from "node:string_decoder";
import { utimesSync } from "node:fs";

const [proxyPath, binaryPath] = process.argv.slice(2);
// We deliberately exercise FEWER iterations than the proxy's circuit
// breaker threshold (MAX_RESTARTS_IN_WINDOW=3 over 30s). The breaker
// itself is exercised by Test 2 (crashing binary). Here we focus on
// proving that legitimate cargo-build-style restarts are absorbed
// transparently. Two iterations are enough to demonstrate no drift
// between successive restarts.
const ITERATIONS = 2;
const INIT_TIMEOUT_MS = 5000;
const RESTART_RESPONSE_TIMEOUT_MS = 15000;
// Long enough for one full restart cycle (debounce 4s + stabilization
// 200ms + restart + re-handshake) to complete before the next round.
const INTER_ITERATION_SLEEP_MS = 8_000;

const proxy = spawn("node", [proxyPath, binaryPath], {
  stdio: ["pipe", "pipe", "inherit"],
  env: { ...process.env },
});

const decoder = new StringDecoder("utf8");
let accum = "";
const pending = new Map(); // id -> { resolve, reject, timer }
const earlyResponses = new Map(); // responses received before waitFor() registers

proxy.stdout.on("data", (chunk) => {
  accum += decoder.write(chunk);
  const lines = accum.split("\n");
  accum = lines.pop();
  for (const line of lines) {
    if (!line) continue;
    let msg;
    try { msg = JSON.parse(line); } catch { continue; }
    if (msg.id !== undefined) {
      if (pending.has(msg.id)) {
        const { resolve, timer } = pending.get(msg.id);
        clearTimeout(timer);
        pending.delete(msg.id);
        resolve(msg);
      } else {
        // Response arrived before waitFor() was called — stash it.
        earlyResponses.set(msg.id, msg);
      }
    }
  }
});

proxy.on("exit", (code, signal) => {
  // If we still have pending requests when proxy exits, fail them.
  for (const [id, { reject, timer }] of pending) {
    clearTimeout(timer);
    reject(new Error(`proxy exited (code=${code} signal=${signal}) before response to id=${id}`));
  }
  pending.clear();
});

function send(method, params, id) {
  const req = { jsonrpc: "2.0", id, method, params };
  proxy.stdin.write(JSON.stringify(req) + "\n");
}

function waitFor(id, timeoutMs) {
  return new Promise((resolve, reject) => {
    // If the response already arrived before this call, resolve immediately.
    if (earlyResponses.has(id)) {
      const msg = earlyResponses.get(id);
      earlyResponses.delete(id);
      resolve(msg);
      return;
    }
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`timeout waiting for id=${id} after ${timeoutMs}ms`));
    }, timeoutMs);
    pending.set(id, { resolve, reject, timer });
  });
}

async function sleep(ms) { return new Promise((r) => setTimeout(r, ms)); }

const results = [];

try {
  // Step 1: initialize handshake
  send("initialize", {
    protocolVersion: "2024-11-05",
    capabilities: {},
    clientInfo: { name: "test-driver", version: "0.0.1" },
  }, 1);
  const initResp = await waitFor(1, INIT_TIMEOUT_MS);
  if (initResp.result === undefined) throw new Error("initialize returned no result");
  results.push({ ok: true, desc: "initialize replied" });

  // MCP protocol requires sending an "initialized" notification.
  proxy.stdin.write(JSON.stringify({
    jsonrpc: "2.0",
    method: "notifications/initialized",
  }) + "\n");

  // Step 2..N: restart loop.
  // Per iteration:
  //   1. touch binary → watcher debounce starts (4s)
  //   2. wait until just before the debounce expires
  //   3. send tools/list right when the kill is imminent: this lands
  //      either just before the kill (must transit normally) or after
  //      (must be buffered in pendingWrites and replayed after
  //      re-handshake). Either way the response must come back.
  //   4. await response with 15s timeout
  for (let i = 0; i < ITERATIONS; i++) {
    const id = 100 + i;

    // Trigger restart by touching the binary (mtime change).
    const now = new Date();
    try { utimesSync(binaryPath, now, now); }
    catch (e) { throw new Error(`utimesSync failed: ${e.message}`); }

    // Sleep until the restart is in progress. Debounce 4000ms +
    // stabilization 200ms = ~4200ms before kill; restart cycle (kill +
    // spawn + initialize replay + health check) takes ~200-400ms. We aim
    // for 4300ms so tools/list arrives squarely inside the restart window.
    await sleep(4300);

    const t0 = Date.now();
    send("tools/list", {}, id);

    try {
      const resp = await waitFor(id, RESTART_RESPONSE_TIMEOUT_MS);
      const dt = Date.now() - t0;
      if (resp.result === undefined && resp.error === undefined) {
        results.push({ ok: false, desc: `tools/list run ${i+1} returned malformed response` });
      } else {
        results.push({ ok: true, desc: `tools/list survived restart (run ${i+1}, ${dt}ms)` });
      }
    } catch (e) {
      results.push({ ok: false, desc: `tools/list run ${i+1}: ${e.message}` });
    }
    // Gap between iterations: long enough for the restart cycle to fully
    // complete, and to keep the rolling circuit-breaker window from
    // accumulating restarts (we use ITERATIONS < threshold).
    await sleep(INTER_ITERATION_SLEEP_MS);
  }
} catch (e) {
  results.push({ ok: false, desc: `driver error: ${e.message}` });
} finally {
  try { proxy.kill("SIGTERM"); } catch {}
  await sleep(300);
  try { proxy.kill("SIGKILL"); } catch {}
}

// Output TAP-ish summary on stdout (single-line JSON per result).
for (const r of results) {
  console.log(JSON.stringify(r));
}
process.exit(0);
NODE_EOF

log "Running nominal restart driver..."
NOMINAL_OUT="$TMPDIR/nominal-out.txt"
set +e
node "$NODE_DRIVER_NOMINAL" "$PROXY" "$WORK_BINARY" >"$NOMINAL_OUT" 2>"$TMPDIR/nominal-err.txt"
DRIVER_EXIT=$?
set -e
if [ "$DRIVER_EXIT" -ne 0 ]; then
  tap_fail "nominal restart driver crashed (exit=$DRIVER_EXIT)"
  log "driver stderr:"; cat "$TMPDIR/nominal-err.txt" >&2 || true
else
  # Each line in NOMINAL_OUT is a JSON object {ok, desc}. Parse it via a
  # single node invocation that processes the whole file (more reliable
  # than per-line bash subshells with set -e).
  PARSED="$(node -e '
    const fs = require("node:fs");
    const path = process.argv[1];
    const lines = fs.readFileSync(path, "utf8").split("\n").filter(Boolean);
    for (const l of lines) {
      try {
        const j = JSON.parse(l);
        process.stdout.write((j.ok ? "OK\t" : "FAIL\t") + (j.desc || "") + "\n");
      } catch (e) {
        process.stdout.write("FAIL\tunparseable: " + l + "\n");
      }
    }
  ' "$NOMINAL_OUT")"
  while IFS=$'\t' read -r verdict desc; do
    [ -z "$verdict" ] && continue
    if [ "$verdict" = "OK" ]; then tap_ok "$desc"; else tap_fail "$desc"; fi
  done <<< "$PARSED"
fi

# --- Test 2: circuit breaker ---
log "=== Test 2: circuit breaker on crash loop ==="

FAKE_BIN="$TMPDIR/fake-crashing-bin"
cat > "$FAKE_BIN" <<'FAKE_EOF'
#!/usr/bin/env bash
# Crash immediately with non-zero exit code.
exit 1
FAKE_EOF
chmod +x "$FAKE_BIN"

# Spawn proxy and wait for it to exit (should happen within 15s thanks
# to the circuit breaker). We feed it stdin so it doesn't get stuck on
# read; we send nothing meaningful (the binary won't respond anyway).
log "Spawning proxy against crashing binary, expecting exit within 15s..."

NODE_DRIVER_CB="$TMPDIR/driver-cb.mjs"
cat > "$NODE_DRIVER_CB" <<'CBNODE_EOF'
// Driver for circuit breaker test.
// Args: <proxy-path> <fake-binary-path>
// Spawns the proxy with stdin kept open (a pipe whose write end we hold
// but never write to), waits for proxy to exit, prints "EXIT <code> <ms>".
import { spawn } from "node:child_process";

const [proxyPath, binaryPath] = process.argv.slice(2);
const start = Date.now();

const proxy = spawn("node", [proxyPath, binaryPath], {
  stdio: ["pipe", "pipe", "inherit"],
});

// Hold the stdin write end open intentionally; never write anything.
// This prevents the proxy from observing "end" on its stdin.

const safetyTimer = setTimeout(() => {
  console.log(`EXIT timeout ${Date.now() - start}`);
  try { proxy.kill("SIGKILL"); } catch {}
  process.exit(0);
}, 18_000);

proxy.on("exit", (code, signal) => {
  clearTimeout(safetyTimer);
  console.log(`EXIT ${code ?? "null"} ${Date.now() - start}`);
  process.exit(0);
});

// Just drain stdout so the proxy doesn't block on write.
proxy.stdout.on("data", () => {});
CBNODE_EOF

CB_START=$(date +%s)
set +e
node "$NODE_DRIVER_CB" "$PROXY" "$FAKE_BIN" >"$TMPDIR/cb-out.txt" 2>"$TMPDIR/cb-err.txt"
CB_DRIVER_EXIT=$?
set -e
CB_END=$(date +%s)
# Parse "EXIT <code> <ms>" from driver output.
CB_LINE="$(cat "$TMPDIR/cb-out.txt" | tail -n 1)"
CB_EXIT="$(echo "$CB_LINE" | awk '{print $2}')"
CB_MS="$(echo "$CB_LINE" | awk '{print $3}')"
CB_DURATION=$((CB_END - CB_START))
log "Driver returned: $CB_LINE (driver exit=$CB_DRIVER_EXIT, wall=${CB_DURATION}s)"
CB_DURATION=$((CB_END - CB_START))

log "Proxy stderr tail:"; tail -n 20 "$TMPDIR/cb-err.txt" >&2 || true

# Success criteria:
#   - proxy exit code != 0 (the breaker should produce non-zero)
#   - proxy exited on its own (CB_EXIT != "timeout")
#   - elapsed time on proxy < 15000ms
if [ "$CB_EXIT" = "timeout" ]; then
  tap_fail "proxy did not exit within 18s (circuit breaker did not trip)"
elif [ "$CB_EXIT" = "0" ]; then
  tap_fail "proxy exited with code 0 (unexpected — breaker should produce non-zero)"
elif [ -n "$CB_MS" ] && [ "$CB_MS" -lt 15000 ]; then
  tap_ok "circuit breaker tripped on crash loop (exit=$CB_EXIT, ${CB_MS}ms)"
else
  tap_fail "proxy exited but too slowly (exit=$CB_EXIT, ${CB_MS}ms)"
fi

# --- Summary ---
log "=== Summary: $PASS passed, $FAIL failed ==="
echo "1..$TEST_NUM"

if [ "$FAIL" -gt 0 ]; then
  exit 1
fi
exit 0
