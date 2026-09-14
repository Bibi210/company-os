// Tests for company/plugins/mcp-proxy-core.mjs (RFC 18011bfc).
// Run via `make test-js` (node --test company/plugins/tests/*.test.mjs).

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  computeBackoff,
  binaryReady,
  buildUnavailableError,
  extractPendingRequestIds,
  computeHealthCheckTimeout,
  shouldRotate,
  rotationPlan,
  formatTelemetryLine,
  BACKOFF_INITIAL_MS,
  BACKOFF_CAP_MS,
  BACKOFF_RESET_AFTER_MS,
  JITTER_RATIO,
  HEALTH_CHECK_INITIAL_MS,
  HEALTH_CHECK_CAP_MS,
  LOG_MAX_BYTES,
  LOG_MAX_ARCHIVES,
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

// ──────────────────── shouldRotate (RFC 5bacb08a D1) ────────────────────

test("shouldRotate — NOMINAL: below the cap keeps the journal, above rotates", () => {
  assert.equal(shouldRotate(100, 10, { maxBytes: 1000 }), false);
  assert.equal(shouldRotate(995, 10, { maxBytes: 1000 }), true);
  assert.equal(shouldRotate(0, 1, { maxBytes: LOG_MAX_BYTES }), false);
});

test("shouldRotate — EDGE: exact equality with the cap does NOT rotate", () => {
  // 990 + 10 === 1000: the cap is a ceiling we may reach, not cross.
  assert.equal(shouldRotate(990, 10, { maxBytes: 1000 }), false);
  assert.equal(shouldRotate(991, 10, { maxBytes: 1000 }), true);
});

test("shouldRotate — NEGATIVE: junk inputs never throw and never rotate blindly", () => {
  assert.equal(shouldRotate(NaN, NaN, { maxBytes: 1000 }), false);
  assert.equal(shouldRotate(-5, -5, { maxBytes: 1000 }), false);
  assert.equal(shouldRotate(undefined, undefined, { maxBytes: 1000 }), false);
  // A meaningless cap disables rotation rather than rotating on every line.
  assert.equal(shouldRotate(10_000, 1, { maxBytes: 0 }), false);
  assert.equal(shouldRotate(10_000, 1, { maxBytes: -1 }), false);
  assert.equal(shouldRotate(10_000, 1, { maxBytes: NaN }), false);
});

// ──────────────────── rotationPlan (RFC 5bacb08a D1) ────────────────────

test("rotationPlan — NOMINAL: oldest archive dropped, others shifted up in order", () => {
  const plan = rotationPlan("x.log", { keptArchives: 3 });
  assert.equal(plan.unlink, "x.log.3");
  assert.deepEqual(plan.renames, [
    { from: "x.log.2", to: "x.log.3" },
    { from: "x.log.1", to: "x.log.2" },
    { from: "x.log", to: "x.log.1" },
  ]);
});

test("rotationPlan — EDGE: order never overwrites an archive still needed", () => {
  const plan = rotationPlan("x.log", { keptArchives: 4 });
  // Every destination, except the last one, must be renamed away BEFORE it
  // is written to. Walking the list, a `to` may only collide with a `from`
  // that appeared earlier.
  const alreadyMoved = new Set();
  for (const { from, to } of plan.renames) {
    assert.ok(
      !plan.renames.some((r) => r.from === to) || alreadyMoved.has(to),
      `${to} would be overwritten while still holding data`,
    );
    alreadyMoved.add(from);
  }
  assert.equal(plan.unlink, "x.log.4");
});

test("rotationPlan — EDGE: a single kept archive still shifts the live journal", () => {
  const plan = rotationPlan("x.log", { keptArchives: 1 });
  assert.equal(plan.unlink, "x.log.1");
  assert.deepEqual(plan.renames, [{ from: "x.log", to: "x.log.1" }]);
});

test("rotationPlan — NEGATIVE: junk inputs yield an inert plan", () => {
  assert.deepEqual(rotationPlan(""), { unlink: null, renames: [] });
  assert.deepEqual(rotationPlan(null), { unlink: null, renames: [] });
  assert.deepEqual(rotationPlan(42), { unlink: null, renames: [] });
  // Zero archive: drop the journal, rename nothing.
  assert.deepEqual(rotationPlan("x.log", { keptArchives: 0 }), {
    unlink: "x.log",
    renames: [],
  });
});

test("rotationPlan — default keeps LOG_MAX_ARCHIVES archives", () => {
  const plan = rotationPlan("x.log");
  assert.equal(plan.unlink, `x.log.${LOG_MAX_ARCHIVES}`);
  assert.equal(plan.renames.length, LOG_MAX_ARCHIVES);
});

// ───────────────── formatTelemetryLine (RFC 5bacb08a D1) ─────────────────

test("formatTelemetryLine — NOMINAL: structured prefix, one trailing newline", () => {
  const line = formatTelemetryLine({
    timestamp: "2026-09-11T10:33:37.000Z",
    incarnation: 3,
    source: "server",
    text: "thread 'tokio-runtime-worker' panicked at engine.rs:2262",
  });
  assert.equal(
    line,
    "2026-09-11T10:33:37.000Z i=3 server | thread 'tokio-runtime-worker' panicked at engine.rs:2262\n",
  );
  assert.equal(line.split("\n").length, 2, "exactly one record, one newline");
});

test("formatTelemetryLine — EDGE: embedded newlines collapse so a record stays one line", () => {
  const line = formatTelemetryLine({
    timestamp: "2026-09-11T10:33:37.000Z",
    incarnation: 1,
    source: "server",
    text: "stack:\n  frame 1\r\n  frame 2",
  });
  assert.equal(line.split("\n").length, 2, "one record, whatever the payload");
  assert.ok(!line.slice(0, -1).includes("\r"), "no stray CR inside the record");
  assert.ok(line.includes("stack:") && line.includes("frame 1") && line.includes("frame 2"));
});

test("formatTelemetryLine — EDGE: incarnation 0 is a real value, not a missing one", () => {
  const line = formatTelemetryLine({
    timestamp: "T",
    incarnation: 0,
    source: "proxy",
    text: "boot",
  });
  assert.equal(line, "T i=0 proxy | boot\n");
});

test("formatTelemetryLine — NEGATIVE: missing or bogus fields degrade, never throw", () => {
  assert.equal(formatTelemetryLine(), "- i=- unknown | \n");
  assert.equal(
    formatTelemetryLine({ timestamp: "T", incarnation: "x", source: "hacker", text: null }),
    "T i=- unknown | \n",
  );
  assert.equal(
    formatTelemetryLine({ timestamp: "T", incarnation: 2, source: "proxy", text: 42 }),
    "T i=2 proxy | 42\n",
  );
});

// ───────────── computeHealthCheckTimeout (RFC 5bacb08a D4) ─────────────

test("computeHealthCheckTimeout — NOMINAL: 10s, 20s, 40s then flat at the cap", () => {
  assert.equal(computeHealthCheckTimeout(0), 10_000);
  assert.equal(computeHealthCheckTimeout(1), 20_000);
  assert.equal(computeHealthCheckTimeout(2), 40_000);
  assert.equal(computeHealthCheckTimeout(3), 40_000);
  assert.equal(computeHealthCheckTimeout(50), 40_000);
});

test("computeHealthCheckTimeout — EDGE: the cap stays UNDER the 45s escalation", () => {
  // The whole point of lowering the ceiling from 60s to 40s (finding 4 of
  // the design review): a health-check window must never outlive the
  // escalation deadline it is supposed to sit below.
  assert.ok(HEALTH_CHECK_CAP_MS < 45_000);
  assert.equal(HEALTH_CHECK_INITIAL_MS, 10_000);
  for (let n = 0; n < 40; n += 1) {
    assert.ok(computeHealthCheckTimeout(n) <= HEALTH_CHECK_CAP_MS);
  }
});

test("computeHealthCheckTimeout — NEGATIVE: junk degrades to the initial window", () => {
  assert.equal(computeHealthCheckTimeout(NaN), 10_000);
  assert.equal(computeHealthCheckTimeout(-3), 10_000);
  assert.equal(computeHealthCheckTimeout(undefined), 10_000);
  assert.equal(computeHealthCheckTimeout(1.9), 20_000); // floored
  assert.equal(computeHealthCheckTimeout(0, { initialMs: 0, capMs: -1 }), 10_000);
});

test("computeHealthCheckTimeout — overrides compress the delays for tests", () => {
  assert.equal(computeHealthCheckTimeout(0, { initialMs: 100, capMs: 400 }), 100);
  assert.equal(computeHealthCheckTimeout(1, { initialMs: 100, capMs: 400 }), 200);
  assert.equal(computeHealthCheckTimeout(9, { initialMs: 100, capMs: 400 }), 400);
});
