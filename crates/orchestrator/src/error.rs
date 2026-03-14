use companyos_config::PersonaId;
use uuid::Uuid;

use crate::types::RoundStatus;

#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error("Review round not found: {id}")]
    RoundNotFound { id: Uuid },

    #[error("Review round is not open: {id} (status: {status})")]
    RoundNotOpen { id: Uuid, status: RoundStatus },

    #[error("Reviewer '{reviewer}' is not a required reviewer for round {round_id}")]
    NotRequiredReviewer { reviewer: PersonaId, round_id: Uuid },

    #[error("Write permit not found: {id}")]
    PermitNotFound { id: Uuid },

    #[error("Write permit already consumed: {id}")]
    PermitAlreadyConsumed { id: Uuid },

    #[error("Artifact not found in index: {id}")]
    ArtifactNotFound { id: String },

    #[error("Cannot read file '{path}': {source}")]
    FileRead {
        path: String,
        source: std::io::Error,
    },

    #[error("Validation failed for '{id}': {errors}")]
    ValidationFailed { id: String, errors: String },

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("YAML parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Invalid enum value: {0}")]
    InvalidEnumValue(String),
}
