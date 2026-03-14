// MCP Proxy — Hot-reload wrapper for Rust MCP servers
//
// Usage: node mcp-proxy.mjs <binary-path> [args...]
//
// Features:
// - Auto-build: if binary doesn't exist, runs cargo build -p <crate>
// - Forward: pipes JSON-RPC messages between opencode (stdio) and child process
// - Binary watch: restarts child when binary file changes on disk (cargo build)
// - MCP re-handshake: replays the initialize request after restart

import { spawn, execSync } from "node:child_process";
import { existsSync, watch, statSync } from "node:fs";
import { basename } from "node:path";

const [binaryPath, ...extraArgs] = process.argv.slice(2);

if (!binaryPath) {
  console.error("[mcp-proxy] Usage: node mcp-proxy.mjs <binary-path> [args...]");
  process.exit(1);
}

// Derive crate package name from binary name (e.g. ./target/debug/companyos-foo → companyos-foo)
const crateName = basename(binaryPath);

// --- State ---
let child = null;
let lastInitRequest = null; // memorize initialize request for re-handshake
let watchDebounce = null;

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
      timeout: 300000, // 5min max
    });
    log("Build complete");
  } catch (e) {
    log(`Build failed: ${e.message}`);
    process.exit(1);
  }
}

// --- Child process management ---
function startChild() {
  child = spawn(binaryPath, extraArgs, {
    stdio: ["pipe", "pipe", "inherit"],
    env: { ...process.env },
  });

  // Forward child stdout → our stdout (to opencode)
  child.stdout.on("data", (chunk) => {
    process.stdout.write(chunk);
  });

  child.on("error", (err) => {
    log(`Child error: ${err.message}`);
  });

  child.on("exit", (code, signal) => {
    if (!restarting) {
      log(`Child exited (code=${code}, signal=${signal}), proxy exiting`);
      process.exit(code ?? 1);
    }
  });

  // If we have a memorized initialize request, replay it
  if (lastInitRequest) {
    log("Replaying initialize handshake");
    child.stdin.write(lastInitRequest);
  }
}

let restarting = false;

function restartChild() {
  if (restarting) return;
  restarting = true;
  log("Restarting child process...");

  if (child) {
    child.kill("SIGTERM");
    child.on("close", () => {
      restarting = false;
      startChild();
    });
    // Force kill after 3s if graceful shutdown fails
    setTimeout(() => {
      if (restarting) {
        child?.kill("SIGKILL");
      }
    }, 3000);
  } else {
    restarting = false;
    startChild();
  }
}

// --- Forward stdin (from opencode) → child, with initialize capture ---
process.stdin.on("data", (chunk) => {
  // Capture initialize request for replay after restart
  const str = chunk.toString();
  // MCP JSON-RPC: look for "method":"initialize" in the message
  if (str.includes('"initialize"') && str.includes('"method"')) {
    lastInitRequest = chunk;
  }
  child?.stdin.write(chunk);
});

process.stdin.on("end", () => {
  // opencode closed stdin, terminate child
  child?.kill("SIGTERM");
  setTimeout(() => process.exit(0), 1000);
});

// --- Watch binary for rebuilds ---
function watchBinary() {
  try {
    watch(binaryPath, () => {
      clearTimeout(watchDebounce);
      watchDebounce = setTimeout(() => {
        // Verify the binary is actually complete (not mid-write)
        try {
          const stat = statSync(binaryPath);
          if (stat.size > 0) {
            log("Binary changed on disk, restarting...");
            restartChild();
          }
        } catch {
          // File might be temporarily gone during build
        }
      }, 1500); // 1.5s debounce for cargo build multi-step writes
    });
  } catch {
    log("Warning: could not watch binary file, auto-restart disabled");
  }
}

// --- Main ---
ensureBinary();
startChild();
watchBinary();
