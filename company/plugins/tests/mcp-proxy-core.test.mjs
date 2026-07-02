// Tests for company/plugins/mcp-proxy-core.mjs (RFC 18011bfc).
// Run via `make test-js` (node --test company/plugins/tests/*.test.mjs).

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  computeBackoff,
  binaryReady,
  buildUnavailableError,
  extractPendingRequestIds,
  BACKOFF_INITIAL_MS,
  BACKOFF_CAP_MS,
  BACKOFF_RESET_AFTER_MS,
  JITTER_RATIO,
} from "../mcp-proxy-core.mjs";

// Deterministic "no jitter" random: 0.5 → (0.5*2 - 1) = 0 → factor 1.
const noJitter = () => 0.5;

// ─────────────────────────── computeBackoff ───────────────────────────

test("computeBackoff — NOMINAL: exponential progression with zero jitter", () => {
  assert.equal(computeBackoff(0, { random: noJitter }), 200); // 200 * 2^0
  assert.equal(computeBackoff(1, { random: noJitter }), 400); // 200 * 2^1
  assert.equal(computeBackoff(2, { random: noJitter }), 800); // 200 * 2^2
  assert.equal(computeBackoff(3, { random: noJitter }), 1600);
});

test("computeBackoff — EDGE: capped at BACKOFF_CAP_MS for high attempts", () => {
  // attempt 20 → 200 * 2^20 ≫ cap; with zero jitter it clamps to cap exactly.
  assert.equal(computeBackoff(20, { random: noJitter }), BACKOFF_CAP_MS);
  // Even the +20% jitter cannot exceed the cap (clamped).
  assert.equal(computeBackoff(20, { random: () => 1 }), BACKOFF_CAP_MS);
});

test("computeBackoff — EDGE: jitter bounded, never negative", () => {
  // random()=0 → factor (1 + (-1)*0.20) = 0.8 → -20%.
  assert.equal(computeBackoff(0, { random: () => 0 }), 200 * 0.8);
  // random()~1 → factor (1 + (~1)*0.20) = ~1.2 → +20%.
  assert.equal(computeBackoff(0, { random: () => 1 }), 200 * 1.2);
  // A degenerate random that would drive negative is clamped to 0.
  assert.equal(
    computeBackoff(0, { random: () => 0, jitterRatio: 5 }),
    0,
  );
});

test("computeBackoff — EDGE: negative attempt treated as 0", () => {
  assert.equal(computeBackoff(-3, { random: noJitter }), 200);
});

test("computeBackoff — constants exported for wrapper (rearm threshold)", () => {
  assert.equal(BACKOFF_INITIAL_MS, 200);
  assert.equal(BACKOFF_CAP_MS, 30_000);
  assert.equal(BACKOFF_RESET_AFTER_MS, 60_000);
  assert.equal(JITTER_RATIO, 0.20);
});

// ─────────────────────────── binaryReady ───────────────────────────

const X_OK = 1; // arbitrary sentinel for the executable bit in mocks

test("binaryReady — NOMINAL: exists and executable → true", () => {
  let accessedWith = null;
  const deps = {
    existsSync: () => true,
    accessSync: (_p, mode) => {
      accessedWith = mode;
    },
    constants: { X_OK },
  };
  assert.equal(binaryReady("/bin/x", deps), true);
  // EDGE: accessSync must be called with X_OK.
  assert.equal(accessedWith, X_OK);
});

test("binaryReady — NÉGATIF: missing file → false, accessSync not called", () => {
  let accessCalled = false;
  const deps = {
    existsSync: () => false,
    accessSync: () => {
      accessCalled = true;
    },
    constants: { X_OK },
  };
  assert.equal(binaryReady("/bin/x", deps), false);
  assert.equal(accessCalled, false);
});

test("binaryReady — NÉGATIF: accessSync throws (EACCES/ENOENT) → false", () => {
  const deps = {
    existsSync: () => true,
    accessSync: () => {
      throw new Error("EACCES");
    },
    constants: { X_OK },
  };
  assert.equal(binaryReady("/bin/x", deps), false);
});

test("binaryReady — guards missing deps", () => {
  assert.throws(() => binaryReady("/bin/x", {}), /deps.existsSync/);
});

// ─────────────────────── buildUnavailableError ───────────────────────

test("buildUnavailableError — NOMINAL: full -32050 object", () => {
  const err = buildUnavailableError(42, {
    crate: "companyos-orchestrator-server",
    state: "waiting_binary",
    downtimeMs: 120000,
    binary: "./target/serve/companyos-orchestrator-server",
  });
  assert.equal(err.jsonrpc, "2.0");
  assert.equal(err.id, 42);
  assert.equal(err.error.code, -32050);
  assert.match(err.error.message, /unavailable for 120s/);
  assert.match(err.error.message, /Do not retry, do not sleep/);
  assert.equal(err.error.data.escalate, true);
  assert.equal(err.error.data.state, "waiting_binary");
  assert.equal(err.error.data.downtime_ms, 120000);
  assert.equal(
    err.error.data.binary,
    "./target/serve/companyos-orchestrator-server",
  );
});

test("buildUnavailableError — EDGE: id=0 preserved (falsy but valid)", () => {
  const err = buildUnavailableError(0, {
    crate: "c",
    state: "backoff",
    downtimeMs: 130000,
    binary: "b",
  });
  assert.equal(err.id, 0);
});

test("buildUnavailableError — EDGE: seconds rounded from ms", () => {
  const err = buildUnavailableError("x", {
    crate: "c",
    state: "backoff",
    downtimeMs: 150500,
    binary: "b",
  });
  // round(150500/1000) = 151
  assert.match(err.error.message, /unavailable for 151s/);
});

// ─────────────────────── extractPendingRequestIds ───────────────────────

test("extractPendingRequestIds — NOMINAL: ids of id-bearing requests", () => {
  const lines = [
    JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" }),
    JSON.stringify({ jsonrpc: "2.0", id: 2, method: "generate_id" }),
  ];
  assert.deepEqual(extractPendingRequestIds(lines), [1, 2]);
});

test("extractPendingRequestIds — NÉGATIF: non-JSON ignored, notifications excluded", () => {
  const lines = [
    "not json at all",
    JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }), // no id
    JSON.stringify({ jsonrpc: "2.0", id: 7, method: "tools/call" }),
  ];
  assert.deepEqual(extractPendingRequestIds(lines), [7]);
});

test("extractPendingRequestIds — EDGE: id=0 included, id=null excluded, empty → []", () => {
  const lines = [
    JSON.stringify({ jsonrpc: "2.0", id: 0, method: "m" }),
    JSON.stringify({ jsonrpc: "2.0", id: null, method: "m" }),
    "",
  ];
  assert.deepEqual(extractPendingRequestIds(lines), [0]);
  assert.deepEqual(extractPendingRequestIds([]), []);
  assert.deepEqual(extractPendingRequestIds(null), []);
});

test("extractPendingRequestIds — custom parse injected", () => {
  const lines = ["A", "B"];
  const fakeParse = (s) => ({ id: s === "A" ? 1 : 2 });
  assert.deepEqual(extractPendingRequestIds(lines, fakeParse), [1, 2]);
});
