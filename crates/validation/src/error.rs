use companyos_config::ArtifactKind;

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("YAML parse error: {0}")]
    YamlParse(#[from] serde_yaml::Error),

    #[error("Unknown artifact kind: '{raw}'")]
    UnknownKind { raw: String },

    #[error("Schema not found for kind: {kind}")]
    SchemaNotFound { kind: ArtifactKind },

    #[error("Schema validation failed:\n{errors}")]
    SchemaValidation { errors: String },

    #[error("Missing required field: {field}")]
    MissingField { field: String },

    #[error("Invalid api_version: expected 'companyos/v1', got '{got}'")]
    InvalidApiVersion { got: String },

    #[error("JSON Schema compile error for '{kind}': {reason}")]
    SchemaCompile { kind: ArtifactKind, reason: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}
