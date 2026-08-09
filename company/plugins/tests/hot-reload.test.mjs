// Data-reload tests (RFC 25b6678c) — verifies the event-driven DATA reload.
//
// CODE hot-reload was REMOVED (RFC 25b6678c): plugin code changes now require an
// opencode restart, so there is nothing to test for core invalidation. Only
// DATA reloads (zones, personas.yml) fire without restart, via readFileSync.
//
// Tests that:
// 1. Zones are loaded on init and reloaded when protected-zones.json changes
// 2. personas.yml is regenerated when a persona file changes
// 3. protected zones config changes trigger a zones re-read (data only)
// 4. defense-in-depth-core.mjs protected zones reload works

import { describe, it, beforeEach } from "node:test";
import { strict as assert } from "node:assert";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import "../defense-in-depth.mjs";
const _test = globalThis.__defenseInDepthTest;
const {
  loadZones,
  generatePersonasYml,
  onProtectedZoneWrite,
  getZones,
} = _test;

import {
  loadProtectedZones,
  isInProtectedZone,
} from "../defense-in-depth-core.mjs";

// --- Helpers ---

function makeTmpRoot() {
  const root = mkdtempSync(join(tmpdir(), "hotreload-test-"));
  return root;
}

function writeZonesFile(root, zones) {
  const dir = join(root, "company/config");
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, "protected-zones.json"), JSON.stringify(zones), "utf-8");
}

function writePersonaFile(root, personasDir, filename, content) {
  const dir = join(root, personasDir);
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, filename), content, "utf-8");
}

const SAMPLE_PERSONA = `api_version: companyos/v1
kind: persona
metadata:
  id: tester
  title: Tester
  author: ceo
  created_at: "2026-03-15"
  display_name: Tester

identity: >
  Test persona for hot-reload tests.

rules:
  must:
    - Test things
  never:
    - Break things

artifacts:
  produces:
    - lesson-learned
  consumes:
    - task-request

review_behavior: >
  Reviews everything.
`;

// ==========================================================================
// Zone loading + reload
// ==========================================================================

describe("loadZones", () => {
  it("loads zones from protected-zones.json", () => {
    const root = makeTmpRoot();
    writeZonesFile(root, {
      prefixes: ["src/", "lib/"],
      files: ["Makefile"],
      personas_dir: "personas",
      personas_out: "out/personas.yml",
      db_path: "data/db.sqlite",
    });

    loadZones(root);
    const zones = getZones();

    assert.deepStrictEqual(zones.prefixes, ["src/", "lib/"]);
    assert.deepStrictEqual(zones.files, ["Makefile"]);
    assert.strictEqual(zones.personas_dir, "personas");
    assert.strictEqual(zones.personas_out, "out/personas.yml");
    assert.strictEqual(zones.db_path, "data/db.sqlite");
  });

  it("keeps previous zones if file is missing", () => {
    const root1 = makeTmpRoot();
    writeZonesFile(root1, { prefixes: ["keep/"], files: [] });
    loadZones(root1);

    // Now load from a dir with no zones file
    const root2 = makeTmpRoot();
    loadZones(root2);

    const zones = getZones();
    assert.deepStrictEqual(zones.prefixes, ["keep/"]);
  });

  it("reloads zones when onProtectedZoneWrite hits the zones file", () => {
    const root = makeTmpRoot();
    writeZonesFile(root, {
      prefixes: ["company/config/", "old/"],
      files: [],
      personas_dir: "company/personas",
      personas_out: "company/config/personas.yml",
    });
    loadZones(root);

    assert.ok(getZones().prefixes.includes("old/"));

    // Simulate zones file change
    writeZonesFile(root, {
      prefixes: ["company/config/", "new/"],
      files: [],
      personas_dir: "company/personas",
      personas_out: "company/config/personas.yml",
    });
    onProtectedZoneWrite(root, "company/config/protected-zones.json");

    assert.ok(getZones().prefixes.includes("new/"));
    assert.ok(!getZones().prefixes.includes("old/"));
  });
});

// ==========================================================================
// No code hot-reload (RFC 25b6678c)
// ==========================================================================

describe("no code hot-reload", () => {
  it("a plugin code change does NOT invalidate the cached core (restart required)", () => {
    const root = makeTmpRoot();
    writeZonesFile(root, {
      prefixes: ["company/plugins/"],
      files: [],
      personas_dir: "company/personas",
      personas_out: "company/config/personas.yml",
    });
    loadZones(root);

    // A write to the plugin dir must NOT trigger any core invalidation: the
    // callback only handles data (zones, personas). It must not throw and must
    // be a no-op for code files.
    assert.doesNotThrow(() =>
      onProtectedZoneWrite(root, "company/plugins/defense-in-depth-core.mjs"),
    );
    // zones untouched by a plugin-code write
    assert.deepStrictEqual(getZones().prefixes, ["company/plugins/"]);
  });
});

// ==========================================================================
// Personas generation on hot-reload
// ==========================================================================

describe("personas hot-reload", () => {
  it("generatePersonasYml creates output from persona files", () => {
    const root = makeTmpRoot();
    const personasDir = "company/personas";
    const personasOut = "company/config/personas.yml";

    writeZonesFile(root, {
      prefixes: ["company/personas/"],
      files: [],
      personas_dir: personasDir,
      personas_out: personasOut,
    });
    loadZones(root);

    writePersonaFile(root, personasDir, "tester.yml", SAMPLE_PERSONA);
    mkdirSync(join(root, "company/config"), { recursive: true });

    generatePersonasYml(root);

    const outPath = join(root, personasOut);
    assert.ok(existsSync(outPath), "personas.yml should be generated");

    const content = readFileSync(outPath, "utf-8");
    assert.ok(content.includes("tester:"), "should contain persona id");
    assert.ok(content.includes("role:"), "should contain role field");
    assert.ok(content.includes("produces:"), "should contain produces field");
    assert.ok(content.includes("consumes:"), "should contain consumes field");
  });

  it("onProtectedZoneWrite regenerates personas when persona file changes", () => {
    const root = makeTmpRoot();
    const personasDir = "company/personas";
    const personasOut = "company/config/personas.yml";

    writeZonesFile(root, {
      prefixes: ["company/personas/"],
      files: [],
      personas_dir: personasDir,
      personas_out: personasOut,
    });
    loadZones(root);

    writePersonaFile(root, personasDir, "alpha.yml", SAMPLE_PERSONA);
    mkdirSync(join(root, "company/config"), { recursive: true });

    // Initial generation
    generatePersonasYml(root);
    const before = readFileSync(join(root, personasOut), "utf-8");
    assert.ok(before.includes("tester:"));

    // Add a second persona
    const secondPersona = SAMPLE_PERSONA.replace("id: tester", "id: second")
      .replace("display_name: Tester", "display_name: Second")
      .replace("Test persona for hot-reload tests.", "Second persona.");
    writePersonaFile(root, personasDir, "second.yml", secondPersona);

    // Trigger hot-reload via callback
    onProtectedZoneWrite(root, "company/personas/second.yml");

    const after = readFileSync(join(root, personasOut), "utf-8");
    assert.ok(after.includes("second:"), "should contain new persona after reload");
    assert.ok(after.includes("tester:"), "should still contain original persona");
  });

  it("generatePersonasYml does nothing if personas dir is empty", () => {
    const root = makeTmpRoot();
    writeZonesFile(root, {
      prefixes: [],
      files: [],
      personas_dir: "company/personas",
      personas_out: "company/config/personas.yml",
    });
    loadZones(root);

    mkdirSync(join(root, "company/personas"), { recursive: true });
    mkdirSync(join(root, "company/config"), { recursive: true });

    generatePersonasYml(root);
    assert.ok(!existsSync(join(root, "company/config/personas.yml")), "should not create file for empty dir");
  });

  // RFC bd416171: the generator emits every scalar via JSON.stringify so any
  // YAML-active content survives without corrupting the document. Regression
  // guard for the latent bug that broke personas.yml under RFC 098f35db: a
  // review_behavior containing " : " was parsed as a nested mapping. This test
  // exercises the three axes on the emitted values (colon-space, quotes,
  // backslash, unicode) with a lossless round-trip. Each emitted value is a
  // JSON string, which is a valid YAML 1.2 double-quoted scalar; we round-trip
  // via JSON.parse (dependency-free) to assert value === source.
  it("generatePersonasYml quotes YAML-active scalars losslessly (RFC bd416171)", () => {
    const root = makeTmpRoot();
    const personasDir = "company/personas";
    const personasOut = "company/config/personas.yml";

    writeZonesFile(root, {
      prefixes: ["company/personas/"],
      files: [],
      personas_dir: personasDir,
      personas_out: personasOut,
    });
    loadZones(root);

    // NEGATIVE + EDGE payloads, all on ONE line each (the generator collapses
    // block scalars to a single line before emitting):
    //  - identity: colon-space, backslash, unicode, hash
    //  - review_behavior: colon-space AND both single and double quotes
    const IDENTITY = `Concoit les systemes : autorite technique. Chemin C:\\\\x #1 — flux "resilient" — café ☕ 日本語`;
    const REVIEW = `Verifie tout : implementation-plans (fidelite), "code" et 'tests' — sans redesign`;

    const persona = `api_version: companyos/v1
kind: persona
metadata:
  id: quoter
  title: Quoter
  author: ceo
  created_at: "2026-07-27"
  display_name: Quoter

identity: >
  ${IDENTITY}

rules:
  must:
    - Do things
  never:
    - Break things

artifacts:
  produces:
    - design-doc
  consumes:
    - task-request

review_behavior: >
  ${REVIEW}
`;

    writePersonaFile(root, personasDir, "quoter.yml", persona);
    mkdirSync(join(root, "company/config"), { recursive: true });

    generatePersonasYml(root);

    const outPath = join(root, personasOut);
    assert.ok(existsSync(outPath), "personas.yml should be generated");
    const content = readFileSync(outPath, "utf-8");

    // Helper: extract the raw text after "  <field>: " for the given field,
    // assert it is a well-formed double-quoted (JSON) scalar, and return the
    // parsed value for the round-trip check.
    const parseField = (field) => {
      const line = content
        .split("\n")
        .find((l) => l.startsWith(`  ${field}: `));
      assert.ok(line, `personas.yml must contain a ${field} line`);
      const raw = line.slice(`  ${field}: `.length);
      // NOMINAL/EDGE: the emitted value MUST be a double-quoted scalar so
      // colon-space is never interpreted as a mapping.
      assert.ok(
        raw.startsWith('"') && raw.endsWith('"'),
        `${field} value must be a double-quoted scalar, got: ${raw}`,
      );
      // A JSON string is a valid YAML 1.2 double-quoted scalar; JSON.parse is a
      // faithful decoder for it. Round-trip must be lossless.
      return JSON.parse(raw);
    };

    assert.strictEqual(parseField("role"), IDENTITY, "role round-trip lossless");
    assert.strictEqual(parseField("review"), REVIEW, "review round-trip lossless");

    // Regression assertion: the whole document has no bare colon-space inside a
    // value that could be re-read as a nested mapping. Every value line after a
    // field key is a quoted scalar.
    for (const field of ["role", "produces", "consumes", "review"]) {
      const line = content
        .split("\n")
        .find((l) => l.startsWith(`  ${field}: `));
      if (!line) continue;
      const raw = line.slice(`  ${field}: `.length);
      assert.ok(
        raw.startsWith('"') && raw.endsWith('"'),
        `${field} must be quoted (no bare scalar): ${raw}`,
      );
    }
  });
});

// ==========================================================================
// Core defense-in-depth-core.mjs: protected zones reload
// ==========================================================================

describe("defense-in-depth-core zones reload", () => {
  it("loadProtectedZones loads zones and isInProtectedZone works", () => {
    const root = makeTmpRoot();
    writeZonesFile(root, {
      prefixes: ["src/", "lib/"],
      files: ["Cargo.toml"],
    });

    loadProtectedZones(root);

    assert.ok(isInProtectedZone("src/main.rs"));
    assert.ok(isInProtectedZone("lib/utils.rs"));
    assert.ok(isInProtectedZone("Cargo.toml"));
    assert.ok(!isInProtectedZone("README.md"));
  });

  it("reloading zones updates protection rules", () => {
    const root = makeTmpRoot();

    // Initial zones
    writeZonesFile(root, { prefixes: ["old/"], files: [] });
    loadProtectedZones(root);
    assert.ok(isInProtectedZone("old/file.rs"));
    assert.ok(!isInProtectedZone("new/file.rs"));

    // Update zones
    writeZonesFile(root, { prefixes: ["new/"], files: [] });
    loadProtectedZones(root);
    assert.ok(!isInProtectedZone("old/file.rs"));
    assert.ok(isInProtectedZone("new/file.rs"));
  });

  it("loadProtectedZones with missing file keeps previous state", () => {
    const root = makeTmpRoot();
    writeZonesFile(root, { prefixes: ["keep/"], files: [] });
    loadProtectedZones(root);
    assert.ok(isInProtectedZone("keep/file.rs"));

    // Load from dir with no zones file
    const emptyRoot = makeTmpRoot();
    loadProtectedZones(emptyRoot);

    // Previous state should be kept
    assert.ok(isInProtectedZone("keep/file.rs"));
  });
});

// ==========================================================================
// End-to-end: full reload cycle
// ==========================================================================

describe("end-to-end reload cycle", () => {
  it("zones change → zones reloaded → personas regenerated (data only, no core reload)", () => {
    const root = makeTmpRoot();
    const personasDir = "company/personas";
    const personasOut = "company/config/personas.yml";

    // Setup initial state
    writeZonesFile(root, {
      prefixes: ["company/config/", "company/personas/", "company/plugins/"],
      files: [],
      personas_dir: personasDir,
      personas_out: personasOut,
    });
    loadZones(root);

    writePersonaFile(root, personasDir, "agent.yml", SAMPLE_PERSONA);
    mkdirSync(join(root, "company/config"), { recursive: true });
    generatePersonasYml(root);

    // Verify initial state
    assert.ok(existsSync(join(root, personasOut)));
    const initial = readFileSync(join(root, personasOut), "utf-8");
    assert.ok(initial.includes("tester:"));

    // Step 1: zones file changes → reload zones (data only, no core reload)
    writeZonesFile(root, {
      prefixes: ["company/config/", "company/personas/", "company/plugins/", "new-zone/"],
      files: [],
      personas_dir: personasDir,
      personas_out: personasOut,
    });
    onProtectedZoneWrite(root, "company/config/protected-zones.json");

    assert.ok(getZones().prefixes.includes("new-zone/"), "zones should be reloaded");

    // Step 2: persona changes → personas.yml regenerated
    const newPersona = SAMPLE_PERSONA.replace("id: tester", "id: new-agent")
      .replace("display_name: Tester", "display_name: New-Agent")
      .replace("Test persona for hot-reload tests.", "Brand new agent.");
    writePersonaFile(root, personasDir, "new-agent.yml", newPersona);
    onProtectedZoneWrite(root, "company/personas/new-agent.yml");

    const updated = readFileSync(join(root, personasOut), "utf-8");
    assert.ok(updated.includes("new-agent:"), "new persona should appear after reload");
    assert.ok(updated.includes("tester:"), "old persona should still be there");
  });
});
