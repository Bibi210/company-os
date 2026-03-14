use std::collections::HashMap;
use std::path::{Path, PathBuf};

use companyos_config::ArtifactKind;
use companyos_config::constants;

use crate::error::ValidationError;

/// Loads and caches compiled JSON Schemas.
pub struct SchemaRegistry {
    schemas_dir: PathBuf,
    schemas: HashMap<ArtifactKind, serde_json::Value>,
    /// The _base.schema.json, loaded for $ref resolution.
    base_schema: Option<serde_json::Value>,
}

impl SchemaRegistry {
    /// Load all schemas from the schemas/ directory.
    pub fn load(schemas_dir: impl AsRef<Path>) -> Result<Self, ValidationError> {
        let schemas_dir = schemas_dir.as_ref().to_path_buf();
        let mut schemas = HashMap::new();
        let mut base_schema = None;

        if !schemas_dir.exists() {
            return Ok(Self {
                schemas_dir,
                schemas,
                base_schema,
            });
        }

        for entry in std::fs::read_dir(&schemas_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some(constants::EXT_JSON) {
                let filename = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default();

                let content = std::fs::read_to_string(&path)?;
                let schema_value: serde_json::Value = serde_json::from_str(&content)?;

                let kind_str = filename.trim_end_matches(constants::SCHEMA_STEM_SUFFIX);

                if kind_str == constants::BASE_SCHEMA_STEM {
                    base_schema = Some(schema_value);
                    continue;
                }

                if let Ok(artifact_kind) = kind_str.parse::<ArtifactKind>() {
                    schemas.insert(artifact_kind, schema_value);
                }
            }
        }

        Ok(Self {
            schemas_dir,
            schemas,
            base_schema,
        })
    }

    /// Get schema JSON value for a given artifact kind.
    pub fn get(&self, kind: ArtifactKind) -> Option<&serde_json::Value> {
        self.schemas.get(&kind)
    }

    /// Get the base schema (for $ref resolution).
    pub fn base_schema(&self) -> Option<&serde_json::Value> {
        self.base_schema.as_ref()
    }

    /// Get the schemas directory path.
    pub fn schemas_dir(&self) -> &Path {
        &self.schemas_dir
    }

    /// List all loaded schema kinds.
    pub fn kinds(&self) -> Vec<ArtifactKind> {
        self.schemas.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use companyos_config::ArtifactKind;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn real_schemas_dir() -> PathBuf {
        workspace_root().join("company/schemas")
    }

    #[test]
    fn test_load_real_schemas() {
        let registry = SchemaRegistry::load(real_schemas_dir()).unwrap();
        let kinds = registry.kinds();
        assert!(
            !kinds.is_empty(),
            "kinds() should be non-empty after loading real schemas"
        );
        assert!(
            kinds.contains(&ArtifactKind::TaskRequest),
            "should contain TaskRequest"
        );
        assert!(kinds.contains(&ArtifactKind::Rfc), "should contain Rfc");
        assert!(
            kinds.contains(&ArtifactKind::DesignDoc),
            "should contain DesignDoc"
        );
    }

    #[test]
    fn test_load_nonexistent_dir() {
        let result = SchemaRegistry::load("/nonexistent/path");
        // The current implementation returns Ok with empty schemas for non-existent dirs,
        // but we verify it doesn't panic and handles gracefully.
        let registry = result.unwrap();
        assert!(registry.kinds().is_empty());
    }

    #[test]
    fn test_base_schema_loaded() {
        let registry = SchemaRegistry::load(real_schemas_dir()).unwrap();
        assert!(
            registry.base_schema().is_some(),
            "base schema should be loaded from real schemas dir"
        );
    }

    #[test]
    fn test_get_unknown_kind_after_load() {
        let registry = SchemaRegistry::load(real_schemas_dir()).unwrap();
        // TaskRequest is a known kind that has a schema file
        assert!(
            registry.get(ArtifactKind::TaskRequest).is_some(),
            "get() for TaskRequest should return Some after loading real schemas"
        );
    }
}
