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
import { existsSync, readFileSync, rmdirSync, unlinkSync } from "node:fs";
import { resolve, relative, dirname } from "node:path";

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

// Artifact dirs subject to naming convention enforcement.
// Keep in sync with spec.rules.file_placement in company/config/shared-rules.yml.
const ARTIFACT_NAMING_PREFIXES = [
  "projects/",
  "company/rfcs/",
  "company/lessons/",
  "company/agent-messages/",
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

function isArtifactPath(rel) {
  return ARTIFACT_NAMING_PREFIXES.some((prefix) => rel.startsWith(prefix));
}

function checkNaming(rootDir, rel) {
  // Delegate to check-artifact-naming.sh (single source of truth for the regex).
  // Returns null on non-zero exit (= naming violation).
  return run(
    `FILE="${rel}" ./company/scripts/check-artifact-naming.sh`,
    rootDir,
  );
}

function run(cmd, cwd) {
  try {
    return execSync(cmd, { cwd, encoding: "utf-8", timeout: 10000 }).trim();
  } catch {
    return null;
  }
}

// Helper that wraps a SQL statement with the same defensive PRAGMA preamble
// used by the Rust binary (busy_timeout=5000, synchronous=NORMAL), so that
// concurrent accesses between the server and the hook don't immediately
// fail with SQLITE_BUSY but wait up to 5s for the lock. This is an
// imperfect mitigation: the hook still opens its own sqlite3 connection,
// which violates the "single writer" invariant of PILIER A. The proper
// fix is to refactor these call sites into MCP tool calls (RFC cdbfee72
// PROPOSITION 5), but the orchestrator MCP transport is stdio-only and
// the opencode plugin context does not expose an MCP client. An amendment
// of the RFC documenting this constraint is part of plan étape 10.0.
function runSqliteWithTimeout(dbPath, sql, cwd) {
  // .timeout is a sqlite3 CLI dot-command equivalent to PRAGMA busy_timeout.
  // It must precede the SQL statement on the same command line.
  return run(
    `sqlite3 "${dbPath}" ".timeout 5000" "${sql}"`,
    cwd,
  );
}

function hasActivePermit(rootDir, filePath) {
  const dbPath = resolve(rootDir, _zones.db_path);
  if (!existsSync(dbPath)) return false;

  const rel = relative(rootDir, resolve(rootDir, filePath));

  const query = `SELECT target_paths FROM write_permits WHERE status = 'active'`;
  const result = runSqliteWithTimeout(dbPath, query, rootDir);
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

// Supprime les dossiers parents vides remontant jusqu'à la racine projet
function cleanupEmptyParentDirs(rootDir, filePath) {
  let dir = dirname(resolve(rootDir, filePath));
  const root = resolve(rootDir);
  while (dir !== root && dir.startsWith(root)) {
    try {
      rmdirSync(dir); // échoue si non vide — comportement voulu
      dir = dirname(dir);
    } catch {
      break;
    }
  }
}

function revertFile(rootDir, filePath) {
  // Cas A : fichier tracké dans HEAD → restaurer depuis HEAD
  // NOTE: git ls-files --error-unmatch retourne exit 0 pour les fichiers stagés
  // mais pas encore committés → faux positif. On utilise git cat-file -e HEAD:<file>
  // qui vérifie uniquement HEAD, pas l'index.
  const isTracked =
    run(`git cat-file -e "HEAD:${filePath}"`, rootDir) !== null;
  if (isTracked) {
    run(`git checkout -- "${filePath}"`, rootDir);
    return;
  }

  // Cas B : fichier staged-new (après reset --soft d'un commit) → désindexer
  const stagedOutput = run(`git ls-files --stage "${filePath}"`, rootDir);
  if (stagedOutput?.trim()) {
    run(`git rm --cached "${filePath}"`, rootDir);
  }

  // Cas B + C : supprimer le fichier du disque
  run(`rm -f "${filePath}"`, rootDir);

  // Nettoyer les dossiers parents vides créés avec le fichier
  cleanupEmptyParentDirs(rootDir, filePath);
}

// Snapshot write_permits to detect tampering via bash
function snapshotPermits(rootDir) {
  const dbPath = resolve(rootDir, _zones.db_path);
  if (!existsSync(dbPath)) return null;
  // Hash of all permits: count + ids + statuses.
  // .timeout 5000 mitigates SQLITE_BUSY contention with the server (see
  // runSqliteWithTimeout comment).
  return runSqliteWithTimeout(
    dbPath,
    "SELECT COUNT(*), GROUP_CONCAT(id || ':' || status) FROM write_permits",
    rootDir,
  );
}

// SOURCE B — ids des permits scellés dans HEAD git.
// Lit company/data/orchestrator.db tel que committé en HEAD, en matérialisant
// une copie temporaire HORS zone protégée (/tmp, couvert par BASH_SAFE_PATHS),
// puis SELECT id dessus. Fiabilité garantie par RFC 359f9162 : tout permit
// légitime est scellé en HEAD au moment du grant atomique.
// Contrat :
//   - retourne un Set<string> d'ids (éventuellement VIDE si HEAD a 0 permit)
//     => Source B EXPLOITABLE, contribue ses ids (0 ou +) à l'union.
//   - retourne null si Source B INEXPLOITABLE (pas de DB en HEAD, git en échec,
//     SELECT en échec, DB corrompue).
// La distinction Set vide vs null est CRITIQUE et ne doit jamais être confondue.
function permitIdsFromHeadDb(rootDir) {
  const dbPath = _zones.db_path;
  if (!dbPath) return null;

  // (1) La DB existe-t-elle en HEAD ? run() renvoie "" si présente, null sinon.
  if (run(`git cat-file -e "HEAD:${dbPath}"`, rootDir) === null) return null;

  // (2) Chemin temporaire unique dans /tmp (jamais en zone protégée).
  const tmpPath = `/tmp/companyos-headdb-${process.pid}-${Date.now()}.db`;

  try {
    // (3) Matérialiser la DB committée. execSync (via run) n'est PAS intercepté
    //     par les guards anti-DB L283 / anti-sqlite3 L293 (ceux-ci ne filtrent
    //     que input.tool === "bash").
    if (run(`git show "HEAD:${dbPath}" > "${tmpPath}"`, rootDir) === null) {
      return null; // git show a échoué
    }
    // (4) SELECT des ids sur la copie /tmp (chemin sûr, sqlite3 légitime ici).
    const out = runSqliteWithTimeout(tmpPath, "SELECT id FROM write_permits", rootDir);
    if (out === null) return null; // SELECT en échec / DB illisible / corrompue

    // (5) Parser : un id par ligne. Set éventuellement vide si HEAD a 0 permit.
    return new Set(out.split("\n").filter(Boolean));
  } catch {
    return null; // toute exception -> Source B inexploitable
  } finally {
    // (6) CLEANUP INCONDITIONNEL : pas de fuite de fichier temporaire.
    try { unlinkSync(tmpPath); } catch { /* déjà absent / jamais créé */ }
  }
}

// SOURCE A — ids extraits du blob snapshot "before" (snapshotPermits).
// Format du blob : "<count>|<id1>:<status1>,<id2>:<status2>,...".
// Réutilise EXACTEMENT le même split que l'ancien beforeIds L228
// (split(",") puis split(":")[0]), MAIS strippe en plus le préfixe "<count>|"
// qui pollue le premier segment : sans ce strip, le premier id deviendrait
// "<count>|<id1>" — un id fantôme qui ne matche jamais un vrai UUID (bénin
// dans l'ancien code, mais on l'évite ici pour la robustesse). Le SET d'ids
// importe, pas l'ordre.
// Contrat :
//   - before === null -> retourne null (Source A absente).
//   - before non null -> Set<string> (VIDE si "0|" : 0 permit dans le snapshot).
function parsePermitIds(before) {
  if (before === null) return null;
  const ids = before
    .split(",")
    .map((segment) => {
      // Strip du préfixe "<count>|" présent uniquement sur le premier segment.
      const pipeIdx = segment.lastIndexOf("|");
      const cleaned = pipeIdx >= 0 ? segment.slice(pipeIdx + 1) : segment;
      return cleaned.split(":")[0];
    })
    .filter(Boolean);
  return new Set(ids);
}

// Construit la baseline légitime par UNION des deux sources.
// baseline = union(Source A si non nulle, Source B si exploitable).
// Contrat :
//   - Source A null ET Source B null -> retourne null : baseline INDÉTERMINABLE
//     (= cas résiduel -> wipe nucléaire dans revertPermitTampering).
//   - sinon -> Set<string> (l'union), éventuellement VIDE : baseline
//     DÉTERMINABLE mais vide (-> DELETE sur TOUT, ferme la faille L230).
// La distinction null (indéterminable) vs Set vide (déterminable-vide) est le
// coeur de l'invariant : sélectif si baseline connue, nucléaire seulement si
// baseline inconnue.
function buildBaseline(rootDir, before) {
  const sourceA = parsePermitIds(before); // Set | null
  const sourceB = permitIdsFromHeadDb(rootDir); // Set | null
  if (sourceA === null && sourceB === null) return null;
  return new Set([...(sourceA ?? []), ...(sourceB ?? [])]);
}

function revertPermitTampering(rootDir, before) {
  const after = snapshotPermits(rootDir);
  if (before === null && after === null) return false; // pas de DB
  if (before === after) return false; // identique, aucune altération

  // Divergence détectée. La baseline légitime est l'UNION du snapshot before
  // (Source A) et des permits scellés en HEAD git (Source B).
  const dbPath = resolve(rootDir, _zones.db_path);
  const baseline = buildBaseline(rootDir, before); // Set | null

  if (baseline === null) {
    // CAS RÉSIDUEL : Source A null ET Source B inexploitable. Aucune baseline
    // légitime affirmable -> wipe total (fail-safe). SEUL chemin nucléaire
    // restant (décision CEO 1, RFC bde023e2). Borné à ce cas strict.
    runSqliteWithTimeout(dbPath, "DELETE FROM write_permits", rootDir);
  } else {
    // CAS NOMINAL : baseline déterminable (même vide). DELETE SÉLECTIF : on
    // supprime tout id HORS baseline. Les ids légitimes (before + HEAD) sont
    // préservés ; tout intrus hors-canal est supprimé.
    // Quoting : les ids sont des UUID v4 (generate_id), sans quote simple ni
    // caractère SQL spécial, et proviennent tous du canal MCP (snapshot + HEAD).
    // L'interpolation `'${id}'` reste sûre dans ce contexte fermé (même
    // hypothèse que l'ancien code L229).
    const placeholders = [...baseline].map((id) => `'${id}'`).join(",");
    if (placeholders) {
      runSqliteWithTimeout(
        dbPath,
        `DELETE FROM write_permits WHERE id NOT IN (${placeholders})`,
        rootDir,
      );
    } else {
      // Baseline déterminable mais VIDE -> aucun id légitime -> DELETE sur TOUT.
      // FERME LA FAILLE L230 : avant, placeholders vide => aucun DELETE émis
      // => un permit injecté pendant un bash sur DB initialement vide survivait.
      runSqliteWithTimeout(dbPath, "DELETE FROM write_permits", rootDir);
    }
  }
  return true;
}

export {
  isYaml,
  isSafeBashPath,
  isVolatile,
  isInProtectedZone,
  loadProtectedZones,
  snapshotPermits,
  parsePermitIds,
  permitIdsFromHeadDb,
  buildBaseline,
  revertPermitTampering,
};

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
      // "write"/"edit"         = native OpenCode tools (direct agent session)
      // "mcp_write"/"mcp_edit" = same tools exposed via MCP to sub-agents (Task tool)
      if (
        (input.tool === "edit" || input.tool === "write" ||
         input.tool === "mcp_edit" || input.tool === "mcp_write") &&
        typeof filePath === "string"
      ) {
        const rel = relative(rootDir, resolve(rootDir, filePath));

        // [1] Naming convention: must precede protected-zone check to avoid
        //     triggering onProtectedZoneWrite for a write that will be reverted.
        if (isYaml(filePath) && isArtifactPath(rel)) {
          if (checkNaming(rootDir, rel) === null) {
            revertFile(rootDir, rel);
            throw new Error(
              diag("ERROR", `UUID-only filename forbidden: ${rel}`, {
                context: `${input.tool}(file_path=${rel})`,
                reason: `Artifact files must follow the <slug>-<8chars-uuid>.yml naming convention. UUID-only names are forbidden. Change reverted.`,
                fix: `Rename to <slug-of-title>-<first-8-chars-of-uuid>.yml (e.g. my-artifact-49354fcb.yml). See RFC 2492822c for slugification rules.`,
              }),
            );
          }
        }

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
              revertFile(rootDir, file);
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
                revertFile(rootDir, f);
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
