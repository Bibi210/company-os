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
  buildBaseline,
  revertPermitTampering,
  snapshotPermits,
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
function gitCommitDb(rootDir) {
  execSync("git init -q", { cwd: rootDir });
  execSync('git config user.email t@t.t && git config user.name t', { cwd: rootDir });
  execSync(`git add -f "${DB_REL}"`, { cwd: rootDir });
  execSync('git commit -q -m seal', { cwd: rootDir });
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
