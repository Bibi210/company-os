use std::path::{Path, PathBuf};

use companyos_config::constants;
use companyos_config::{ArtifactId, ArtifactKind};

use crate::error::ValidationError;
use crate::schema_registry::SchemaRegistry;

/// Result of validating a single artifact.
#[derive(Debug, Clone)]
pub struct ValidationReport {
    pub kind: ArtifactKind,
    pub id: ArtifactId,
    pub is_valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Mechanism 20a (RFC 0197fbe5): the exact generated file exempted from
/// validation. `company/config/personas.yml` is auto-generated from the
/// persona YAML files (see the defense-in-depth wrapper) and carries NO
/// `api_version`, so `validate_yaml_str` would reject it with
/// `InvalidApiVersion`. It is a generated artifact, not a CompanyOS artifact.
const PERSONAS_YML_EXEMPTION: &str = "company/config/personas.yml";

/// Main validation entry point.
pub struct ArtifactValidator {
    registry: SchemaRegistry,
    /// Repo root used to resolve relative paths for placement validation
    /// (mechanism 9, RFC a4ee8b6a). `None` = placement check skipped
    /// (schema-only), preserving the legacy behaviour of every non-configured
    /// call-site.
    root: Option<PathBuf>,
}

impl ArtifactValidator {
    pub fn new(registry: SchemaRegistry) -> Self {
        Self {
            registry,
            root: None,
        }
    }

    /// Additive builder: configure the repo root so `validate_file` /
    /// `validate_dir` also enforce kind↔path placement. Only the
    /// `companyos-yaml-validator` binary sets this; every other caller keeps
    /// schema-only validation.
    pub fn with_root(mut self, root: PathBuf) -> Self {
        self.root = Some(root);
        self
    }

    /// Validate a YAML string against its schema (kind is extracted from the YAML).
    ///
    /// NOTE: a bare string carries no path, so placement CANNOT be checked
    /// here (tool `validate_yaml`). Placement is enforced only through
    /// `validate_file` / `validate_dir`, which know the path.
    pub fn validate_yaml_str(&self, yaml: &str) -> Result<ValidationReport, ValidationError> {
        let yaml_value: serde_yaml::Value = serde_yaml::from_str(yaml)?;
        let json_value: serde_json::Value =
            serde_json::to_value(&yaml_value).map_err(ValidationError::Json)?;

        self.validate_json_value(&json_value)
    }

    /// Validate a YAML file. When a root is configured and `path` resolves
    /// under it, kind↔path placement is enforced as a blocking error.
    ///
    /// Mechanism 20a (RFC 0197fbe5): `company/config/personas.yml` is EXEMPT.
    /// The check is placed EN TÊTE (before read_to_string / validate_yaml_str)
    /// because the generated file has no `api_version` and would otherwise be
    /// rejected with `InvalidApiVersion`. ROOT-GATED: the exemption resolves
    /// the relative path via the same canonicalize + strip_prefix machinery as
    /// the placement check, so it only applies when a root is configured. A
    /// schema-only validator (no `with_root`, e.g. some integration tests) does
    /// NOT apply the exemption and still returns `Err(InvalidApiVersion)` on
    /// personas.yml — a documented, harmless behaviour (the shipped-artifacts
    /// test already accepts both Ok-valid and Err-InvalidApiVersion).
    pub fn validate_file(&self, path: &Path) -> Result<ValidationReport, ValidationError> {
        if self.is_personas_yml_exemption(path) {
            return Ok(ValidationReport {
                kind: ArtifactKind::Persona,
                id: ArtifactId("company/config/personas.yml".to_string()),
                is_valid: true,
                errors: Vec::new(),
                warnings: vec!["generated file, skipped (not an artifact)".to_string()],
            });
        }
        let content = std::fs::read_to_string(path)?;
        let mut report = self.validate_yaml_str(&content)?;
        self.check_placement(path, &mut report);
        Ok(report)
    }

    /// Mechanism 20a (RFC 0197fbe5): does `path` resolve EXACTLY onto
    /// `company/config/personas.yml` under the configured root? Root-gated: a
    /// validator without `with_root` returns false (no exemption). Uses the
    /// same canonicalize + strip_prefix resolution as `check_placement`.
    fn is_personas_yml_exemption(&self, path: &Path) -> bool {
        let Some(root) = &self.root else {
            return false;
        };
        let (Ok(root_c), Ok(path_c)) = (root.canonicalize(), path.canonicalize()) else {
            return false;
        };
        let Ok(rel) = path_c.strip_prefix(&root_c) else {
            return false;
        };
        rel.to_str() == Some(PERSONAS_YML_EXEMPTION)
    }

    /// Batch validate all YAML files under a directory (recursive).
    pub fn validate_dir(
        &self,
        dir: &Path,
    ) -> Vec<(PathBuf, Result<ValidationReport, ValidationError>)> {
        let mut results = Vec::new();
        if let Ok(entries) = walkdir(dir) {
            for path in entries {
                let result = self.validate_file(&path);
                results.push((path, result));
            }
        }
        results
    }

    /// Mechanism 9 (RFC a4ee8b6a): enforce kind↔path placement as a blocking
    /// error when a root is configured AND `path` resolves under it. Skipped
    /// (schema-only) when no root is set or when the path lies outside the
    /// root (fail-open, e.g. test fixtures in tempdirs). `report` is only
    /// touched when a placement error is found, so a report already invalid
    /// on schema grounds is unaffected.
    fn check_placement(&self, path: &Path, report: &mut ValidationReport) {
        let Some(root) = &self.root else {
            return;
        };
        // Resolve both sides through canonicalize so symlinks and relative
        // segments compare correctly. If either side cannot be canonicalized
        // (path missing, permissions), skip: fail-open.
        let (Ok(root_c), Ok(path_c)) = (root.canonicalize(), path.canonicalize()) else {
            return;
        };
        let Ok(rel) = path_c.strip_prefix(&root_c) else {
            // Path is outside the root → skip (fail-open).
            return;
        };
        if let Some(msg) = crate::placement::check_placement(report.kind, rel) {
            report.errors.push(msg);
            report.is_valid = false;
        }
    }

    fn validate_json_value(
        &self,
        json_value: &serde_json::Value,
    ) -> Result<ValidationReport, ValidationError> {
        let api_version = json_value
            .get(constants::FIELD_API_VERSION)
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if api_version != constants::API_VERSION {
            return Err(ValidationError::InvalidApiVersion {
                got: api_version.to_string(),
            });
        }

        let kind_str = json_value
            .get(constants::FIELD_KIND)
            .and_then(|v| v.as_str())
            .ok_or_else(|| ValidationError::MissingField {
                field: constants::FIELD_KIND.to_string(),
            })?;

        let kind: ArtifactKind = kind_str.parse().map_err(|_| ValidationError::UnknownKind {
            raw: kind_str.to_string(),
        })?;

        let id = ArtifactId(
            json_value
                .get(constants::FIELD_METADATA)
                .and_then(|m| m.get(constants::FIELD_ID))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
        );

        let schema_value = self
            .registry
            .get(kind)
            .ok_or(ValidationError::SchemaNotFound { kind })?;

        let opts = jsonschema::options().with_draft(jsonschema::Draft::Draft202012);

        let opts = if let Some(base) = self.registry.base_schema() {
            let resource = jsonschema::Resource::from_contents(base.clone()).map_err(|e| {
                ValidationError::SchemaCompile {
                    kind,
                    reason: format!("failed to load _base schema: {e}"),
                }
            })?;
            opts.with_resource(constants::BASE_SCHEMA_URI, resource)
        } else {
            opts
        };

        let validator = opts
            .build(schema_value)
            .map_err(|e| ValidationError::SchemaCompile {
                kind,
                reason: e.to_string(),
            })?;

        let mut errors = Vec::new();
        for error in validator.iter_errors(json_value) {
            errors.push(format!("{} at {}", error, error.instance_path));
        }

        Ok(ValidationReport {
            kind,
            id,
            is_valid: errors.is_empty(),
            errors,
            warnings: Vec::new(),
        })
    }
}

/// Recursively walk a directory and collect all .yml/.yaml files.
fn walkdir(dir: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut files = Vec::new();
    if !dir.is_dir() {
        return Ok(files);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            files.extend(walkdir(&path)?);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && (ext == constants::EXT_YML || ext == constants::EXT_YAML)
        {
            files.push(path);
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn setup_validator() -> ArtifactValidator {
        let schemas_dir = workspace_root().join("company/schemas");
        let registry = crate::SchemaRegistry::load(&schemas_dir).unwrap();
        ArtifactValidator::new(registry)
    }

    #[test]
    fn test_validate_valid_yaml() {
        let validator = setup_validator();
        let yaml = r#"
api_version: "companyos/v1"
kind: "task-request"
metadata:
  id: "a0000007-0000-4000-8000-000000000007"
  title: "Test Task"
  author: "pm"
  created_at: "2025-01-01"
  project: "company-os"
spec:
  acceptance_criteria:
    - "criterion one"
    - "criterion two"
  priority: high
  status: backlog
"#;
        let report = validator.validate_yaml_str(yaml).unwrap();
        assert!(
            report.is_valid,
            "valid task-request YAML should pass validation, errors: {:?}",
            report.errors
        );
    }

    #[test]
    fn test_validate_missing_api_version() {
        let validator = setup_validator();
        let yaml = r#"
kind: "task-request"
metadata:
  id: "test-001"
  title: "Test"
  author: "pm"
  created_at: "2025-01-01"
spec:
  acceptance_criteria:
    - "done"
"#;
        let result = validator.validate_yaml_str(yaml);
        assert!(result.is_err(), "missing api_version should return Err");
    }

    #[test]
    fn test_validate_wrong_api_version() {
        let validator = setup_validator();
        let yaml = r#"
api_version: "v2"
kind: "task-request"
metadata:
  id: "test-001"
  title: "Test"
  author: "pm"
  created_at: "2025-01-01"
spec:
  acceptance_criteria:
    - "done"
"#;
        let result = validator.validate_yaml_str(yaml);
        assert!(result.is_err(), "wrong api_version should return Err");
    }

    #[test]
    fn test_validate_missing_kind() {
        let validator = setup_validator();
        let yaml = r#"
api_version: "companyos/v1"
metadata:
  id: "test-001"
  title: "Test"
  author: "pm"
  created_at: "2025-01-01"
"#;
        let result = validator.validate_yaml_str(yaml);
        assert!(result.is_err(), "missing kind should return Err");
    }

    #[test]
    fn test_validate_unknown_kind() {
        let validator = setup_validator();
        let yaml = r#"
api_version: "companyos/v1"
kind: "foobar"
metadata:
  id: "test-001"
  title: "Test"
  author: "pm"
  created_at: "2025-01-01"
"#;
        let result = validator.validate_yaml_str(yaml);
        assert!(result.is_err(), "unknown kind 'foobar' should return Err");
    }

    #[test]
    fn test_validate_schema_errors_reported() {
        let validator = setup_validator();
        // Valid envelope but missing required 'spec' field for task-request
        let yaml = r#"
api_version: "companyos/v1"
kind: "task-request"
metadata:
  id: "test-001"
  title: "Test"
  author: "pm"
  created_at: "2025-01-01"
"#;
        let report = validator.validate_yaml_str(yaml).unwrap();
        assert!(!report.is_valid, "missing spec should make validation fail");
        assert!(
            !report.errors.is_empty(),
            "errors should be non-empty when spec is missing"
        );
    }

    // --- Mechanism 9: placement enforcement via with_root ---

    const RFC_YAML: &str = r#"
api_version: "companyos/v1"
kind: "rfc"
metadata:
  id: "a0000009-0000-4000-8000-000000000009"
  title: "Placement test RFC"
  author: "architect"
  created_at: "2026-07-03"
  status: draft
spec:
  motivation: "m"
  proposal: "p"
  impact: "i"
  affected_files:
    - "company/config/x.yml"
"#;

    fn setup_validator_with_root(root: &std::path::Path) -> ArtifactValidator {
        let schemas_dir = workspace_root().join("company/schemas");
        let registry = crate::SchemaRegistry::load(&schemas_dir).unwrap();
        ArtifactValidator::new(registry).with_root(root.to_path_buf())
    }

    #[test]
    fn test_placement_correct_location_accepted() {
        let tmp = std::env::temp_dir().join(format!("placement-ok-{}", std::process::id()));
        let dir = tmp.join("company/rfcs");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("placement-a0000009.yml");
        std::fs::write(&file, RFC_YAML).unwrap();

        let validator = setup_validator_with_root(&tmp);
        let report = validator.validate_file(&file).unwrap();
        assert!(
            report.is_valid,
            "rfc under company/rfcs/ must pass placement: {:?}",
            report.errors
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_placement_wrong_location_rejected() {
        let tmp = std::env::temp_dir().join(format!("placement-ko-{}", std::process::id()));
        // rfc placed under company/lessons/ → wrong.
        let dir = tmp.join("company/lessons");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("misplaced-a0000009.yml");
        std::fs::write(&file, RFC_YAML).unwrap();

        let validator = setup_validator_with_root(&tmp);
        let report = validator.validate_file(&file).unwrap();
        assert!(
            !report.is_valid,
            "rfc under company/lessons/ must fail placement"
        );
        assert!(
            report.errors.iter().any(|e| e.contains("placement error")),
            "expected a placement error, got {:?}",
            report.errors
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_placement_skipped_without_root() {
        // Same misplaced file, but a validator WITHOUT with_root must ignore
        // placement entirely (schema-only) — preserves legacy behaviour.
        let tmp = std::env::temp_dir().join(format!("placement-noroot-{}", std::process::id()));
        let dir = tmp.join("company/lessons");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("misplaced-a0000009.yml");
        std::fs::write(&file, RFC_YAML).unwrap();

        let validator = setup_validator(); // no root
        let report = validator.validate_file(&file).unwrap();
        assert!(
            report.is_valid,
            "without root, placement must be skipped: {:?}",
            report.errors
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_placement_path_outside_root_skipped() {
        // File lives OUTSIDE the configured root → fail-open skip.
        let root = std::env::temp_dir().join(format!("placement-root-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let other = std::env::temp_dir().join(format!("placement-other-{}", std::process::id()));
        let dir = other.join("company/lessons");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("misplaced-a0000009.yml");
        std::fs::write(&file, RFC_YAML).unwrap();

        let validator = setup_validator_with_root(&root);
        let report = validator.validate_file(&file).unwrap();
        assert!(
            report.is_valid,
            "path outside root must skip placement: {:?}",
            report.errors
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    // --- Mechanism 20a: personas.yml exemption ---

    // Content WITHOUT api_version (like the real generated personas.yml).
    const PERSONAS_YML_CONTENT: &str =
        "# Auto-generated — do not edit manually\narchitect:\n  role: x\n";

    // NOMINAL: with root, company/config/personas.yml is exempted (valid report
    // with the note), WITHOUT parsing the content.
    #[test]
    fn test_personas_yml_exemption_with_root() {
        let tmp = std::env::temp_dir().join(format!("personas-exempt-{}", std::process::id()));
        let dir = tmp.join("company/config");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("personas.yml");
        std::fs::write(&file, PERSONAS_YML_CONTENT).unwrap();

        let validator = setup_validator_with_root(&tmp);
        let report = validator.validate_file(&file).unwrap();
        assert!(report.is_valid, "personas.yml must be exempted (valid)");
        assert!(
            report.warnings.iter().any(|w| w.contains("generated file")),
            "exemption note present: {:?}",
            report.warnings
        );
        // validate_dir inherits the exemption (it calls validate_file).
        let results = validator.validate_dir(&dir);
        let personas = results
            .iter()
            .find(|(p, _)| p.file_name().and_then(|n| n.to_str()) == Some("personas.yml"))
            .expect("personas.yml walked");
        assert!(
            personas.1.as_ref().unwrap().is_valid,
            "dir path exempts too"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    // NÉGATIF: a personas.yml at a DIFFERENT path is NOT exempted (fails on
    // missing api_version like any non-artifact).
    #[test]
    fn test_personas_yml_elsewhere_not_exempted() {
        let tmp = std::env::temp_dir().join(format!("personas-elsewhere-{}", std::process::id()));
        // Wrong location: company/personas.yml (not company/config/).
        let dir = tmp.join("company");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("personas.yml");
        std::fs::write(&file, PERSONAS_YML_CONTENT).unwrap();

        let validator = setup_validator_with_root(&tmp);
        let result = validator.validate_file(&file);
        assert!(
            matches!(result, Err(ValidationError::InvalidApiVersion { .. })),
            "personas.yml elsewhere must NOT be exempted, got {result:?}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    // EDGE: schema-only validator (no root) does NOT apply the exemption —
    // documented behaviour (returns InvalidApiVersion). Unchanged.
    #[test]
    fn test_personas_yml_no_root_not_exempted() {
        let tmp = std::env::temp_dir().join(format!("personas-noroot-{}", std::process::id()));
        let dir = tmp.join("company/config");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("personas.yml");
        std::fs::write(&file, PERSONAS_YML_CONTENT).unwrap();

        let validator = setup_validator(); // no with_root
        let result = validator.validate_file(&file);
        assert!(
            matches!(result, Err(ValidationError::InvalidApiVersion { .. })),
            "without root, the exemption does not trigger (documented), got {result:?}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }
}
