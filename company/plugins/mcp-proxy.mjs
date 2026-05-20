// MCP Proxy — Hot-reload wrapper for Rust MCP servers
//
// Usage: node mcp-proxy.mjs <binary-path> [args...]
//
// Implements RFC d9399863 (proxy MCP resilience). Features:
// - Auto-build: if binary doesn't exist, runs cargo build -p <crate>
// - NDJSON parser with persistent StringDecoder per direction (UTF-8 safe)
// - Explicit state machine: starting | ready | stopping | restarting | stopped
// - FIFO buffer for stdin chunks with backpressure (pause/resume stdin)
// - Active health check via initialize response interception
// - Drain-aware async write loop into child.stdin
// - Circuit breaker against restart loops (3 restarts / 30s window)
// - Calibrated watcher debounce (4s) with size stabilization
// - mcp_reconnect rule in shared-rules.yml is now a last-resort safety net

import { spawn, execSync } from "node:child_process";
import { existsSync, watch, statSync } from "node:fs";
import { basename } from "node:path";
import { StringDecoder } from "node:string_decoder";

const [binaryPath, ...extraArgs] = process.argv.slice(2);

if (!binaryPath) {
  console.error("[mcp-proxy] Usage: node mcp-proxy.mjs <binary-path> [args...]");
  process.exit(1);
}

const crateName = basename(binaryPath);

// --- Tunables ---
const HIGH_WATER = 256; // pending chunks before pausing process.stdin
const LOW_WATER = 64; // pending chunks remaining before resuming
const RESTART_WINDOW_MS = 30_000;
const MAX_RESTARTS_IN_WINDOW = 3;
const HEALTH_CHECK_TIMEOUT_MS = 10_000;
const WATCH_DEBOUNCE_MS = 4_000;
const STABILITY_RECHECK_MS = 200;
const STDOUT_ACCUM_LIMIT = 1_048_576; // 1 MB safety cap on stdout line accumulator

// --- State ---
let child = null;
let state = "starting"; // starting | ready | stopping | restarting | stopped
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

// Stdout raw chunks buffered while state ∈ {starting, restarting}.
// Drained (minus the initialize response) when state transitions to ready.
let stdoutRawBuffer = [];

let watchDebounce = null;
let healthCheckTimer = null;

// Circuit breaker — unified counter (cf. RFC DÉCISION 6).
let restartTimestamps = [];

// True until the first successful transition to "ready". On the first
// startup, the initialize response MUST be forwarded to opencode (the
// client is waiting for it). On subsequent re-handshakes after a restart,
// the response is swallowed because opencode already received it once and
// would be confused by a second one.
let isFirstStartup = true;

// --- Logging ---
function log(msg) {
  console.error(`[mcp-proxy:${crateName}] ${msg}`);
}

// --- Auto-build if binary missing ---
function ensureBinary() {
  if (existsSync(binaryPath)) return;
  log(`Binary not found at ${binaryPath}, building...`);
  try {
    execSync(`cargo build -p ${crateName}`, {
      stdio: ["ignore", "inherit", "inherit"],
      timeout: 300_000,
    });
    log("Build complete");
  } catch (e) {
    log(`Build failed: ${e.message}`);
    process.exit(1);
  }
}

// --- NDJSON helpers ---
// Returns the array of complete lines extracted from `chunk`, updating
// the decoder's internal state and `accumRef.value` (residual partial line).
function feedAndExtract(decoder, accumRef, chunk) {
  const text = decoder.write(chunk);
  accumRef.value += text;
  const lines = accumRef.value.split("\n");
  accumRef.value = lines.pop(); // last element is the trailing partial line
  return lines.filter((l) => l.length > 0);
}

function captureInitIdIfMatch(line) {
  // Try to identify a JSON-RPC initialize request and capture its id.
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

// --- Circuit breaker ---
function recordRestart() {
  const now = Date.now();
  restartTimestamps.push(now);
  restartTimestamps = restartTimestamps.filter(
    (ts) => now - ts < RESTART_WINDOW_MS,
  );
}

function isCircuitOpen() {
  return restartTimestamps.length >= MAX_RESTARTS_IN_WINDOW;
}

// --- Async drain-aware write loop ---
async function drainPendingWrites() {
  while (pendingWrites.length > 0) {
    if (!child || !child.stdin || !child.stdin.writable) {
      // Child died mid-drain; abort silently — onChildExit will decide.
      return;
    }
    const chunk = pendingWrites.shift();
    const ok = child.stdin.write(chunk);
    if (!ok) {
      // Wait for the child's stdin buffer to drain before next write.
      await new Promise((resolve) => {
        if (!child || !child.stdin) return resolve();
        child.stdin.once("drain", resolve);
      });
    }
    // Resume process.stdin once we cross the low watermark.
    if (stdinPaused && pendingWrites.length < LOW_WATER) {
      process.stdin.resume();
      stdinPaused = false;
    }
  }
  // Final safety resume.
  if (stdinPaused) {
    process.stdin.resume();
    stdinPaused = false;
  }
}

// --- Stdout buffer flush helper ---
// Reconcatenates stdoutRawBuffer, removes the initialize-response line
// (initLine + "\n"), and writes the remainder to process.stdout.
function flushStdoutBufferExcludingInitResponse(initLine) {
  if (stdoutRawBuffer.length === 0) return;
  const total = Buffer.concat(stdoutRawBuffer);
  // Search for the exact line + newline in the raw bytes.
  const needle = Buffer.from(initLine + "\n", "utf8");
  const idx = total.indexOf(needle);
  if (idx === -1) {
    // Defensive: didn't find it. Write the whole thing minus a best-effort
    // attempt at stripping the line (give up and forward all).
    log(
      "Warning: could not locate init response in stdoutRawBuffer; forwarding raw",
    );
    process.stdout.write(total);
    return;
  }
  if (idx > 0) {
    process.stdout.write(total.subarray(0, idx));
  }
  const after = idx + needle.length;
  if (after < total.length) {
    process.stdout.write(total.subarray(after));
  }
}

// --- Health check timeout handler ---
function handleHealthCheckTimeout() {
  log("Health check timeout — child did not respond to initialize, restarting");
  if (!child) {
    // Already gone; rely on onChildExit path.
    return;
  }
  // Mark stopping so onChildExit knows this is intentional.
  state = "stopping";
  try {
    child.kill("SIGTERM");
  } catch {}
  const killTimer = setTimeout(() => {
    try {
      child?.kill("SIGKILL");
    } catch {}
  }, 1000);
  child.once("close", () => {
    clearTimeout(killTimer);
    recordRestart();
    if (isCircuitOpen()) {
      log(
        `Circuit breaker: ${MAX_RESTARTS_IN_WINDOW} restarts in ${RESTART_WINDOW_MS}ms, giving up`,
      );
      process.exit(1);
    }
    state = "restarting";
    startChild();
  });
}

// --- Child stdout handler ---
function onChildStdoutData(chunk) {
  // Always feed the decoder to stay aligned on UTF-8 + line boundaries,
  // regardless of state.
  const ref = { value: stdoutAccum };
  const messages = feedAndExtract(stdoutDecoder, ref, chunk);
  stdoutAccum = ref.value;

  // Safety cap to avoid unbounded accumulation on malformed streams.
  if (stdoutAccum.length > STDOUT_ACCUM_LIMIT) {
    log(
      `Warning: stdoutAccum exceeded ${STDOUT_ACCUM_LIMIT} bytes (framing issue?), flushing`,
    );
    // Flush as-is to avoid losing data, then reset.
    process.stdout.write(Buffer.from(stdoutAccum, "utf8"));
    stdoutAccum = "";
  }

  if (state === "ready") {
    // Transit transparent.
    process.stdout.write(chunk);
    return;
  }

  // state ∈ {starting, restarting}: full buffering, intercept initialize response.
  stdoutRawBuffer.push(chunk);

  for (const line of messages) {
    if (isInitResponse(line, lastInitRequestId)) {
      log(`Health check OK (initialize id=${lastInitRequestId} matched)`);
      state = "ready";
      if (healthCheckTimer) {
        clearTimeout(healthCheckTimer);
        healthCheckTimer = null;
      }
      if (isFirstStartup) {
        // First startup: forward the full buffer (including initialize
        // response) to the client — it is waiting for that response.
        const total = Buffer.concat(stdoutRawBuffer);
        if (total.length > 0) process.stdout.write(total);
        isFirstStartup = false;
      } else {
        // Re-handshake after restart: swallow the initialize response,
        // forward everything else.
        flushStdoutBufferExcludingInitResponse(line);
        // Replay the initialized notification BEFORE draining buffered
        // requests, otherwise the server rejects them.
        if (lastInitializedNotification && child && child.stdin && child.stdin.writable) {
          try {
            child.stdin.write(lastInitializedNotification);
          } catch (e) {
            log(`Failed to replay initialized notification: ${e.message}`);
          }
        }
      }
      stdoutRawBuffer = [];
      // Drain pending writes asynchronously. Don't await — let the proxy
      // continue handling new events while the drain proceeds.
      drainPendingWrites().catch((e) => {
        log(`drainPendingWrites error: ${e.message}`);
      });
      return;
    }
  }
}

// --- Child exit handler ---
function onChildExit(code, signal) {
  if (state === "stopping" || state === "restarting") {
    // Expected: restartChild or handleHealthCheckTimeout will follow up
    // via the once("close", ...) handler attached on the child.
    return;
  }
  // Unexpected crash.
  recordRestart();
  if (isCircuitOpen()) {
    log(
      `Circuit breaker: child crashed ${MAX_RESTARTS_IN_WINDOW} times in ${RESTART_WINDOW_MS}ms, exiting`,
    );
    state = "stopped";
    process.exit(code ?? 1);
  }
  log(
    `Child crashed unexpectedly (code=${code}, signal=${signal}), respawning`,
  );
  state = "restarting";
  startChild();
}

// --- Spawn child process ---
function startChild() {
  // Anti-deadlock: if process.stdin was paused by backpressure on the
  // previous child, the drain event will never fire (stream is dead).
  // Resume now; new chunks will land in pendingWrites because state will
  // be "starting" below.
  if (stdinPaused) {
    process.stdin.resume();
    stdinPaused = false;
  }

  state = "starting";
  // Fresh child means fresh stdout decoding context.
  stdoutAccum = "";
  stdoutRawBuffer = [];

  child = spawn(binaryPath, extraArgs, {
    stdio: ["pipe", "pipe", "inherit"],
    env: { ...process.env },
  });

  child.stdout.on("data", onChildStdoutData);
  child.on("error", (err) => {
    log(`Child error: ${err.message}`);
  });
  child.on("exit", onChildExit);

  // If we already have a remembered initialize request (i.e. this is a
  // restart, not the first startup), replay it and arm the health check.
  if (lastInitRequest) {
    log("Replaying initialize handshake");
    try {
      child.stdin.write(lastInitRequest);
    } catch (e) {
      log(`Failed to write initialize: ${e.message}`);
    }
    if (healthCheckTimer) clearTimeout(healthCheckTimer);
    healthCheckTimer = setTimeout(
      handleHealthCheckTimeout,
      HEALTH_CHECK_TIMEOUT_MS,
    );
  }
  // First startup: the health check timer will be armed when we observe
  // the first initialize request from opencode (cf. onStdinData below).
}

// --- Restart child voluntarily (e.g. binary changed on disk) ---
function restartChild() {
  if (state === "stopping" || state === "restarting") {
    // Already in motion; ignore duplicate triggers.
    return;
  }
  recordRestart();
  if (isCircuitOpen()) {
    log(
      `Circuit breaker: ${MAX_RESTARTS_IN_WINDOW} restarts in ${RESTART_WINDOW_MS}ms, giving up`,
    );
    process.exit(1);
  }
  log("Restarting child process (binary changed)...");
  state = "stopping";

  if (!child) {
    state = "restarting";
    startChild();
    return;
  }

  const prevChild = child;
  const killTimer = setTimeout(() => {
    try {
      prevChild?.kill("SIGKILL");
    } catch {}
  }, 1000);

  prevChild.once("close", () => {
    clearTimeout(killTimer);
    state = "restarting";
    startChild();
  });

  try {
    prevChild.kill("SIGTERM");
  } catch (e) {
    log(`kill SIGTERM failed: ${e.message}`);
  }
}

// --- Stdin handler (from opencode → child) ---
function onStdinData(chunk) {
  // Always feed the NDJSON parser to capture initialize id, even in fast path.
  const ref = { value: stdinAccum };
  const messages = feedAndExtract(stdinDecoder, ref, chunk);
  stdinAccum = ref.value;

  // Did we see an initialize request in this chunk? Used to decide whether
  // this chunk must be forwarded immediately (first-startup handshake) even
  // when state === "starting".
  let chunkContainsInitialize = false;

  for (const line of messages) {
    // Capture initialize request and the initialized notification that
    // follows: both must be replayed verbatim on restart for the MCP
    // server's state machine (initialize → initialized → requests).
    try {
      const msg = JSON.parse(line);
      if (msg && msg.method === "initialize" && msg.id !== undefined) {
        lastInitRequestId = msg.id;
        lastInitRequest = Buffer.from(line + "\n", "utf8");
        chunkContainsInitialize = true;
        if (state === "starting" && healthCheckTimer === null) {
          healthCheckTimer = setTimeout(
            handleHealthCheckTimeout,
            HEALTH_CHECK_TIMEOUT_MS,
          );
        }
      } else if (msg && msg.method === "notifications/initialized") {
        // Persist the initialized notification — must be replayed after
        // the initialize response on every restart, otherwise the server
        // rejects subsequent requests with "expect initialized notification".
        lastInitializedNotification = Buffer.from(line + "\n", "utf8");
      }
    } catch {
      // Non-JSON or partial — captureInitIdIfMatch path already tolerant.
      captureInitIdIfMatch(line);
    }
  }

  // Routing the raw chunk (never the decoded text — preserves bytes exactly).
  const fastPath =
    state === "ready" &&
    pendingWrites.length === 0 &&
    child &&
    child.stdin &&
    child.stdin.writable;

  // First-startup special case: when the initialize request from the client
  // arrives, we MUST forward it to the child even though state === "starting",
  // otherwise we'd buffer it and the child would never reply (deadlock — the
  // transition to "ready" is gated on receiving the initialize response).
  // Subsequent messages (e.g. notifications/initialized) follow the normal
  // buffered path and are drained when state becomes "ready".
  const firstStartupInit =
    chunkContainsInitialize &&
    state === "starting" &&
    child &&
    child.stdin &&
    child.stdin.writable;

  if (fastPath || firstStartupInit) {
    const ok = child.stdin.write(chunk);
    if (!ok) {
      // Backpressure from child: pause stdin until child drains.
      process.stdin.pause();
      stdinPaused = true;
      child.stdin.once("drain", () => {
        process.stdin.resume();
        stdinPaused = false;
      });
    }
    return;
  }

  // Buffered path: state is not "ready" (restart in progress) or a drain
  // is still flushing prior chunks.
  if (pendingWrites.length === 0) {
    log(`Buffering chunk (state=${state}, pendingWrites becoming 1)`);
  }
  pendingWrites.push(chunk);
  if (pendingWrites.length >= HIGH_WATER && !stdinPaused) {
    log(`Buffer high water mark (${HIGH_WATER}) hit, pausing stdin`);
    process.stdin.pause();
    stdinPaused = true;
  }
}

process.stdin.on("data", onStdinData);

process.stdin.on("end", () => {
  // opencode closed stdin: shutdown sequence.
  log("stdin closed by parent, terminating child");
  state = "stopping";
  try {
    child?.kill("SIGTERM");
  } catch {}
  setTimeout(() => process.exit(0), 1000);
});

// --- Watch binary for rebuilds ---
function watchBinary() {
  try {
    watch(binaryPath, () => {
      if (watchDebounce) clearTimeout(watchDebounce);
      watchDebounce = setTimeout(() => {
        // Stability check: compare size across STABILITY_RECHECK_MS to
        // make sure the binary write is complete.
        let firstSize;
        try {
          firstSize = statSync(binaryPath).size;
        } catch {
          // File temporarily missing during atomic rename — debounce again.
          watchBinaryRedebounce();
          return;
        }
        if (firstSize === 0) {
          watchBinaryRedebounce();
          return;
        }
        setTimeout(() => {
          let secondSize;
          try {
            secondSize = statSync(binaryPath).size;
          } catch {
            watchBinaryRedebounce();
            return;
          }
          if (secondSize === firstSize && secondSize > 0) {
            log("Binary changed on disk and stable, restarting...");
            restartChild();
          } else {
            watchBinaryRedebounce();
          }
        }, STABILITY_RECHECK_MS);
      }, WATCH_DEBOUNCE_MS);
    });
  } catch {
    log("Warning: could not watch binary file, auto-restart disabled");
  }
}

function watchBinaryRedebounce() {
  if (watchDebounce) clearTimeout(watchDebounce);
  watchDebounce = setTimeout(() => {
    try {
      const s = statSync(binaryPath);
      if (s.size > 0) {
        log("Binary changed on disk (re-debounced), restarting...");
        restartChild();
      }
    } catch {}
  }, WATCH_DEBOUNCE_MS);
}

// --- Main ---
ensureBinary();
startChild();
watchBinary();
