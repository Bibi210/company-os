// Defense in Depth — CORE logic (hot-reloadable)
//
// Layer 1: Validates writes (JSON Schema + protected zone + write permit)
// Layer 2: Guards bash commands (git snapshot before, revert check after)
//
// Hot-reload: when a write with active permit succeeds on a protected zone,
// the wrapper is notified via onProtectedZoneWrite callback to reload what's needed.
//
// Diagnostic format: [companyos:defense-in-depth] SEVERITY: message
//                    | context: ...
//                    | reason:  ...
//                    | fix:     ...

import { execSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { resolve, relative } from "node:path";

// Bootstrap: the one hardcoded path — everything else is read from it
const ZONES_FILE = "company/config/protected-zones.json";

// Loaded from protected-zones.json on init
let _zones = { prefixes: [], files: [], db_path: "" };

function loadProtectedZones(rootDir) {
  try {
    _zones = JSON.parse(readFileSync(resolve(rootDir, ZONES_FILE), "utf-8"));
  } catch { /* keep previous */ }
}

function isInProtectedZone(rel) {
  return (
    _zones.prefixes.some((prefix) => rel.startsWith(prefix)) ||
    _zones.files.includes(rel)
  );
}

const YAML_EXTENSIONS = [".yml", ".yaml"];
const BASH_SAFE_PATHS = ["target/", "/tmp/", ".cache/", "node_modules/"];

const VOLATILE_PATTERNS = [
  /\.db$/, /\.db-shm$/, /\.db-wal$/, /\.db-journal$/, /\.lock$/, /\.pid$/,
];

const C = "defense-in-depth";

function diag(severity, message, { context, reason, fix } = {}) {
  let out = `[companyos:${C}] ${severity}: ${message}`;
  if (context) out += `\n| context: ${context}`;
  if (reason) out += `\n| reason:  ${reason}`;
  if (fix) out += `\n| fix:     ${fix}`;
  return out;
}

function isYaml(filePath) {
  return YAML_EXTENSIONS.some((ext) => filePath.endsWith(ext));
}

function isSafeBashPath(filePath) {
  return BASH_SAFE_PATHS.some(
    (safe) => filePath.startsWith(safe) || filePath.startsWith("/" + safe),
  );
}

function isVolatile(filePath) {
  return VOLATILE_PATTERNS.some((pattern) => pattern.test(filePath));
}

function run(cmd, cwd) {
  try {
    return execSync(cmd, { cwd, encoding: "utf-8", timeout: 10000 }).trim();
  } catch {
    return null;
  }
}

function hasActivePermit(rootDir, filePath) {
  const dbPath = resolve(rootDir, _zones.db_path);
  if (!existsSync(dbPath)) return false;

  const rel = relative(rootDir, resolve(rootDir, filePath));

  const query = `SELECT target_paths FROM write_permits WHERE status = 'active'`;
  const result = run(`sqlite3 "${dbPath}" "${query}"`, rootDir);
  if (!result) return false;

  for (const line of result.split("\n")) {
    try {
      const paths = JSON.parse(line);
      for (const pattern of paths) {
        if (rel.startsWith(pattern) || rel === pattern) return true;
        if (pattern.endsWith("*") && rel.startsWith(pattern.slice(0, -1)))
          return true;
      }
    } catch {
      if (rel.startsWith(line) || rel === line) return true;
    }
  }
  return false;
}

function validateYaml(rootDir, filePath) {
  const result = run(
    `./target/release/companyos-yaml-validator --file "${filePath}"`,
    rootDir,
  );
  if (result === null) {
    return run(
      `./target/debug/companyos-yaml-validator --file "${filePath}"`,
      rootDir,
    );
  }
  return result;
}

function autoIndex(rootDir, filePath) {
  return (
    run(`./target/release/companyos-orchestrator-server --index "${filePath}"`, rootDir) ??
    run(`./target/debug/companyos-orchestrator-server --index "${filePath}"`, rootDir)
  );
}

function revertFile(rootDir, filePath) {
  run(`git checkout -- "${filePath}"`, rootDir);
}

// Snapshot write_permits to detect tampering via bash
function snapshotPermits(rootDir) {
  const dbPath = resolve(rootDir, _zones.db_path);
  if (!existsSync(dbPath)) return null;
  // Hash of all permits: count + ids + statuses
  return run(`sqlite3 "${dbPath}" "SELECT COUNT(*), GROUP_CONCAT(id || ':' || status) FROM write_permits"`, rootDir);
}

function revertPermitTampering(rootDir, before) {
  const after = snapshotPermits(rootDir);
  if (before === null && after === null) return false;
  if (before === after) return false;
  // Permits were tampered — restore from backup or delete new ones
  const dbPath = resolve(rootDir, _zones.db_path);
  // Nuclear option: delete any permits not in the before snapshot
  if (before === null) {
    // DB didn't exist before, now it does — wipe permits
    run(`sqlite3 "${dbPath}" "DELETE FROM write_permits"`, rootDir);
  } else {
    // Extract IDs from before snapshot and delete anything new
    const beforeIds = (before.split(",").map(s => s.split(":")[0]).filter(Boolean));
    const placeholders = beforeIds.map(id => `'${id}'`).join(",");
    if (placeholders) {
      run(`sqlite3 "${dbPath}" "DELETE FROM write_permits WHERE id NOT IN (${placeholders})"`, rootDir);
    }
  }
  return true;
}

export { isYaml, isSafeBashPath, isVolatile, isInProtectedZone, loadProtectedZones };

export const createHandlers = (rootDir, sessions, onProtectedZoneWrite) => {
  // Load protected zones on init
  loadProtectedZones(rootDir);

  function getState(sessionId) {
    if (!sessions.has(sessionId)) {
      sessions.set(sessionId, {
        gitHead: null,
        gitDiffStat: null,
        gitDiffBefore: null,
        permitSnapshot: null,
      });
    }
    return sessions.get(sessionId);
  }

  return {
    // -----------------------------------------------------------------
    // Layer 0 (before): Auth & DB access guard
    // -----------------------------------------------------------------
    "tool.execute.before": async (input) => {
      // Block cross-persona authentication
      if (input.tool === "mcp_orchestrator_authenticate") {
        const persona = input.args?.persona;
        const agent = input.agent;
        if (agent && persona && agent !== persona) {
          throw new Error(
            diag("ERROR", `Authentication denied: agent '${agent}' cannot authenticate as '${persona}'`, {
              context: `authenticate(persona=${persona})`,
              reason: `Each agent can only authenticate as itself. Cross-persona authentication is forbidden.`,
              fix: `Call authenticate(persona="${agent}") instead.`,
            }),
          );
        }
      }

      // Block bash commands that access the DB directly
      if (input.tool === "bash") {
        const cmd = input.args?.command || "";
        const dbFile = _zones.db_path;
        if (dbFile && cmd.includes(dbFile)) {
          throw new Error(
            diag("ERROR", "Direct database access blocked", {
              context: `bash command references ${dbFile}`,
              reason: "Direct access to the orchestrator database is forbidden. Use MCP tools instead.",
              fix: "Use orchestrator MCP tools (search, grant_write_permit, etc.) to interact with the database.",
            }),
          );
        }
        // Also block sqlite3 + any .db file in protected zones
        if (/sqlite3\s/.test(cmd) && _zones.prefixes.some((p) => cmd.includes(p))) {
          throw new Error(
            diag("ERROR", "Direct database access blocked", {
              context: "bash command uses sqlite3 on protected zone",
              reason: "Running sqlite3 on files in protected zones is forbidden.",
              fix: "Use MCP tools to interact with the orchestrator.",
            }),
          );
        }
      }

      // -----------------------------------------------------------------
      // Layer 2 (before): Snapshot git state before bash commands
      // -----------------------------------------------------------------
      if (input.tool === "bash") {
        const state = getState(input.sessionID);
        state.gitHead = run("git rev-parse HEAD", rootDir);
        state.gitDiffStat = run("git diff --stat", rootDir);
        state.gitDiffBefore = run("git diff --name-only", rootDir) ?? "";
        state.permitSnapshot = snapshotPermits(rootDir);
      }
    },

    // -----------------------------------------------------------------
    // Layer 1 + Layer 2 (after): Validate writes, guard bash
    // -----------------------------------------------------------------
    "tool.execute.after": async (input) => {
      const filePath = input.args?.file_path || input.args?.filePath;

      // ---------------------------------------------------------------
      // Layer 1: write/edit enforcement
      // ---------------------------------------------------------------
      if (
        (input.tool === "edit" || input.tool === "write") &&
        typeof filePath === "string"
      ) {
        const rel = relative(rootDir, resolve(rootDir, filePath));

        if (isInProtectedZone(rel)) {
          if (!hasActivePermit(rootDir, filePath)) {
            revertFile(rootDir, rel);
            throw new Error(
              diag("ERROR", `Write blocked in protected zone: ${rel}`, {
                context: `${input.tool}(file_path=${rel})`,
                reason: `File is in a protected zone and no active write permit covers it. Change reverted.`,
                fix: `Request a write permit via grant_write_permit (CEO only, requires an approved RFC)`,
              }),
            );
          }

          // Write with permit succeeded on a protected zone → notify wrapper
          onProtectedZoneWrite?.(rel);
        }

        if (isYaml(filePath)) {
          const result = validateYaml(rootDir, rel);
          if (result === null) {
            revertFile(rootDir, rel);
            throw new Error(
              diag("ERROR", `Schema validation failed: ${rel}`, {
                context: `${input.tool}(file_path=${rel})`,
                reason: `The file does not pass JSON Schema validation. Change reverted.`,
                fix: `Check api_version, kind, and metadata.id fields. Use validate_yaml to debug.`,
              }),
            );
          }

          autoIndex(rootDir, rel);
        }
      }

      // ---------------------------------------------------------------
      // Layer 2: bash aftermath check
      // ---------------------------------------------------------------
      if (input.tool === "bash") {
        const state = getState(input.sessionID);

        // Layer 3: detect DB tampering (self-granted permits)
        if (revertPermitTampering(rootDir, state.permitSnapshot)) {
          throw new Error(
            diag("ERROR", "Write permit tampering detected", {
              context: "bash command aftermath — permit integrity check",
              reason: "The write_permits table was modified by a bash command. Unauthorized permits have been removed.",
              fix: "Write permits can only be granted via the grant_write_permit MCP tool (CEO only).",
            }),
          );
        }

        const diffBefore = new Set(
          (state.gitDiffBefore ?? "").split("\n").filter(Boolean),
        );

        const changedFiles = run("git diff --name-only", rootDir);
        if (changedFiles) {
          for (const file of changedFiles.split("\n")) {
            if (!file) continue;
            if (isSafeBashPath(file)) continue;
            if (isVolatile(file)) continue;
            if (diffBefore.has(file)) continue;

            if (isInProtectedZone(file) && !hasActivePermit(rootDir, file)) {
              run(`git checkout -- "${file}"`, rootDir);
              throw new Error(
                diag("ERROR", `Bash modified protected zone: ${file}`, {
                  context: `bash command aftermath check`,
                  reason: `The bash command modified a protected file without a write permit. Change reverted.`,
                  fix: `Request a write permit via grant_write_permit before modifying protected zones`,
                }),
              );
            }
          }
        }

        // 2b. Check unauthorized commits
        const currentHead = run("git rev-parse HEAD", rootDir);
        if (state.gitHead && currentHead && currentHead !== state.gitHead) {
          const commitFiles = run(
            `git diff --name-only ${state.gitHead} ${currentHead}`,
            rootDir,
          );
          if (commitFiles) {
            const unauthorized = commitFiles
              .split("\n")
              .filter(
                (f) =>
                  f &&
                  !isSafeBashPath(f) &&
                  !isVolatile(f) &&
                  isInProtectedZone(f) &&
                  !hasActivePermit(rootDir, f),
              );
            if (unauthorized.length > 0) {
              run(`git reset --soft ${state.gitHead}`, rootDir);
              for (const f of unauthorized) {
                run(`git checkout -- "${f}"`, rootDir);
              }
              throw new Error(
                diag(
                  "ERROR",
                  `Unauthorized commit touching protected zones`,
                  {
                    context: `bash command created commit ${currentHead.slice(0, 8)}`,
                    reason: `Commit modified protected files without write permit: ${unauthorized.join(", ")}. Commit reset, files reverted.`,
                    fix: `Request write permits for all protected files before committing`,
                  },
                ),
              );
            }
          }
        }

        // Reset snapshot
        state.gitHead = null;
        state.gitDiffStat = null;
        state.gitDiffBefore = null;
        state.permitSnapshot = null;
      }
    },
  };
};
