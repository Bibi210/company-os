import { describe, it } from "node:test";
import { strict as assert } from "node:assert";
import "../defense-in-depth.mjs";
const _test = globalThis.__defenseInDepthTest;
const { parsePersonaYaml, parseYamlList } = _test;

const samplePersona = `api_version: v1
kind: Persona
metadata:
  id: architect
identity: >
  You are the architect persona.
  You design systems.
review_behavior: >
  Review for architecture patterns
  and structural integrity.
capabilities:
  produces:
    - rfc
    - design-doc
  consumes:
    - task
    - issue
`;

describe("parsePersonaYaml", () => {
  it("extracts id and identity", () => {
    const result = parsePersonaYaml(samplePersona);
    assert.equal(result.id, "architect");
    assert.ok(result.identity.includes("architect persona"));
  });

  it("extracts produces and consumes arrays", () => {
    const result = parsePersonaYaml(samplePersona);
    assert.deepEqual(result.produces, ["rfc", "design-doc"]);
    assert.deepEqual(result.consumes, ["task", "issue"]);
  });

  it("extracts review_behavior", () => {
    const result = parsePersonaYaml(samplePersona);
    assert.ok(result.review.includes("architecture patterns"));
  });
});

describe("parseYamlList", () => {
  it("parses a yaml list under a given key", () => {
    const content = `  items:\n    - alpha\n    - beta\n    - gamma\n`;
    const result = parseYamlList(content, "items");
    assert.deepEqual(result, ["alpha", "beta", "gamma"]);
  });
});
