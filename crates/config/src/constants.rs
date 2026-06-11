//! Centralized constants for Company OS.
//! No magic literals should appear outside this module.

// --- API ---
pub const API_VERSION: &str = "companyos/v1";

// --- Schema ---
pub const BASE_SCHEMA_URI: &str = "https://companyos.dev/schemas/_base.schema.json";
pub const BASE_SCHEMA_STEM: &str = "_base";
pub const SCHEMA_STEM_SUFFIX: &str = ".schema";
pub const SCHEMA_EXTENSION: &str = ".schema.json";

// --- JSON field names (artifact envelope) ---
pub const FIELD_API_VERSION: &str = "api_version";
pub const FIELD_KIND: &str = "kind";
pub const FIELD_METADATA: &str = "metadata";
pub const FIELD_ID: &str = "id";

// --- Protected zones ---
// Single source of truth: company/config/protected-zones.json
// Loaded at runtime via protected_zones module.
pub const PROTECTED_ZONES_FILE: &str = "company/config/protected-zones.json";

// --- File paths (relative to root) ---
pub const CONFIG_FLOW_CONTROL: &str = "company/config/flow-control.yml";
pub const CONFIG_REVIEW_PROTOCOL: &str = "company/config/review-protocol.yml";
pub const PERSONAS_DIR: &str = "company/personas";
pub const SCHEMAS_DIR: &str = "company/schemas";
pub const LESSONS_DIR: &str = "company/lessons";
pub const ARTIFACTS_DIR: &str = "company";
pub const PROJECTS_DIR: &str = "projects";

// --- Environment ---
pub const ENV_COMPANYOS_ROOT: &str = "COMPANYOS_ROOT";

// --- Data directory ---
pub const DATA_DIR: &str = "company/data";
pub const DB_FILENAME: &str = "orchestrator.db";

// --- File extensions ---
pub const EXT_YML: &str = "yml";
pub const EXT_YAML: &str = "yaml";
pub const EXT_JSON: &str = "json";

// --- Artifact index (SQLite tables) ---
pub const TABLE_ARTIFACTS: &str = "artifacts";
pub const TABLE_ARTIFACTS_FTS: &str = "artifacts_fts";
pub const TABLE_ARTIFACT_RELATIONS: &str = "artifact_relations";

// --- Search defaults ---
pub const DEFAULT_SEARCH_LIMIT: usize = 10;

// --- Orchestrator defaults ---
pub const DEFAULT_MAX_ITERATIONS: u32 = 3;

// --- Component names (for diagnostics) ---
pub const COMPONENT_ORCHESTRATOR: &str = "orchestrator";
pub const COMPONENT_YAML_VALIDATOR: &str = "yaml-validator";
pub const COMPONENT_PRE_COMMIT: &str = "pre-commit";
pub const COMPONENT_DEFENSE: &str = "defense-in-depth";
