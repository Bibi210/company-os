// Integration tests for company/plugins/mcp-proxy.mjs (RFC 5bacb08a D8).
//
// These spawn the REAL proxy against the fake server of ../harness/, so
// they cover what the pure core tests cannot: the spawn/stdio wiring, the
// telemetry files, the progressive health check and the escalation.
//
// They live in this SUBDIRECTORY on purpose. `make test-js` globs
// `company/plugins/tests/*.test.mjs`, which is NOT recursive, so nothing
// here runs inside `make ci` (which the pre-commit runs on every commit).
// Process integration is slower and more timing-sensitive than pure logic,
// and it must never block a commit, nor a deploy-serve, for a machine
// hiccup. Run them explicitly:
//
//     make test-js-integration
//
// Every delay is compressed through the env overrides of the proxy, so the
// whole file runs in a couple of seconds instead of the ~30 s the same
// scenarios would cost at production values.

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, existsSync, readdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const PROXY = resolve(import.meta.dirname, "../../mcp-proxy.mjs");
const FAKE = resolve(import.meta.dirname, "../harness/fake-mcp-server.mjs");

/// Spawn the proxy with a fresh COMPANYOS_ROOT and compressed timings.
function startProxy(env = {}) {
  const root = mkdtempSync(join(tmpdir(), "companyos-proxy-it-"));
  const child = spawn(process.execPath, [PROXY, FAKE], {
    stdio: ["pipe", "pipe", "pipe"],
    env: {
      ...process.env,
      COMPANYOS_ROOT: root,
      MCP_PROXY_HEALTH_CHECK_MS: "250",
      MCP_PROXY_HEALTH_CHECK_CAP_MS: "1000",
      MCP_PROXY_UNAVAILABLE_MS: "1500",
      ...env,
    },
  });
  const stderr = [];
  child.stderr.on("data", (c) => stderr.push(c.toString()));
  const stdout = [];
  child.stdout.on("data", (c) => stdout.push(c.toString()));
  return { child, root, stderr, stdout, journal: () => journalOf(root) };
}

function journalOf(root) {
  const dir = join(root, "company", "data", "logs");
  if (!existsSync(dir)) return "";
  return readdirSync(dir)
    .map((f) => readFileSync(join(dir, f), "utf8"))
    .join("");
}

const initialize = () =>
  `${JSON.stringify({
    jsonrpc: "2.0",
    id: 1,
    method: "initialize",
    params: { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "it", version: "0" } },
  })}\n`;

/// Poll `predicate` until true or `budget` ms elapse.
async function waitFor(predicate, budget = 8000, step = 50) {
  const deadline = Date.now() + budget;
  while (Date.now() < deadline) {
    if (predicate()) return true;
    await new Promise((r) => setTimeout(r, step));
  }
  return false;
}

test("NOMINAL: a slow boot eventually reaches ready, in a bounded number of cycles", async () => {
  // The fake answers initialize after 600 ms while the first window is
  // 250 ms: the first incarnations are killed, the window doubles, and the
  // proxy must converge instead of looping forever. This is the exact
  // failure that required two human reboots on 2026-09-09.
  const p = startProxy({ FAKE_INIT_DELAY_MS: "600" });
  p.child.stdin.write(initialize());

  const ready = await waitFor(() => p.stderr.join("").includes("Health check OK"));
  const log = p.stderr.join("");
  p.child.kill("SIGKILL");

  assert.ok(ready, `proxy never reached ready:\n${log}`);
  const armed = [...log.matchAll(/Health check armed for (\d+)ms/g)].map((m) => Number(m[1]));
  assert.ok(armed.length >= 2, `expected at least one rearm, got ${armed.join(",")}`);
  assert.ok(
    armed.some((d) => d > 250),
    `the window must have grown past the initial one: ${armed.join(",")}`,
  );
  assert.ok(
    armed.every((d) => d <= 1000),
    `the window must never exceed the cap: ${armed.join(",")}`,
  );
  // Bounded: 250 -> 500 -> 1000 is enough to clear a 600 ms boot.
  assert.ok(armed.length <= 8, `too many cycles for a 600ms boot: ${armed.length}`);
});

test("NOMINAL: the journal persists both the proxy events and the child stderr", async () => {
  const p = startProxy({ FAKE_STDERR_LINE: "PANIC-LIKE line from the server" });
  p.child.stdin.write(initialize());
  await waitFor(() => p.journal().includes("PANIC-LIKE"));
  const journal = p.journal();
  p.child.kill("SIGKILL");

  assert.ok(journal.length > 0, "a journal file must exist under company/data/logs/");
  assert.match(journal, /i=\d+ proxy \| /, "proxy events must be recorded with their incarnation");
  assert.match(journal, /i=\d+ server \| PANIC-LIKE line from the server/, "child stderr must be captured");
  // One record per line: this is what makes an incident greppable.
  for (const line of journal.split("\n").filter(Boolean)) {
    assert.match(line, /^\S+ i=\S+ (proxy|server) \| /, `malformed record: ${line}`);
  }
});

test("NOMINAL: the incarnation number is handed to the child at spawn", async () => {
  const p = startProxy({ FAKE_INCARNATION_ECHO: "1" });
  p.child.stdin.write(initialize());
  await waitFor(() => p.journal().includes("incarnation="));
  const journal = p.journal();
  p.child.kill("SIGKILL");

  assert.match(
    journal,
    /server \| incarnation=\d+/,
    "the child must receive MCP_INCARNATION, which is what lets a crash trace be matched with this journal",
  );
});

test("NEGATIVE: a child that keeps dying triggers the -32050 escalation, and supervision continues", async () => {
  // The fake dies 200 ms after each spawn, so the proxy never reaches
  // ready and the unavailability clock (compressed to 1.5 s) fires.
  const p = startProxy({ FAKE_EXIT_AFTER_MS: "200", FAKE_INIT_DELAY_MS: "5000" });
  p.child.stdin.write(initialize());
  // A second request is what the escalation answers: the buffered one. It
  // must go out in its OWN chunk, after the handshake one, otherwise the
  // first-startup fast path forwards both to the child and nothing is ever
  // buffered for the escalation to answer.
  await new Promise((r) => setTimeout(r, 400));
  p.child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id: 42, method: "tools/list" })}\n`);

  const escalated = await waitFor(() => p.stdout.join("").includes("-32050"));
  // The escalation is a SIGNAL, not a stop: wait for the NEXT respawn to
  // be scheduled after it, which is the observable proof that supervision
  // survived the escalation.
  const supervisionContinued = await waitFor(() => {
    const log = p.stderr.join("");
    const i = log.indexOf("escalating pending requests");
    return i >= 0 && log.slice(i).includes("Respawn scheduled");
  });
  const out = p.stdout.join("");
  const log = p.stderr.join("");
  p.child.kill("SIGKILL");

  assert.ok(escalated, `no -32050 emitted:\n${out}\n${log}`);
  const answer = out
    .split("\n")
    .filter(Boolean)
    .map((l) => JSON.parse(l))
    .find((m) => m.error?.code === -32050);
  assert.equal(answer.id, 42, "the escalation must answer the pending request by id");
  assert.equal(answer.error.data.escalate, true, "escalate:true is what prescribes human escalation");
  assert.ok(supervisionContinued, `supervision must continue after an escalation:\n${log}`);
});

test("EDGE: telemetry stays non-lethal when the journal cannot be written", async () => {
  // COMPANYOS_ROOT points at a path under /proc: mkdir fails, so the
  // journal is unavailable. The proxy must keep relaying regardless, and
  // the child's stderr must still be drained (invariant of non-lethality).
  const p = startProxy({ COMPANYOS_ROOT: "/proc/companyos-nowhere", FAKE_STDERR_LINE: "still alive" });
  p.child.stdin.write(initialize());

  const ready = await waitFor(() => p.stderr.join("").includes("Health check OK"));
  const log = p.stderr.join("");
  const alive = p.child.exitCode === null;
  p.child.kill("SIGKILL");

  assert.ok(ready, `the proxy must still reach ready without a journal:\n${log}`);
  assert.ok(alive, "the proxy must not die because telemetry is unavailable");
  assert.match(log, /still alive/, "the child stderr must still be teed live");
  // No assertion on a "telemetry unavailable" message here: under /proc the
  // directory creation does not fail, it HANGS, so the callback never runs
  // and nothing is ever reported. That silence IS the property under test:
  // a wedged filesystem must cost the journal, never the relay. It is also
  // why the creation had to become asynchronous.
  assert.equal(p.journal(), "", "no journal must have been produced");
});
