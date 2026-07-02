#!/usr/bin/env bash
# test-mcp-proxy-restart.sh
#
# Integration test for company/plugins/mcp-proxy.mjs (RFC 18011bfc).
#
# The proxy now has NO path to definitive death (volet A): the circuit breaker
# is gone, replaced by a capped backoff with rearm; a failed spawn and a crash
# follow the same latched onChildGone path; a missing binary yields
# waiting_binary (never an exit); past a cap the proxy answers pending requests
# with a JSON-RPC -32050 escalation error and then recovers.
#
# Scenarios:
#   Test 1 — atomic promotion → single controlled restart: promote the binary
#            (cp .tmp + chmod + mv -f), verify tools/list still gets a response.
#   Test 2 — binary removed → waiting_binary WITHOUT exit, then respawn when it
#            reappears (proxy stays alive throughout).
#   Test 3 — crash-loop → GROWING backoff WITHOUT exit (this REPLACES the old
#            circuit-breaker test: the assertion is INVERTED — the proxy must
#            NOT exit — cf. Implementer review note #2), then recovery.
#   Test 4 — unavailable > cap → receive the -32050 error, then recover.
#            Uses MCP_PROXY_UNAVAILABLE_MS to shorten the cap for the test.
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

# A shared "serve" dir for the tests, mimicking target/serve/.
SERVE="$TMPDIR/serve"
mkdir -p "$SERVE"

# atomic_promote <src> <dst-basename> : cp .tmp + chmod + mv -f, same fs.
atomic_promote() {
  local src="$1" name="$2"
  local tmp="$SERVE/.${name}.tmp" dst="$SERVE/${name}"
  cp "$src" "$tmp"
  chmod +x "$tmp"
  mv -f "$tmp" "$dst"
}

# ─────────────────────────── Test 1 & 2 & 4 driver ───────────────────────────
# A single node driver exercises the JSON-RPC dance for the scenarios that need
# request/response correlation (promotion, waiting_binary+respawn, escalation).
NODE_DRIVER="$TMPDIR/driver.mjs"
cat > "$NODE_DRIVER" <<'NODE_EOF'
// Args: <proxy-path> <served-binary-path> <good-binary-src> <serve-dir> <scenario>
import { spawn } from "node:child_process";
import { StringDecoder } from "node:string_decoder";
import { copyFileSync, chmodSync, renameSync, rmSync } from "node:fs";
import { basename } from "node:path";

const [proxyPath, servedPath, goodSrc, serveDir, scenario] = process.argv.slice(2);
const name = basename(servedPath);

function atomicPromote() {
  const tmp = `${serveDir}/.${name}.tmp`;
  copyFileSync(goodSrc, tmp);
  chmodSync(tmp, 0o755);
  renameSync(tmp, servedPath);
}
function removeBinary() {
  try { rmSync(servedPath); } catch {}
}

// Environment for escalation scenario: short cap.
const env = { ...process.env };
if (scenario === "escalation") env.MCP_PROXY_UNAVAILABLE_MS = "1500";

const proxy = spawn("node", [proxyPath, servedPath], {
  stdio: ["pipe", "pipe", "inherit"],
  env,
});

const decoder = new StringDecoder("utf8");
let accum = "";
const pending = new Map();
const early = new Map();
let proxyExited = false;
let proxyExitInfo = null;

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
        early.set(msg.id, msg);
      }
    }
  }
});

proxy.on("exit", (code, signal) => {
  proxyExited = true;
  proxyExitInfo = { code, signal };
  for (const [id, { reject, timer }] of pending) {
    clearTimeout(timer);
    reject(new Error(`proxy exited (code=${code} signal=${signal}) before response to id=${id}`));
  }
  pending.clear();
});

function send(method, params, id) {
  proxy.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
}
function waitFor(id, timeoutMs) {
  return new Promise((resolve, reject) => {
    if (early.has(id)) { const m = early.get(id); early.delete(id); return resolve(m); }
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`timeout waiting for id=${id} after ${timeoutMs}ms`)); }, timeoutMs);
    pending.set(id, { resolve, reject, timer });
  });
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const results = [];

async function handshake() {
  send("initialize", { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "test-driver", version: "0.0.1" } }, 1);
  const initResp = await waitFor(1, 8000);
  if (initResp.result === undefined) throw new Error("initialize returned no result");
  proxy.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");
}

try {
  if (scenario === "promotion") {
    // Binary present from the start.
    atomicPromote();
    await handshake();
    results.push({ ok: true, desc: "promotion: initialize replied" });
    // Promote (atomic rename) → controlled restart.
    atomicPromote();
    await sleep(1200); // debounce 500ms + restart + re-handshake
    send("tools/list", {}, 2);
    const resp = await waitFor(2, 15000);
    const good = resp.result !== undefined || resp.error !== undefined;
    results.push({ ok: good && !proxyExited, desc: `promotion: tools/list survived controlled restart (exited=${proxyExited})` });

  } else if (scenario === "waiting_binary") {
    // Binary present, handshake, then remove it, then bring it back.
    atomicPromote();
    await handshake();
    results.push({ ok: true, desc: "waiting_binary: initialize replied" });
    removeBinary();
    await sleep(300);
    // Kill the child so the proxy notices the binary is gone on respawn.
    // (The running server keeps its inode alive; we simulate a crash by
    //  removing then relying on a promotion to trigger a fresh spawn.)
    // Send a request while unavailable — it must be buffered, not error out
    // (cap is default 120s here, so no -32050).
    send("tools/list", {}, 2);
    await sleep(500);
    if (proxyExited) throw new Error("proxy exited while binary was absent (must stay in waiting_binary)");
    results.push({ ok: !proxyExited, desc: "waiting_binary: proxy alive with binary absent" });
    // Bring the binary back → respawn + buffered request replayed.
    atomicPromote();
    const resp = await waitFor(2, 15000);
    const good = resp.result !== undefined || resp.error !== undefined;
    results.push({ ok: good && !proxyExited, desc: `waiting_binary: buffered request answered after respawn (exited=${proxyExited})` });

  } else if (scenario === "escalation") {
    // Binary ABSENT from the start → waiting_binary; short cap → -32050.
    removeBinary();
    // No handshake possible (no child yet). Send a request with an id.
    send("tools/list", {}, 42);
    // Cap is 1500ms; wait past it.
    const resp = await waitFor(42, 8000);
    const isEsc = resp.error && resp.error.code === -32050 && resp.error.data && resp.error.data.escalate === true;
    results.push({ ok: isEsc && !proxyExited, desc: `escalation: received -32050 escalate=true (exited=${proxyExited})` });
    // Now recover: promote the binary, then re-handshake (the fresh server
    // expects initialize → initialized before any request), then a fresh call.
    atomicPromote();
    await sleep(1500);
    try {
      // Re-handshake against the now-present server. Use a fresh init id.
      send("initialize", { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "test-driver", version: "0.0.1" } }, 50);
      const initResp = await waitFor(50, 15000);
      if (initResp.result === undefined) throw new Error("recovery initialize returned no result");
      proxy.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");
      await sleep(200);
      send("tools/list", {}, 43);
      const r2 = await waitFor(43, 15000);
      const good = r2.result !== undefined || r2.error !== undefined;
      // After recovery a fresh request should NOT be a -32050.
      const recovered = good && !(r2.error && r2.error.code === -32050);
      results.push({ ok: recovered, desc: `escalation: normal operation resumed after recovery` });
    } catch (e) {
      results.push({ ok: false, desc: `escalation: recovery failed: ${e.message}` });
    }
  }
} catch (e) {
  results.push({ ok: false, desc: `${scenario}: driver error: ${e.message}` });
} finally {
  try { proxy.kill("SIGTERM"); } catch {}
  await sleep(300);
  try { proxy.kill("SIGKILL"); } catch {}
}

for (const r of results) console.log(JSON.stringify(r));
process.exit(0);
NODE_EOF

run_driver_scenario() {
  local scenario="$1" servedname="$2"
  local served="$SERVE/$servedname"
  local out="$TMPDIR/${scenario}-out.txt"
  set +e
  node "$NODE_DRIVER" "$PROXY" "$served" "$TARGET_BINARY" "$SERVE" "$scenario" \
    >"$out" 2>"$TMPDIR/${scenario}-err.txt"
  local rc=$?
  set -e
  if [ "$rc" -ne 0 ]; then
    tap_fail "$scenario driver crashed (exit=$rc)"
    cat "$TMPDIR/${scenario}-err.txt" >&2 || true
    return
  fi
  local parsed
  parsed="$(node -e '
    const fs=require("node:fs");
    const lines=fs.readFileSync(process.argv[1],"utf8").split("\n").filter(Boolean);
    for(const l of lines){try{const j=JSON.parse(l);process.stdout.write((j.ok?"OK\t":"FAIL\t")+(j.desc||"")+"\n");}catch{process.stdout.write("FAIL\tunparseable: "+l+"\n");}}
  ' "$out")"
  while IFS=$'\t' read -r verdict desc; do
    [ -z "$verdict" ] && continue
    if [ "$verdict" = "OK" ]; then tap_ok "$desc"; else tap_fail "$desc"; fi
  done <<< "$parsed"
}

log "=== Test 1: atomic promotion → single controlled restart ==="
run_driver_scenario "promotion" "companyos-yaml-validator"

log "=== Test 2: binary removed → waiting_binary without exit, then respawn ==="
run_driver_scenario "waiting_binary" "companyos-yaml-validator"

# ─────────────────────────── Test 3: crash-loop backoff ───────────────────────────
# ASSERTION INVERTED vs the old circuit-breaker test: the proxy must NOT exit.
log "=== Test 3: crash-loop → growing backoff WITHOUT exit (assertion inverted) ==="

CRASH_SERVE="$TMPDIR/crash-serve"
mkdir -p "$CRASH_SERVE"
FAKE_BIN="$CRASH_SERVE/companyos-yaml-validator"
cat > "$FAKE_BIN" <<'FAKE_EOF'
#!/usr/bin/env bash
exit 1
FAKE_EOF
chmod +x "$FAKE_BIN"

NODE_DRIVER_CB="$TMPDIR/driver-cb.mjs"
cat > "$NODE_DRIVER_CB" <<'CBNODE_EOF'
// Args: <proxy-path> <crashing-binary-path>
// Spawn proxy against a crashing binary; hold stdin open; verify it does NOT
// exit within the observation window and that backoff delays GROW.
import { spawn } from "node:child_process";

const [proxyPath, binaryPath] = process.argv.slice(2);
const proxy = spawn("node", [proxyPath, binaryPath], { stdio: ["pipe", "pipe", "pipe"] });

let exited = false;
let exitInfo = null;
const respawnDelays = [];
let stderrAccum = "";

proxy.stderr.on("data", (b) => {
  stderrAccum += b.toString();
  // Parse "Respawn scheduled in <n>ms" lines to observe growth.
  const re = /Respawn scheduled in (\d+)ms/g;
  let m;
  while ((m = re.exec(stderrAccum)) !== null) {
    const v = Number(m[1]);
    if (!respawnDelays.includes(v)) respawnDelays.push(v);
  }
  process.stderr.write(b);
});
proxy.stdout.on("data", () => {});
proxy.on("exit", (code, signal) => { exited = true; exitInfo = { code, signal }; });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

(async () => {
  // Observe for 12s: with initial 200ms backoff, factor 2, we should see
  // several scheduled respawns with increasing delays, and no exit.
  await sleep(12_000);
  const grew = respawnDelays.length >= 2 &&
    respawnDelays[respawnDelays.length - 1] > respawnDelays[0];
  console.log(JSON.stringify({
    exited,
    exitInfo,
    respawnCount: respawnDelays.length,
    firstDelay: respawnDelays[0] ?? null,
    lastDelay: respawnDelays[respawnDelays.length - 1] ?? null,
    grew,
  }));
  try { proxy.kill("SIGKILL"); } catch {}
  process.exit(0);
})();
CBNODE_EOF

set +e
node "$NODE_DRIVER_CB" "$PROXY" "$FAKE_BIN" >"$TMPDIR/cb-out.txt" 2>"$TMPDIR/cb-err.txt"
set -e
CB_JSON="$(tail -n 1 "$TMPDIR/cb-out.txt")"
log "crash-loop observation: $CB_JSON"

CB_EXITED="$(node -e 'const j=JSON.parse(process.argv[1]);process.stdout.write(String(j.exited))' "$CB_JSON" 2>/dev/null || echo "parse_error")"
CB_GREW="$(node -e 'const j=JSON.parse(process.argv[1]);process.stdout.write(String(j.grew))' "$CB_JSON" 2>/dev/null || echo "parse_error")"
CB_COUNT="$(node -e 'const j=JSON.parse(process.argv[1]);process.stdout.write(String(j.respawnCount))' "$CB_JSON" 2>/dev/null || echo "0")"

if [ "$CB_EXITED" = "false" ]; then
  tap_ok "crash-loop: proxy did NOT exit (no more circuit-breaker suicide)"
else
  tap_fail "crash-loop: proxy exited (expected: stays alive with growing backoff)"
fi

if [ "$CB_GREW" = "true" ]; then
  tap_ok "crash-loop: backoff delays grew across $CB_COUNT respawns"
else
  tap_fail "crash-loop: backoff did not grow as expected ($CB_JSON)"
fi

# Recovery: replace the crashing binary with a good one (atomic), spawn a fresh
# proxy to confirm it comes up (the crash-loop proxy above was killed).
log "=== Test 3b: crash-loop recovery (good binary promoted) ==="
cp "$TARGET_BINARY" "$CRASH_SERVE/.companyos-yaml-validator.tmp"
chmod +x "$CRASH_SERVE/.companyos-yaml-validator.tmp"
mv -f "$CRASH_SERVE/.companyos-yaml-validator.tmp" "$FAKE_BIN"

NODE_DRIVER_REC="$TMPDIR/driver-rec.mjs"
cat > "$NODE_DRIVER_REC" <<'RECNODE_EOF'
import { spawn } from "node:child_process";
import { StringDecoder } from "node:string_decoder";
const [proxyPath, binaryPath] = process.argv.slice(2);
const proxy = spawn("node", [proxyPath, binaryPath], { stdio: ["pipe", "pipe", "inherit"] });
const decoder = new StringDecoder("utf8");
let accum = "";
const pending = new Map();
proxy.stdout.on("data", (c) => {
  accum += decoder.write(c);
  const lines = accum.split("\n"); accum = lines.pop();
  for (const l of lines) { if(!l) continue; let m; try{m=JSON.parse(l);}catch{continue;}
    if (m.id !== undefined && pending.has(m.id)) { const {resolve,timer}=pending.get(m.id); clearTimeout(timer); pending.delete(m.id); resolve(m); } }
});
function send(method, params, id){ proxy.stdin.write(JSON.stringify({jsonrpc:"2.0",id,method,params})+"\n"); }
function waitFor(id, ms){ return new Promise((res,rej)=>{ const t=setTimeout(()=>{pending.delete(id);rej(new Error("timeout"));},ms); pending.set(id,{resolve:res,reject:rej,timer:t}); }); }
const sleep=(ms)=>new Promise(r=>setTimeout(r,ms));
(async()=>{
  let ok=false;
  try {
    // Full MCP handshake (initialize → wait reply → initialized → tools/list),
    // identical to the proven handshake() of the main driver.
    send("initialize", { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "rec-driver", version: "0.0.1" } }, 1);
    const ir=await waitFor(1,10000);
    if (ir.result!==undefined){
      proxy.stdin.write(JSON.stringify({jsonrpc:"2.0",method:"notifications/initialized"})+"\n");
      await sleep(200);
      send("tools/list", {}, 2);
      const r2 = await waitFor(2, 10000);
      ok = (r2.result !== undefined || r2.error !== undefined);
    }
  } catch(e){ ok=false; }
  console.log(JSON.stringify({ recovered: ok }));
  try{proxy.kill("SIGKILL");}catch{}
  process.exit(0);
})();
RECNODE_EOF

set +e
node "$NODE_DRIVER_REC" "$PROXY" "$FAKE_BIN" >"$TMPDIR/rec-out.txt" 2>"$TMPDIR/rec-err.txt"
set -e
REC_JSON="$(tail -n 1 "$TMPDIR/rec-out.txt")"
REC_OK="$(node -e 'const j=JSON.parse(process.argv[1]);process.stdout.write(String(j.recovered))' "$REC_JSON" 2>/dev/null || echo "false")"
if [ "$REC_OK" = "true" ]; then
  tap_ok "crash-loop recovery: proxy came up after good binary promoted"
else
  tap_fail "crash-loop recovery: proxy did not recover ($REC_JSON)"
fi

# ─────────────────────────── Test 4: escalation -32050 ───────────────────────────
log "=== Test 4: unavailable > cap → -32050 escalation, then recovery ==="
# Use a dedicated serve dir so the binary is truly absent at start.
ESC_SERVE="$TMPDIR/esc-serve"
mkdir -p "$ESC_SERVE"
run_driver_scenario_esc() {
  local out="$TMPDIR/escalation-out.txt"
  set +e
  MCP_PROXY_UNAVAILABLE_MS=1500 node "$NODE_DRIVER" "$PROXY" "$ESC_SERVE/companyos-yaml-validator" "$TARGET_BINARY" "$ESC_SERVE" "escalation" \
    >"$out" 2>"$TMPDIR/escalation-err.txt"
  local rc=$?
  set -e
  if [ "$rc" -ne 0 ]; then tap_fail "escalation driver crashed (exit=$rc)"; cat "$TMPDIR/escalation-err.txt" >&2 || true; return; fi
  local parsed
  parsed="$(node -e '
    const fs=require("node:fs");
    const lines=fs.readFileSync(process.argv[1],"utf8").split("\n").filter(Boolean);
    for(const l of lines){try{const j=JSON.parse(l);process.stdout.write((j.ok?"OK\t":"FAIL\t")+(j.desc||"")+"\n");}catch{process.stdout.write("FAIL\tunparseable: "+l+"\n");}}
  ' "$out")"
  while IFS=$'\t' read -r verdict desc; do
    [ -z "$verdict" ] && continue
    if [ "$verdict" = "OK" ]; then tap_ok "$desc"; else tap_fail "$desc"; fi
  done <<< "$parsed"
}
run_driver_scenario_esc

# --- Summary ---
log "=== Summary: $PASS passed, $FAIL failed ==="
echo "1..$TEST_NUM"

if [ "$FAIL" -gt 0 ]; then
  exit 1
fi
exit 0
