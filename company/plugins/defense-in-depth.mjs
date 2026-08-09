// Defense in Depth — OpenCode plugin for Company OS
//
// Thin wrapper with event-driven DATA reload (RFC 25b6678c):
// - personas.yml is regenerated when a persona file is modified (via permit)
// - protected-zones.json changes trigger a zones re-read (readFileSync)
// CODE hot-reload was REMOVED (RFC 25b6678c): the import "?t=mtime" cache-bust
// was not reliable under the Bun runtime opencode embeds and masked an incident
// for three days by running stale code while appearing to reload. The core is
// now imported ONCE per process. ANY plugin code change (core or wrapper)
// requires an opencode RESTART (explicit promotion pattern, RFC 18011bfc).
// Only DATA (zones, personas.yml) reloads without restart, via readFileSync.

import { resolve, dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { readdirSync, readFileSync, writeFileSync } from "node:fs";

const __dirname = dirname(fileURLToPath(import.meta.url));
const CORE_PATH = resolve(__dirname, "defense-in-depth-core.mjs");

// Bootstrap constant — the file that defines everything else
const ZONES_FILE = "company/config/protected-zones.json";

// Shared state that persists across reloads
const sessions = new Map();

// Cached core module
let cachedCreateHandlers = null;

// Cached zones (read once at startup + on zones file change)
let zones = { prefixes: [], files: [], personas_dir: "", personas_out: "" };

function loadZones(rootDir) {
  try {
    zones = JSON.parse(readFileSync(resolve(rootDir, ZONES_FILE), "utf-8"));
  } catch { /* keep previous */ }
}

async function loadCore() {
  // RFC 25b6678c: import the core ONCE per process, no cache-busting query
  // string. A plugin code change requires an opencode restart to take effect.
  if (cachedCreateHandlers) return cachedCreateHandlers;
  const mod = await import(pathToFileURL(CORE_PATH).href);
  cachedCreateHandlers = mod.createHandlers;
  return cachedCreateHandlers;
}

// ---------------------------------------------------------------------------
// Auto-generate personas.yml from persona YAML files
// ---------------------------------------------------------------------------
function parsePersonaYaml(content) {
  const idMatch = content.match(/^\s+id:\s*(.+)$/m);
  const nameMatch = content.match(/^\s+display_name:\s*(.+)$/m);
  const identityMatch = content.match(/^identity:\s*>\s*\n([\s\S]*?)(?=\n\w|\n$)/m);
  const reviewMatch = content.match(/^review_behavior:\s*>\s*\n([\s\S]*?)(?=\n\w|\n$)/m);

  const id = idMatch ? idMatch[1].trim() : null;
  const name = nameMatch ? nameMatch[1].trim() : null;
  const identity = identityMatch ? identityMatch[1].replace(/\n\s*/g, " ").trim() : null;
  const review = reviewMatch ? reviewMatch[1].replace(/\n\s*/g, " ").trim() : null;

  const produces = parseYamlList(content, "produces");
  const consumes = parseYamlList(content, "consumes");

  return { id, name, identity, produces, consumes, review };
}

function parseYamlList(content, key) {
  const match = content.match(new RegExp(`^\\s+${key}:\\s*\\n((?:\\s+-\\s+.+\\n?)*)`, "m"));
  if (!match) return [];
  return match[1].match(/- (.+)/g)?.map((l) => l.replace(/^- /, "").trim()) ?? [];
}

function generatePersonasYml(rootDir) {
  const dir = resolve(rootDir, zones.personas_dir);
  const outPath = resolve(rootDir, zones.personas_out);
  const entries = [];

  try {
    for (const f of readdirSync(dir).sort()) {
      if (f.startsWith(".") || (!f.endsWith(".yml") && !f.endsWith(".yaml"))) continue;
      const content = readFileSync(resolve(dir, f), "utf-8");
      const { id, name, identity, produces, consumes, review } = parsePersonaYaml(content);
      if (id && identity) {
        entries.push({ id, name, identity, produces, consumes, review });
      }
    }
  } catch { return; }

  if (entries.length === 0) return;

  const lines = [
    "# Auto-generated — do not edit manually",
    `# Source: ${zones.personas_dir}/*.yml`,
    "",
  ];

  for (const { name, identity, produces, consumes, review } of entries) {
    lines.push(`${name.toLowerCase()}:`);
    lines.push(`  role: ${JSON.stringify(identity)}`);
    if (produces.length) lines.push(`  produces: ${JSON.stringify(produces.join(", "))}`);
    if (consumes.length) lines.push(`  consumes: ${JSON.stringify(consumes.join(", "))}`);
    if (review) lines.push(`  review: ${JSON.stringify(review)}`);
    lines.push("");
  }

  writeFileSync(outPath, lines.join("\n") + "\n", "utf-8");
}

// ---------------------------------------------------------------------------
// Callback: a write with permit just succeeded on a protected zone file
// ---------------------------------------------------------------------------
function onProtectedZoneWrite(rootDir, relPath) {
  // RFC 25b6678c: only DATA reloads here. Plugin CODE changes are NOT
  // hot-reloaded any more — they require an opencode restart.
  for (const prefix of zones.prefixes) {
    if (!relPath.startsWith(prefix)) continue;

    // The zones file itself changed → reload zones (data, readFileSync).
    if (relPath === ZONES_FILE) {
      loadZones(rootDir);
      return;
    }

    // Personas dir → regenerate personas.yml (data).
    if (relPath.startsWith(zones.personas_dir)) {
      generatePersonasYml(rootDir);
    }
  }
}

// ---------------------------------------------------------------------------

// Test bridge — NOT exported. Exporting a plain object crashes the OpenCode plugin loader
// (it tries to call all exports as plugin functions). Access via globalThis in tests.
const _test = {
  parsePersonaYaml,
  parseYamlList,
  loadZones,
  generatePersonasYml,
  onProtectedZoneWrite,
  getCachedCreateHandlers: () => cachedCreateHandlers,
  getZones: () => zones,
};

// Make _test accessible for tests without exporting it as a named export
if (typeof globalThis !== "undefined") {
  globalThis.__defenseInDepthTest = _test;
}

export const DefenseInDepth = async ({ directory }) => {
  const rootDir = directory;

  // Load zones and generate personas.yml on startup
  loadZones(rootDir);
  generatePersonasYml(rootDir);

  return {
    "tool.execute.before": async (input, output) => {
      const createHandlers = await loadCore();
      const handlers = createHandlers(rootDir, sessions, (rel) => onProtectedZoneWrite(rootDir, rel));
      return handlers["tool.execute.before"](input, output);
    },

    "tool.execute.after": async (input) => {
      const createHandlers = await loadCore();
      const handlers = createHandlers(rootDir, sessions, (rel) => onProtectedZoneWrite(rootDir, rel));
      return handlers["tool.execute.after"](input);
    },
  };
};
