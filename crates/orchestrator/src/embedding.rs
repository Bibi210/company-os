//! Local multilingual text embeddings for hybrid search (RFC bdee1af4).
//!
//! Wraps `fastembed::TextEmbedding` with the project-wide constants:
//!
//! - Model: `MultilingualE5Small` (118M params, dim 384, int8). Multilingue
//!   natif FR + EN technique, footprint < 200 MB.
//! - Determinism: `embed(text)` returns the same vector on a same machine
//!   for the same input, modulo a cosine similarity > 0.9999 tolerance
//!   (RFC F-design-2 iteration 3).
//! - Runtime autonomy: weights are loaded from a local cache. If absent at
//!   boot, the server exits with [`OrchestratorError::EmbeddingModelMissing`]
//!   instructing the operator to run `--prefetch-embeddings`. No download
//!   at runtime during a `search()` or `index_artifact()` call (RFC
//!   iteration 3 axis (i)).
//!
//! ## Architecture marker
//!
//! The `model_version()` constant encodes both the model identifier and the
//! host architecture (e.g. `multilingual-e5-small-v1+x86_64`). A binary
//! that loads a DB with a different marker at boot triggers a wipe +
//! reindex_all automatic (RFC F-design-2 iteration 3, étape 5 of the
//! implementation-plan 640e2894).

use std::path::{Path, PathBuf};

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

use crate::error::OrchestratorError;

/// Stable identifier for the embedding model wired into this binary.
///
/// Format: `<model-slug>-<version>+<arch>`. The architecture suffix is
/// derived from [`std::env::consts::ARCH`] and ensures that a DB indexed
/// on one architecture (e.g. `x86_64`) is rebuilt on a different one
/// (e.g. `aarch64`) where float-precision drift could otherwise produce
/// silently incoherent kNN results.
///
/// Any change to the model or to the architecture invalidates existing
/// vectors and triggers an automatic `wipe + reindex_all` at boot.
pub fn model_version() -> String {
    format!("multilingual-e5-small-v1+{}", std::env::consts::ARCH)
}

/// Output dimension of the embedding vector. Must match the model.
pub const EMBEDDING_DIM: usize = 384;

/// Maximum input length (in chars) before truncation. The model itself
/// truncates at ~512 tokens; this conservative ceiling (8192 chars ~=
/// 2000 tokens) lets fastembed do the token-level truncation without
/// the Rust wrapper having to count tokens.
pub const MAX_INPUT_CHARS: usize = 8192;

/// Relative path of the local embedding cache from the project root.
/// The model file (`model.onnx`, tokenizer, config) lives under this
/// directory after a `--prefetch-embeddings` invocation.
pub const CACHE_DIR_REL: &str = "company/data/embeddings/cache";

/// Resolve the absolute cache directory path from a project root.
pub fn cache_dir(root: &str) -> PathBuf {
    Path::new(root).join(CACHE_DIR_REL)
}

/// Thin, thread-friendly wrapper around `fastembed::TextEmbedding`.
///
/// The inner `TextEmbedding` is held by mutable reference internally
/// (fastembed's `embed` takes `&mut self`), but the engine consumes
/// `Arc<Embedder>` via a `Mutex` since indexing is already serialised
/// by the engine-wide tokio Mutex.
pub struct Embedder {
    inner: std::sync::Mutex<TextEmbedding>,
}

impl Embedder {
    /// Initialise the embedder from a local cache. Will NOT attempt any
    /// network access: if the cache is missing or incomplete, returns
    /// [`OrchestratorError::EmbeddingModelMissing`] with the cache path
    /// in the message.
    ///
    /// Sets `HF_HUB_OFFLINE=1` in the process environment as a safety
    /// belt — fastembed/hf-hub honour this variable and skip download
    /// attempts, surfacing a hard error instead of a slow network call.
    pub fn load_from_cache(root: &str) -> Result<Self, OrchestratorError> {
        // Belt-and-braces: instruct hf-hub to never reach for the network,
        // even if the cache lookup somehow misses. This guarantees the
        // RFC iteration 3 axis (i) constraint (autonomous runtime) holds.
        // SAFETY: setting an env var is safe in single-threaded init code,
        // and the orchestrator binary is fully sequential during boot
        // before serving MCP traffic.
        unsafe { std::env::set_var("HF_HUB_OFFLINE", "1") };

        let cache = cache_dir(root);
        if !cache.exists() {
            return Err(OrchestratorError::EmbeddingModelMissing {
                path: cache.display().to_string(),
            });
        }

        let options = TextInitOptions::new(EmbeddingModel::MultilingualE5Small)
            .with_cache_dir(cache.clone())
            .with_show_download_progress(false);

        let inner = TextEmbedding::try_new(options).map_err(|e| {
            // The most likely cause here is a missing/partial cache — we
            // surface a clear "missing model" error rather than the
            // verbose anyhow chain from fastembed.
            OrchestratorError::EmbeddingModelMissing {
                path: format!("{} (fastembed: {e})", cache.display()),
            }
        })?;

        Ok(Self {
            inner: std::sync::Mutex::new(inner),
        })
    }

    /// Pre-fetch the model files into the local cache. Runs ONLINE (no
    /// `HF_HUB_OFFLINE` here), used exclusively from the CLI mode
    /// `--prefetch-embeddings`. Idempotent.
    pub fn prefetch_to_cache(root: &str) -> Result<PathBuf, OrchestratorError> {
        let cache = cache_dir(root);
        std::fs::create_dir_all(&cache).map_err(|e| OrchestratorError::EmbeddingFailed {
            reason: format!("Failed to create cache dir '{}': {e}", cache.display()),
        })?;

        // Unset HF_HUB_OFFLINE if it leaked from a parent invocation.
        // SAFETY: pre-fetch is a sequential CLI mode, no concurrent envs.
        unsafe { std::env::remove_var("HF_HUB_OFFLINE") };

        let options = TextInitOptions::new(EmbeddingModel::MultilingualE5Small)
            .with_cache_dir(cache.clone())
            .with_show_download_progress(true);

        // Loading the model downloads it on first run, no-op on subsequent
        // runs.
        let _ =
            TextEmbedding::try_new(options).map_err(|e| OrchestratorError::EmbeddingFailed {
                reason: format!("Failed to fetch embedding model: {e}"),
            })?;

        Ok(cache)
    }

    /// Embed a single text. Truncates to [`MAX_INPUT_CHARS`] from the
    /// right on a char boundary.
    pub fn embed_text(&self, text: &str) -> Result<Vec<f32>, OrchestratorError> {
        let trimmed = truncate_chars(text, MAX_INPUT_CHARS);

        let mut guard = self
            .inner
            .lock()
            .map_err(|e| OrchestratorError::EmbeddingFailed {
                reason: format!("Embedder mutex poisoned: {e}"),
            })?;

        let mut embeddings = guard.embed(vec![trimmed], Some(1)).map_err(|e| {
            OrchestratorError::EmbeddingFailed {
                reason: format!("fastembed inference failed: {e}"),
            }
        })?;

        let vec = embeddings
            .pop()
            .ok_or_else(|| OrchestratorError::EmbeddingFailed {
                reason: "fastembed returned 0 vectors for 1 input".into(),
            })?;

        if vec.len() != EMBEDDING_DIM {
            return Err(OrchestratorError::EmbeddingFailed {
                reason: format!(
                    "fastembed returned dim {} but EMBEDDING_DIM = {EMBEDDING_DIM}",
                    vec.len()
                ),
            });
        }

        Ok(vec)
    }

    /// Build the embeddable view of an artifact YAML by kind and embed
    /// it. The view is `title + ". " + description + ". " + tags + body`
    /// where `body` depends on the kind (cf. plan 640e2894 étape 4).
    pub fn embed_artifact_view(
        &self,
        yaml: &serde_json::Value,
        kind: &str,
    ) -> Result<Vec<f32>, OrchestratorError> {
        let view = build_embedding_view(yaml, kind);
        self.embed_text(&view)
    }
}

/// Construct the text view of an artifact that will be embedded. Pure
/// function, exposed for testing and re-use by reindex_all.
pub fn build_embedding_view(yaml: &serde_json::Value, kind: &str) -> String {
    let metadata = yaml.get("metadata");
    let title = metadata
        .and_then(|m| m.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let description = metadata
        .and_then(|m| m.get("description"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let tags: Vec<&str> = metadata
        .and_then(|m| m.get("tags"))
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|t| t.as_str()).collect())
        .unwrap_or_default();

    let mut parts: Vec<String> = Vec::with_capacity(4);
    parts.push(title.to_string());
    parts.push(description.to_string());
    if !tags.is_empty() {
        parts.push(tags.join(", "));
    }

    let spec = yaml.get("spec");
    let body = match kind {
        "task-request" => spec_field(spec, &["description"]),
        "design-doc" => spec_field(spec, &["overview"]),
        "rfc" => {
            let m = spec_field(spec, &["motivation"]);
            let p = spec_field(spec, &["proposal"]);
            join_non_empty(&[m, p])
        }
        "implementation-plan" => spec
            .and_then(|s| s.get("steps"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|step| step.get("description").and_then(|d| d.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default(),
        "diagnostic-report" => join_non_empty(&[
            spec_field(spec, &["symptom"]),
            spec_field(spec, &["root_cause"]),
            spec_field(spec, &["resolution"]),
        ]),
        "lesson-learned" => join_non_empty(&[
            spec_field(spec, &["context"]),
            spec_field(spec, &["insight"]),
            spec_field(spec, &["recommendation"]),
        ]),
        "review-report" => join_non_empty(&[
            spec_field(spec, &["summary"]),
            spec.and_then(|s| s.get("findings"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|f| f.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default(),
        ]),
        _ => String::new(),
    };
    if !body.is_empty() {
        parts.push(body);
    }

    parts.join(". ")
}

fn spec_field(spec: Option<&serde_json::Value>, keys: &[&str]) -> String {
    let mut cur = spec;
    for k in keys {
        cur = cur.and_then(|v| v.get(*k));
    }
    cur.and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

fn join_non_empty(parts: &[String]) -> String {
    parts
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Truncate a string to at most `max_chars` characters on a char
/// boundary (no panic on multi-byte UTF-8 inputs).
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_short_passthrough() {
        assert_eq!(truncate_chars("hello", 100), "hello");
    }

    #[test]
    fn truncate_chars_long_cuts() {
        let s = "a".repeat(10000);
        let t = truncate_chars(&s, 8192);
        assert_eq!(t.chars().count(), 8192);
    }

    #[test]
    fn truncate_chars_multibyte_safe() {
        // Each "é" is 2 bytes in UTF-8 but 1 char.
        let s = "é".repeat(1000);
        let t = truncate_chars(&s, 500);
        assert_eq!(t.chars().count(), 500);
    }

    #[test]
    fn model_version_contains_arch() {
        let v = model_version();
        assert!(v.contains(std::env::consts::ARCH));
        assert!(v.starts_with("multilingual-e5-small-v1"));
    }

    #[test]
    fn build_view_universal_prefix() {
        let yaml: serde_json::Value = serde_yaml::from_str(
            r#"
metadata:
  title: "My RFC"
  description: "A short summary"
  tags: ["foo", "bar"]
"#,
        )
        .unwrap();
        let view = build_embedding_view(&yaml, "rfc");
        assert!(view.contains("My RFC"));
        assert!(view.contains("A short summary"));
        assert!(view.contains("foo, bar"));
    }

    #[test]
    fn build_view_rfc_includes_motivation_and_proposal() {
        let yaml: serde_json::Value = serde_yaml::from_str(
            r#"
metadata:
  title: "T"
  description: "D"
  tags: []
spec:
  motivation: "because"
  proposal: "do this"
"#,
        )
        .unwrap();
        let view = build_embedding_view(&yaml, "rfc");
        assert!(view.contains("because"));
        assert!(view.contains("do this"));
    }

    #[test]
    fn build_view_unknown_kind_falls_back_to_metadata_only() {
        let yaml: serde_json::Value = serde_yaml::from_str(
            r#"
metadata:
  title: "T"
  description: "D"
  tags: []
spec:
  whatever: "ignored"
"#,
        )
        .unwrap();
        let view = build_embedding_view(&yaml, "fictional-kind");
        assert!(view.contains("T"));
        assert!(view.contains("D"));
        assert!(!view.contains("ignored"));
    }

    #[test]
    fn build_view_lesson_concatenates_three_fields() {
        let yaml: serde_json::Value = serde_yaml::from_str(
            r#"
metadata:
  title: "Lesson"
  description: ""
  tags: []
spec:
  context: "ctx"
  insight: "ins"
  recommendation: "rec"
"#,
        )
        .unwrap();
        let view = build_embedding_view(&yaml, "lesson-learned");
        assert!(view.contains("ctx"));
        assert!(view.contains("ins"));
        assert!(view.contains("rec"));
    }
}
