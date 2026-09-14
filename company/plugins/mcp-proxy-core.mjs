// MCP Proxy — CORE pure logic (unit-testable, no side effects)
//
// Implements the pure computational bricks of RFC 18011bfc (definitive MCP
// availability). This module NEVER spawns, touches fs directly, or calls
// process.exit: every fs dependency is injected so the functions stay pure
// and mockable. Same pattern as defense-in-depth-core.mjs (core split from
// the impure wrapper mcp-proxy.mjs).
//
// Exports:
//   - computeBackoff(attempt, opts)          — exponential backoff + jitter (RFC A2)
//   - binaryReady(path, deps)                — binary precondition existsSync + X_OK (RFC A3)
//   - buildUnavailableError(id, ctx)         — JSON-RPC -32050 escalation error (RFC C2)
//   - extractPendingRequestIds(lines, parse) — ids of pending id-bearing requests (RFC C2)
//   - shouldRotate(current, incoming, opts)  — size-based journal rotation (RFC 5bacb08a D1)
//   - rotationPlan(basePath, opts)           — bounded archive shuffling (RFC 5bacb08a D1)
//   - formatTelemetryLine(entry)             — structured journal line (RFC 5bacb08a D1)
//   - constants: BACKOFF_INITIAL_MS, BACKOFF_CAP_MS, BACKOFF_RESET_AFTER_MS, JITTER_RATIO,
//               LOG_MAX_BYTES, LOG_MAX_ARCHIVES

// --- Backoff tunables (RFC A2, tranché) ---
export const BACKOFF_INITIAL_MS = 200;
export const BACKOFF_CAP_MS = 30_000;
export const BACKOFF_RESET_AFTER_MS = 60_000;
export const JITTER_RATIO = 0.20;

// computeBackoff — exponential backoff capped with ±20% jitter.
//
// attempt: 0-based retry number (0 → initial delay).
// opts.random: () => number in [0,1); injected for deterministic tests
//              (default Math.random). random()===0.5 yields zero jitter.
//
// base    = min(INITIAL * 2^attempt, CAP)
// jittered = base * (1 + (random()*2 - 1) * JITTER_RATIO)
// result  = clamp(jittered, 0, CAP)
export function computeBackoff(attempt, opts = {}) {
  const random = opts.random ?? Math.random;
  const initial = opts.initialMs ?? BACKOFF_INITIAL_MS;
  const cap = opts.capMs ?? BACKOFF_CAP_MS;
  const jitterRatio = opts.jitterRatio ?? JITTER_RATIO;

  const safeAttempt = attempt < 0 ? 0 : attempt;
  // Guard against Infinity on very large attempts: cap the exponent effect.
  const exp = Math.pow(2, safeAttempt);
  const base = Math.min(initial * exp, cap);
  const jitterFactor = 1 + (random() * 2 - 1) * jitterRatio;
  const jittered = base * jitterFactor;
  // Clamp to [0, CAP]: jitter must never produce a negative delay, and the
  // upper jitter must never exceed the cap.
  if (jittered < 0) return 0;
  if (jittered > cap) return cap;
  return jittered;
}

// binaryReady — precondition before spawning the child (RFC A3).
//
// Returns true iff the binary exists AND is executable (X_OK). fs functions
// are injected via deps so the function stays pure/testable:
//   deps.existsSync(path) -> boolean
//   deps.accessSync(path, mode) -> throws if not accessible
//   deps.constants.X_OK -> the executable-permission bit
export function binaryReady(path, deps) {
  if (!deps || typeof deps.existsSync !== "function") {
    throw new Error("binaryReady: deps.existsSync is required");
  }
  if (!deps.existsSync(path)) return false;
  try {
    deps.accessSync(path, deps.constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

// buildUnavailableError — JSON-RPC -32050 escalation error (RFC C2).
//
// Returns the response OBJECT (the caller serializes to NDJSON + "\n").
// ctx = { crate, state, downtimeMs, binary }.
// The message is reproduced verbatim from the RFC; <n> = round(downtimeMs/1000).
export function buildUnavailableError(id, ctx = {}) {
  const { crate, state, downtimeMs, binary } = ctx;
  const seconds = Math.round((downtimeMs ?? 0) / 1000);
  return {
    jsonrpc: "2.0",
    id,
    error: {
      code: -32050,
      message: `MCP server '${crate}' unavailable for ${seconds}s. Do not retry, do not sleep: report this failure to the user and stop.`,
      data: {
        escalate: true,
        state,
        downtime_ms: downtimeMs,
        binary,
      },
    },
  };
}

// extractPendingRequestIds — ids of pending id-bearing JSON-RPC requests (RFC C2).
//
// lines: array of NDJSON lines (already split). parse: JSON parser (default
// JSON.parse, injected for tests). Returns the array of ids for messages that
// carry an id (id !== undefined && id !== null). Notifications (no id) and
// non-JSON lines are ignored. Note: id===0 is a valid JSON-RPC id and IS kept.
export function extractPendingRequestIds(lines, parse = JSON.parse) {
  const ids = [];
  if (!Array.isArray(lines)) return ids;
  for (const line of lines) {
    if (typeof line !== "string" || line.length === 0) continue;
    let msg;
    try {
      msg = parse(line);
    } catch {
      continue; // non-JSON / partial — ignore
    }
    if (msg && typeof msg === "object" && msg.id !== undefined && msg.id !== null) {
      ids.push(msg.id);
    }
  }
  return ids;
}

// --- Persisted telemetry (RFC 5bacb08a D1) ---
//
// The proxy is the single writer of the recovery telemetry: its own event
// journal AND the child's stderr, captured through a pipe. These three
// functions hold every DECISION of that channel so they can be unit-tested
// without touching a filesystem; the impure wrapper only executes them.

/// Size of a journal past which it is rotated. 5 MB holds a very long
/// incident (a full backtrace is a few kB) while staying trivial to read.
export const LOG_MAX_BYTES = 5_242_880;

/// Number of rotated archives kept beside the live journal. Bounded on
/// purpose: telemetry must never be able to fill the disk. Crash traces,
/// which are the irreplaceable artefact, are NOT rotated (RFC 5bacb08a
/// D3b); journals are reconstructible context, so they are.
export const LOG_MAX_ARCHIVES = 3;

// shouldRotate — would appending `incomingBytes` push the journal past the cap?
//
// currentBytes: size of the live journal right now.
// incomingBytes: size of the line about to be appended.
// opts.maxBytes: cap, defaults to LOG_MAX_BYTES.
//
// Non-finite or negative inputs are treated as 0: a telemetry decision must
// never throw, and refusing to rotate is the safe default (the cap is a
// disk-usage guard, not a correctness invariant).
export function shouldRotate(currentBytes, incomingBytes, opts = {}) {
  const max = opts.maxBytes ?? LOG_MAX_BYTES;
  const current = Number.isFinite(currentBytes) && currentBytes > 0 ? currentBytes : 0;
  const incoming = Number.isFinite(incomingBytes) && incomingBytes > 0 ? incomingBytes : 0;
  if (!Number.isFinite(max) || max <= 0) return false;
  return current + incoming > max;
}

// rotationPlan — the renames that free `basePath` for a fresh journal.
//
// Returns { unlink, renames } where `unlink` is the archive that falls off
// the end (or null when none exists yet) and `renames` is an ORDERED list
// of {from, to} moves. The order matters: oldest first, so no move ever
// overwrites an archive that a later move still needs.
//
// With basePath "x.log" and 3 archives:
//   unlink  = "x.log.3"
//   renames = x.log.2 -> x.log.3, x.log.1 -> x.log.2, x.log -> x.log.1
export function rotationPlan(basePath, opts = {}) {
  const kept = opts.keptArchives ?? LOG_MAX_ARCHIVES;
  if (typeof basePath !== "string" || basePath.length === 0) {
    return { unlink: null, renames: [] };
  }
  if (!Number.isFinite(kept) || kept < 1) {
    // No archive kept: the live journal is simply dropped.
    return { unlink: basePath, renames: [] };
  }
  const renames = [];
  for (let i = kept - 1; i >= 1; i -= 1) {
    renames.push({ from: `${basePath}.${i}`, to: `${basePath}.${i + 1}` });
  }
  renames.push({ from: basePath, to: `${basePath}.1` });
  return { unlink: `${basePath}.${kept}`, renames };
}

// formatTelemetryLine — one journal record, always exactly one line.
//
// entry = { timestamp, incarnation, source, text }
//   timestamp:   ISO 8601 UTC string (injected, never read from a clock here)
//   incarnation: child incarnation number the proxy is counting
//   source:      "proxy" or "server"; anything else is recorded as "unknown"
//   text:        the message; embedded CR/LF are collapsed so one record
//                stays one line and the journal remains greppable
//
// Shape: `<timestamp> i=<incarnation> <source> | <text>\n`
export function formatTelemetryLine(entry = {}) {
  const { timestamp, incarnation, source, text } = entry;
  const ts = typeof timestamp === "string" && timestamp.length > 0 ? timestamp : "-";
  const inc = Number.isFinite(incarnation) ? incarnation : "-";
  const src = source === "proxy" || source === "server" ? source : "unknown";
  const body = (text === undefined || text === null ? "" : String(text))
    .replace(/\r?\n/g, " ")
    .replace(/\r/g, " ");
  return `${ts} i=${inc} ${src} | ${body}\n`;
}
