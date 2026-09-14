#!/usr/bin/env node
// Fake MCP server for the proxy integration tests (RFC 5bacb08a D8).
//
// Speaks just enough NDJSON JSON-RPC to be supervised by mcp-proxy.mjs,
// and nothing else. Every behaviour the tests need is driven by the
// environment, so one binary covers the three scenarios:
//
//   FAKE_INIT_DELAY_MS   delay before answering `initialize` (default 0).
//                        Above the proxy's health-check window, this
//                        reproduces the "slow but living boot" that used
//                        to be killed forever.
//   FAKE_STDERR_LINE     line written to stderr at startup, to assert the
//                        proxy captures and persists the child's stderr.
//   FAKE_EXIT_AFTER_MS   exit(1) after this delay, to exercise the death
//                        and respawn path.
//   FAKE_INCARNATION_ECHO  when "1", echo the received MCP_INCARNATION on
//                        stderr, proving the proxy hands it over.
//
// It must never exit on its own otherwise: the proxy is the supervisor.

const initDelay = Number(process.env.FAKE_INIT_DELAY_MS ?? 0);
const stderrLine = process.env.FAKE_STDERR_LINE ?? "";
const exitAfter = Number(process.env.FAKE_EXIT_AFTER_MS ?? 0);

if (stderrLine) process.stderr.write(`${stderrLine}\n`);
if (process.env.FAKE_INCARNATION_ECHO === "1") {
  process.stderr.write(`incarnation=${process.env.MCP_INCARNATION ?? "<unset>"}\n`);
}
if (Number.isFinite(exitAfter) && exitAfter > 0) {
  setTimeout(() => process.exit(1), exitAfter);
}

let buf = "";
process.stdin.on("data", (chunk) => {
  buf += chunk.toString();
  let idx;
  while ((idx = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, idx);
    buf = buf.slice(idx + 1);
    if (!line) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    if (msg.method === "initialize" && msg.id !== undefined) {
      const reply = {
        jsonrpc: "2.0",
        id: msg.id,
        result: {
          protocolVersion: "2024-11-05",
          capabilities: { tools: {} },
          serverInfo: { name: "fake-mcp-server", version: "0.1.0" },
        },
      };
      const send = () => process.stdout.write(`${JSON.stringify(reply)}\n`);
      if (Number.isFinite(initDelay) && initDelay > 0) setTimeout(send, initDelay);
      else send();
    } else if (msg.id !== undefined) {
      // Any other request: answer immediately so a test can check the
      // proxy relays traffic once ready.
      process.stdout.write(
        `${JSON.stringify({ jsonrpc: "2.0", id: msg.id, result: { echo: msg.method } })}\n`,
      );
    }
  }
});

// Stay alive until the supervisor decides otherwise.
setInterval(() => {}, 1 << 30);
