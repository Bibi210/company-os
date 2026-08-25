import { describe, it } from "node:test";
import { strict as assert } from "node:assert";
import {
  isYaml,
  isSafeBashPath,
  isVolatile,
  isInProtectedZone,
  loadProtectedZones,
  parsePermitIds,
  permitIdsFromHeadDb,
  permitIdsFromHeadSeal,
  buildBaseline,
  revertPermitTampering,
  snapshotPermits,
  hasActivePermit,
  resolveAgent,
  AUTH_TOOL_RE,
  UNAUTH_SESSION_MSG,
  createHandlers,
} from "../defense-in-depth-core.mjs";
import { writeFileSync, mkdirSync, rmSync, existsSync, readdirSync } from "node:fs";
import { mkdtempSync } from "node:fs";
import { execSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";

describe("isYaml", () => {
  it("returns true for .yml", () => {
    assert.equal(isYaml("foo.yml"), true);
  });

  it("returns true for .yaml", () => {
    assert.equal(isYaml("foo.yaml"), true);
  });

  it("returns false for .json", () => {
    assert.equal(isYaml("foo.json"), false);
  });
});

describe("isSafeBashPath", () => {
  it("returns true for target/debug/", () => {
    assert.equal(isSafeBashPath("target/debug/foo"), true);
  });

  it("returns true for /tmp/", () => {
    assert.equal(isSafeBashPath("/tmp/test"), true);
  });

  it("returns false for company/config/", () => {
    assert.equal(isSafeBashPath("company/config/foo"), false);
  });
});

describe("isVolatile", () => {
  it("returns true for .db files", () => {
    assert.equal(isVolatile("data.db"), true);
  });

  it("returns true for .lock files", () => {
    assert.equal(isVolatile("data.lock"), true);
  });

  it("returns false for .yml files", () => {
    assert.equal(isVolatile("data.yml"), false);
  });
});

describe("isInProtectedZone", () => {
  const tmpDir = mkdtempSync(join(tmpdir(), "did-test-"));
  const zonesDir = join(tmpDir, "company", "config");
  mkdirSync(zonesDir, { recursive: true });
  writeFileSync(
    join(zonesDir, "protected-zones.json"),
    JSON.stringify({ prefixes: ["src/"], files: ["Makefile"] }),
  );
  loadProtectedZones(tmpDir);

  it("returns true for file under protected prefix", () => {
    assert.equal(isInProtectedZone("src/main.rs"), true);
  });

  it("returns true for exact protected file", () => {
    assert.equal(isInProtectedZone("Makefile"), true);
  });

  it("returns false for unprotected file", () => {
    assert.equal(isInProtectedZone("README.md"), false);
  });
});

// ---------------------------------------------------------------------------
// Infrastructure pour les tests du revert sélectif (RFC bde023e2).
// Chaque test crée son propre rootDir tmp avec la zones config et une DB
// sqlite réelle ; les tests Source B initialisent en plus un repo git tmp.
// Cleanup en fin de test (rmSync recursive).
// ---------------------------------------------------------------------------

const DB_REL = "company/data/orchestrator.db";

// sqlite3 inline (même binaire que le hook). Lance la commande dans le rootDir.
function sql(rootDir, dbAbs, statement) {
  execSync(`sqlite3 "${dbAbs}" "${statement.replace(/"/g, '\\"')}"`, {
    cwd: rootDir,
    encoding: "utf-8",
  });
}

// Crée un rootDir tmp avec protected-zones.json (db_path = DB_REL) et,
// si permits != null, une DB sqlite avec la table write_permits remplie.
// permits === null => aucune DB créée (simule "pas de DB").
function setupRoot(permits) {
  const rootDir = mkdtempSync(join(tmpdir(), "did-permit-"));
  const cfgDir = join(rootDir, "company", "config");
  mkdirSync(cfgDir, { recursive: true });
  writeFileSync(
    join(cfgDir, "protected-zones.json"),
    JSON.stringify({ prefixes: ["company/plugins/"], files: [], db_path: DB_REL }),
  );
  loadProtectedZones(rootDir);

  const dbAbs = join(rootDir, DB_REL);
  if (permits !== null) {
    mkdirSync(join(rootDir, "company", "data"), { recursive: true });
    sql(rootDir, dbAbs, "CREATE TABLE write_permits (id TEXT PRIMARY KEY, status TEXT)");
    for (const [id, status] of permits) {
      sql(rootDir, dbAbs, `INSERT INTO write_permits (id, status) VALUES ('${id}', '${status}')`);
    }
  }
  return { rootDir, dbAbs };
}

// Initialise un repo git dans rootDir et commit la DB courante (la scelle en HEAD).
// Hermétique contre la config utilisateur (RFC 324e8a33, symétrie avec le helper
// Rust hermetic_git_cmd) : GIT_CONFIG_GLOBAL/SYSTEM=/dev/null neutralisent un
// commit.gpgsign=true global (qui déclencherait gpg/pinentry sans TTY sous
// make test-js), et commit.gpgsign=false en config LOCALE double la ceinture.
function gitCommitDb(rootDir) {
  const env = {
    ...process.env,
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_CONFIG_SYSTEM: "/dev/null",
  };
  execSync("git init -q", { cwd: rootDir, env });
  execSync('git config user.email t@t.t && git config user.name t', { cwd: rootDir, env });
  execSync("git config commit.gpgsign false", { cwd: rootDir, env });
  execSync(`git add -f "${DB_REL}"`, { cwd: rootDir, env });
  execSync('git commit -q -m seal', { cwd: rootDir, env });
}

// Lit l'ensemble des ids actuellement dans la DB (pour assertions).
function currentIds(rootDir, dbAbs) {
  const out = execSync(`sqlite3 "${dbAbs}" "SELECT id FROM write_permits"`, {
    cwd: rootDir,
    encoding: "utf-8",
  }).trim();
  return new Set(out.split("\n").filter(Boolean));
}

function cleanup(rootDir) {
  rmSync(rootDir, { recursive: true, force: true });
}

const UUID_A = "aaaaaaaa-0000-4000-8000-000000000001";
const UUID_B = "bbbbbbbb-0000-4000-8000-000000000002";
const UUID_C = "cccccccc-0000-4000-8000-000000000003";
const UUID_INTRUDER = "dddddddd-0000-4000-8000-00000000dead";

// ---------------------------------------------------------------------------
// parsePermitIds (Source A) — pur
// ---------------------------------------------------------------------------
describe("parsePermitIds (Source A)", () => {
  it("n2: parse '3|id1:active,id2:consumed,id3:active' -> {id1,id2,id3} (strip count|)", () => {
    const blob = `3|${UUID_A}:active,${UUID_B}:consumed,${UUID_C}:active`;
    const set = parsePermitIds(blob);
    assert.deepEqual([...set].sort(), [UUID_A, UUID_B, UUID_C].sort());
    // Le préfixe "3|" ne doit PAS polluer le premier id.
    assert.ok(!set.has(`3|${UUID_A}`));
  });

  it("returns null when before === null (Source A absente)", () => {
    assert.equal(parsePermitIds(null), null);
  });

  it("returns empty Set for '0|' (snapshot avec 0 permit)", () => {
    const set = parsePermitIds("0|");
    assert.equal(set.size, 0);
    assert.ok(set instanceof Set);
  });
});

// ---------------------------------------------------------------------------
// buildBaseline — union des deux sources, distinction null vs Set vide
// ---------------------------------------------------------------------------
describe("buildBaseline (union Source A + Source B)", () => {
  it("n3: union de Source A {a,b} et Source B {b,c} -> {a,b,c}", () => {
    const { rootDir } = setupRoot([[UUID_B, "active"], [UUID_C, "active"]]);
    try {
      gitCommitDb(rootDir); // Source B = {b,c}
      const before = `2|${UUID_A}:active,${UUID_B}:active`; // Source A = {a,b}
      const baseline = buildBaseline(rootDir, before);
      assert.deepEqual([...baseline].sort(), [UUID_A, UUID_B, UUID_C].sort());
    } finally {
      cleanup(rootDir);
    }
  });

  it("returns null SSI Source A null ET Source B inexploitable", () => {
    // Pas de DB, pas de git repo -> Source B null ; before null -> Source A null.
    const { rootDir } = setupRoot(null);
    try {
      assert.equal(buildBaseline(rootDir, null), null);
    } finally {
      cleanup(rootDir);
    }
  });

  it("returns empty Set (déterminable-vide) quand A='0|' et B vide (HEAD 0 permit)", () => {
    const { rootDir } = setupRoot([]); // DB existe, table vide
    try {
      gitCommitDb(rootDir); // Source B = Set vide
      const baseline = buildBaseline(rootDir, "0|"); // Source A = Set vide
      assert.ok(baseline instanceof Set);
      assert.equal(baseline.size, 0); // déterminable mais vide, PAS null
    } finally {
      cleanup(rootDir);
    }
  });

  it("Source A seule quand Source B inexploitable (pas de git)", () => {
    const { rootDir } = setupRoot([[UUID_A, "active"]]); // DB sans repo git
    try {
      const baseline = buildBaseline(rootDir, `1|${UUID_A}:active`);
      assert.deepEqual([...baseline], [UUID_A]); // B null, baseline = A
    } finally {
      cleanup(rootDir);
    }
  });
});

// ---------------------------------------------------------------------------
// permitIdsFromHeadSeal (Source B JSON, RFC cde13417 A1.7) + fallback legacy
// ---------------------------------------------------------------------------
const SEAL_REL = "company/data/permits-seal.json";

// Root avec seal_path déclaré. Si sealPermits != null, écrit permits-seal.json
// (format canonique) ; si commit, l'ajoute en HEAD. Si dbPermits != null, crée
// AUSSI la DB legacy (pour tester le fallback). git=true init un repo.
function setupRootSeal({ sealPermits = null, dbPermits = null, commit = false }) {
  const rootDir = mkdtempSync(join(tmpdir(), "did-seal-"));
  const cfgDir = join(rootDir, "company", "config");
  mkdirSync(cfgDir, { recursive: true });
  writeFileSync(
    join(cfgDir, "protected-zones.json"),
    JSON.stringify({
      prefixes: ["company/plugins/"],
      files: [],
      db_path: DB_REL,
      seal_path: SEAL_REL,
    }),
  );
  loadProtectedZones(rootDir);
  mkdirSync(join(rootDir, "company", "data"), { recursive: true });

  if (dbPermits !== null) {
    const dbAbs = join(rootDir, DB_REL);
    sql(rootDir, dbAbs, "CREATE TABLE write_permits (id TEXT PRIMARY KEY, status TEXT)");
    for (const [id, status] of dbPermits) {
      sql(rootDir, dbAbs, `INSERT INTO write_permits (id, status) VALUES ('${id}', '${status}')`);
    }
  }
  if (sealPermits !== null) {
    const seal = {
      version: 1,
      permits: sealPermits.map(([id, status]) => ({ id, status })),
    };
    writeFileSync(join(rootDir, SEAL_REL), JSON.stringify(seal, null, 2) + "\n");
  }
  const env = { ...process.env, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_SYSTEM: "/dev/null" };
  if (commit) {
    execSync("git init -q", { cwd: rootDir, env });
    execSync("git config user.email t@t.t && git config user.name t", { cwd: rootDir, env });
    execSync("git config commit.gpgsign false", { cwd: rootDir, env });
    if (sealPermits !== null) execSync(`git add "${SEAL_REL}"`, { cwd: rootDir, env });
    if (dbPermits !== null) execSync(`git add -f "${DB_REL}"`, { cwd: rootDir, env });
    execSync("git commit -q -m seal", { cwd: rootDir, env });
  }
  return { rootDir };
}

describe("permitIdsFromHeadSeal (Source B JSON, RFC cde13417)", () => {
  it("NOMINAL: seal JSON valide en HEAD -> Set des ids", () => {
    const { rootDir } = setupRootSeal({
      sealPermits: [[UUID_A, "active"], [UUID_B, "consumed"]],
      commit: true,
    });
    try {
      const set = permitIdsFromHeadSeal(rootDir);
      assert.deepEqual([...set].sort(), [UUID_A, UUID_B].sort());
    } finally {
      cleanup(rootDir);
    }
  });

  it("EDGE: seal JSON vide en HEAD -> Set VIDE exploitable (pas null)", () => {
    const { rootDir } = setupRootSeal({ sealPermits: [], commit: true });
    try {
      const set = permitIdsFromHeadSeal(rootDir);
      assert.ok(set instanceof Set);
      assert.equal(set.size, 0);
    } finally {
      cleanup(rootDir);
    }
  });

  it("NÉGATIF: seal absent de HEAD -> fallback legacy DB HEAD", () => {
    // Pas de seal committé, mais une DB legacy committée -> fallback l'utilise.
    const { rootDir } = setupRootSeal({
      dbPermits: [[UUID_C, "active"]],
      commit: true,
    });
    try {
      const set = permitIdsFromHeadSeal(rootDir);
      assert.deepEqual([...set], [UUID_C], "fallback legacy doit remonter la DB HEAD");
    } finally {
      cleanup(rootDir);
    }
  });

  it("EDGE: seal JSON corrompu -> fallback legacy; sans DB -> null", () => {
    const { rootDir } = setupRootSeal({ commit: false });
    try {
      // Écrit un seal corrompu et le commite en HEAD.
      writeFileSync(join(rootDir, SEAL_REL), "{ not json");
      const env = { ...process.env, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_SYSTEM: "/dev/null" };
      execSync("git init -q", { cwd: rootDir, env });
      execSync("git config user.email t@t.t && git config user.name t", { cwd: rootDir, env });
      execSync("git config commit.gpgsign false", { cwd: rootDir, env });
      execSync(`git add "${SEAL_REL}"`, { cwd: rootDir, env });
      execSync("git commit -q -m corrupt", { cwd: rootDir, env });
      // Ni DB ni seal exploitable -> null.
      assert.equal(permitIdsFromHeadSeal(rootDir), null);
    } finally {
      cleanup(rootDir);
    }
  });
});

describe("buildBaseline avec Source B JSON (RFC cde13417)", () => {
  it("NOMINAL: union Source A snapshot + Source B seal JSON", () => {
    const { rootDir } = setupRootSeal({
      sealPermits: [[UUID_B, "active"], [UUID_C, "active"]],
      commit: true,
    });
    try {
      const before = `2|${UUID_A}:active,${UUID_B}:active`; // Source A = {a,b}
      const baseline = buildBaseline(rootDir, before);
      assert.deepEqual([...baseline].sort(), [UUID_A, UUID_B, UUID_C].sort());
    } finally {
      cleanup(rootDir);
    }
  });

  it("NÉGATIF: A null ET seal null ET legacy null -> baseline null (nucléaire borné)", () => {
    const { rootDir } = setupRootSeal({ commit: false }); // pas de git, pas de seal, pas de DB
    try {
      assert.equal(buildBaseline(rootDir, null), null);
    } finally {
      cleanup(rootDir);
    }
  });

  it("EDGE: A '0|' + seal vide -> baseline Set vide déterminable (DELETE sur tout)", () => {
    const { rootDir } = setupRootSeal({ sealPermits: [], commit: true });
    try {
      const baseline = buildBaseline(rootDir, "0|");
      assert.ok(baseline instanceof Set);
      assert.equal(baseline.size, 0);
    } finally {
      cleanup(rootDir);
    }
  });
});

// ---------------------------------------------------------------------------
// revertPermitTampering — coeur du correctif (triaxial)
// ---------------------------------------------------------------------------
describe("revertPermitTampering — NOMINAL", () => {
  it("n1: intrus apparu pendant le bash, légitimes préservés (inverse de ed7776fb)", () => {
    // before = 2 légitimes (a,b). On en capture le snapshot, puis on injecte
    // un intrus directement en DB pour simuler l'altération hors-canal.
    const { rootDir, dbAbs } = setupRoot([[UUID_A, "active"], [UUID_B, "active"]]);
    try {
      gitCommitDb(rootDir); // a,b scellés en HEAD
      const before = snapshotPermits(rootDir);
      // Intrus injecté pendant le bash.
      sql(rootDir, dbAbs, `INSERT INTO write_permits (id, status) VALUES ('${UUID_INTRUDER}', 'active')`);
      const reverted = revertPermitTampering(rootDir, before);
      assert.equal(reverted, true);
      const ids = currentIds(rootDir, dbAbs);
      assert.ok(ids.has(UUID_A) && ids.has(UUID_B), "légitimes préservés");
      assert.ok(!ids.has(UUID_INTRUDER), "intrus supprimé");
    } finally {
      cleanup(rootDir);
    }
  });
});

describe("revertPermitTampering — NÉGATIF", () => {
  it("g1: intrus ni dans before ni dans HEAD -> supprimé, légitimes préservés (invariant 3)", () => {
    const { rootDir, dbAbs } = setupRoot([[UUID_A, "active"]]);
    try {
      gitCommitDb(rootDir); // a scellé en HEAD
      const before = snapshotPermits(rootDir); // before = {a}
      sql(rootDir, dbAbs, `INSERT INTO write_permits (id, status) VALUES ('${UUID_INTRUDER}', 'active')`);
      revertPermitTampering(rootDir, before);
      const ids = currentIds(rootDir, dbAbs);
      assert.deepEqual([...ids], [UUID_A]);
    } finally {
      cleanup(rootDir);
    }
  });

  it("g2: baseline connue mais VIDE (before '0|', HEAD 0) -> DELETE total, intrus supprimé (ferme L230)", () => {
    const { rootDir, dbAbs } = setupRoot([]); // table vide
    try {
      gitCommitDb(rootDir); // HEAD = 0 permit
      const before = snapshotPermits(rootDir); // "0|" (count 0)
      // Intrus injecté sur une DB initialement vide.
      sql(rootDir, dbAbs, `INSERT INTO write_permits (id, status) VALUES ('${UUID_INTRUDER}', 'active')`);
      const reverted = revertPermitTampering(rootDir, before);
      assert.equal(reverted, true);
      const ids = currentIds(rootDir, dbAbs);
      assert.equal(ids.size, 0, "FAILLE L230 fermée : l'intrus est supprimé même baseline vide");
    } finally {
      cleanup(rootDir);
    }
  });

  it("g3: Source A null ET Source B inexploitable -> WIPE résiduel borné (cas résiduel)", () => {
    // before === null (Source A absente, ex: DB inexistante avant le bash),
    // pas de repo git (Source B inexploitable). after non-null (DB créée +
    // intrus pendant le bash) -> divergence -> baseline null -> wipe.
    const { rootDir, dbAbs } = setupRoot([[UUID_INTRUDER, "active"]]); // DB "apparue" avec un intrus
    try {
      // pas de gitCommitDb -> Source B null ; before = null -> Source A null.
      const reverted = revertPermitTampering(rootDir, null);
      assert.equal(reverted, true);
      const ids = currentIds(rootDir, dbAbs);
      assert.equal(ids.size, 0, "wipe résiduel : tout supprimé faute de baseline");
    } finally {
      cleanup(rootDir);
    }
  });

  it("g4: Source B en échec mais Source A présente -> sélectif sur Source A seule", () => {
    const { rootDir, dbAbs } = setupRoot([[UUID_A, "active"]]); // pas de git -> Source B null
    try {
      const before = snapshotPermits(rootDir); // Source A = {a}
      sql(rootDir, dbAbs, `INSERT INTO write_permits (id, status) VALUES ('${UUID_INTRUDER}', 'active')`);
      revertPermitTampering(rootDir, before);
      const ids = currentIds(rootDir, dbAbs);
      assert.ok(ids.has(UUID_A) && !ids.has(UUID_INTRUDER), "sélectif sur A, jamais moins sûr");
    } finally {
      cleanup(rootDir);
    }
  });
});

describe("revertPermitTampering — EDGE", () => {
  it("e1: table vide avant et après, aucune altération -> return false, aucun DELETE", () => {
    const { rootDir } = setupRoot([]);
    try {
      const before = snapshotPermits(rootDir);
      assert.equal(revertPermitTampering(rootDir, before), false);
    } finally {
      cleanup(rootDir);
    }
  });

  it("e2: un seul permit légitime inchangé -> before===after -> false", () => {
    const { rootDir, dbAbs } = setupRoot([[UUID_A, "active"]]);
    try {
      const before = snapshotPermits(rootDir);
      assert.equal(revertPermitTampering(rootDir, before), false);
      assert.deepEqual([...currentIds(rootDir, dbAbs)], [UUID_A]); // intact
    } finally {
      cleanup(rootDir);
    }
  });

  it("e3: tous les permits illégitimes (aucun dans baseline) -> DELETE sélectif les supprime tous", () => {
    // before = {a} (légitime), mais pendant le bash a est supprimé et deux
    // intrus apparaissent ; HEAD ne contient que a. baseline = {a}, donc les
    // deux intrus (seuls présents) sont hors baseline -> tous supprimés.
    const { rootDir, dbAbs } = setupRoot([[UUID_A, "active"]]);
    try {
      gitCommitDb(rootDir);
      const before = snapshotPermits(rootDir); // {a}
      sql(rootDir, dbAbs, `DELETE FROM write_permits WHERE id = '${UUID_A}'`);
      sql(rootDir, dbAbs, `INSERT INTO write_permits (id, status) VALUES ('${UUID_B}', 'active')`);
      sql(rootDir, dbAbs, `INSERT INTO write_permits (id, status) VALUES ('${UUID_INTRUDER}', 'active')`);
      revertPermitTampering(rootDir, before);
      assert.equal(currentIds(rootDir, dbAbs).size, 0, "tous hors baseline supprimés");
    } finally {
      cleanup(rootDir);
    }
  });

  it("e4: permit scellé en HEAD entre deux bash -> union le préserve (neutralise 4f000127/05224964)", () => {
    // Scénario 359f9162 : un grant atomique a scellé un nouveau permit (b) en
    // HEAD APRÈS le snapshot before (qui ne contient que a). before = {a},
    // HEAD = {a,b}. b est légitime (scellé) et ne doit PAS être supprimé.
    const { rootDir, dbAbs } = setupRoot([[UUID_A, "active"], [UUID_B, "active"]]);
    try {
      gitCommitDb(rootDir); // HEAD = {a,b}
      // before ne "voit" que a (snapshot antérieur au grant de b).
      const before = `1|${UUID_A}:active`;
      // Un intrus apparaît aussi pendant le bash.
      sql(rootDir, dbAbs, `INSERT INTO write_permits (id, status) VALUES ('${UUID_INTRUDER}', 'active')`);
      revertPermitTampering(rootDir, before);
      const ids = currentIds(rootDir, dbAbs);
      assert.ok(ids.has(UUID_A), "a préservé (Source A)");
      assert.ok(ids.has(UUID_B), "b préservé (Source B / scellé HEAD)");
      assert.ok(!ids.has(UUID_INTRUDER), "intrus supprimé");
    } finally {
      cleanup(rootDir);
    }
  });

  it("e5: permitIdsFromHeadDb nettoie son fichier /tmp (succès)", () => {
    const before = new Set(readdirSync("/tmp").filter((f) => f.startsWith("companyos-headdb-")));
    const { rootDir } = setupRoot([[UUID_A, "active"]]);
    try {
      gitCommitDb(rootDir);
      const set = permitIdsFromHeadDb(rootDir);
      assert.deepEqual([...set], [UUID_A]);
      const after = new Set(readdirSync("/tmp").filter((f) => f.startsWith("companyos-headdb-")));
      // Aucun nouveau fichier temporaire ne subsiste.
      for (const f of after) {
        assert.ok(before.has(f), `fichier temporaire fuité: ${f}`);
      }
    } finally {
      cleanup(rootDir);
    }
  });

  it("e5b: permitIdsFromHeadDb retourne null et ne fuit pas quand HEAD n'a pas de DB", () => {
    const before = new Set(readdirSync("/tmp").filter((f) => f.startsWith("companyos-headdb-")));
    const { rootDir } = setupRoot([[UUID_A, "active"]]); // DB existe mais pas de git
    try {
      assert.equal(permitIdsFromHeadDb(rootDir), null);
      const after = new Set(readdirSync("/tmp").filter((f) => f.startsWith("companyos-headdb-")));
      assert.equal(after.size, before.size, "pas de fuite /tmp même en échec");
    } finally {
      cleanup(rootDir);
    }
  });
});

// =====================================================================
// Mechanism 13 (RFC a4ee8b6a): hasActivePermit croise granted_to == agent
// =====================================================================

// Setup a root with a full write_permits table (granted_to + target_paths).
function setupPermitRoot(rows) {
  const rootDir = mkdtempSync(join(tmpdir(), "did-granted-"));
  const cfgDir = join(rootDir, "company", "config");
  mkdirSync(cfgDir, { recursive: true });
  writeFileSync(
    join(cfgDir, "protected-zones.json"),
    JSON.stringify({ prefixes: ["crates/", "company/"], files: [], db_path: DB_REL }),
  );
  loadProtectedZones(rootDir);
  const dbAbs = join(rootDir, DB_REL);
  mkdirSync(join(rootDir, "company", "data"), { recursive: true });
  sql(
    rootDir,
    dbAbs,
    "CREATE TABLE write_permits (id TEXT PRIMARY KEY, status TEXT, granted_to TEXT, target_paths TEXT)",
  );
  let i = 0;
  for (const [status, grantedTo, paths] of rows) {
    const pj = JSON.stringify(paths).replace(/'/g, "''");
    sql(
      rootDir,
      dbAbs,
      `INSERT INTO write_permits (id, status, granted_to, target_paths) VALUES ('p${i++}', '${status}', '${grantedTo}', '${pj}')`,
    );
  }
  return { rootDir };
}

describe("hasActivePermit (mechanism 13: granted_to == agent)", () => {
  it("nominal: matches when granted_to === agent AND path covered", () => {
    const { rootDir } = setupPermitRoot([
      ["active", "implementer", ["crates/x.rs"]],
    ]);
    try {
      assert.equal(
        hasActivePermit(rootDir, "crates/x.rs", "implementer"),
        true,
        "beneficiary writing a covered path is allowed",
      );
    } finally {
      rmSync(rootDir, { recursive: true, force: true });
    }
  });

  it("negative: permit of agent A does NOT cover agent B", () => {
    const { rootDir } = setupPermitRoot([
      ["active", "architect", ["crates/x.rs"]],
    ]);
    try {
      assert.equal(
        hasActivePermit(rootDir, "crates/x.rs", "implementer"),
        false,
        "an agent cannot write under another agent's permit",
      );
    } finally {
      rmSync(rootDir, { recursive: true, force: true });
    }
  });

  it("edge: agent undefined → no match (fail-safe strict)", () => {
    const { rootDir } = setupPermitRoot([
      ["active", "implementer", ["crates/x.rs"]],
    ]);
    try {
      assert.equal(hasActivePermit(rootDir, "crates/x.rs", undefined), false);
      assert.equal(hasActivePermit(rootDir, "crates/x.rs", ""), false);
    } finally {
      rmSync(rootDir, { recursive: true, force: true });
    }
  });

  it("edge: consumed permit does not match even for the beneficiary", () => {
    const { rootDir } = setupPermitRoot([
      ["consumed", "implementer", ["crates/x.rs"]],
    ]);
    try {
      assert.equal(
        hasActivePermit(rootDir, "crates/x.rs", "implementer"),
        false,
        "only active permits count",
      );
    } finally {
      rmSync(rootDir, { recursive: true, force: true });
    }
  });

  it("edge: glob suffix pattern still matches for the beneficiary", () => {
    const { rootDir } = setupPermitRoot([
      ["active", "implementer", ["crates/sub/*"]],
    ]);
    try {
      assert.equal(
        hasActivePermit(rootDir, "crates/sub/deep/y.rs", "implementer"),
        true,
      );
    } finally {
      rmSync(rootDir, { recursive: true, force: true });
    }
  });
});

// =====================================================================
// Agent identity (RFC 25b6678c): resolveAgent + AUTH_TOOL_RE
// Triaxial: nominal (mapping resolves), négatif (fail-closed / no mapping),
// edge (last-wins, input.agent priority, restart, 3 tool-name forms).
// =====================================================================

// Simulate the sessions Map with a per-session persona (as the hook records it
// in tool.execute.after on a successful authenticate).
function sessionsWith(entries) {
  const m = new Map();
  for (const [sid, persona] of entries) m.set(sid, { persona });
  return m;
}

describe("resolveAgent (identity source)", () => {
  it("nominal: mapping resolves the persona for the session", () => {
    // authenticate(persona=implementer) observed in `after` → sessions has it.
    const sessions = sessionsWith([["s1", "implementer"]]);
    assert.equal(
      resolveAgent({ sessionID: "s1" }, sessions),
      "implementer",
      "write in the same session resolves to the mapped persona",
    );
  });

  it("négatif: session with no authenticate → null (fail-closed)", () => {
    const sessions = sessionsWith([]); // nothing recorded
    assert.equal(resolveAgent({ sessionID: "s1" }, sessions), null);
  });

  it("négatif: a different session's mapping does not leak", () => {
    const sessions = sessionsWith([["s1", "implementer"]]);
    assert.equal(resolveAgent({ sessionID: "s2" }, sessions), null);
  });

  it("négatif: empty persona in the mapping resolves to null", () => {
    // An empty args.persona is never recorded, but guard against a falsy value.
    const sessions = new Map([["s1", { persona: "" }]]);
    assert.equal(resolveAgent({ sessionID: "s1" }, sessions), null);
  });

  it("edge: last-wins — authenticate pm then implementer → implementer", () => {
    // The recording is last-wins; here we simulate the final state of the Map.
    const sessions = sessionsWith([["s1", "pm"]]);
    sessions.set("s1", { persona: "implementer" }); // second authenticate overwrote
    assert.equal(resolveAgent({ sessionID: "s1" }, sessions), "implementer");
  });

  it("edge: input.agent (future runtime) takes priority over a divergent mapping", () => {
    const sessions = sessionsWith([["s1", "pm"]]);
    assert.equal(
      resolveAgent({ sessionID: "s1", agent: "architect" }, sessions),
      "architect",
      "runtime-provided agent wins over the mapping",
    );
  });

  it("edge: empty input.agent falls back to the mapping", () => {
    const sessions = sessionsWith([["s1", "implementer"]]);
    assert.equal(resolveAgent({ sessionID: "", agent: "" }, sessions), null);
    assert.equal(
      resolveAgent({ sessionID: "s1", agent: "" }, sessions),
      "implementer",
    );
  });

  it("edge: restart — empty sessions Map → null (mapping lost)", () => {
    const sessions = new Map(); // fresh process after restart
    assert.equal(resolveAgent({ sessionID: "s1" }, sessions), null);
  });

  it("edge: no sessionID and no agent → null", () => {
    assert.equal(resolveAgent({}, new Map()), null);
    assert.equal(resolveAgent(undefined, undefined), null);
  });
});

describe("AUTH_TOOL_RE (authenticate tool-name detection)", () => {
  it("edge: matches the three forms of the authenticate tool name", () => {
    assert.ok(AUTH_TOOL_RE.test("orchestrator_authenticate"), "real form seen by the hook");
    assert.ok(AUTH_TOOL_RE.test("mcp_orchestrator_authenticate"), "old code form");
    assert.ok(AUTH_TOOL_RE.test("mcp_Orchestrator_authenticate"), "MCP server-name casing");
  });

  it("négatif: does not match unrelated tools", () => {
    assert.ok(!AUTH_TOOL_RE.test("bash"));
    assert.ok(!AUTH_TOOL_RE.test("write"));
    assert.ok(!AUTH_TOOL_RE.test("orchestrator_search"));
    assert.ok(!AUTH_TOOL_RE.test("authenticate_orchestrator"));
    assert.ok(!AUTH_TOOL_RE.test(""));
  });
});

// =====================================================================
// End-to-end identity: hasActivePermit fed by resolveAgent (RFC 25b6678c)
// mirrors the three call-sites, which now pass resolveAgent(input) instead of
// input.agent brut.
// =====================================================================
describe("hasActivePermit fed by resolveAgent (call-site behaviour)", () => {
  it("nominal: mapped persona + covering permit → write allowed", () => {
    const { rootDir } = setupPermitRoot([
      ["active", "implementer", ["company/plugins/"]],
    ]);
    try {
      const sessions = sessionsWith([["s1", "implementer"]]);
      const agent = resolveAgent({ sessionID: "s1" }, sessions);
      assert.equal(
        hasActivePermit(rootDir, "company/plugins/x.mjs", agent),
        true,
      );
    } finally {
      rmSync(rootDir, { recursive: true, force: true });
    }
  });

  it("négatif: unauthenticated session → resolveAgent null → no permit matches", () => {
    const { rootDir } = setupPermitRoot([
      ["active", "implementer", ["company/plugins/"]],
    ]);
    try {
      const agent = resolveAgent({ sessionID: "s1" }, new Map());
      assert.equal(agent, null);
      assert.equal(
        hasActivePermit(rootDir, "company/plugins/x.mjs", agent),
        false,
        "fail-closed: unknown identity never matches a permit",
      );
    } finally {
      rmSync(rootDir, { recursive: true, force: true });
    }
  });

  it("négatif: authenticate pm then write under implementer's permit → mismatch blocks", () => {
    const { rootDir } = setupPermitRoot([
      ["active", "implementer", ["company/plugins/"]],
    ]);
    try {
      const sessions = sessionsWith([["s1", "pm"]]);
      const agent = resolveAgent({ sessionID: "s1" }, sessions);
      assert.equal(agent, "pm");
      assert.equal(
        hasActivePermit(rootDir, "company/plugins/x.mjs", agent),
        false,
        "a permit granted to implementer does not cover pm",
      );
    } finally {
      rmSync(rootDir, { recursive: true, force: true });
    }
  });

  it("edge: the fail-closed message is the actionable authenticate hint", () => {
    // The call-sites emit UNAUTH_SESSION_MSG when resolveAgent returns null.
    assert.match(UNAUTH_SESSION_MSG, /authenticate\(persona=/);
    assert.match(UNAUTH_SESSION_MSG, /zone protégée/);
  });
});

// =====================================================================
// Guard seal_path (RFC cde13417 A1.7) — bash + write/edit d'agent.
// =====================================================================
describe("guard seal_path (RFC cde13417)", () => {
  // Root minimal avec seal_path déclaré (pas de git nécessaire).
  function guardRoot() {
    const rootDir = mkdtempSync(join(tmpdir(), "did-guard-seal-"));
    const cfgDir = join(rootDir, "company", "config");
    mkdirSync(cfgDir, { recursive: true });
    writeFileSync(
      join(cfgDir, "protected-zones.json"),
      JSON.stringify({
        prefixes: ["company/plugins/"],
        files: [],
        db_path: DB_REL,
        seal_path: SEAL_REL,
      }),
    );
    mkdirSync(join(rootDir, "company", "data"), { recursive: true });
    return rootDir;
  }

  it("NÉGATIF: bash référençant le seal_path est refusé", async () => {
    const rootDir = guardRoot();
    try {
      const handlers = createHandlers(rootDir, new Map(), () => {});
      await assert.rejects(
        () =>
          handlers["tool.execute.before"]({
            tool: "bash",
            args: { command: `git rm --cached ${SEAL_REL}` },
            sessionID: "s1",
          }),
        /Permits seal access blocked/,
      );
    } finally {
      cleanup(rootDir);
    }
  });

  it("NOMINAL: write/edit d'agent sur le seal_path est refusé et reverté", async () => {
    const rootDir = guardRoot();
    try {
      // Écrit le fichier pour que revertFile ait quelque chose à annuler.
      writeFileSync(join(rootDir, SEAL_REL), "{}");
      const handlers = createHandlers(rootDir, new Map(), () => {});
      await assert.rejects(
        () =>
          handlers["tool.execute.after"]({
            tool: "write",
            args: { file_path: SEAL_REL },
            sessionID: "s1",
          }),
        /permits seal is machine-owned/,
      );
    } finally {
      cleanup(rootDir);
    }
  });

  it("EDGE: write sur un AUTRE .json de company/data/ n'est PAS bloqué par ce guard", async () => {
    const rootDir = guardRoot();
    try {
      const otherRel = "company/data/other.json";
      writeFileSync(join(rootDir, otherRel), "{}");
      const handlers = createHandlers(rootDir, new Map(), () => {});
      // Ne doit PAS rejeter avec le message du guard seal (le fichier n'est ni
      // le seal ni en zone protégée company/data n'étant pas un prefix).
      await handlers["tool.execute.after"]({
        tool: "write",
        args: { file_path: otherRel },
        sessionID: "s1",
      });
      assert.ok(existsSync(join(rootDir, otherRel)), "other.json ne doit pas être reverté");
    } finally {
      cleanup(rootDir);
    }
  });
});
