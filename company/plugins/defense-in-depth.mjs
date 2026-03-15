// Defense in Depth — OpenCode plugin for Company OS
//
// Thin wrapper with event-driven hot-reload:
// - Core logic is reimported when a plugin file is modified (via permit)
// - personas.yml is regenerated when a persona file is modified (via permit)
// - protected-zones.json changes trigger core reload (zones are re-read on init)
// No mtime polling — reload only fires when a write with permit hits a protected zone.

import { resolve, dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { readdirSync, readFileSync, writeFileSync, statSync } from "node:fs";

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
  if (cachedCreateHandlers) return cachedCreateHandlers;
  const mtime = statSync(CORE_PATH).mtimeMs;
  const mod = await import(pathToFileURL(CORE_PATH).href + `?t=${mtime}`);
  cachedCreateHandlers = mod.createHandlers;
  return cachedCreateHandlers;
}

function invalidateCore() {
  cachedCreateHandlers = null;
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
    lines.push(`  role: ${identity}`);
    if (produces.length) lines.push(`  produces: ${produces.join(", ")}`);
    if (consumes.length) lines.push(`  consumes: ${consumes.join(", ")}`);
    if (review) lines.push(`  review: ${review}`);
    lines.push("");
  }

  writeFileSync(outPath, lines.join("\n") + "\n", "utf-8");
}

// ---------------------------------------------------------------------------
// Callback: a write with permit just succeeded on a protected zone file
// ---------------------------------------------------------------------------
function onProtectedZoneWrite(rootDir, relPath) {
  // Check each prefix from the zones config to decide what to reload
  for (const prefix of zones.prefixes) {
    if (!relPath.startsWith(prefix)) continue;

    // The zones file itself changed → reload zones + invalidate core
    if (relPath === ZONES_FILE) {
      loadZones(rootDir);
      invalidateCore();
      return;
    }

    // Plugin dir → invalidate core so next call reimports
    if (CORE_PATH.endsWith(relPath.split("/").pop())) {
      invalidateCore();
    }

    // Personas dir → regenerate personas.yml
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
  invalidateCore,
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
