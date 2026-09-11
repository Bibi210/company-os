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
/// Dedicated write-permit seal file (RFC cde13417 A1.1). Canonical JSON
/// export of the `write_permits` table, committed in pathspec by the
/// server. Replaces the whole-DB blob previously committed for sealing
/// (RFC 359f9162). Lives under [`DATA_DIR`].
pub const SEAL_FILENAME: &str = "permits-seal.json";

// --- Runtime recovery telemetry (RFC 5bacb08a) ---
//
// INVARIANT, do not regress: none of the paths below may ever carry a
// `.yml` or `.yaml` extension. [`crate::watcher::classify`] maps any YAML
// living outside the protected zones to `ConfigChangeKind::Artifacts`, so a
// YAML file under [`DATA_DIR`] would make every write trigger a full
// `reindex_all` (the exact code path of the SIGABRT series this telemetry
// exists to diagnose), and `make validate` (`--batch company/`) would try
// to validate it against an artifact schema and fail. Use `.log`, `.txt`
// or `.json` only. These files are gitignored: they are runtime state
// written by the proxy and the served servers, never by an agent.

/// Directory of the rotating telemetry journals written by the MCP proxy,
/// one file per supervised crate (RFC 5bacb08a D1). Lives under
/// [`DATA_DIR`].
pub const LOGS_DIR: &str = "company/data/logs";

/// Directory of the pre-unwind crash traces written by the panic hook of
/// every served server (RFC 5bacb08a D3b). One file per crash, never
/// rotated and never purged: a trace must survive the process, the session
/// and the reboot. Lives under [`DATA_DIR`].
pub const CRASHES_DIR: &str = "company/data/crashes";

/// Extension of a single crash trace file. Plain text so a trace stays
/// readable with no tooling at all, and so the file can never be mistaken
/// for an artifact by the watcher or the validator.
pub const CRASH_TRACE_EXT: &str = "txt";

/// Coredump baseline snapshotted on the first `make doctor` run and
/// compared against on every later run (RFC 5bacb08a D6). Lives under
/// [`DATA_DIR`].
pub const COREDUMP_BASELINE_FILENAME: &str = "coredump-baseline.json";

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
