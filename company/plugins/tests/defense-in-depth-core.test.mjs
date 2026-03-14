import { describe, it } from "node:test";
import { strict as assert } from "node:assert";
import { isYaml, isSafeBashPath, isVolatile, isInProtectedZone, loadProtectedZones } from "../defense-in-depth-core.mjs";
import { writeFileSync, mkdirSync } from "node:fs";
import { mkdtempSync } from "node:fs";
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
