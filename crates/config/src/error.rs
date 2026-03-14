use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Config file not found: {path}")]
    NotFound { path: PathBuf },

    #[error("YAML parse error in {path}: {source}")]
    YamlParse {
        path: PathBuf,
        source: serde_yaml::Error,
    },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Missing required field '{field}' in {path}")]
    MissingField { field: String, path: PathBuf },
}
