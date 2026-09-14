// MCP Proxy — supervising wrapper for Rust MCP servers
//
// Usage: node mcp-proxy.mjs <binary-path> [args...]
//
// Implements RFC 18011bfc (definitive MCP availability), supersede partiel of
// RFC d9399863. The proxy has NO path to definitive death:
//   - Volet A (supervision without abandon): the circuit breaker is GONE. A
//     capped exponential backoff with rearm replaces it; the backoff always
//     DEFERS, never gives up. 'exit', 'error' and 'close' are all routed to a
//     single onChildGone handler, guarded by a per-incarnation latch (runs
//     exactly once), which closes the zombie mode. A failed spawn
//     (ENOENT/ETXTBSY: 'error'+'close' without 'exit') follows the same path
//     as a crash. Binary precondition (existsSync + X_OK) gates every spawn;
//     if it fails the proxy enters waiting_binary and waits for a directory
//     watch event. Watch is on the PARENT DIRECTORY (survives renames),
//     filtered on the basename, debounce 500ms (atomic rename has no
//     intermediate state to sample).
//   - Volet B (served binary bootstrap): on startup, if the served binary is
//     absent, invoke company/scripts/deploy-serve.sh <crate> ONCE (async,
//     non-blocking). On failure the proxy does NOT exit — it enters
//     waiting_binary.
//   - Volet C (event-driven readiness): the FIFO buffer is the contract.
//     During any unavailability, incoming messages wait in pendingWrites and
//     are replayed after re-handshake. Past a cap (UNAVAILABLE_ESCALATION_MS,
//     120s, env-overridable via MCP_PROXY_UNAVAILABLE_MS for tests), the proxy
//     answers pending id-bearing requests itself with a JSON-RPC -32050
//     escalation error; notifications (no id) are logged and dropped.
//
// The only remaining process.exit are: argv usage error at boot, and exit(0)
// on the stdin-end path (legitimate end of session).
//
// Preserved from d9399863: NDJSON parser with per-direction StringDecoder,
// FIFO pendingWrites with backpressure (HIGH_WATER/LOW_WATER, pause/resume),
// replay of initialize + notifications/initialized, swallowing of the
// re-handshake initialize response, drain-aware write loop, post-respawn
// health check (10s) as CONFIRMATION only (no periodic polling).

import { spawn } from "node:child_process";
import {
  existsSync,
  accessSync,
  constants as fsConstants,
  watch,
  mkdirSync,
  statSync,
  renameSync,
  unlinkSync,
  createWriteStream,
} from "node:fs";
import { basename, dirname, join } from "node:path";
import { StringDecoder } from "node:string_decoder";

import {
  computeBackoff,
  computeHealthCheckTimeout,
  binaryReady,
  buildUnavailableError,
  extractPendingRequestIds,
  shouldRotate,
  rotationPlan,
  formatTelemetryLine,
  BACKOFF_RESET_AFTER_MS,
  HEALTH_CHECK_INITIAL_MS as CORE_HEALTH_CHECK_INITIAL_MS,
  HEALTH_CHECK_CAP_MS as CORE_HEALTH_CHECK_CAP_MS,
} from "./mcp-proxy-core.mjs";

const [binaryPath, ...extraArgs] = process.argv.slice(2);

if (!binaryPath) {
  console.error("[mcp-proxy] Usage: node mcp-proxy.mjs <binary-path> [args...]");
  process.exit(1); // argv usage error — one of the two allowed exits
}

const crateName = basename(binaryPath);
const binaryDir = dirname(binaryPath);
const binaryBase = basename(binaryPath);

// --- Tunables ---
const HIGH_WATER = 256; // pending chunks before pausing process.stdin
const LOW_WATER = 64; // pending chunks remaining before resuming
const WATCH_DEBOUNCE_MS = 500; // atomic rename: no intermediate state to sample
const STDOUT_ACCUM_LIMIT = 1_048_576; // 1 MB safety cap on stdout line accumulator

// --- Env-overridable timing (RFC 5bacb08a D8) ---
//
// EVERY temporal constant of the proxy is overridable, generalizing the
// pattern that already existed for the escalation cap. Rationale: the
// integration test of the progressive health check has to observe at
// least one doubling, which costs 30 s of wall clock at production
// values. With overrides the same scenario runs in under 3 s, which is
// the difference between a test that exists and a test nobody runs. The
// DEFAULTS below remain imperative in code.
function envMs(name, fallback) {
  const raw = process.env[name];
  if (raw !== undefined) {
    const n = Number(raw);
    if (Number.isFinite(n) && n > 0) return n;
  }
  return fallback;
}

// First health-check window (RFC 5bacb08a D4). Doubles on each consecutive
// unconfirmed incarnation, capped, and rearmed as soon as one reaches ready.
const HEALTH_CHECK_INITIAL_MS = envMs(
  "MCP_PROXY_HEALTH_CHECK_MS",
  CORE_HEALTH_CHECK_INITIAL_MS,
);
const HEALTH_CHECK_CAP_MS = envMs(
  "MCP_PROXY_HEALTH_CHECK_CAP_MS",
  CORE_HEALTH_CHECK_CAP_MS,
);
// Cap after which the proxy escalates via -32050 (RFC C2, value superseded
// by RFC 5bacb08a D5: 120 s -> 45 s). 45 s sits UNDER the 60 s opencode
// client timeout with 15 s of margin, so the first failure an agent ever
// sees during a long outage is the structured error that prescribes human
// escalation, never the opaque -32001. It also sits ABOVE the 40 s
// health-check ceiling, so a single window cannot straddle it.
const UNAVAILABLE_ESCALATION_MS = envMs("MCP_PROXY_UNAVAILABLE_MS", 45_000);

// --- fs deps injected into the pure core (keeps core testable) ---
const fsDeps = { existsSync, accessSync, constants: fsConstants };

// --- State machine (RFC A1) ---
// starting | ready | stopping | restarting | backoff | waiting_binary | stopped
let child = null;
let state = "starting";
let childIncarnation = 0; // bumped on every spawn; the per-incarnation latch key
let childGoneLatch = -1; // incarnation for which onChildGone already ran

let lastInitRequest = null; // raw Buffer for replay tel quel
let lastInitRequestId = null; // captured via NDJSON parse
let lastInitializedNotification = null; // raw Buffer of "notifications/initialized"
let pendingWrites = []; // FIFO of raw Buffer chunks
let stdinPaused = false;

// NDJSON parser state (one decoder + accumulator per direction)
const stdinDecoder = new StringDecoder("utf8");
const stdoutDecoder = new StringDecoder("utf8");
let stdinAccum = "";
let stdoutAccum = "";

// Child stderr is NOT a JSON-RPC direction: it is captured line by line for
// the telemetry journal (RFC 5bacb08a D1), with its own decoder so a
// multi-byte character split across two chunks is never mangled.
const stderrDecoder = new StringDecoder("utf8");
let stderrAccum = "";

// Stdout raw chunks buffered while state ∈ {starting, restarting}.
let stdoutRawBuffer = [];

let watchDebounce = null;
let healthCheckTimer = null;
let backoffTimer = null;
let killTimer = null;

// Progressive health-check bookkeeping (RFC 5bacb08a D4). Counts the
// incarnations that failed to confirm IN A ROW; reset the moment one
// reaches ready, so a server that comes back healthy gets the fast 10 s
// detection again.
let consecutiveHealthTimeouts = 0;

// Backoff bookkeeping (RFC A2)
let backoffAttempt = 0;
let readySince = null; // timestamp of last transition to ready (for rearm)

// Unavailability clock (RFC C2)
let unavailableSince = null; // timestamp of entry into a non-ready state
let escalationTimer = null;
let bootstrapAttempted = false;

let isFirstStartup = true;

// --- Persisted telemetry (RFC 5bacb08a D1) ---
//
// The proxy is the SINGLE writer of the recovery telemetry: its own event
// journal AND the child's stderr, captured through a pipe, both landing in
// company/data/logs/<crate>.log with a structured prefix (ISO timestamp,
// incarnation, source). Before this, every stderr of the chain went to an
// opencode socket that is never persisted, and each incident was
// unrecoverable post mortem: six SIGABRT of the served orchestrator left
// nothing to read.
//
// INVARIANT OF NON-LETHALITY (RFC 5bacb08a D1). This channel is
// best-effort and can NEVER kill or block the proxy, nor the child. The
// volet A contract of RFC 18011bfc, "no code path leads to the definitive
// death of the proxy", survives D1 untouched. Three concrete rules:
//   1. the journal stream ALWAYS carries an 'error' listener. Without one
//      Node promotes a stream error to an uncaught exception, which kills
//      the proxy and takes the whole MCP channel down with it.
//   2. every telemetry call swallows its failures and degrades to
//      stderr-only. Losing telemetry costs observability, never
//      availability.
//   3. the child's stderr pipe is drained UNCONDITIONALLY, journal or no
//      journal (cf. onChildStderrData).
const COMPANYOS_ROOT = process.env.COMPANYOS_ROOT ?? ".";
const LOG_DIR = join(COMPANYOS_ROOT, "company", "data", "logs");
const LOG_PATH = join(LOG_DIR, `${crateName}.log`);
// Mirrors companyos_config::constants::ENV_MCP_INCARNATION: the Rust panic
// hook reads it to stamp a crash trace with the incarnation number only
// the proxy knows how to count. Keep the two sides in sync.
const ENV_INCARNATION = "MCP_INCARNATION";
// Cap on the stderr line accumulator, mirroring STDOUT_ACCUM_LIMIT. Rust
// backtraces are long and a chunk does not necessarily end on a newline.
const STDERR_ACCUM_LIMIT = 1_048_576;

let logStream = null;
let logBytes = 0;

function openTelemetry() {
  try {
    mkdirSync(LOG_DIR, { recursive: true });
    logBytes = existsSync(LOG_PATH) ? statSync(LOG_PATH).size : 0;
    const stream = createWriteStream(LOG_PATH, { flags: "a" });
    // MANDATORY (invariant rule 1). Never remove this listener.
    stream.on("error", (e) => {
      if (logStream === stream) logStream = null;
      console.error(
        `[mcp-proxy:${crateName}] telemetry disabled after write error: ${e.message}`,
      );
    });
    logStream = stream;
  } catch (e) {
    logStream = null;
    console.error(
      `[mcp-proxy:${crateName}] telemetry unavailable (${e.message}); stderr only`,
    );
  }
}

function rotateTelemetry() {
  try {
    const plan = rotationPlan(LOG_PATH);
    if (logStream) {
      logStream.end();
      logStream = null;
    }
    if (plan.unlink && existsSync(plan.unlink)) unlinkSync(plan.unlink);
    for (const { from, to } of plan.renames) {
      if (existsSync(from)) renameSync(from, to);
    }
  } catch (e) {
    console.error(
      `[mcp-proxy:${crateName}] telemetry rotation failed: ${e.message}`,
    );
  }
  logBytes = 0;
  openTelemetry();
}

function writeTelemetry(source, text) {
  if (!logStream) return;
  try {
    const line = formatTelemetryLine({
      timestamp: new Date().toISOString(),
      incarnation: childIncarnation,
      source,
      text,
    });
    const bytes = Buffer.byteLength(line, "utf8");
    if (shouldRotate(logBytes, bytes)) rotateTelemetry();
    if (!logStream) return; // rotation may have degraded the channel
    logStream.write(line);
    logBytes += bytes;
  } catch {
    // Observability only (invariant rule 2): swallow and keep relaying.
  }
}

// --- Logging ---
function log(msg) {
  console.error(`[mcp-proxy:${crateName}] ${msg}`);
  writeTelemetry("proxy", msg);
}

// --- NDJSON helpers ---
function feedAndExtract(decoder, accumRef, chunk) {
  const text = decoder.write(chunk);
  accumRef.value += text;
  const lines = accumRef.value.split("\n");
  accumRef.value = lines.pop();
  return lines.filter((l) => l.length > 0);
}

function captureInitIdIfMatch(line) {
  try {
    const msg = JSON.parse(line);
    if (msg && msg.method === "initialize" && msg.id !== undefined) {
      lastInitRequestId = msg.id;
    }
  } catch {
    // Not JSON or partial — ignore.
  }
}

function isInitResponse(line, expectedId) {
  if (expectedId === null || expectedId === undefined) return false;
  try {
    const msg = JSON.parse(line);
    return (
      msg &&
      msg.id === expectedId &&
      (msg.result !== undefined || msg.error !== undefined)
    );
  } catch {
    return false;
  }
}

// --- Unavailability clock + escalation (RFC C2) ---
function enterUnavailable() {
  if (unavailableSince === null) {
    unavailableSince = Date.now();
  }
  if (escalationTimer === null) {
    const remaining =
      UNAVAILABLE_ESCALATION_MS - (Date.now() - unavailableSince);
    escalationTimer = setTimeout(
      onEscalationDeadline,
      Math.max(0, remaining),
    );
  }
}

function clearUnavailable() {
  unavailableSince = null;
  if (escalationTimer) {
    clearTimeout(escalationTimer);
    escalationTimer = null;
  }
}

function downtimeMs() {
  return unavailableSince === null ? 0 : Date.now() - unavailableSince;
}

function respondUnavailable(id) {
  const errObj = buildUnavailableError(id, {
    crate: crateName,
    state,
    downtimeMs: downtimeMs(),
    binary: binaryPath,
  });
  try {
    process.stdout.write(JSON.stringify(errObj) + "\n");
  } catch (e) {
    log(`Failed to write -32050 escalation: ${e.message}`);
  }
}

// When the cap is crossed while still unavailable: answer every pending
// id-bearing request with -32050, drop notifications, and keep answering new
// requests until readiness returns.
function onEscalationDeadline() {
  escalationTimer = null;
  if (state === "ready") return; // recovered in the meantime
  log(
    `Unavailable for ${Math.round(downtimeMs() / 1000)}s (cap ${UNAVAILABLE_ESCALATION_MS}ms) — escalating pending requests via -32050`,
  );
  // Parse buffered chunks to extract pending request ids.
  const buffered = Buffer.concat(pendingWrites).toString("utf8");
  const lines = buffered.split("\n").filter((l) => l.length > 0);
  const ids = extractPendingRequestIds(lines);
  for (const id of ids) respondUnavailable(id);
  // Drop the buffered requests we just answered (notifications included: they
  // are logged-and-dropped by contract).
  if (pendingWrites.length > 0) {
    log(
      `Dropping ${pendingWrites.length} buffered chunk(s) after escalation (${ids.length} id-bearing request(s) answered)`,
    );
    pendingWrites = [];
    if (stdinPaused) {
      process.stdin.resume();
      stdinPaused = false;
    }
  }
}

// --- Async drain-aware write loop (RFC C1) ---
async function drainPendingWrites() {
  while (pendingWrites.length > 0) {
    if (!child || !child.stdin || !child.stdin.writable) {
      return; // child died mid-drain; onChildGone will decide
    }
    const chunk = pendingWrites.shift();
    const ok = child.stdin.write(chunk);
    if (!ok) {
      await new Promise((resolve) => {
        if (!child || !child.stdin) return resolve();
        child.stdin.once("drain", resolve);
      });
    }
    if (stdinPaused && pendingWrites.length < LOW_WATER) {
      process.stdin.resume();
      stdinPaused = false;
    }
  }
  if (stdinPaused) {
    process.stdin.resume();
    stdinPaused = false;
  }
}

// --- Stdout buffer flush helper (swallow the re-handshake init response) ---
function flushStdoutBufferExcludingInitResponse(initLine) {
  if (stdoutRawBuffer.length === 0) return;
  const total = Buffer.concat(stdoutRawBuffer);
  const needle = Buffer.from(initLine + "\n", "utf8");
  const idx = total.indexOf(needle);
  if (idx === -1) {
    log("Warning: could not locate init response in stdoutRawBuffer; forwarding raw");
    process.stdout.write(total);
    return;
  }
  if (idx > 0) process.stdout.write(total.subarray(0, idx));
  const after = idx + needle.length;
  if (after < total.length) process.stdout.write(total.subarray(after));
}

// --- Bootstrap the served binary via deploy-serve.sh (RFC B4) ---
function bootstrapBinary() {
  if (bootstrapAttempted) return;
  bootstrapAttempted = true;
  log(`Served binary absent — bootstrapping via deploy-serve.sh ${crateName}`);
  try {
    const proc = spawn(
      "./company/scripts/deploy-serve.sh",
      [crateName],
      { stdio: ["ignore", "inherit", "inherit"], env: { ...process.env } },
    );
    proc.on("exit", (code) => {
      if (code === 0) {
        log("Bootstrap deploy-serve succeeded");
        // The directory watcher will observe the rename and trigger a spawn.
      } else {
        log(`Bootstrap deploy-serve failed (code=${code}); staying in waiting_binary`);
      }
    });
    proc.on("error", (err) => {
      log(`Bootstrap deploy-serve spawn error: ${err.message}; staying in waiting_binary`);
    });
  } catch (e) {
    log(`Bootstrap deploy-serve threw: ${e.message}; staying in waiting_binary`);
  }
}

// --- Health check arming with progressive timeout (RFC 5bacb08a D4) ---
function armHealthCheck() {
  if (healthCheckTimer) clearTimeout(healthCheckTimer);
  const delay = computeHealthCheckTimeout(consecutiveHealthTimeouts, {
    initialMs: HEALTH_CHECK_INITIAL_MS,
    capMs: HEALTH_CHECK_CAP_MS,
  });
  log(
    `Health check armed for ${Math.round(delay)}ms (consecutive timeouts: ${consecutiveHealthTimeouts})`,
  );
  healthCheckTimer = setTimeout(handleHealthCheckTimeout, delay);
}

// --- Health check timeout: confirmation only, never exit (RFC A5) ---
function handleHealthCheckTimeout() {
  consecutiveHealthTimeouts += 1;
  log("Health check timeout — child did not confirm initialize, cycling");
  healthCheckTimer = null;
  if (!child) return; // already gone; onChildGone path handles it
  // Killing the child triggers 'exit'/'close' → onChildGone (latched) → backoff.
  state = "stopping";
  try {
    child.kill("SIGTERM");
  } catch {}
  if (killTimer) clearTimeout(killTimer);
  killTimer = setTimeout(() => {
    try {
      child?.kill("SIGKILL");
    } catch {}
  }, 1000);
}

// --- Child stdout handler ---
function onChildStdoutData(chunk) {
  const ref = { value: stdoutAccum };
  const messages = feedAndExtract(stdoutDecoder, ref, chunk);
  stdoutAccum = ref.value;

  if (stdoutAccum.length > STDOUT_ACCUM_LIMIT) {
    log(`Warning: stdoutAccum exceeded ${STDOUT_ACCUM_LIMIT} bytes (framing issue?), flushing`);
    process.stdout.write(Buffer.from(stdoutAccum, "utf8"));
    stdoutAccum = "";
  }

  if (state === "ready") {
    process.stdout.write(chunk);
    return;
  }

  // state ∈ {starting, restarting}: full buffering, intercept initialize response.
  stdoutRawBuffer.push(chunk);

  for (const line of messages) {
    if (isInitResponse(line, lastInitRequestId)) {
      log(`Health check OK (initialize id=${lastInitRequestId} matched)`);
      state = "ready";
      // Rearm the progression: this incarnation is alive, the next
      // stillborn one must be detected in 10 s again (RFC 5bacb08a D4).
      consecutiveHealthTimeouts = 0;
      readySince = Date.now();
      clearUnavailable();
      if (healthCheckTimer) {
        clearTimeout(healthCheckTimer);
        healthCheckTimer = null;
      }
      if (isFirstStartup) {
        const total = Buffer.concat(stdoutRawBuffer);
        if (total.length > 0) process.stdout.write(total);
        isFirstStartup = false;
      } else {
        flushStdoutBufferExcludingInitResponse(line);
        if (
          lastInitializedNotification &&
          child &&
          child.stdin &&
          child.stdin.writable
        ) {
          try {
            child.stdin.write(lastInitializedNotification);
          } catch (e) {
            log(`Failed to replay initialized notification: ${e.message}`);
          }
        }
      }
      stdoutRawBuffer = [];
      drainPendingWrites().catch((e) => {
        log(`drainPendingWrites error: ${e.message}`);
      });
      return;
    }
  }
}

// --- Child stderr handler: capture + tee (RFC 5bacb08a D1) ---
function onChildStderrData(chunk) {
  // Live behaviour preserved: the operator keeps seeing the server stderr
  // in real time, exactly as with the previous `inherit`.
  try {
    process.stderr.write(chunk);
  } catch {
    // EPIPE on a closed stderr must not kill the proxy.
  }
  // UNCONDITIONAL DRAIN (invariant rule 3). Consuming this pipe is not
  // optional: stop reading and the OS pipe buffer (~64 KB) fills, then the
  // child BLOCKS in write(2) at the precise moment a panic is dumping its
  // backtrace. That would be a worse failure than the blindness being
  // fixed here, so the parsing below runs whatever the journal's state.
  const ref = { value: stderrAccum };
  const lines = feedAndExtract(stderrDecoder, ref, chunk);
  stderrAccum = ref.value;
  if (stderrAccum.length > STDERR_ACCUM_LIMIT) {
    // One very long line, or a stream with no newline at all: flush it
    // instead of growing the accumulator without bound.
    lines.push(stderrAccum);
    stderrAccum = "";
  }
  for (const line of lines) writeTelemetry("server", line);
}

// --- Unified child-gone handler + per-incarnation latch (RFC A4) ---
// Routed from 'exit', 'error' AND 'close'. Runs exactly once per child
// incarnation, whatever the order/combination of events emitted.
function onChildGone(incarnation, reason) {
  if (incarnation !== childIncarnation) return; // stale event from an old child
  if (childGoneLatch === incarnation) return; // already handled this incarnation
  childGoneLatch = incarnation;

  if (killTimer) {
    clearTimeout(killTimer);
    killTimer = null;
  }
  if (healthCheckTimer) {
    clearTimeout(healthCheckTimer);
    healthCheckTimer = null;
  }

  child = null;
  log(`Child gone (incarnation=${incarnation}, reason=${reason}); scheduling respawn`);
  // We are unavailable now: start the escalation clock (idempotent).
  enterUnavailable();
  scheduleRespawn();
}

// --- Schedule a respawn with backoff / waiting_binary (RFC A2, A3) ---
function scheduleRespawn() {
  // Rearm: a child that stayed ready long enough resets the backoff history.
  if (readySince !== null && Date.now() - readySince >= BACKOFF_RESET_AFTER_MS) {
    backoffAttempt = 0;
  }
  readySince = null;

  if (!binaryReady(binaryPath, fsDeps)) {
    state = "waiting_binary";
    log("Binary not present/executable — waiting_binary (watch will trigger respawn)");
    bootstrapBinary(); // one-shot; no-op after first attempt
    // The directory watcher is the primary trigger; the backoff timer below is
    // only a safety net so we still retry even if the watch event is missed.
  } else {
    state = "backoff";
  }

  const delay = computeBackoff(backoffAttempt);
  backoffAttempt += 1;
  if (backoffTimer) clearTimeout(backoffTimer);
  backoffTimer = setTimeout(() => {
    backoffTimer = null;
    // Re-check the precondition at fire time (it may have appeared/vanished).
    if (binaryReady(binaryPath, fsDeps)) {
      startChild();
    } else {
      // Still absent: stay in waiting_binary, keep the net armed.
      scheduleRespawn();
    }
  }, delay);
  log(`Respawn scheduled in ${Math.round(delay)}ms (attempt ${backoffAttempt}, state=${state})`);
}

// --- Spawn child process ---
function startChild() {
  if (backoffTimer) {
    clearTimeout(backoffTimer);
    backoffTimer = null;
  }

  if (!binaryReady(binaryPath, fsDeps)) {
    // Guard: never spawn a missing/non-exec binary.
    scheduleRespawn();
    return;
  }

  // Anti-deadlock: resume stdin if paused by backpressure on the previous child.
  if (stdinPaused) {
    process.stdin.resume();
    stdinPaused = false;
  }

  state = "starting";
  enterUnavailable(); // still not ready until the health check confirms
  stdoutAccum = "";
  stdoutRawBuffer = [];
  stderrAccum = "";

  childIncarnation += 1;
  const incarnation = childIncarnation;

  let spawned;
  try {
    spawned = spawn(binaryPath, extraArgs, {
      // RFC 5bacb08a D1: the third descriptor moves from `inherit` to
      // `pipe` so the child's stderr, where raw Rust panic messages and
      // the "panicked while panicking" abort line go, is captured and
      // persisted. stdin and stdout are untouched: they remain the only
      // channels of the NDJSON framing.
      stdio: ["pipe", "pipe", "pipe"],
      // The incarnation number travels to the child so its crash traces
      // can be matched with this journal (read by the Rust panic hook).
      env: { ...process.env, [ENV_INCARNATION]: String(incarnation) },
    });
  } catch (e) {
    // Synchronous spawn failure (rare) — treat as child gone immediately.
    log(`spawn threw synchronously: ${e.message}`);
    onChildGone(incarnation, `spawn-threw:${e.message}`);
    return;
  }
  child = spawned;

  child.stdout.on("data", onChildStdoutData);
  if (child.stderr) child.stderr.on("data", onChildStderrData);
  // All three lifecycle events route to the same latched handler (RFC A4).
  child.on("error", (err) => onChildGone(incarnation, `error:${err.message}`));
  child.on("exit", (code, signal) =>
    onChildGone(incarnation, `exit:code=${code},signal=${signal}`),
  );
  child.on("close", () => onChildGone(incarnation, "close"));

  // Restart case: replay the remembered initialize and arm the health check.
  if (lastInitRequest) {
    log("Replaying initialize handshake");
    try {
      child.stdin.write(lastInitRequest);
    } catch (e) {
      log(`Failed to write initialize: ${e.message}`);
    }
    armHealthCheck();
  }
  // First startup: the health check timer is armed when we observe the first
  // initialize request from opencode (cf. onStdinData below).
}

// --- Stdin handler (from opencode → child) ---
function onStdinData(chunk) {
  const ref = { value: stdinAccum };
  const messages = feedAndExtract(stdinDecoder, ref, chunk);
  stdinAccum = ref.value;

  let chunkContainsInitialize = false;

  for (const line of messages) {
    try {
      const msg = JSON.parse(line);
      if (msg && msg.method === "initialize" && msg.id !== undefined) {
        lastInitRequestId = msg.id;
        lastInitRequest = Buffer.from(line + "\n", "utf8");
        chunkContainsInitialize = true;
        if (state === "starting" && healthCheckTimer === null) {
          armHealthCheck();
        }
      } else if (msg && msg.method === "notifications/initialized") {
        lastInitializedNotification = Buffer.from(line + "\n", "utf8");
      }
    } catch {
      captureInitIdIfMatch(line);
    }
  }

  const fastPath =
    state === "ready" &&
    pendingWrites.length === 0 &&
    child &&
    child.stdin &&
    child.stdin.writable;

  const firstStartupInit =
    chunkContainsInitialize &&
    state === "starting" &&
    child &&
    child.stdin &&
    child.stdin.writable;

  if (fastPath || firstStartupInit) {
    const ok = child.stdin.write(chunk);
    if (!ok) {
      process.stdin.pause();
      stdinPaused = true;
      child.stdin.once("drain", () => {
        process.stdin.resume();
        stdinPaused = false;
      });
    }
    return;
  }

  // Buffered path: not ready (restart/backoff/waiting_binary in progress) or a
  // drain is still flushing. The buffer IS the readiness contract (RFC C1).
  if (pendingWrites.length === 0) {
    log(`Buffering chunk (state=${state}, pendingWrites becoming 1)`);
  }
  pendingWrites.push(chunk);
  // If we have already crossed the escalation cap, answer id-bearing requests
  // in this chunk immediately rather than letting them rot in the buffer.
  if (
    unavailableSince !== null &&
    escalationTimer === null &&
    Date.now() - unavailableSince >= UNAVAILABLE_ESCALATION_MS
  ) {
    onEscalationDeadline();
  }
  if (pendingWrites.length >= HIGH_WATER && !stdinPaused) {
    log(`Buffer high water mark (${HIGH_WATER}) hit, pausing stdin`);
    process.stdin.pause();
    stdinPaused = true;
  }
}

process.stdin.on("data", onStdinData);

process.stdin.on("end", () => {
  // opencode closed stdin: legitimate shutdown. This is the ONLY path to
  // `stopped` and one of the two allowed process.exit.
  log("stdin closed by parent, terminating child");
  state = "stopped";
  try {
    child?.kill("SIGTERM"); // optional chaining: safe even in backoff/waiting_binary
  } catch {}
  setTimeout(() => process.exit(0), 1000);
});

// --- Watch the PARENT DIRECTORY for the served binary (RFC A3) ---
// inotify on a directory survives renames; we filter events on the basename.
function watchBinaryDir() {
  try {
    watch(binaryDir, (_eventType, filename) => {
      // filename may be null on some platforms; if so, we cannot filter and
      // fall through to a debounced re-check.
      if (filename && filename !== binaryBase) {
        // Ignore events for other files (e.g. the ".<bin>.tmp" scratch file
        // whose basename does not match the served binary).
        return;
      }
      if (watchDebounce) clearTimeout(watchDebounce);
      watchDebounce = setTimeout(() => {
        if (!binaryReady(binaryPath, fsDeps)) return; // not there yet
        if (state === "waiting_binary" || state === "backoff") {
          log("Served binary appeared/updated — spawning now");
          startChild();
        } else if (state === "ready") {
          // A promotion happened while we were serving: controlled restart.
          log("Served binary changed on disk — controlled restart");
          restartChild();
        }
        // In starting/restarting/stopping: let the in-flight cycle finish; the
        // backoff net or the next event will reconcile.
      }, WATCH_DEBOUNCE_MS);
    });
  } catch {
    log("Warning: could not watch binary directory, event-driven respawn disabled (backoff net still active)");
  }
}

// --- Controlled restart (e.g. binary promoted on disk) ---
function restartChild() {
  if (state === "stopping" || state === "restarting") return; // already in motion
  state = "restarting";
  enterUnavailable();
  const prevChild = child;
  if (!prevChild) {
    startChild();
    return;
  }
  const prevIncarnation = childIncarnation;
  if (killTimer) clearTimeout(killTimer);
  killTimer = setTimeout(() => {
    try {
      prevChild?.kill("SIGKILL");
    } catch {}
  }, 1000);
  // The kill triggers exit/close → onChildGone(prevIncarnation) → scheduleRespawn.
  try {
    prevChild.kill("SIGTERM");
  } catch (e) {
    log(`kill SIGTERM failed: ${e.message}`);
    // Force the lifecycle if the kill could not be delivered.
    onChildGone(prevIncarnation, "kill-failed");
  }
}

// --- Main ---
openTelemetry();
log(
  `Proxy starting (binary=${binaryPath}, telemetry=${logStream ? LOG_PATH : "stderr only"})`,
);
if (!binaryReady(binaryPath, fsDeps)) {
  // Served binary absent at boot: enter waiting_binary, bootstrap, and let the
  // watcher / backoff net bring us up. Never exit here (RFC B4).
  enterUnavailable();
  scheduleRespawn();
} else {
  startChild();
}
watchBinaryDir();
