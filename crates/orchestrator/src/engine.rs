use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use companyos_config::{ArtifactKind, PersonaId, constants};
use companyos_validation::ArtifactValidator;
use uuid::Uuid;

use crate::embedding::Embedder;

/// Mechanism 16 (RFC 0197fbe5) — historical cutoff for the author↔produces
/// matrix. Artifacts whose `metadata.created_at` is strictly BEFORE this date
/// (the RFC date) are exempt from the produces check: the pre-existing corpus
/// predates the rule. Strict lower bound: `created_at == cutoff` is NOT exempt.
/// ISO-8601 date strings compare lexicographically the same as chronologically.
pub const AUTHOR_PRODUCES_CUTOFF: &str = "2026-07-12";

/// The three affected-files lists extracted from an RFC's
/// `spec.affected_files` (mechanism 14/15, RFC 0197fbe5). FORME A (flat array)
/// populates `modified`; FORME B (object) populates the three keys.
#[derive(Debug, Clone, Default)]
pub struct AffectedFiles {
    pub modified: Vec<String>,
    pub created: Vec<String>,
    pub deleted: Vec<String>,
}

impl AffectedFiles {
    /// The union modified + created + deleted (candidate target_paths).
    pub fn union(&self) -> Vec<String> {
        let mut v = self.modified.clone();
        v.extend(self.created.iter().cloned());
        v.extend(self.deleted.iter().cloned());
        v
    }
}

/// Canonical on-disk seal of the `write_permits` table (RFC cde13417 A1.1).
/// Serialized deterministically by
/// [`OrchestratorEngine::write_permits_seal`]: `version` is fixed, permits
/// are pre-sorted by id, and field order is the declaration order below.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SealFile {
    version: u32,
    permits: Vec<SealPermit>,
}

/// One permit entry in the canonical seal. Strings are used for timestamps
/// (RFC 3339) and ids so the JSON is stable and human-diffable. Field order
/// here IS the emitted key order (serde_json preserves struct field order).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SealPermit {
    id: String,
    rfc_id: String,
    granted_to: String,
    target_paths: Vec<String>,
    status: String,
    granted_by: String,
    granted_at: String,
    consumed_at: Option<String>,
}

impl From<&WritePermit> for SealPermit {
    fn from(p: &WritePermit) -> Self {
        SealPermit {
            id: p.id.to_string(),
            rfc_id: p.rfc_id.to_string(),
            granted_to: p.granted_to.as_str().to_string(),
            target_paths: p.target_paths.iter().map(|t| t.0.clone()).collect(),
            status: p.status.to_string(),
            granted_by: p.granted_by.as_str().to_string(),
            granted_at: p.granted_at.to_rfc3339(),
            consumed_at: p.consumed_at.map(|t| t.to_rfc3339()),
        }
    }
}

impl SealPermit {
    /// Rebuild a [`WritePermit`] from a seal entry. Parses ids, personas,
    /// path patterns, status and timestamps back into their typed form.
    fn to_permit(&self) -> Result<WritePermit, OrchestratorError> {
        use std::str::FromStr;
        let parse_uuid = |s: &str| {
            Uuid::parse_str(s).map_err(|e| OrchestratorError::IntegrityFailure {
                details: format!("invalid uuid in permit seal: '{s}' ({e})"),
            })
        };
        let parse_ts = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| OrchestratorError::IntegrityFailure {
                    details: format!("invalid timestamp in permit seal: '{s}' ({e})"),
                })
        };
        let granted_to = PersonaId::from_str(&self.granted_to)
            .map_err(|e| OrchestratorError::InvalidEnumValue(format!("granted_to: {e}")))?;
        let granted_by = PersonaId::from_str(&self.granted_by)
            .map_err(|e| OrchestratorError::InvalidEnumValue(format!("granted_by: {e}")))?;
        let status = crate::types::PermitStatus::from_str(&self.status)
            .map_err(OrchestratorError::InvalidEnumValue)?;
        let consumed_at = match &self.consumed_at {
            Some(s) => Some(parse_ts(s)?),
            None => None,
        };
        Ok(WritePermit {
            id: parse_uuid(&self.id)?,
            rfc_id: parse_uuid(&self.rfc_id)?,
            granted_to,
            target_paths: self
                .target_paths
                .iter()
                .map(|s| crate::types::PathPattern(s.clone()))
                .collect(),
            status,
            granted_by,
            granted_at: parse_ts(&self.granted_at)?,
            consumed_at,
        })
    }
}

/// Result of an RFC status auto-update triggered by close_round.
#[derive(Debug, Clone, PartialEq)]
pub enum RfcUpdateResult {
    /// RFC status was updated (approved or rejected).
    Updated { new_status: String },
    /// RFC was already in the target status (idempotent).
    AlreadyUpToDate,
    /// The artifact was not an RFC — no update needed.
    NotAnRfc,
    /// Update attempted but failed (non-fatal: round is still closed).
    Failed(String),
}

/// Outcome of [`OrchestratorEngine::set_rfc_implemented`].
///
/// Carries everything `main.rs` needs to build the MCP JSON response WITHOUT
/// re-reading the file. `already_implemented` distinguishes a real
/// approved -> implemented transition from the idempotent success case (RFC
/// 1c0f2570 decision CEO 1: an already-implemented RFC is a success, never an
/// error, and its original `implemented_at` is preserved).
#[derive(Debug, Clone, PartialEq)]
pub struct SetImplementedOutcome {
    pub rfc_id: Uuid,
    pub previous_status: String,
    pub new_status: String,
    pub implemented_at: String,
    pub file_path: String,
    pub already_implemented: bool,
}

/// Outcome of `supersede_artifact` (mechanism 10, RFC a4ee8b6a). Reports
/// which files were touched and whether the call was an idempotent no-op.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SupersedeOutcome {
    pub old_id: String,
    pub new_id: String,
    pub old_file: String,
    pub new_file: String,
    pub old_changed: bool,
    pub new_changed: bool,
    pub idempotent: bool,
}

use crate::db::OrchestratorDb;
use crate::error::OrchestratorError;
use crate::roadmap_summary::{
    self, ALL_STATUSES, ALL_TIMEFRAMES, DriftWarning, RoadmapCounters, RoadmapHeader, RoadmapItem,
    RoadmapItemRef, RoadmapListEntry, RoadmapSelector, RoadmapSummary, SourceStatus,
    effective_status,
};
use crate::types::*;

/// Kind discriminator used everywhere the engine needs to scope operations
/// to roadmap artifacts. Keep in sync with crates/config/src/types.rs.
const ROADMAP_KIND: &str = "roadmap";

/// Minimal projection of an artifact YAML used to extract `metadata.status`
/// without deserializing the whole document. Tolerant by design: a missing
/// `status` (or unreadable YAML) yields `None`, which the caller maps to the
/// neutral `planned` fallback.
#[derive(serde::Deserialize)]
struct MetadataStatusProbe {
    metadata: MetadataStatusInner,
}

#[derive(serde::Deserialize)]
struct MetadataStatusInner {
    status: Option<String>,
}

/// Parse `metadata.status` from an RFC YAML content. Returns `None` if the
/// YAML is unparseable or carries no status field.
fn parse_rfc_metadata_status(content: &str) -> Option<String> {
    serde_yaml::from_str::<MetadataStatusProbe>(content)
        .ok()
        .and_then(|p| p.metadata.status)
}

/// Top-K candidate set size per axis BEFORE fusion. The RRF then selects
/// the top `limit` from the union. 50 is the canonical choice from the
/// hybrid search literature and gives ample slack for an N=10 final
/// answer (RFC bdee1af4 proposition 3, plan 640e2894 étape 1).
const SEARCH_RETRIEVE_K: usize = 50;

/// Mode of the search pipeline. `Hybrid` is the default everywhere; the
/// other two exist for callers that want to bypass fusion (e.g. an
/// agent that knows the answer is a UUID and wants strict lexical).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchMode {
    Lexical,
    Semantic,
    #[default]
    Hybrid,
}

/// Request payload for [`OrchestratorEngine::search_hybrid`].
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub mode: SearchMode,
    pub filters: crate::db::SearchFilters,
    pub limit: usize,
    pub rerank: bool,
    pub hyde: bool,
    pub explain: bool,
}

/// Response payload for [`OrchestratorEngine::search_hybrid`].
#[derive(Debug, Clone)]
pub struct SearchResponse {
    pub results: Vec<ArtifactSummary>,
    pub explain: Option<ExplainTrace>,
}

/// Optional per-call trace data, populated when `explain=true`.
#[derive(Debug, Clone, Default)]
pub struct ExplainTrace {
    pub mode_applied: String,
    pub candidate_set_sizes_lexical: usize,
    pub candidate_set_sizes_semantic: usize,
    pub candidate_set_sizes_fused: usize,
    pub latency_ms: u64,
}

/// Global indexation status, returned by `index_status` (no per-path).
/// All counts come from the live SQLite tables (`COUNT(*)`).
#[derive(Debug, Clone, Default)]
pub struct IndexStatusGlobal {
    pub artifacts_count: usize,
    pub fts_count: usize,
    pub vec_count: usize,
    pub triplet_coherent: bool,
    pub last_indexed_at: Option<String>,
    pub file_watcher_alive: bool,
    pub pending_index_queue_size: usize,
}

/// Per-path indexation status, returned when `index_status` is called
/// with a `path` parameter. Mirrors the inspection contract specified
/// in RFC bdee1af4 proposition 8.
#[derive(Debug, Clone, Default)]
pub struct IndexStatusPerPath {
    pub indexed_at: Option<String>,
    pub file_mtime: Option<String>,
    pub stale: bool,
    pub present_in_fts: bool,
    pub present_in_vec: bool,
}

pub struct OrchestratorEngine {
    db: OrchestratorDb,
    max_iterations: u32,
    /// Embedder used by `index_artifact` and `search` (semantic path).
    /// `None` is permitted for tests that do not exercise the indexing or
    /// search pipelines (e.g. pure review-round / consensus / permit
    /// tests). Indexing or semantic search without an embedder returns
    /// [`OrchestratorError::EmbeddingFailed`] with a clear message.
    embedder: Option<Arc<Embedder>>,
}

impl OrchestratorEngine {
    /// Construct an engine wired with an embedder. This is the production
    /// constructor used by `run_server` and `run_index`.
    pub fn new(db: OrchestratorDb, max_iterations: u32, embedder: Arc<Embedder>) -> Self {
        Self {
            db,
            max_iterations,
            embedder: Some(embedder),
        }
    }

    /// Construct an engine without an embedder. Reserved for tests of
    /// review-round / consensus / permit machinery that never call
    /// `index_artifact` or `search`. Any such call returns
    /// [`OrchestratorError::EmbeddingFailed`] with the message
    /// "engine constructed without embedder".
    pub fn new_without_embedder(db: OrchestratorDb, max_iterations: u32) -> Self {
        Self {
            db,
            max_iterations,
            embedder: None,
        }
    }

    /// Borrow the engine's shared embedder. Used by `search()` to embed
    /// the query under the same model as the persisted vectors. Returns
    /// `None` when the engine was constructed via `new_without_embedder`.
    pub fn embedder(&self) -> Option<&Arc<Embedder>> {
        self.embedder.as_ref()
    }

    fn require_embedder(&self) -> Result<&Arc<Embedder>, OrchestratorError> {
        self.embedder
            .as_ref()
            .ok_or_else(|| OrchestratorError::EmbeddingFailed {
                reason: "engine constructed without embedder (test mode); \
                         cannot index or run semantic search"
                    .into(),
            })
    }

    pub fn set_max_iterations(&mut self, val: u32) {
        self.max_iterations = val;
    }

    // --- Review Round Operations ---

    pub fn initiate_review_round(
        &self,
        artifact_path: ArtifactPath,
        artifact_kind: ArtifactKind,
        author: PersonaId,
        required_reviewers: Vec<PersonaId>,
    ) -> Result<ReviewRound, OrchestratorError> {
        // GARDE 1a (RFC 8bf78218, self_review_forbidden): the author can never
        // be a required reviewer of their own artifact.
        if required_reviewers.contains(&author) {
            return Err(OrchestratorError::SelfReviewForbidden {
                author,
                round_context: format!("required_reviewers of {artifact_path}"),
            });
        }

        let now = Utc::now();
        let round = ReviewRound {
            id: Uuid::new_v4(),
            artifact_path,
            artifact_kind,
            author,
            required_reviewers,
            status: RoundStatus::Open,
            iteration: 1,
            max_iterations: self.max_iterations,
            votes: Vec::new(),
            created_at: now,
            updated_at: now,
        };

        self.db.create_round(&round)?;
        Ok(round)
    }

    pub fn submit_vote(
        &self,
        round_id: Uuid,
        reviewer: PersonaId,
        verdict: ReviewVerdict,
        findings: Vec<Finding>,
        notes: Option<String>,
    ) -> Result<ReviewRound, OrchestratorError> {
        let mut round = self
            .db
            .get_round(round_id)?
            .ok_or_else(|| OrchestratorError::RoundNotFound { id: round_id })?;

        // GARDE 1b (RFC 8bf78218, self_review_forbidden): the author can never
        // vote on their own artifact, including on a pre-existing round.
        if reviewer == round.author {
            return Err(OrchestratorError::SelfReviewForbidden {
                author: round.author,
                round_context: format!("vote on round {round_id}"),
            });
        }

        if round.status != RoundStatus::Open && round.status != RoundStatus::RevisionRequired {
            return Err(OrchestratorError::RoundNotOpen {
                id: round_id,
                status: round.status,
            });
        }

        if !round.required_reviewers.contains(&reviewer) {
            return Err(OrchestratorError::NotRequiredReviewer { reviewer, round_id });
        }

        // GARDE 2a (RFC 8bf78218, review_findings_honesty): an approve verdict
        // with non-empty findings is contradictory. Corrective findings imply
        // request_changes; non-corrective observations belong in `notes`.
        if verdict == ReviewVerdict::Approve && !findings.is_empty() {
            return Err(OrchestratorError::ApproveWithFindings {
                count: findings.len(),
            });
        }

        round.votes.retain(|v| v.reviewer != reviewer);

        round.votes.push(ReviewVote {
            reviewer,
            verdict,
            findings,
            notes,
            submitted_at: Utc::now(),
        });

        round.updated_at = Utc::now();
        self.db.update_round(&round)?;
        Ok(round)
    }

    pub fn check_consensus(&self, round_id: Uuid) -> Result<ConsensusResult, OrchestratorError> {
        let round = self
            .db
            .get_round(round_id)?
            .ok_or_else(|| OrchestratorError::RoundNotFound { id: round_id })?;

        Ok(compute_consensus(&round))
    }

    pub fn close_round(&self, round_id: Uuid) -> Result<ReviewRound, OrchestratorError> {
        let mut round = self
            .db
            .get_round(round_id)?
            .ok_or_else(|| OrchestratorError::RoundNotFound { id: round_id })?;

        round.status = RoundStatus::Closed;
        round.updated_at = Utc::now();
        self.db.update_round(&round)?;
        Ok(round)
    }

    /// Close a review round and, if the artifact is an RFC, auto-update its YAML status.
    /// This implements rfc-auto-approve-001 §1: automatic status transition after consensus.
    ///
    /// Returns (closed_round, rfc_update_result).
    /// Errors in the RFC update are non-fatal: the round is closed regardless.
    pub fn close_round_with_rfc_update(
        &self,
        round_id: Uuid,
        root: &str,
    ) -> Result<(ReviewRound, RfcUpdateResult), OrchestratorError> {
        // Step A: close the round (always)
        let round = self.close_round(round_id)?;

        // Step B: if not an RFC, nothing more to do
        if round.artifact_kind != ArtifactKind::Rfc {
            return Ok((round, RfcUpdateResult::NotAnRfc));
        }

        // Step B: compute the consensus result from the closed round
        let consensus = compute_consensus(&round);
        let new_status = match consensus {
            ConsensusResult::ConsensusReached => {
                // All required reviewers approved
                "approved"
            }
            ConsensusResult::RevisionRequired | ConsensusResult::EscalationNeeded => "rejected",
            ConsensusResult::WaitingForVotes => {
                // Round closed before all votes — non-fatal
                return Ok((
                    round,
                    RfcUpdateResult::Failed(
                        "close_review_round called before all votes were submitted".into(),
                    ),
                ));
            }
        };

        // Locate the RFC YAML file via the artifact index
        let artifact_id = round
            .artifact_path
            .0
            .split('/')
            .next_back()
            .unwrap_or("")
            .trim_end_matches(".yml");
        let rfc_update =
            self.update_rfc_status_in_file(root, &round.artifact_path.0, artifact_id, new_status);
        Ok((round, rfc_update))
    }

    /// Update metadata.status (and approved_at / rejected_at) in an RFC YAML file.
    /// Idempotent: if the status is already correct, returns AlreadyUpToDate.
    fn update_rfc_status_in_file(
        &self,
        root: &str,
        artifact_path: &str,
        _artifact_id: &str,
        new_status: &str,
    ) -> RfcUpdateResult {
        // Build the full filesystem path
        let full_path = if artifact_path.starts_with('/') {
            artifact_path.to_string()
        } else {
            format!("{root}/{artifact_path}")
        };

        let content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(e) => return RfcUpdateResult::Failed(format!("cannot read {full_path}: {e}")),
        };

        // Parse YAML to check current status (idempotency)
        let parsed: serde_yaml::Value = match serde_yaml::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                return RfcUpdateResult::Failed(format!("cannot parse YAML {full_path}: {e}"));
            }
        };

        let current_status = parsed
            .get("metadata")
            .and_then(|m| m.get("status"))
            .and_then(|s| s.as_str())
            .unwrap_or("draft");

        if current_status == new_status {
            return RfcUpdateResult::AlreadyUpToDate;
        }

        // Build timestamp
        let now_iso = Utc::now().to_rfc3339();

        // Update status line and add timestamp using string replacement
        // Strategy: replace `status: <old>` with `status: <new>` in the metadata block,
        // then append the timestamp field.
        let timestamp_field = match new_status {
            "approved" => format!("  approved_at: \"{now_iso}\""),
            "implemented" => format!("  implemented_at: \"{now_iso}\""),
            _ => format!("  rejected_at: \"{now_iso}\""),
        };

        // Replace status in metadata block
        let updated = if content.contains(&format!("  status: {current_status}")) {
            content.replace(
                &format!("  status: {current_status}"),
                &format!("  status: {new_status}\n{timestamp_field}"),
            )
        } else if content.contains(&format!("  status: \"{current_status}\"")) {
            content.replace(
                &format!("  status: \"{current_status}\""),
                &format!("  status: {new_status}\n{timestamp_field}"),
            )
        } else {
            // status field not found — insert after title or id line in metadata
            // Find the metadata block and insert status after the first field
            let insert_marker = "  author:";
            if content.contains(insert_marker) {
                content.replacen(
                    insert_marker,
                    &format!("  status: {new_status}\n{timestamp_field}\n{insert_marker}"),
                    1,
                )
            } else {
                return RfcUpdateResult::Failed(format!(
                    "cannot locate status field or insertion point in {full_path}"
                ));
            }
        };

        match std::fs::write(&full_path, &updated) {
            Ok(()) => RfcUpdateResult::Updated {
                new_status: new_status.to_string(),
            },
            Err(e) => RfcUpdateResult::Failed(format!("cannot write {full_path}: {e}")),
        }
    }

    /// Transition an approved RFC to `status: implemented`, stamping
    /// `implemented_at`. Server-side lifecycle transition (RFC 1c0f2570),
    /// modelled on `close_round_with_rfc_update`: writes through the SINGLE
    /// `update_rfc_status_in_file` path, requires NO write permit.
    ///
    /// Transition matrix:
    ///   - `approved`    -> writes `implemented`, `already_implemented = false`.
    ///   - `implemented` -> idempotent success, original `implemented_at`
    ///     preserved, NO write (`already_implemented = true`).
    ///   - `draft` / `review` / `rejected` -> `ValidationFailed` naming the
    ///     current status.
    ///   - kind != rfc          -> `ValidationFailed` naming the actual kind.
    ///   - id absent from index -> `ArtifactNotFound`.
    pub fn set_rfc_implemented(
        &self,
        rfc_id: Uuid,
        root: &str,
    ) -> Result<SetImplementedOutcome, OrchestratorError> {
        let id_str = rfc_id.to_string();

        // 1. Resolve the artifact via the index.
        let artifact = self
            .db
            .get_artifact(&id_str)?
            .ok_or_else(|| OrchestratorError::ArtifactNotFound { id: id_str.clone() })?;

        // 2. Verify it is an RFC.
        if artifact.kind != "rfc" {
            return Err(OrchestratorError::ValidationFailed {
                id: id_str.clone(),
                errors: format!("expected kind 'rfc', found '{}'", artifact.kind),
            });
        }

        // 3. Coherence check: the indexed file_path must live under
        //    company/rfcs/ (defends against index incoherence, RFC §3.b).
        let file_path = artifact.file_path.clone();
        if !file_path.starts_with("company/rfcs/") {
            return Err(OrchestratorError::ValidationFailed {
                id: id_str.clone(),
                errors: format!(
                    "indexed file_path '{file_path}' is not under company/rfcs/ — possible index incoherence"
                ),
            });
        }

        // 4. Read and parse the YAML once to obtain the current status.
        let full_path = if file_path.starts_with('/') {
            file_path.clone()
        } else {
            format!("{root}/{file_path}")
        };
        let content =
            std::fs::read_to_string(&full_path).map_err(|source| OrchestratorError::FileRead {
                path: full_path.clone(),
                source,
            })?;
        let parsed: serde_yaml::Value = serde_yaml::from_str(&content)?;
        let previous_status = parsed
            .get("metadata")
            .and_then(|m| m.get("status"))
            .and_then(|s| s.as_str())
            .unwrap_or("draft")
            .to_string();

        // 5. Apply the transition matrix.
        match previous_status.as_str() {
            "implemented" => {
                // Idempotent success (decision CEO 1): preserve the original
                // implemented_at, NO write.
                let existing_implemented_at = parsed
                    .get("metadata")
                    .and_then(|m| m.get("implemented_at"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                Ok(SetImplementedOutcome {
                    rfc_id,
                    previous_status,
                    new_status: "implemented".to_string(),
                    implemented_at: existing_implemented_at,
                    file_path,
                    already_implemented: true,
                })
            }
            "approved" => {
                // Real transition: delegate the write to the SINGLE write path.
                match self.update_rfc_status_in_file(root, &file_path, &id_str, "implemented") {
                    RfcUpdateResult::Updated { .. } => {
                        // Re-read implemented_at from disk so the outcome carries
                        // the exact stamped value (single source of truth).
                        let written = std::fs::read_to_string(&full_path).map_err(|source| {
                            OrchestratorError::FileRead {
                                path: full_path.clone(),
                                source,
                            }
                        })?;
                        let stamped = serde_yaml::from_str::<serde_yaml::Value>(&written)
                            .ok()
                            .and_then(|v| {
                                v.get("metadata")
                                    .and_then(|m| m.get("implemented_at"))
                                    .and_then(|s| s.as_str())
                                    .map(|s| s.to_string())
                            })
                            .unwrap_or_default();
                        Ok(SetImplementedOutcome {
                            rfc_id,
                            previous_status,
                            new_status: "implemented".to_string(),
                            implemented_at: stamped,
                            file_path,
                            already_implemented: false,
                        })
                    }
                    RfcUpdateResult::AlreadyUpToDate => {
                        // Should not happen (previous_status was "approved"),
                        // but treat defensively as idempotent success.
                        Ok(SetImplementedOutcome {
                            rfc_id,
                            previous_status,
                            new_status: "implemented".to_string(),
                            implemented_at: String::new(),
                            file_path,
                            already_implemented: true,
                        })
                    }
                    RfcUpdateResult::Failed(reason) => Err(OrchestratorError::ValidationFailed {
                        id: id_str,
                        errors: format!("failed to write status: {reason}"),
                    }),
                    RfcUpdateResult::NotAnRfc => Err(OrchestratorError::ValidationFailed {
                        id: id_str,
                        errors: "unexpected NotAnRfc result during RFC status write".to_string(),
                    }),
                }
            }
            other => Err(OrchestratorError::ValidationFailed {
                id: id_str,
                errors: format!(
                    "cannot mark as implemented: current status is '{other}', only 'approved' allowed"
                ),
            }),
        }
    }

    /// Supersede `old_id` by `new_id` with two atomic writes (mechanism 10,
    /// RFC a4ee8b6a; lesson 9b2c1951). Server-side lifecycle transition,
    /// requires NO write permit (same model as `set_rfc_implemented`):
    ///
    ///   1. Early-returns: self-supersession, unresolved id, or either
    ///      file in a protected zone → dedicated error, nothing written.
    ///   2. Both new contents computed IN MEMORY by targeted textual edit
    ///      (same family as `update_rfc_status_in_file`, preserves block
    ///      scalars). The NEW gains `supersedes`; the OLD gains
    ///      `superseded-by` AND a dated `[SUPERSEDED-BY ...]` marker at the
    ///      head of `metadata.description`.
    ///   3. Both contents validated against the schema BEFORE any disk
    ///      write (fail-safe against a textual-edit bug).
    ///   4. Writes: OLD first (the obsolescence annotation is the
    ///      structurally missing piece), then NEW; on 2nd-write failure the
    ///      OLD is rolled back from the original in-memory content.
    ///   5. Both files re-indexed so `search()` reflects the annotation.
    ///
    /// Idempotent: replaying with the same ids duplicates neither the
    /// `related` links nor the description marker.
    pub fn supersede_artifact(
        &mut self,
        old_id: &str,
        new_id: &str,
        note: Option<&str>,
        root: &str,
        validator: &ArtifactValidator,
    ) -> Result<SupersedeOutcome, OrchestratorError> {
        // 1a. Self-supersession refused.
        if old_id == new_id {
            return Err(OrchestratorError::SelfSupersession {
                id: old_id.to_string(),
            });
        }

        // 1b. Resolve both artifacts via the index.
        let old_art =
            self.db
                .get_artifact(old_id)?
                .ok_or_else(|| OrchestratorError::ArtifactNotFound {
                    id: old_id.to_string(),
                })?;
        let new_art =
            self.db
                .get_artifact(new_id)?
                .ok_or_else(|| OrchestratorError::ArtifactNotFound {
                    id: new_id.to_string(),
                })?;

        // 1c. Refuse if either file lives in a protected zone.
        let root_path = std::path::Path::new(root);
        for fp in [&old_art.file_path, &new_art.file_path] {
            if companyos_config::protected_zones::is_protected(root_path, fp) {
                return Err(OrchestratorError::SupersedeProtectedZone { path: fp.clone() });
            }
        }

        let old_full = format!("{root}/{}", old_art.file_path);
        let new_full = format!("{root}/{}", new_art.file_path);

        let old_orig =
            std::fs::read_to_string(&old_full).map_err(|source| OrchestratorError::FileRead {
                path: old_full.clone(),
                source,
            })?;
        let new_orig =
            std::fs::read_to_string(&new_full).map_err(|source| OrchestratorError::FileRead {
                path: new_full.clone(),
                source,
            })?;

        // 2. Compute both new contents in memory.
        let date = Utc::now().format("%Y-%m-%d").to_string();
        let new_updated = add_related_link(&new_orig, old_id, &old_art.kind, "supersedes");
        let old_with_link = add_related_link(&old_orig, new_id, &new_art.kind, "superseded-by");
        let old_updated = insert_supersede_marker(&old_with_link, new_id, &date, note);

        // 3. Validate both modified contents BEFORE any disk write.
        for (label, content) in [("new", &new_updated), ("old", &old_updated)] {
            let report = validator.validate_yaml_str(content).map_err(|e| {
                OrchestratorError::ValidationFailed {
                    id: format!("supersede {label}"),
                    errors: e.to_string(),
                }
            })?;
            if !report.is_valid {
                return Err(OrchestratorError::ValidationFailed {
                    id: format!("supersede {label}"),
                    errors: report.errors.join("; "),
                });
            }
        }

        // Idempotence: if nothing changed on either side, return early.
        let old_changed = old_updated != old_orig;
        let new_changed = new_updated != new_orig;

        // 4. Writes: OLD first, then NEW; rollback OLD on NEW failure.
        if old_changed {
            std::fs::write(&old_full, &old_updated).map_err(|source| {
                OrchestratorError::FileRead {
                    path: old_full.clone(),
                    source,
                }
            })?;
        }
        if new_changed && let Err(source) = std::fs::write(&new_full, &new_updated) {
            // Rollback best-effort of the OLD write.
            if old_changed && let Err(rb) = std::fs::write(&old_full, &old_orig) {
                eprintln!("supersede: NEW write failed AND rollback of OLD failed: {rb}");
            }
            return Err(OrchestratorError::FileRead {
                path: new_full.clone(),
                source,
            });
        }

        // 5. Re-index both files so search() reflects the annotation.
        // Best-effort: an indexing failure (e.g. no embedder in a test)
        // must not undo the successful writes.
        let _ = self.index_artifact(root, &old_art.file_path, validator);
        let _ = self.index_artifact(root, &new_art.file_path, validator);

        Ok(SupersedeOutcome {
            old_id: old_id.to_string(),
            new_id: new_id.to_string(),
            old_file: old_art.file_path,
            new_file: new_art.file_path,
            old_changed,
            new_changed,
            idempotent: !old_changed && !new_changed,
        })
    }

    /// Warn when an artifact declares `supersedes` toward a target that does
    /// NOT carry the reciprocal `superseded-by` (mechanism 10c, RFC
    /// a4ee8b6a). Non-blocking by construction: the LLM judges WHAT to
    /// supersede, this only checks that both writes happened.
    pub fn supersession_warnings(&self, root: &str, artifact: &IndexedArtifact) -> Vec<String> {
        let full = format!("{root}/{}", artifact.file_path);
        let Ok(content) = std::fs::read_to_string(&full) else {
            return Vec::new();
        };
        let Ok(yaml) = serde_yaml::from_str::<serde_json::Value>(&content) else {
            return Vec::new();
        };
        let mut warnings = Vec::new();
        for rel in extract_relations(&yaml) {
            if rel.relationship != "supersedes" {
                continue;
            }
            let Ok(Some(target)) = self.db.get_artifact(&rel.target_id) else {
                continue;
            };
            let target_full = format!("{root}/{}", target.file_path);
            let has_reciprocal = std::fs::read_to_string(&target_full)
                .ok()
                .and_then(|c| serde_yaml::from_str::<serde_json::Value>(&c).ok())
                .map(|ty| {
                    extract_relations(&ty)
                        .iter()
                        .any(|r| r.relationship == "superseded-by" && r.target_id == artifact.id)
                })
                .unwrap_or(false);
            if !has_reciprocal {
                warnings.push(format!(
                    "supersedes sans réciproque : la cible {} ne porte pas superseded-by ; \
                     utiliser supersede_artifact",
                    rel.target_id
                ));
            }
        }
        warnings
    }

    /// Mechanism 17a (RFC 0197fbe5) — single-file dangling-related detection.
    /// For each `related[].id` declared by `artifact`, verify the target is an
    /// indexed artifact; a missing target yields a non-blocking warning.
    /// Mirrors `supersession_warnings`: read the YAML, extract relations, look
    /// each target up in the index. NON-BLOCKING (imposed by the task-request).
    /// EDGE (RFC 17a): two brand-new artifacts referencing each other can
    /// produce a transient false positive here depending on write order (it
    /// disappears once the second is indexed); the bulk `reindex_all` pass does
    /// not have this problem thanks to the global SQL query
    /// ([`OrchestratorDb::dangling_related_links`]).
    pub fn related_integrity_warnings(
        &self,
        root: &str,
        artifact: &IndexedArtifact,
    ) -> Vec<String> {
        let full = format!("{root}/{}", artifact.file_path);
        let Ok(content) = std::fs::read_to_string(&full) else {
            return Vec::new();
        };
        let Ok(yaml) = serde_yaml::from_str::<serde_json::Value>(&content) else {
            return Vec::new();
        };
        let mut warnings = Vec::new();
        for rel in extract_relations(&yaml) {
            match self.db.get_artifact(&rel.target_id) {
                Ok(Some(_)) => {}
                Ok(None) => warnings.push(format!(
                    "related[].id {} introuvable dans l'index — lien pendant : id erroné \
                     (typo ?) ou artifact supprimé/jamais créé",
                    rel.target_id
                )),
                Err(_) => {}
            }
        }
        warnings
    }

    /// Mechanism 16 (RFC 0197fbe5) — author↔produces matrix, WARNING not
    /// rejection (a refused artifact would become invisible to `search()`,
    /// worse than the signalled violation). The persona YAML files are the
    /// SINGLE source of truth (no hard-coded "universal kinds" constant): read
    /// `company/personas/<author>.yml` fresh and warn if `artifact.kind` is not
    /// in `artifacts.produces`.
    ///
    /// Périmètre tranché (RFC 16c): system kinds are EXEMPT (governance, gated
    /// by protected zones + RFC cycle); an unknown author (no persona YAML) is
    /// skipped silently; a `created_at` strictly BEFORE
    /// [`AUTHOR_PRODUCES_CUTOFF`] is exempt (historical corpus). The cutoff is a
    /// strict lower bound: `created_at == cutoff` is NOT exempt.
    pub fn author_produces_warnings(&self, root: &str, artifact: &IndexedArtifact) -> Vec<String> {
        // System kinds are exempt (gouvernance).
        const SYSTEM_KINDS: [&str; 5] = [
            "persona",
            "project-config",
            "flow-control",
            "review-protocol",
            "human-review-triggers",
        ];
        if SYSTEM_KINDS.contains(&artifact.kind.as_str()) {
            return Vec::new();
        }

        let full = format!("{root}/{}", artifact.file_path);
        let Ok(content) = std::fs::read_to_string(&full) else {
            return Vec::new();
        };
        let Ok(yaml) = serde_yaml::from_str::<serde_json::Value>(&content) else {
            return Vec::new();
        };

        // Historical cutoff: exempt artifacts created strictly before the RFC.
        let created_at = yaml
            .pointer("/metadata/created_at")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !created_at.is_empty() && created_at < AUTHOR_PRODUCES_CUTOFF {
            return Vec::new();
        }

        let author = yaml
            .pointer("/metadata/author")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if author.is_empty() {
            return Vec::new();
        }

        // Read the author's persona YAML fresh. Unknown author => skip silently.
        let persona_path = format!("{root}/company/personas/{author}.yml");
        let Ok(persona_content) = std::fs::read_to_string(&persona_path) else {
            return Vec::new();
        };
        let Ok(persona_yaml) = serde_yaml::from_str::<serde_json::Value>(&persona_content) else {
            return Vec::new();
        };
        let produces: Vec<String> = persona_yaml
            .pointer("/artifacts/produces")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        if produces.iter().any(|k| k == &artifact.kind) {
            Vec::new()
        } else {
            vec![format!(
                "author '{author}' ne produit pas le kind '{}' selon sa persona \
                 (artifacts.produces) — vérifier l'attribution ou amender la persona par RFC",
                artifact.kind
            )]
        }
    }

    /// Mechanism 19c (RFC 0197fbe5) — capitalization reminders for a resolved
    /// diagnostic-report with no linked lesson. Single-file surface only (the
    /// reminder concerns the MOMENT of resolution; in bulk it would spam
    /// history). NON-BLOCKING.
    pub fn capitalization_reminders(&self, root: &str, artifact: &IndexedArtifact) -> Vec<String> {
        if artifact.kind != "diagnostic-report" {
            return Vec::new();
        }
        let full = format!("{root}/{}", artifact.file_path);
        let Ok(content) = std::fs::read_to_string(&full) else {
            return Vec::new();
        };
        let Ok(yaml) = serde_yaml::from_str::<serde_json::Value>(&content) else {
            return Vec::new();
        };
        let status = yaml
            .pointer("/spec/status")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if status != "resolved" {
            return Vec::new();
        }
        match self.db.has_linked_lesson(&artifact.id) {
            Ok(true) => Vec::new(),
            Ok(false) => vec![format!(
                "diagnostic-report {} resolved sans lesson liée — la règle \
                 lesson_after_every_artifact attend une capitalisation (context/insight/\
                 recommendation) ; créer une lesson et la lier via metadata.related",
                artifact.id
            )],
            Err(_) => Vec::new(),
        }
    }

    /// Mechanism 14d (RFC 0197fbe5) — single-file warnings aggregator. Union of
    /// supersession asymmetry (10c), related-integrity dangling links (17a),
    /// author↔produces (16), and capitalization reminders (19c). The three
    /// server single-file call-sites (index_now, index_artifact, CLI --index)
    /// switch to this so their existing `warnings` field enriches without a
    /// shape change. `supersession_warnings` stays public and UNCHANGED.
    pub fn artifact_warnings(&self, root: &str, artifact: &IndexedArtifact) -> Vec<String> {
        let mut w = self.supersession_warnings(root, artifact);
        w.extend(self.related_integrity_warnings(root, artifact));
        w.extend(self.author_produces_warnings(root, artifact));
        w.extend(self.capitalization_reminders(root, artifact));
        w
    }

    /// Extract `spec.affected_files` from an RFC YAML value into three lists
    /// (modified, created, deleted). Supports FORME A (flat array of strings,
    /// counted as `modified`) and FORME B (object with optional modified /
    /// created / deleted keys). Mechanism 14/15 (RFC 0197fbe5). Returns `None`
    /// only when the `affected_files` key is entirely absent (distinct from an
    /// empty declaration).
    /// Read + parse a YAML artifact into a `serde_json::Value` (mechanism
    /// 14/15/18, RFC 0197fbe5). Returns `None` on any read/parse failure. Keeps
    /// serde_yaml an implementation detail of this crate (the server crate does
    /// not depend on serde_yaml).
    pub fn read_yaml_value(&self, root: &str, file_path: &str) -> Option<serde_json::Value> {
        let full = if file_path.starts_with('/') {
            file_path.to_string()
        } else {
            format!("{root}/{file_path}")
        };
        let content = std::fs::read_to_string(&full).ok()?;
        serde_yaml::from_str(&content).ok()
    }

    pub fn extract_affected_files(rfc_yaml: &serde_json::Value) -> Option<AffectedFiles> {
        let af = rfc_yaml.pointer("/spec/affected_files")?;
        let mut out = AffectedFiles::default();
        if let Some(arr) = af.as_array() {
            // FORME A: flat list -> modified.
            out.modified = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        } else if let Some(obj) = af.as_object() {
            let take = |key: &str| -> Vec<String> {
                obj.get(key)
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            out.modified = take("modified");
            out.created = take("created");
            out.deleted = take("deleted");
        } else {
            return None;
        }
        Some(out)
    }

    /// Mechanism 14 (RFC 0197fbe5) — pre-check the permit scope at grant time.
    /// NON-BLOCKING: for each file in the RFC's affected_files union
    /// (modified + created + deleted), verify at least one `target_path`
    /// covers it (same [`crate::db::path_matches`] semantics as the permit
    /// checker). Uncovered files, or an RFC declaring no affected_files, yield
    /// a warning. The grant is NEVER refused for a scope gap (the CEO may scope
    /// narrower on purpose; the algorithm signals, the LLM judges).
    pub fn rfc_scope_warnings(
        &self,
        root: &str,
        rfc_file_path: &str,
        target_paths: &[PathPattern],
    ) -> Vec<String> {
        let full = if rfc_file_path.starts_with('/') {
            rfc_file_path.to_string()
        } else {
            format!("{root}/{rfc_file_path}")
        };
        let Ok(content) = std::fs::read_to_string(&full) else {
            return vec![format!(
                "pré-check de périmètre impossible : le fichier RFC {rfc_file_path} est illisible"
            )];
        };
        let Ok(yaml) = serde_yaml::from_str::<serde_json::Value>(&content) else {
            return vec![format!(
                "pré-check de périmètre impossible : le RFC {rfc_file_path} n'est pas un YAML valide"
            )];
        };
        let Some(af) = Self::extract_affected_files(&yaml) else {
            return vec![
                "pré-check de périmètre impossible : le RFC ne déclare pas affected_files"
                    .to_string(),
            ];
        };
        let mut warnings = Vec::new();
        for file in af.union() {
            let covered = target_paths
                .iter()
                .any(|tp| crate::db::path_matches(tp, &file));
            if !covered {
                warnings.push(format!(
                    "RFC affected_files: {file} n'est couvert par aucun target_path — si ce \
                     fichier sera écrit pendant l'implémentation, le hook la bloquera ; étendre \
                     target_paths ou assumer le scoping étroit"
                ));
            }
        }
        warnings
    }

    /// Mechanism 18 (RFC 0197fbe5) — write_permit_gate. NON-BLOCKING warning.
    /// If the RFC's affected_files union touches code (`crates/**` or
    /// `company/plugins/**`), look for a linked implementation-plan (either
    /// relation direction) with at least one Closed round in consensus. Absence
    /// of a plan, or a plan without such a round, yields a warning. `rfc_id` is
    /// the RFC uuid; `rfc_yaml` is its parsed content (already read by M14).
    pub fn write_permit_gate_warnings(
        &self,
        rfc_id: &str,
        rfc_yaml: &serde_json::Value,
    ) -> Vec<String> {
        let Some(af) = Self::extract_affected_files(rfc_yaml) else {
            return Vec::new();
        };
        let touches_code = af
            .union()
            .iter()
            .any(|f| f.starts_with("crates/") || f.starts_with("company/plugins/"));
        if !touches_code {
            return Vec::new();
        }

        // Resolve linked implementation-plans (both directions).
        let Ok(links) = self.db.get_relations(rfc_id) else {
            return Vec::new();
        };
        let plan_ids: Vec<String> = links
            .iter()
            .filter(|l| l.kind.as_deref() == Some("implementation-plan"))
            .map(|l| l.id.clone())
            .collect();

        if plan_ids.is_empty() {
            return vec![format!(
                "le RFC touche du code mais aucun implementation-plan lié n'existe dans l'index \
                 (write_permit_gate) — lier un plan approuvé via metadata.related"
            )];
        }

        let mut warnings = Vec::new();
        for plan_id in plan_ids {
            let Ok(Some(plan_art)) = self.db.get_artifact(&plan_id) else {
                continue;
            };
            let rounds = self
                .db
                .list_rounds_by_artifact_path(&plan_art.file_path)
                .unwrap_or_default();
            let has_consensus = rounds.iter().any(|r| {
                r.status == RoundStatus::Closed
                    && compute_consensus(r) == ConsensusResult::ConsensusReached
            });
            if !has_consensus {
                let short = &plan_id[..plan_id.len().min(8)];
                warnings.push(format!(
                    "plan {short} lié mais aucun review round fermé en consensus trouvé — NB : \
                     rounds éphémères, faux positif attendu après autorepair (write_permit_gate)"
                ));
            }
        }
        warnings
    }

    /// Mechanism 15 (RFC 0197fbe5) — evaluate the path-matchable human-review
    /// triggers against a grant. BLOCKING when a trigger matches and the caller
    /// did not confirm user approval. Reads `human-review-triggers.yml` fresh.
    /// FAIL-SAFE: an unreadable/invalid triggers file refuses the grant.
    ///
    /// Semantics of `on`:
    /// - `any`: matched against the target_paths (a glob target that COVERS the
    ///   trigger path fires, conservative) AND against the affected_files union
    ///   (literal: a trigger path that covers an affected file fires).
    /// - `deleted`: matched against affected_files.deleted only.
    ///
    /// Returns `Ok(())` when no trigger fires OR when `user_approval_confirmed`.
    pub fn evaluate_human_review_triggers(
        &self,
        root: &str,
        target_paths: &[PathPattern],
        affected: &Option<AffectedFiles>,
        user_approval_confirmed: bool,
    ) -> Result<(), OrchestratorError> {
        let triggers_path = format!("{root}/company/config/human-review-triggers.yml");
        let content = std::fs::read_to_string(&triggers_path).map_err(|e| {
            OrchestratorError::HumanReviewTriggersUnreadable {
                reason: format!("read failed: {e}"),
            }
        })?;
        let yaml: serde_json::Value = serde_yaml::from_str(&content).map_err(|e| {
            OrchestratorError::HumanReviewTriggersUnreadable {
                reason: format!("parse failed: {e}"),
            }
        })?;

        let triggers = yaml
            .pointer("/spec/triggers")
            .and_then(|v| v.as_array())
            .ok_or_else(|| OrchestratorError::HumanReviewTriggersUnreadable {
                reason: "spec.triggers missing or not an array".to_string(),
            })?;

        let deleted: Vec<String> = affected
            .as_ref()
            .map(|a| a.deleted.clone())
            .unwrap_or_default();
        let union: Vec<String> = affected.as_ref().map(|a| a.union()).unwrap_or_default();

        let mut matched: Vec<String> = Vec::new();
        for t in triggers {
            let Some(m) = t.get("match") else { continue };
            let paths: Vec<String> = m
                .get("paths")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let on = m.get("on").and_then(|v| v.as_str()).unwrap_or("any");
            let condition = t
                .get("condition")
                .and_then(|v| v.as_str())
                .unwrap_or("<unnamed trigger>");

            let fired = match on {
                "deleted" => paths.iter().any(|tp| {
                    deleted
                        .iter()
                        .any(|f| crate::db::path_matches(&PathPattern(tp.clone()), f))
                }),
                _ => {
                    // any: target_paths as patterns covering the trigger path,
                    // OR trigger path covering an affected file (literal).
                    let via_target = paths.iter().any(|tp| {
                        target_paths
                            .iter()
                            .any(|target| crate::db::path_matches(target, tp))
                    });
                    let via_affected = paths.iter().any(|tp| {
                        union
                            .iter()
                            .any(|f| crate::db::path_matches(&PathPattern(tp.clone()), f))
                    });
                    via_target || via_affected
                }
            };
            if fired {
                matched.push(format!("- {condition} (match.on={on}, paths={paths:?})"));
            }
        }

        if !matched.is_empty() && !user_approval_confirmed {
            return Err(OrchestratorError::HumanReviewTriggered {
                count: matched.len(),
                triggers: matched.join("\n"),
            });
        }
        Ok(())
    }

    pub fn start_revision(&self, round_id: Uuid) -> Result<ReviewRound, OrchestratorError> {
        let mut round = self
            .db
            .get_round(round_id)?
            .ok_or_else(|| OrchestratorError::RoundNotFound { id: round_id })?;

        round.iteration += 1;
        round.votes.clear();
        round.status = RoundStatus::Open;
        round.updated_at = Utc::now();
        self.db.update_round(&round)?;
        Ok(round)
    }

    // --- Write Permit Operations ---

    pub fn grant_permit(
        &self,
        rfc_id: Uuid,
        granted_to: PersonaId,
        target_paths: Vec<PathPattern>,
    ) -> Result<WritePermit, OrchestratorError> {
        let now = Utc::now();
        let permit = WritePermit {
            id: Uuid::new_v4(),
            rfc_id,
            granted_to,
            target_paths,
            status: PermitStatus::Active,
            granted_by: PersonaId::Ceo,
            granted_at: now,
            consumed_at: None,
        };

        self.db.create_permit(&permit)?;
        Ok(permit)
    }

    pub fn check_permit(
        &self,
        persona: PersonaId,
        path: &str,
    ) -> Result<Option<WritePermit>, OrchestratorError> {
        self.db.check_permit(persona, path)
    }

    /// Delete a single permit by id (targeted rollback). Delegates to
    /// [`OrchestratorDb::delete_permit`]. Used by the atomic
    /// `grant_write_permit` path (RFC 359f9162) to remove a permit whose
    /// seal failed, without touching other active permits.
    pub fn delete_permit(&self, id: Uuid) -> Result<(), OrchestratorError> {
        self.db.delete_permit(id)
    }

    /// Fetch a permit by id. Delegates to [`OrchestratorDb::get_permit`].
    /// Used by GARDE 3 (RFC 8bf78218) in `consume_write_permit` to obtain the
    /// permit's target_paths before checking the worktree.
    pub fn get_permit(&self, id: Uuid) -> Result<Option<WritePermit>, OrchestratorError> {
        self.db.get_permit(id)
    }

    /// Resolve an indexed artifact by id. Delegates to
    /// [`OrchestratorDb::get_artifact`]. Used by GARDE 4 (RFC 8bf78218) in
    /// `grant_write_permit` to verify the RFC kind/status before granting.
    pub fn get_artifact(
        &self,
        id: &str,
    ) -> Result<Option<crate::types::IndexedArtifact>, OrchestratorError> {
        self.db.get_artifact(id)
    }

    /// Read `metadata.status` from the YAML artifact at `file_path` (relative
    /// to `root`, or absolute). Returns "draft" when the field is absent,
    /// mirroring the convention used by `set_rfc_implemented`. Used by GARDE 4
    /// (RFC 8bf78218) so the grant path reads the source-of-truth status
    /// rather than an indexed column. Shares the read+parse approach with
    /// `set_rfc_implemented`.
    pub fn read_artifact_status(
        &self,
        root: &str,
        file_path: &str,
    ) -> Result<String, OrchestratorError> {
        let full_path = if file_path.starts_with('/') {
            file_path.to_string()
        } else {
            format!("{root}/{file_path}")
        };
        let content =
            std::fs::read_to_string(&full_path).map_err(|source| OrchestratorError::FileRead {
                path: full_path.clone(),
                source,
            })?;
        let parsed: serde_yaml::Value = serde_yaml::from_str(&content)?;
        let status = parsed
            .get("metadata")
            .and_then(|m| m.get("status"))
            .and_then(|s| s.as_str())
            .unwrap_or("draft")
            .to_string();
        Ok(status)
    }

    /// Idempotency lookup for the atomic grant (RFC 359f9162). Delegates
    /// to [`OrchestratorDb::find_permit_by_grant`].
    pub fn find_permit_by_grant(
        &self,
        rfc_id: Uuid,
        granted_to: &PersonaId,
        target_paths: &[PathPattern],
    ) -> Result<Option<WritePermit>, OrchestratorError> {
        self.db
            .find_permit_by_grant(rfc_id, granted_to, target_paths)
    }

    pub fn consume_permit(&self, permit_id: Uuid) -> Result<(), OrchestratorError> {
        self.db.consume_permit(permit_id)
    }

    /// List every permit currently in `active` status (mechanism 11, RFC
    /// a4ee8b6a). Passthrough to [`OrchestratorDb::list_active_permits`],
    /// used by the `reload_config` tool to refuse a hot-reload while a
    /// write-permit window is still open.
    pub fn list_active_permits(&self) -> Result<Vec<WritePermit>, OrchestratorError> {
        self.db.list_active_permits()
    }

    /// Opaque blob describing the current state of `write_permits`.
    /// Backs the MCP tool `snapshot_permits_state` used by the
    /// defense-in-depth hook to detect tampering.
    pub fn snapshot_permits(&self) -> Result<String, OrchestratorError> {
        self.db.snapshot_permits()
    }

    /// Restore `write_permits` to a previous snapshot. `None` wipes all
    /// rows (nuclear). Backs the MCP tool `revert_permits_to_snapshot`.
    /// Returns the number of permits deleted.
    pub fn restore_permits_from_snapshot(
        &self,
        snapshot: Option<&str>,
    ) -> Result<usize, OrchestratorError> {
        self.db.restore_permits_from_snapshot(snapshot)
    }

    /// Database integrity check, used by the autorepair sequence at
    /// server boot (PILIER D). Returns true iff `PRAGMA integrity_check`
    /// returns exactly "ok".
    pub fn integrity_check(&self) -> Result<bool, OrchestratorError> {
        self.db.integrity_check()
    }

    /// Execute `PRAGMA wal_checkpoint(TRUNCATE)`. Used by the graceful
    /// shutdown sequence (PILIER C) to flush WAL frames before exit.
    pub fn checkpoint_truncate(&self) -> Result<(), OrchestratorError> {
        self.db.checkpoint_truncate()
    }

    /// List every permit (all statuses), ordered by id. Passthrough to
    /// [`OrchestratorDb::list_all_permits`] (RFC cde13417 A1.3). Backs the
    /// canonical seal export.
    pub fn list_all_permits(&self) -> Result<Vec<WritePermit>, OrchestratorError> {
        self.db.list_all_permits()
    }

    /// Insert a permit row verbatim. Passthrough to
    /// [`OrchestratorDb::insert_permit_row`] (RFC cde13417 A1.3). Used by
    /// [`Self::reseed_permits_from_seal`].
    pub fn insert_permit_row(&self, permit: &WritePermit) -> Result<(), OrchestratorError> {
        self.db.insert_permit_row(permit)
    }

    /// Revert a consumed permit back to `active`. Passthrough to
    /// [`OrchestratorDb::unconsume_permit`] (RFC cde13417 A1.3). Rollback of
    /// a consume whose seal commit failed (symmetric to the grant rollback).
    pub fn unconsume_permit(&self, id: Uuid) -> Result<(), OrchestratorError> {
        self.db.unconsume_permit(id)
    }

    /// Export the `write_permits` table as a canonical, deterministic JSON
    /// seal (RFC cde13417 A1.1) and write it atomically to
    /// `<root>/company/data/permits-seal.json`. Returns the path relative to
    /// `root`.
    ///
    /// Canonical form: `version` is fixed at 1; `permits` are sorted by id
    /// ascending (via [`OrchestratorDb::list_all_permits`]); each permit's
    /// keys are emitted in a fixed declaration order. Two identical table
    /// states therefore produce byte-identical JSON, so a re-seal with no
    /// change yields `NothingToCommit` at the git layer.
    ///
    /// Atomicity: the JSON is written to a sibling temp file then
    /// `std::fs::rename`d over the target, so a reader never observes a
    /// partial file.
    pub fn write_permits_seal(&self, root: &str) -> Result<String, OrchestratorError> {
        let permits = self.db.list_all_permits()?;
        let seal = SealFile {
            version: 1,
            permits: permits.iter().map(SealPermit::from).collect(),
        };
        // Pretty JSON with a trailing newline: stable, human-diffable in
        // git history, and byte-identical for identical table states.
        let mut json = serde_json::to_string_pretty(&seal)?;
        json.push('\n');

        let rel_path = format!("{}/{}", constants::DATA_DIR, constants::SEAL_FILENAME);
        let dir = format!("{root}/{}", constants::DATA_DIR);
        std::fs::create_dir_all(&dir)?;
        let final_path = format!("{root}/{rel_path}");
        // Unique temp sibling in the SAME directory (rename must be atomic,
        // i.e. same filesystem). PID + nanos avoid collisions across
        // concurrent boots/grants.
        let tmp_path = format!(
            "{dir}/.{}.tmp.{}.{}",
            constants::SEAL_FILENAME,
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        std::fs::write(&tmp_path, json.as_bytes())?;
        std::fs::rename(&tmp_path, &final_path)?;
        Ok(rel_path)
    }

    /// Reconstruct the `write_permits` table from a canonical seal JSON
    /// (RFC cde13417 A1.5, PILIER D extension). REFUSES when the table is
    /// not empty: reseed only ever reconstructs an empty store; a non-empty
    /// table is a live state (post-revert, post-rollback) that must never be
    /// overwritten. Returns the number of permits inserted.
    ///
    /// Caller contract: the JSON MUST come from HEAD (the tamper-evident
    /// anchor), never from the on-disk file. An invalid JSON surfaces a
    /// parse error the boot treats as a non-fatal warning.
    pub fn reseed_permits_from_seal(&self, seal_json: &str) -> Result<usize, OrchestratorError> {
        let existing = self.db.list_all_permits()?;
        if !existing.is_empty() {
            return Err(OrchestratorError::IntegrityFailure {
                details: format!(
                    "permit seal reseed refused: write_permits is not empty ({} row(s)); reseed only reconstructs an empty table (RFC cde13417 A1.5)",
                    existing.len()
                ),
            });
        }
        let seal: SealFile = serde_json::from_str(seal_json)?;
        let mut count = 0usize;
        for sp in &seal.permits {
            let permit = sp.to_permit()?;
            self.db.insert_permit_row(&permit)?;
            count += 1;
        }
        Ok(count)
    }

    /// Tear down the engine and return ownership of the inner DB
    /// connection. Used by the boot autorepair (PILIER D) when
    /// integrity_check fails: the caller drops the DB, removes the files,
    /// reopens fresh, and rebuilds.
    pub fn into_db_for_rebuild(self) -> OrchestratorDb {
        self.db
    }

    // --- Artifact Index Operations ---

    /// Index a single artifact file. Reads YAML, validates, extracts
    /// metadata, computes the dense embedding, and upserts the four
    /// index tables atomically.
    ///
    /// Rationale (RFC bdee1af4 proposition 7): the embedding step happens
    /// BEFORE the transaction opens, so a model failure (OOM, panic in
    /// the runtime, invalid input) leaves the existing artifact data
    /// untouched. If the embedding succeeds, all four tables are updated
    /// in a single transaction.
    pub fn index_artifact(
        &mut self,
        root: &str,
        file_path: &str,
        validator: &ArtifactValidator,
    ) -> Result<IndexedArtifact, OrchestratorError> {
        let full_path = format!("{root}/{file_path}");
        let content =
            std::fs::read_to_string(&full_path).map_err(|e| OrchestratorError::FileRead {
                path: file_path.to_string(),
                source: e,
            })?;

        // Validate
        let report = validator.validate_yaml_str(&content).map_err(|e| {
            OrchestratorError::ValidationFailed {
                id: file_path.to_string(),
                errors: e.to_string(),
            }
        })?;

        if !report.is_valid {
            return Err(OrchestratorError::ValidationFailed {
                id: file_path.to_string(),
                errors: report.errors.join("; "),
            });
        }

        // Parse YAML for metadata extraction
        let yaml: serde_json::Value = serde_yaml::from_str(&content)?;
        let artifact = extract_artifact_metadata(&yaml, file_path);
        let searchable = extract_searchable_content(&yaml);
        let relations = extract_relations(&yaml);

        // Embedding step. Failure here -> early return, no DB write.
        let embedding = self
            .require_embedder()?
            .embed_artifact_view(&yaml, &artifact.kind)?;

        // Extract structured filter columns. These mirror the YAML
        // metadata fields and feed into the SearchFilters WHERE clause.
        let author = yaml
            .pointer("/metadata/author")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let project = yaml
            .pointer("/metadata/project")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let created_at = yaml
            .pointer("/metadata/created_at")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        self.db.upsert_artifact_full(
            &artifact,
            &searchable,
            &embedding,
            &relations,
            author.as_deref(),
            project.as_deref(),
            created_at.as_deref(),
        )?;
        Ok(artifact)
    }

    /// Hybrid search entry point. Drives the lexical + semantic + fusion
    /// pipeline per the request mode and filters.
    pub fn search_hybrid(&self, req: SearchRequest) -> Result<SearchResponse, OrchestratorError> {
        let started = std::time::Instant::now();
        let mut trace = ExplainTrace {
            mode_applied: format!("{:?}", req.mode),
            ..Default::default()
        };

        // (1) Empty query handling
        let query_empty = req.query.trim().is_empty();
        let has_filter = req.filters.kinds.is_some()
            || req.filters.tags.is_some()
            || req.filters.id_prefix.is_some();

        if query_empty && !has_filter {
            return Err(OrchestratorError::ValidationFailed {
                id: "search".into(),
                errors: "empty query requires at least one filter".into(),
            });
        }
        if query_empty && has_filter {
            // List-mode: filtered scan ordered by created_at desc (recent
            // first), bypassing FTS and embed entirely.
            let summaries = self.list_with_filters(&req.filters, req.limit)?;
            trace.candidate_set_sizes_fused = summaries.len();
            return Ok(SearchResponse {
                results: summaries,
                explain: if req.explain { Some(trace) } else { None },
            });
        }

        // (2) Lexical path
        let fts_query =
            crate::query::sanitize_fts_query(&req.query, crate::query::QueryMode::Natural);
        let lex = match req.mode {
            SearchMode::Semantic => Vec::new(),
            _ => self
                .db
                .search_lexical(&fts_query, &req.filters, SEARCH_RETRIEVE_K)?,
        };
        trace.candidate_set_sizes_lexical = lex.len();

        // (3) Semantic path
        let sem = match req.mode {
            SearchMode::Lexical => Vec::new(),
            _ => {
                let embedder = self.require_embedder()?;
                // HyDE / rerank stubs — étape 13.
                if req.hyde {
                    return Err(OrchestratorError::AnthropicKeyMissing {
                        feature: "hyde (step 13 not yet wired)".into(),
                    });
                }
                let q_embedding = embedder.embed_text(&req.query)?;
                self.db
                    .search_semantic(&q_embedding, &req.filters, SEARCH_RETRIEVE_K)?
            }
        };
        trace.candidate_set_sizes_semantic = sem.len();

        // (4) Fusion
        let fused = match req.mode {
            SearchMode::Lexical => lex
                .iter()
                .map(|r| crate::fusion::FusedResult {
                    id: r.id.clone(),
                    score: 1.0 / (crate::fusion::DEFAULT_RRF_K + r.rank as f64),
                    lexical_rank: Some(r.rank),
                    semantic_rank: None,
                })
                .take(req.limit)
                .collect::<Vec<_>>(),
            SearchMode::Semantic => sem
                .iter()
                .map(|r| crate::fusion::FusedResult {
                    id: r.id.clone(),
                    score: 1.0 / (crate::fusion::DEFAULT_RRF_K + r.rank as f64),
                    lexical_rank: None,
                    semantic_rank: Some(r.rank),
                })
                .take(req.limit)
                .collect::<Vec<_>>(),
            SearchMode::Hybrid => {
                crate::fusion::rrf_fuse(&lex, &sem, crate::fusion::DEFAULT_RRF_K, req.limit)
            }
        };
        trace.candidate_set_sizes_fused = fused.len();

        // (5) Hydrate
        let mut results: Vec<ArtifactSummary> = Vec::with_capacity(fused.len());
        for r in &fused {
            if let Some(art) = self.db.get_artifact(&r.id)? {
                let tags: Vec<String> =
                    serde_json::from_str(&serde_json::to_string(&art.tags)?).unwrap_or_default();
                results.push(ArtifactSummary {
                    id: art.id,
                    kind: art.kind,
                    title: art.title,
                    description: art.description,
                    tags,
                });
            }
        }

        // (6) Rerank (stub — étape 13)
        if req.rerank {
            return Err(OrchestratorError::AnthropicKeyMissing {
                feature: "rerank (step 13 not yet wired)".into(),
            });
        }

        trace.latency_ms = started.elapsed().as_millis() as u64;

        Ok(SearchResponse {
            results,
            explain: if req.explain { Some(trace) } else { None },
        })
    }

    /// Filtered list mode (empty query + filters). Returns artifacts
    /// matching the filters ordered by created_at desc (recent first),
    /// or by indexed_at desc when created_at is NULL.
    fn list_with_filters(
        &self,
        filters: &crate::db::SearchFilters,
        limit: usize,
    ) -> Result<Vec<ArtifactSummary>, OrchestratorError> {
        self.db.list_with_filters(filters, limit)
    }

    /// Compute global indexation status.
    ///
    /// `watcher_alive` is provided by the caller (main.rs reads its
    /// `Arc<AtomicBool>`) — the engine has no direct view of the file
    /// watcher task. `queue_size` similarly: the watcher in this
    /// codebase has no intermediate mpsc channel, so callers can pass
    /// 0 with the understanding documented in the RFC proposition 8.
    pub fn index_status_global(
        &self,
        watcher_alive: bool,
        queue_size: usize,
    ) -> Result<IndexStatusGlobal, OrchestratorError> {
        let (a, f, v) = self.db.index_table_counts()?;
        let triplet_coherent = a == f && f == v;
        let last_indexed_at = self.db.last_indexed_at()?;
        Ok(IndexStatusGlobal {
            artifacts_count: a,
            fts_count: f,
            vec_count: v,
            triplet_coherent,
            last_indexed_at,
            file_watcher_alive: watcher_alive,
            pending_index_queue_size: queue_size,
        })
    }

    /// Compute per-path indexation status. `root` is the project root
    /// so we can stat the file mtime; `path` is the relative path of
    /// the YAML file (matching `artifacts.file_path`).
    pub fn index_status_per_path(
        &self,
        root: &str,
        path: &str,
    ) -> Result<IndexStatusPerPath, OrchestratorError> {
        let id_opt = self.db.artifact_id_by_path(path)?;
        let mut out = IndexStatusPerPath::default();

        if let Some(id) = id_opt
            && let Some((_fp, indexed_at, in_fts, in_vec)) = self.db.artifact_by_id_status(&id)?
        {
            out.indexed_at = Some(indexed_at);
            out.present_in_fts = in_fts;
            out.present_in_vec = in_vec;
        }

        // mtime via fs::metadata.
        let abs_path = format!("{root}/{path}");
        if let Ok(meta) = std::fs::metadata(&abs_path)
            && let Ok(mtime) = meta.modified()
            && let Ok(dur) = mtime.duration_since(std::time::UNIX_EPOCH)
        {
            // Convert to RFC3339 for symmetry with indexed_at.
            let secs = dur.as_secs() as i64;
            let nanos = dur.subsec_nanos();
            let dt = chrono::DateTime::<Utc>::from_timestamp(secs, nanos).unwrap_or_else(Utc::now);
            out.file_mtime = Some(dt.to_rfc3339());

            // Stale flag: file mtime strictly greater than indexed_at.
            if let Some(ix_at) = &out.indexed_at
                && let Ok(ix_dt) = chrono::DateTime::parse_from_rfc3339(ix_at)
            {
                out.stale = dt > ix_dt.with_timezone(&Utc);
            }
        }

        Ok(out)
    }

    /// Get full artifact content by ID. Reads the file from disk.
    pub fn get(&self, id: &str, root: &str) -> Result<String, OrchestratorError> {
        let artifact = self
            .db
            .get_artifact(id)?
            .ok_or_else(|| OrchestratorError::ArtifactNotFound { id: id.to_string() })?;

        let full_path = format!("{root}/{}", artifact.file_path);
        std::fs::read_to_string(&full_path).map_err(|e| OrchestratorError::FileRead {
            path: artifact.file_path,
            source: e,
        })
    }

    /// Get all relations for an artifact (bidirectional).
    pub fn related(&self, id: &str) -> Result<Vec<RelationLink>, OrchestratorError> {
        self.db.get_relations(id)
    }

    /// Reminders for related task-requests that are still open at the moment
    /// an RFC is marked implemented (RFC cde13417 A3, family M19). Resolves
    /// the RFC's relations in BOTH directions, keeps `kind == task-request`,
    /// reads each one's `spec.status` from its YAML source at read time
    /// (calcul-à-la-lecture, RFC a5f25718 — the artifacts table has no status
    /// column), and emits a reminder for any status outside {done, cancelled}.
    /// NON blocking by construction: the caller merely surfaces the strings;
    /// the LLM decides. Never fails on a single unreadable task-request YAML
    /// (that entry is silently skipped), so one orphan can't suppress the rest.
    pub fn open_task_request_reminders(&self, root: &str, rfc_id: Uuid) -> Vec<String> {
        let links = match self.db.get_relations(&rfc_id.to_string()) {
            Ok(l) => l,
            Err(_) => return Vec::new(),
        };
        let mut seen: Vec<String> = Vec::new();
        let mut out: Vec<String> = Vec::new();
        for link in links {
            if link.kind.as_deref() != Some("task-request") {
                continue;
            }
            if seen.contains(&link.id) {
                continue;
            }
            seen.push(link.id.clone());

            // Resolve the file path and read spec.status from the YAML.
            let artifact = match self.db.get_artifact(&link.id) {
                Ok(Some(a)) => a,
                _ => continue,
            };
            let full_path = format!("{root}/{}", artifact.file_path);
            let content = match std::fs::read_to_string(&full_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let parsed: serde_yaml::Value = match serde_yaml::from_str(&content) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let status = parsed
                .get("spec")
                .and_then(|s| s.get("status"))
                .and_then(|s| s.as_str())
                .unwrap_or("backlog")
                .to_string();
            if status == "done" || status == "cancelled" {
                continue;
            }
            let id8: String = link.id.chars().take(8).collect();
            let title = link
                .title
                .clone()
                .unwrap_or_else(|| "(untitled)".to_string());
            out.push(format!(
                "Related task-request {id8} '{title}' is still '{status}' — if this RFC completes it, set it to done (or cancelled); if other work remains, leave it open."
            ));
        }
        out
    }

    // --- Roadmap tools ---

    /// Resolve the [`SourceStatus`] of a roadmap item's referenced artifact.
    ///
    /// - `rfc`: look up the indexed artifact by id, read its YAML and parse
    ///   `metadata.status`. Not indexed / unreadable -> [`SourceStatus::None`].
    /// - `project`: stat `projects/<slug>/` under `root`.
    /// - `loose`: no source -> [`SourceStatus::None`] (callers never map it).
    ///
    /// Pure FS/DB lookup, never fails: a missing source degrades to `None` so
    /// a single orphan ref cannot poison the whole summary (design CAS NÉGATIF).
    fn resolve_source_status(&self, root: &str, item_ref: &RoadmapItemRef) -> SourceStatus {
        match item_ref {
            RoadmapItemRef::Rfc { id } => match self.db.get_artifact(id) {
                Ok(Some(a)) => {
                    let full_path = format!("{root}/{}", a.file_path);
                    match std::fs::read_to_string(&full_path) {
                        Ok(content) => match parse_rfc_metadata_status(&content) {
                            Some(status) => SourceStatus::Rfc(status),
                            None => SourceStatus::None,
                        },
                        Err(_) => SourceStatus::None,
                    }
                }
                _ => SourceStatus::None,
            },
            RoadmapItemRef::Project { project_slug } => {
                let dir = format!("{root}/projects/{project_slug}");
                SourceStatus::Project(Path::new(&dir).is_dir())
            }
            RoadmapItemRef::Loose { .. } => SourceStatus::None,
        }
    }

    /// List indexed roadmaps with light-weight per-roadmap counters.
    /// Filterable by `status` ("active"/"archived") and/or `domain`.
    ///
    /// Corrupt or unreadable roadmap YAMLs are skipped with a tracing warn,
    /// not propagated as errors — a single broken file must not poison the
    /// listing for the rest.
    ///
    /// Output is sorted (active first, then blocked_count desc, then title asc).
    pub fn list_roadmaps(
        &self,
        root: &str,
        status_filter: Option<&str>,
        domain_filter: Option<&str>,
    ) -> Result<Vec<RoadmapListEntry>, OrchestratorError> {
        let indexed = self.db.list_by_kind(ROADMAP_KIND)?;

        let mut entries: Vec<RoadmapListEntry> = Vec::new();
        for artifact in indexed {
            let full_path = format!("{root}/{}", artifact.file_path);
            let content = match std::fs::read_to_string(&full_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!(
                        "[companyos-orchestrator] WARN: skipping unreadable roadmap '{full_path}': {e}"
                    );
                    continue;
                }
            };
            let parsed = match roadmap_summary::parse_roadmap_yaml(&content) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!(
                        "[companyos-orchestrator] WARN: skipping unparseable roadmap '{full_path}': {e}"
                    );
                    continue;
                }
            };

            // Compute the effective status of each item (auto-sync at read
            // time) so the counters reflect the source artifacts, not the
            // possibly-stale YAML status.
            let items = &parsed.spec.items;
            let mut blocked_count = 0usize;
            let mut in_progress_count = 0usize;
            for item in items {
                let source = self.resolve_source_status(root, &item.ref_);
                let (eff, _drift) = effective_status(&item.ref_, &item.status, &source);
                match eff.as_str() {
                    "blocked" => blocked_count += 1,
                    "in_progress" => in_progress_count += 1,
                    _ => {}
                }
            }

            entries.push(RoadmapListEntry {
                id: parsed.metadata.id,
                title: parsed.metadata.title,
                domain: parsed.spec.domain,
                status: parsed.spec.status,
                items_count: items.len(),
                blocked_count,
                in_progress_count,
                file_path: artifact.file_path,
            });
        }

        // Apply filters
        if let Some(s) = status_filter {
            entries.retain(|e| e.status == s);
        }
        if let Some(d) = domain_filter {
            entries.retain(|e| e.domain == d);
        }

        // Sort: active first, then blocked_count desc, then title asc.
        entries.sort_by(|a, b| {
            let a_active = a.status == "active";
            let b_active = b.status == "active";
            b_active
                .cmp(&a_active)
                .then_with(|| b.blocked_count.cmp(&a.blocked_count))
                .then_with(|| a.title.cmp(&b.title))
        });

        Ok(entries)
    }

    /// Summarize a roadmap by id or domain. Returns a structured view with
    /// narrative + items grouped by both timeframe and status, with blocked
    /// items highlighted.
    pub fn summarize_roadmap(
        &self,
        root: &str,
        selector: RoadmapSelector,
    ) -> Result<RoadmapSummary, OrchestratorError> {
        // 1. Resolve the indexed artifact for the requested roadmap.
        let artifact = match selector {
            RoadmapSelector::ById(id) => match self.db.get_artifact(&id)? {
                None => {
                    return Err(OrchestratorError::RoadmapNotFound {
                        selector: format!("id={id}"),
                    });
                }
                Some(a) if a.kind != ROADMAP_KIND => {
                    return Err(OrchestratorError::RoadmapKindMismatch {
                        id,
                        actual_kind: a.kind,
                    });
                }
                Some(a) => a,
            },
            RoadmapSelector::ByDomain(domain) => {
                let indexed = self.db.list_by_kind(ROADMAP_KIND)?;
                let mut candidates: Vec<IndexedArtifact> = Vec::new();
                for a in indexed {
                    let full_path = format!("{root}/{}", a.file_path);
                    let content = match std::fs::read_to_string(&full_path) {
                        Ok(c) => c,
                        Err(e) => {
                            eprintln!(
                                "[companyos-orchestrator] WARN: skipping unreadable roadmap during domain resolution '{full_path}': {e}"
                            );
                            continue;
                        }
                    };
                    let parsed = match roadmap_summary::parse_roadmap_yaml(&content) {
                        Ok(p) => p,
                        Err(e) => {
                            eprintln!(
                                "[companyos-orchestrator] WARN: skipping unparseable roadmap during domain resolution '{full_path}': {e}"
                            );
                            continue;
                        }
                    };
                    if parsed.spec.domain == domain && parsed.spec.status == "active" {
                        candidates.push(a);
                    }
                }
                match candidates.len() {
                    0 => {
                        return Err(OrchestratorError::RoadmapNotFound {
                            selector: format!("domain={domain}"),
                        });
                    }
                    1 => candidates.into_iter().next().unwrap(),
                    _ => {
                        return Err(OrchestratorError::RoadmapAmbiguousDomain {
                            domain,
                            candidate_ids: candidates.into_iter().map(|a| a.id).collect(),
                        });
                    }
                }
            }
        };

        // 2. Read and parse the resolved roadmap YAML.
        let full_path = format!("{root}/{}", artifact.file_path);
        let content = std::fs::read_to_string(&full_path).map_err(|e| {
            OrchestratorError::RoadmapParseFailed {
                path: full_path.clone(),
                reason: format!("io error: {e}"),
            }
        })?;
        let parsed = roadmap_summary::parse_roadmap_yaml(&content).map_err(|e| {
            OrchestratorError::RoadmapParseFailed {
                path: full_path.clone(),
                reason: format!("yaml error: {e}"),
            }
        })?;

        // 3. Auto-sync: compute the effective status of each item at read
        // time from its source artifact, collect drift warnings, and build
        // aggregations on the COMPUTED status (not the possibly-stale YAML).
        let items = parsed.spec.items;
        let mut effective_items: Vec<RoadmapItem> = Vec::with_capacity(items.len());
        let mut drift_warnings: Vec<DriftWarning> = Vec::new();

        for item in &items {
            let source = self.resolve_source_status(root, &item.ref_);
            let (eff_status, drift) = effective_status(&item.ref_, &item.status, &source);

            // Drift warnings exclude blocked (short-circuit -> drift=false)
            // and loose (no source -> drift=false) by construction.
            if drift {
                let cause = match (&item.ref_, &source) {
                    (RoadmapItemRef::Rfc { .. }, SourceStatus::None) => Some(
                        "source RFC introuvable (non indexée, illisible ou orpheline)".to_string(),
                    ),
                    _ => None,
                };
                drift_warnings.push(DriftWarning {
                    key: item.key.clone(),
                    yaml_status: item.status.clone(),
                    mapped_status: eff_status.clone(),
                    ref_: item.ref_.clone(),
                    cause,
                });
            }

            // Clone the item with its status substituted by the effective one
            // so the existing pure helpers aggregate on the computed status.
            let mut eff_item = item.clone();
            eff_item.status = eff_status;
            effective_items.push(eff_item);
        }

        let blocked_items: Vec<_> = effective_items
            .iter()
            .filter(|i| i.status == "blocked")
            .cloned()
            .collect();

        let mut by_status = roadmap_summary::count_by_status(&effective_items);
        // Timeframe stays on the YAML timeframe (NOT auto-synced: it is a PM
        // reading dimension). effective_items preserves the original timeframe.
        let mut by_timeframe = roadmap_summary::count_by_timeframe(&effective_items);
        let mut items_by_status =
            roadmap_summary::group_items_by(&effective_items, |i| i.status.clone());
        let mut items_by_timeframe =
            roadmap_summary::group_items_by(&effective_items, |i| i.timeframe.clone());

        // Zero-init canonical keys so the output JSON shape is stable.
        for s in ALL_STATUSES {
            by_status.entry((*s).into()).or_insert(0);
            items_by_status.entry((*s).into()).or_default();
        }
        for t in ALL_TIMEFRAMES {
            by_timeframe.entry((*t).into()).or_insert(0);
            items_by_timeframe.entry((*t).into()).or_default();
        }

        Ok(RoadmapSummary {
            roadmap: RoadmapHeader {
                id: parsed.metadata.id,
                title: parsed.metadata.title,
                domain: parsed.spec.domain,
                status: parsed.spec.status,
                narrative: parsed.spec.narrative,
            },
            summary: RoadmapCounters {
                items_total: effective_items.len(),
                blocked_count: blocked_items.len(),
                by_status,
                by_timeframe,
            },
            blocked_items,
            items_by_timeframe,
            items_by_status,
            drift_warnings,
        })
    }

    /// Reindex all artifacts under the given directory.
    ///
    /// Mutating because each `index_artifact` opens a transaction on the
    /// underlying `Connection` (`&mut Connection::transaction`). Called
    /// from the boot path (PILIER D) and on file watcher Artifacts events.
    pub fn reindex_all(
        &mut self,
        root: &str,
        validator: &ArtifactValidator,
    ) -> Result<ReindexOutcome, OrchestratorError> {
        self.db.delete_all_artifacts()?;
        // After delete_all, persist the current model_version so a future
        // boot can detect a mismatch and trigger a wipe + reindex.
        self.db
            .set_model_version(&crate::embedding::model_version())?;

        let mut count = 0;
        let scan_roots = [constants::ARTIFACTS_DIR, constants::PROJECTS_DIR];

        // Collect first to avoid holding a closure borrow while we mutate
        // self via index_artifact.
        let mut yaml_paths: Vec<String> = Vec::new();
        for dir_name in &scan_roots {
            let scan_dir = format!("{root}/{dir_name}");
            walk_yaml_files(Path::new(&scan_dir), &mut |path| {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string();
                let rel = rel.trim_start_matches('/').to_string();
                yaml_paths.push(rel);
            });
        }

        // Keep the successfully-indexed artifacts to run the per-artifact
        // warning families (10c supersession, 16 author-produces) AFTER the
        // whole corpus is indexed, so cross-artifact lookups see the full set.
        let mut indexed: Vec<IndexedArtifact> = Vec::new();
        for rel in yaml_paths {
            if let Ok(artifact) = self.index_artifact(root, &rel, validator) {
                count += 1;
                indexed.push(artifact);
            }
        }

        // Mechanism 21 (RFC 0197fbe5): collect the non-blocking warnings.
        // Bulk families only: supersession asymmetry (10c) and author↔produces
        // (16) per artifact, plus dangling related links (17b) via one global
        // SQL pass (order-insensitive). NOT the capitalization reminders (19c):
        // those are single-file only (moment-of-resolution), spamming history
        // in bulk.
        let mut warnings = Vec::new();
        for artifact in &indexed {
            warnings.extend(self.supersession_warnings(root, artifact));
            warnings.extend(self.author_produces_warnings(root, artifact));
        }
        for (source_id, target_id) in self.db.dangling_related_links()? {
            warnings.push(format!(
                "related[].id {target_id} introuvable dans l'index — lien pendant \
                 (source {source_id}) : id erroné (typo ?) ou artifact supprimé/jamais créé"
            ));
        }

        Ok(ReindexOutcome { count, warnings })
    }
}

/// Pure function: compute consensus from a round's state.
pub fn compute_consensus(round: &ReviewRound) -> ConsensusResult {
    let required = &round.required_reviewers;

    let all_voted = required
        .iter()
        .all(|r| round.votes.iter().any(|v| v.reviewer == *r));

    if !all_voted {
        return ConsensusResult::WaitingForVotes;
    }

    let has_request_changes = round
        .votes
        .iter()
        .any(|v| required.contains(&v.reviewer) && v.verdict == ReviewVerdict::RequestChanges);

    if !has_request_changes {
        return ConsensusResult::ConsensusReached;
    }

    if round.iteration >= round.max_iterations {
        ConsensusResult::EscalationNeeded
    } else {
        ConsensusResult::RevisionRequired
    }
}

// --- Helpers ---

fn extract_artifact_metadata(yaml: &serde_json::Value, file_path: &str) -> IndexedArtifact {
    let metadata = yaml.get("metadata").cloned().unwrap_or_default();
    let id = metadata
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let kind = yaml
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let title = metadata
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let description = metadata
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let tags = metadata
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    IndexedArtifact {
        id,
        kind,
        title,
        description,
        tags,
        file_path: file_path.to_string(),
        indexed_at: Utc::now().to_rfc3339(),
    }
}

fn extract_searchable_content(yaml: &serde_json::Value) -> String {
    let spec = yaml.get("spec").cloned().unwrap_or_default();
    collect_strings(&spec).join(" ")
}

fn collect_strings(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(arr) => arr.iter().flat_map(collect_strings).collect(),
        serde_json::Value::Object(map) => map.values().flat_map(collect_strings).collect(),
        _ => vec![],
    }
}

/// Add a `{id, kind, relationship}` entry to `metadata.related` by targeted
/// textual edit (mechanism 10, RFC a4ee8b6a), preserving the file's existing
/// formatting and block scalars (same family as `update_rfc_status_in_file`).
/// Idempotent: if an entry with the same `id` AND `relationship` already
/// exists, the content is returned unchanged. If no `related:` block exists,
/// one is created right after the `metadata:` line's first field block.
fn add_related_link(content: &str, target_id: &str, kind: &str, relationship: &str) -> String {
    // Idempotence: bail if the (id, relationship) pair is already present.
    // Cheap structural check via a re-parse of the existing relations.
    if let Ok(yaml) = serde_yaml::from_str::<serde_json::Value>(content) {
        let already = extract_relations(&yaml)
            .iter()
            .any(|r| r.target_id == target_id && r.relationship == relationship);
        if already {
            return content.to_string();
        }
    }

    let entry =
        format!("    - id: {target_id}\n      kind: {kind}\n      relationship: {relationship}\n");

    // Case A: a `  related:` line exists under metadata → insert the entry
    // right after it (as the first list item).
    if let Some(pos) = content.find("\n  related:") {
        // Find the end of the `  related:` line.
        let line_start = pos + 1; // skip the leading '\n'
        if let Some(nl) = content[line_start..].find('\n') {
            let insert_at = line_start + nl + 1;
            let mut out = String::with_capacity(content.len() + entry.len());
            out.push_str(&content[..insert_at]);
            out.push_str(&entry);
            out.push_str(&content[insert_at..]);
            return out;
        }
    }

    // Case B: no related block. Create one just before `\nspec:` (top-level),
    // which every artifact has. Insert `  related:` + the entry.
    let block = format!("  related:\n{entry}");
    if let Some(pos) = content.find("\nspec:") {
        let insert_at = pos + 1; // before "spec:"
        let mut out = String::with_capacity(content.len() + block.len());
        out.push_str(&content[..insert_at]);
        out.push_str(&block);
        out.push_str(&content[insert_at..]);
        return out;
    }

    // Fallback: append at the end (should not happen for a valid artifact).
    format!("{content}\n{block}")
}

/// Insert a dated `[SUPERSEDED-BY <8chars> le <date>]` marker at the HEAD of
/// `metadata.description` (mechanism 10, RFC a4ee8b6a; lesson 9b2c1951: the
/// retrieval surface must carry the obsolescence). Handles both a block
/// scalar (`description: >`) and an inline/quoted string. Idempotent: if a
/// `[SUPERSEDED-BY` marker is already present anywhere in the description
/// region, the content is returned unchanged.
fn insert_supersede_marker(content: &str, new_id: &str, date: &str, note: Option<&str>) -> String {
    // Idempotence: never stack markers.
    if content.contains("[SUPERSEDED-BY") {
        return content.to_string();
    }

    let short = &new_id[..new_id.len().min(8)];
    let note_suffix = match note {
        Some(n) if !n.trim().is_empty() => format!(" {}", n.trim()),
        _ => String::new(),
    };
    let marker_text = format!("[SUPERSEDED-BY {short} le {date}]{note_suffix}");

    // Locate the `  description:` line under metadata.
    let Some(desc_pos) = content.find("\n  description:") else {
        return content.to_string();
    };
    let line_start = desc_pos + 1;
    let Some(nl_off) = content[line_start..].find('\n') else {
        return content.to_string();
    };
    let desc_line = &content[line_start..line_start + nl_off];
    let after_line = line_start + nl_off + 1;

    // Block scalar form: `  description: >` or `  description: |` (with
    // optional chomping indicator). The content follows on indented lines.
    let value = desc_line
        .split_once("description:")
        .map(|x| x.1)
        .unwrap_or("")
        .trim();
    let is_block_scalar = value.starts_with('>') || value.starts_with('|');

    if is_block_scalar {
        // Determine the indentation of the first content line and insert the
        // marker as a new first content line with the same indentation.
        let rest = &content[after_line..];
        let indent = rest
            .lines()
            .next()
            .map(|l| &l[..l.len() - l.trim_start().len()])
            .filter(|ind| !ind.is_empty())
            .unwrap_or("    ")
            .to_string();
        let mut out = String::with_capacity(content.len() + marker_text.len() + indent.len() + 1);
        out.push_str(&content[..after_line]);
        out.push_str(&indent);
        out.push_str(&marker_text);
        out.push('\n');
        out.push_str(&content[after_line..]);
        out
    } else {
        // Inline string form: `  description: "..."` or `  description: ...`.
        // Rewrite the value with the marker prepended, preserving quoting.
        let (prefix, body) =
            desc_line.split_at(desc_line.find("description:").unwrap() + "description:".len());
        let body_trim = body.trim();
        let new_line =
            if body_trim.starts_with('"') && body_trim.ends_with('"') && body_trim.len() >= 2 {
                let inner = &body_trim[1..body_trim.len() - 1];
                format!("{prefix} \"{marker_text} {inner}\"")
            } else if body_trim.is_empty() {
                format!("{prefix} \"{marker_text}\"")
            } else {
                format!("{prefix} \"{marker_text} {body_trim}\"")
            };
        let mut out = String::with_capacity(content.len() + marker_text.len() + 8);
        out.push_str(&content[..line_start]);
        out.push_str(&new_line);
        out.push('\n');
        out.push_str(&content[after_line..]);
        out
    }
}

fn extract_relations(yaml: &serde_json::Value) -> Vec<ParsedRelation> {
    yaml.pointer("/metadata/related")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let target_id = item.get("id")?.as_str()?.to_string();
                    let relationship = item.get("relationship")?.as_str()?.to_string();
                    Some(ParsedRelation {
                        target_id,
                        relationship,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn walk_yaml_files(dir: &Path, callback: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_yaml_files(&path, callback);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && (ext == constants::EXT_YML || ext == constants::EXT_YAML)
        {
            callback(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AUTHOR_PRODUCES_CUTOFF;
    use crate::error::OrchestratorError;
    use crate::types::{ParsedRelation, PathPattern, PermitStatus, ReviewRound, RoundStatus};
    use crate::{
        ArtifactPath, ConsensusResult, Finding, IndexedArtifact, OrchestratorDb,
        OrchestratorEngine, ReviewVerdict,
    };
    use chrono::Utc;
    use companyos_config::{ArtifactKind, PersonaId};
    use companyos_validation::ArtifactValidator;
    use uuid::Uuid;

    fn setup_engine() -> OrchestratorEngine {
        let db = OrchestratorDb::open_in_memory().expect("open in-memory db");
        db.migrate().expect("migrate");
        OrchestratorEngine::new_without_embedder(db, 3)
    }

    // --- Permit seal / reseed tests (RFC cde13417 A1.1 + A1.5) ---

    struct SealTempRoot {
        path: std::path::PathBuf,
    }
    impl SealTempRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("seal-eng-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
        fn root(&self) -> &str {
            self.path.to_str().unwrap()
        }
    }
    impl Drop for SealTempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    // NOMINAL: write_permits_seal exports a canonical JSON reflecting the
    // table (one active + one consumed), sorted by id, with a version field.
    #[test]
    fn test_write_permits_seal_canonical() {
        let tmp = SealTempRoot::new();
        let engine = setup_engine();
        let p1 = engine
            .grant_permit(
                Uuid::new_v4(),
                PersonaId::Implementer,
                vec![PathPattern("a".into())],
            )
            .unwrap();
        let _p2 = engine
            .grant_permit(
                Uuid::new_v4(),
                PersonaId::Implementer,
                vec![PathPattern("b".into())],
            )
            .unwrap();
        engine.consume_permit(p1.id).unwrap();

        let rel = engine.write_permits_seal(tmp.root()).unwrap();
        assert_eq!(rel, "company/data/permits-seal.json");
        let content = std::fs::read_to_string(format!("{}/{rel}", tmp.root())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["version"], 1);
        let permits = v["permits"].as_array().unwrap();
        assert_eq!(permits.len(), 2);
        // Sorted by id ascending → byte-stable.
        let ids: Vec<&str> = permits.iter().map(|p| p["id"].as_str().unwrap()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "permits must be sorted by id");
    }

    // EDGE: an empty table exports {"version":1,"permits":[]}.
    #[test]
    fn test_write_permits_seal_empty() {
        let tmp = SealTempRoot::new();
        let engine = setup_engine();
        let rel = engine.write_permits_seal(tmp.root()).unwrap();
        let content = std::fs::read_to_string(format!("{}/{rel}", tmp.root())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["version"], 1);
        assert!(v["permits"].as_array().unwrap().is_empty());
    }

    // NOMINAL: reseed reconstructs an empty table from a seal JSON,
    // preserving status and timestamps; count matches.
    #[test]
    fn test_reseed_permits_from_seal_nominal() {
        let tmp = SealTempRoot::new();
        let src = setup_engine();
        let p = src
            .grant_permit(
                Uuid::new_v4(),
                PersonaId::Implementer,
                vec![PathPattern("x".into())],
            )
            .unwrap();
        src.consume_permit(p.id).unwrap();
        let rel = src.write_permits_seal(tmp.root()).unwrap();
        let json = std::fs::read_to_string(format!("{}/{rel}", tmp.root())).unwrap();

        // Fresh (empty) engine reseeds from the JSON.
        let dst = setup_engine();
        let n = dst.reseed_permits_from_seal(&json).unwrap();
        assert_eq!(n, 1);
        let fetched = dst.get_permit(p.id).unwrap().unwrap();
        assert_eq!(fetched.status, PermitStatus::Consumed);
    }

    // NÉGATIF: reseed refuses a non-empty table (never clobbers live state).
    #[test]
    fn test_reseed_refuses_non_empty_table() {
        let engine = setup_engine();
        engine
            .grant_permit(
                Uuid::new_v4(),
                PersonaId::Implementer,
                vec![PathPattern("x".into())],
            )
            .unwrap();
        let json = r#"{"version":1,"permits":[]}"#;
        let res = engine.reseed_permits_from_seal(json);
        assert!(
            matches!(res, Err(OrchestratorError::IntegrityFailure { .. })),
            "reseed on a non-empty table must be refused"
        );
    }

    // NÉGATIF: invalid JSON surfaces a parse error (boot treats as warning).
    #[test]
    fn test_reseed_invalid_json_errors() {
        let engine = setup_engine();
        let res = engine.reseed_permits_from_seal("not json");
        assert!(res.is_err());
    }

    // EDGE: reseed an empty seal on an empty table inserts nothing, Ok(0).
    #[test]
    fn test_reseed_empty_seal_ok_zero() {
        let engine = setup_engine();
        let n = engine
            .reseed_permits_from_seal(r#"{"version":1,"permits":[]}"#)
            .unwrap();
        assert_eq!(n, 0);
    }

    // --- open_task_request_reminders tests (RFC cde13417 A3) ---

    // Seed an RFC that relates to a task-request, index the task-request with
    // the given status written to its YAML on disk. Returns the rfc id.
    fn seed_rfc_with_task_request(
        engine: &mut OrchestratorEngine,
        root: &str,
        tr_status: &str,
    ) -> Uuid {
        let rfc_id = Uuid::new_v4();
        let tr_id = Uuid::new_v4();
        // task-request YAML on disk with spec.status.
        let tr_rel = format!("projects/company-os/task-requests/{tr_id}.yml");
        let tr_abs = format!("{root}/{tr_rel}");
        std::fs::create_dir_all(format!("{root}/projects/company-os/task-requests")).unwrap();
        let tr_yaml = format!(
            "api_version: companyos/v1\nkind: task-request\nmetadata:\n  id: {tr_id}\n  title: TR fixture\n  author: pm\n  created_at: \"2026-08-25\"\nspec:\n  status: {tr_status}\n"
        );
        std::fs::write(&tr_abs, tr_yaml).unwrap();

        // Index the task-request (kind + file_path resolvable by the reminder).
        let tr_art = crate::IndexedArtifact {
            id: tr_id.to_string(),
            kind: "task-request".into(),
            title: "TR fixture".into(),
            description: String::new(),
            tags: vec![],
            file_path: tr_rel,
            indexed_at: Utc::now().to_rfc3339(),
        };
        engine
            .db
            .upsert_artifact(&tr_art, "", &test_dummy_embedding(), &[])
            .unwrap();

        // Index the RFC with an outgoing relation to the task-request.
        let rfc_art = crate::IndexedArtifact {
            id: rfc_id.to_string(),
            kind: "rfc".into(),
            title: "RFC fixture".into(),
            description: String::new(),
            tags: vec![],
            file_path: format!("company/rfcs/{rfc_id}.yml"),
            indexed_at: Utc::now().to_rfc3339(),
        };
        engine
            .db
            .upsert_artifact(
                &rfc_art,
                "",
                &test_dummy_embedding(),
                &[ParsedRelation {
                    target_id: tr_id.to_string(),
                    relationship: "input".into(),
                }],
            )
            .unwrap();
        rfc_id
    }

    // NOMINAL: a related task-request still in backlog yields one reminder.
    #[test]
    fn test_reminders_open_task_request() {
        let tmp = SealTempRoot::new();
        let mut engine = setup_engine();
        let rfc_id = seed_rfc_with_task_request(&mut engine, tmp.root(), "backlog");
        let reminders = engine.open_task_request_reminders(tmp.root(), rfc_id);
        assert_eq!(reminders.len(), 1, "backlog TR must produce a reminder");
        assert!(reminders[0].contains("still 'backlog'"));
    }

    // NÉGATIF: a done/cancelled task-request produces no reminder.
    #[test]
    fn test_reminders_done_task_request_silent() {
        let tmp = SealTempRoot::new();
        let mut engine = setup_engine();
        let rfc_id = seed_rfc_with_task_request(&mut engine, tmp.root(), "done");
        assert!(
            engine
                .open_task_request_reminders(tmp.root(), rfc_id)
                .is_empty()
        );
    }

    // EDGE: an RFC with no related task-request yields an empty vec.
    #[test]
    fn test_reminders_no_related_empty() {
        let tmp = SealTempRoot::new();
        let engine = setup_engine();
        let reminders = engine.open_task_request_reminders(tmp.root(), Uuid::new_v4());
        assert!(reminders.is_empty());
    }

    // --- Consensus tests ---

    #[test]
    fn test_consensus_waiting_no_votes() {
        let engine = setup_engine();
        let round = engine
            .initiate_review_round(
                ArtifactPath("artifacts/rfc/test.yml".into()),
                ArtifactKind::Rfc,
                PersonaId::Pm,
                vec![PersonaId::Architect, PersonaId::Ceo],
            )
            .unwrap();

        let result = engine.check_consensus(round.id).unwrap();
        assert_eq!(result, ConsensusResult::WaitingForVotes);
    }

    #[test]
    fn test_consensus_waiting_partial() {
        let engine = setup_engine();
        let round = engine
            .initiate_review_round(
                ArtifactPath("artifacts/rfc/test.yml".into()),
                ArtifactKind::Rfc,
                PersonaId::Pm,
                vec![PersonaId::Architect, PersonaId::Ceo],
            )
            .unwrap();

        engine
            .submit_vote(
                round.id,
                PersonaId::Architect,
                ReviewVerdict::Approve,
                vec![],
                None,
            )
            .unwrap();

        let result = engine.check_consensus(round.id).unwrap();
        assert_eq!(result, ConsensusResult::WaitingForVotes);
    }

    #[test]
    fn test_consensus_all_approve() {
        let engine = setup_engine();
        let round = engine
            .initiate_review_round(
                ArtifactPath("artifacts/rfc/test.yml".into()),
                ArtifactKind::Rfc,
                PersonaId::Pm,
                vec![PersonaId::Architect, PersonaId::Ceo],
            )
            .unwrap();

        engine
            .submit_vote(
                round.id,
                PersonaId::Architect,
                ReviewVerdict::Approve,
                vec![],
                None,
            )
            .unwrap();
        engine
            .submit_vote(
                round.id,
                PersonaId::Ceo,
                ReviewVerdict::Approve,
                vec![],
                None,
            )
            .unwrap();

        let result = engine.check_consensus(round.id).unwrap();
        assert_eq!(result, ConsensusResult::ConsensusReached);
    }

    #[test]
    fn test_consensus_request_changes_under_max() {
        let engine = setup_engine();
        let round = engine
            .initiate_review_round(
                ArtifactPath("artifacts/rfc/test.yml".into()),
                ArtifactKind::Rfc,
                PersonaId::Pm,
                vec![PersonaId::Architect],
            )
            .unwrap();

        assert_eq!(round.iteration, 1);

        engine
            .submit_vote(
                round.id,
                PersonaId::Architect,
                ReviewVerdict::RequestChanges,
                vec![Finding("needs work".into())],
                None,
            )
            .unwrap();

        let result = engine.check_consensus(round.id).unwrap();
        assert_eq!(result, ConsensusResult::RevisionRequired);
    }

    #[test]
    fn test_consensus_escalation_at_max() {
        let engine = setup_engine();
        let round = engine
            .initiate_review_round(
                ArtifactPath("artifacts/rfc/test.yml".into()),
                ArtifactKind::Rfc,
                PersonaId::Pm,
                vec![PersonaId::Architect],
            )
            .unwrap();

        // iteration 1 -> start_revision -> iteration 2
        let round = engine.start_revision(round.id).unwrap();
        assert_eq!(round.iteration, 2);

        // iteration 2 -> start_revision -> iteration 3 (max)
        let round = engine.start_revision(round.id).unwrap();
        assert_eq!(round.iteration, 3);

        engine
            .submit_vote(
                round.id,
                PersonaId::Architect,
                ReviewVerdict::RequestChanges,
                vec![Finding("still broken".into())],
                None,
            )
            .unwrap();

        let result = engine.check_consensus(round.id).unwrap();
        assert_eq!(result, ConsensusResult::EscalationNeeded);
    }

    // --- Vote edge-case tests ---

    #[test]
    fn test_vote_replaces_previous() {
        let engine = setup_engine();
        let round = engine
            .initiate_review_round(
                ArtifactPath("artifacts/rfc/test.yml".into()),
                ArtifactKind::Rfc,
                PersonaId::Pm,
                vec![PersonaId::Architect, PersonaId::Ceo],
            )
            .unwrap();

        engine
            .submit_vote(
                round.id,
                PersonaId::Architect,
                ReviewVerdict::RequestChanges,
                vec![Finding("issue".into())],
                None,
            )
            .unwrap();

        // Same reviewer votes again — should replace, not accumulate
        let updated = engine
            .submit_vote(
                round.id,
                PersonaId::Architect,
                ReviewVerdict::Approve,
                vec![],
                None,
            )
            .unwrap();

        assert_eq!(updated.votes.len(), 1);
        assert_eq!(updated.votes[0].verdict, ReviewVerdict::Approve);
    }

    #[test]
    fn test_vote_non_reviewer_rejected() {
        let engine = setup_engine();
        let round = engine
            .initiate_review_round(
                ArtifactPath("artifacts/rfc/test.yml".into()),
                ArtifactKind::Rfc,
                PersonaId::Pm,
                vec![PersonaId::Architect],
            )
            .unwrap();

        // Ceo is not in required_reviewers
        let result = engine.submit_vote(
            round.id,
            PersonaId::Ceo,
            ReviewVerdict::Approve,
            vec![],
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_vote_closed_round_rejected() {
        let engine = setup_engine();
        let round = engine
            .initiate_review_round(
                ArtifactPath("artifacts/rfc/test.yml".into()),
                ArtifactKind::Rfc,
                PersonaId::Pm,
                vec![PersonaId::Architect],
            )
            .unwrap();

        engine.close_round(round.id).unwrap();

        let result = engine.submit_vote(
            round.id,
            PersonaId::Architect,
            ReviewVerdict::Approve,
            vec![],
            None,
        );
        assert!(result.is_err());
    }

    // --- GARDE 1 (self_review_forbidden) + GARDE 2 (review_findings_honesty
    //     + notes) tests — RFC 8bf78218 ---

    #[test]
    fn test_initiate_rejects_author_in_reviewers() {
        // GARDE 1a: the author cannot be a required reviewer of their own artifact.
        let engine = setup_engine();
        let result = engine.initiate_review_round(
            ArtifactPath("artifacts/rfc/test.yml".into()),
            ArtifactKind::Rfc,
            PersonaId::Architect,
            vec![PersonaId::Architect, PersonaId::Ceo],
        );
        assert!(matches!(
            result,
            Err(OrchestratorError::SelfReviewForbidden { .. })
        ));
    }

    #[test]
    fn test_submit_vote_rejects_author() {
        // GARDE 1b: even on a pre-existing round, the author cannot vote on
        // their own artifact. We craft a round whose author is also listed in
        // required_reviewers by writing it directly to the DB (bypassing the
        // initiate guard) to prove the vote-time guard stands alone.
        let engine = setup_engine();
        let round = ReviewRound {
            id: Uuid::new_v4(),
            artifact_path: ArtifactPath("artifacts/rfc/test.yml".into()),
            artifact_kind: ArtifactKind::Rfc,
            author: PersonaId::Architect,
            required_reviewers: vec![PersonaId::Architect, PersonaId::Ceo],
            status: RoundStatus::Open,
            iteration: 1,
            max_iterations: 3,
            votes: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        engine.db.create_round(&round).unwrap();

        let result = engine.submit_vote(
            round.id,
            PersonaId::Architect,
            ReviewVerdict::Approve,
            vec![],
            None,
        );
        assert!(matches!(
            result,
            Err(OrchestratorError::SelfReviewForbidden { .. })
        ));
    }

    #[test]
    fn test_submit_vote_approve_with_findings_rejected() {
        // GARDE 2a: approve + non-empty findings is contradictory.
        let engine = setup_engine();
        let round = engine
            .initiate_review_round(
                ArtifactPath("artifacts/rfc/test.yml".into()),
                ArtifactKind::Rfc,
                PersonaId::Pm,
                vec![PersonaId::Architect],
            )
            .unwrap();

        let result = engine.submit_vote(
            round.id,
            PersonaId::Architect,
            ReviewVerdict::Approve,
            vec![Finding("a corrective remark".into())],
            None,
        );
        assert!(matches!(
            result,
            Err(OrchestratorError::ApproveWithFindings { count: 1 })
        ));
    }

    #[test]
    fn test_submit_vote_approve_with_notes_accepted() {
        // GARDE 2b: approve + empty findings + notes is accepted; notes stored.
        let engine = setup_engine();
        let round = engine
            .initiate_review_round(
                ArtifactPath("artifacts/rfc/test.yml".into()),
                ArtifactKind::Rfc,
                PersonaId::Pm,
                vec![PersonaId::Architect],
            )
            .unwrap();

        let updated = engine
            .submit_vote(
                round.id,
                PersonaId::Architect,
                ReviewVerdict::Approve,
                vec![],
                Some("non-corrective observation".into()),
            )
            .unwrap();
        assert_eq!(
            updated.votes[0].notes.as_deref(),
            Some("non-corrective observation")
        );
    }

    #[test]
    fn test_submit_vote_request_changes_with_findings_accepted() {
        // Nominal path preserved: request_changes + findings is accepted.
        let engine = setup_engine();
        let round = engine
            .initiate_review_round(
                ArtifactPath("artifacts/rfc/test.yml".into()),
                ArtifactKind::Rfc,
                PersonaId::Pm,
                vec![PersonaId::Architect],
            )
            .unwrap();

        let updated = engine
            .submit_vote(
                round.id,
                PersonaId::Architect,
                ReviewVerdict::RequestChanges,
                vec![Finding("must fix".into())],
                None,
            )
            .unwrap();
        assert_eq!(updated.votes[0].verdict, ReviewVerdict::RequestChanges);
        assert_eq!(updated.votes[0].findings.len(), 1);
    }

    #[test]
    fn test_submit_vote_request_changes_with_notes_accepted() {
        // notes are accepted with request_changes too.
        let engine = setup_engine();
        let round = engine
            .initiate_review_round(
                ArtifactPath("artifacts/rfc/test.yml".into()),
                ArtifactKind::Rfc,
                PersonaId::Pm,
                vec![PersonaId::Architect],
            )
            .unwrap();

        let updated = engine
            .submit_vote(
                round.id,
                PersonaId::Architect,
                ReviewVerdict::RequestChanges,
                vec![Finding("must fix".into())],
                Some("for the record".into()),
            )
            .unwrap();
        assert_eq!(updated.votes[0].notes.as_deref(), Some("for the record"));
    }

    // --- Roadmap tools tests ---

    use crate::roadmap_summary::RoadmapSelector;

    /// Self-cleaning temp root: a unique directory under std::env::temp_dir()
    /// removed on drop. Avoids pulling `tempfile` into orchestrator's deps.
    struct RoadmapTestRoot {
        path: std::path::PathBuf,
    }

    impl RoadmapTestRoot {
        fn new() -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "companyos-orchestrator-roadmap-test-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&p).expect("create temp root");
            std::fs::create_dir_all(p.join("company/roadmaps")).expect("create roadmaps dir");
            std::fs::create_dir_all(p.join("company/schemas")).expect("create schemas dir");
            RoadmapTestRoot { path: p }
        }

        fn root_str(&self) -> &str {
            self.path.to_str().expect("utf-8 root path")
        }

        /// Write a roadmap YAML fixture under company/roadmaps/<id>.yml.
        /// Items are passed as (key, ref_type, ref_value, timeframe, status).
        fn write_roadmap(
            &self,
            id: &str,
            title: &str,
            domain: &str,
            status: &str,
            items: &[(&str, &str, &str, &str, &str)],
        ) -> String {
            let items_yaml: String = items
                .iter()
                .map(|(key, rtype, rval, tf, st)| {
                    let ref_block = match *rtype {
                        "project" => format!("    ref:\n      type: project\n      project_slug: {rval}"),
                        "rfc" => format!("    ref:\n      type: rfc\n      id: {rval}"),
                        _ => format!(
                            "    ref:\n      type: loose\n      category: idea\n      label: \"{rval}\""
                        ),
                    };
                    format!(
                        "  - key: {key}\n    title: \"Item {key}\"\n{ref_block}\n    timeframe: {tf}\n    status: {st}\n"
                    )
                })
                .collect();

            let mut content = String::new();
            content.push_str("api_version: companyos/v1\n");
            content.push_str("kind: roadmap\n");
            content.push_str("metadata:\n");
            content.push_str(&format!("  id: {id}\n"));
            content.push_str(&format!("  title: \"{title}\"\n"));
            content.push_str("  author: pm\n");
            content.push_str("  created_at: \"2026-05-20\"\n");
            content.push_str("  description: \"fixture\"\n");
            content.push_str("  tags: [test]\n");
            content.push_str("spec:\n");
            content.push_str(&format!("  domain: {domain}\n"));
            content.push_str(&format!("  status: {status}\n"));
            content.push_str(&format!("  narrative: \"Narrative for {title}\"\n"));
            content.push_str("  items:\n");
            content.push_str(&items_yaml);
            let rel_path = format!("company/roadmaps/{id}.yml");
            let abs = self.path.join(&rel_path);
            std::fs::write(&abs, &content).expect("write roadmap");
            rel_path
        }

        /// Write a raw file (used for corrupt fixtures and non-roadmap kinds).
        fn write_raw(&self, rel_path: &str, content: &str) {
            let abs = self.path.join(rel_path);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&abs, content).expect("write raw");
        }
    }

    impl Drop for RoadmapTestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Dummy unit vector of the correct dimension for test-only direct
    /// inserts. Bypasses the embedder while keeping the SQL pipeline
    /// honest.
    fn test_dummy_embedding() -> Vec<f32> {
        let mut v = vec![0.0_f32; crate::embedding::EMBEDDING_DIM];
        v[0] = 1.0;
        v
    }

    /// Index a roadmap directly via db.upsert_artifact, bypassing schema
    /// validation. We want full control over the YAML (including malformed
    /// fixtures) without depending on a populated schema registry.
    fn index_roadmap_directly(
        engine: &mut OrchestratorEngine,
        root: &RoadmapTestRoot,
        id: &str,
        file_path: &str,
    ) {
        let artifact = crate::IndexedArtifact {
            id: id.into(),
            kind: "roadmap".into(),
            title: format!("title-{id}"),
            description: String::new(),
            tags: vec![],
            file_path: file_path.into(),
            indexed_at: chrono::Utc::now().to_rfc3339(),
        };
        let content = std::fs::read_to_string(root.path.join(file_path)).unwrap_or_default();
        engine
            .db
            .upsert_artifact(&artifact, &content, &test_dummy_embedding(), &[])
            .expect("upsert");
    }

    fn index_artifact_with_kind(
        engine: &mut OrchestratorEngine,
        id: &str,
        kind: &str,
        file_path: &str,
    ) {
        let artifact = crate::IndexedArtifact {
            id: id.into(),
            kind: kind.into(),
            title: format!("title-{id}"),
            description: String::new(),
            tags: vec![],
            file_path: file_path.into(),
            indexed_at: chrono::Utc::now().to_rfc3339(),
        };
        engine
            .db
            .upsert_artifact(&artifact, "", &test_dummy_embedding(), &[])
            .expect("upsert");
    }

    // --- list_roadmaps tests ---

    #[test]
    fn test_list_roadmaps_filters_by_status() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();

        let id_a = "11111111-1111-1111-1111-111111111111";
        let id_b = "22222222-2222-2222-2222-222222222222";
        let p_a = root.write_roadmap(id_a, "Active one", "dom-a", "active", &[]);
        let p_b = root.write_roadmap(id_b, "Archived one", "dom-b", "archived", &[]);
        index_roadmap_directly(&mut engine, &root, id_a, &p_a);
        index_roadmap_directly(&mut engine, &root, id_b, &p_b);

        let active = engine
            .list_roadmaps(root.root_str(), Some("active"), None)
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, id_a);

        let archived = engine
            .list_roadmaps(root.root_str(), Some("archived"), None)
            .unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].id, id_b);

        let all = engine.list_roadmaps(root.root_str(), None, None).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_list_roadmaps_filters_by_domain() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();

        let ids = [
            ("33333333-3333-3333-3333-333333333333", "dom-a"),
            ("44444444-4444-4444-4444-444444444444", "dom-a"),
            ("55555555-5555-5555-5555-555555555555", "dom-b"),
        ];
        for (i, (id, dom)) in ids.iter().enumerate() {
            let p = root.write_roadmap(id, &format!("R{i}"), dom, "active", &[]);
            index_roadmap_directly(&mut engine, &root, id, &p);
        }

        let dom_a = engine
            .list_roadmaps(root.root_str(), None, Some("dom-a"))
            .unwrap();
        assert_eq!(dom_a.len(), 2);
        assert!(dom_a.iter().all(|e| e.domain == "dom-a"));

        let dom_b = engine
            .list_roadmaps(root.root_str(), None, Some("dom-b"))
            .unwrap();
        assert_eq!(dom_b.len(), 1);
    }

    #[test]
    fn test_list_roadmaps_sorts_active_first_then_blocked_desc() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();

        let id_a = "aaaaaaaa-1111-1111-1111-111111111111";
        let id_b = "bbbbbbbb-2222-2222-2222-222222222222";
        let id_c = "cccccccc-3333-3333-3333-333333333333";

        // A: active, 0 blocked, title "Alpha"
        let p_a = root.write_roadmap(id_a, "Alpha", "d", "active", &[]);
        // B: active, 2 blocked, title "Beta"
        let p_b = root.write_roadmap(
            id_b,
            "Beta",
            "d",
            "active",
            &[
                ("b1", "loose", "x", "present", "blocked"),
                ("b2", "loose", "x", "present", "blocked"),
                ("b3", "loose", "x", "past", "done"),
            ],
        );
        // C: archived, 5 blocked, title "Gamma"
        let p_c = root.write_roadmap(
            id_c,
            "Gamma",
            "d",
            "archived",
            &[
                ("c1", "loose", "x", "past", "blocked"),
                ("c2", "loose", "x", "past", "blocked"),
                ("c3", "loose", "x", "past", "blocked"),
                ("c4", "loose", "x", "past", "blocked"),
                ("c5", "loose", "x", "past", "blocked"),
            ],
        );
        index_roadmap_directly(&mut engine, &root, id_a, &p_a);
        index_roadmap_directly(&mut engine, &root, id_b, &p_b);
        index_roadmap_directly(&mut engine, &root, id_c, &p_c);

        let entries = engine.list_roadmaps(root.root_str(), None, None).unwrap();
        assert_eq!(entries.len(), 3);
        // Expected order: B (active, 2 blocked), A (active, 0 blocked), C (archived)
        assert_eq!(
            entries[0].id, id_b,
            "B should be first (active + most blocked)"
        );
        assert_eq!(
            entries[1].id, id_a,
            "A should be second (active, 0 blocked)"
        );
        assert_eq!(entries[2].id, id_c, "C should be last (archived)");
    }

    #[test]
    fn test_list_roadmaps_skips_corrupt_yaml_with_warning() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();

        let id_good = "66666666-6666-6666-6666-666666666666";
        let id_bad = "77777777-7777-7777-7777-777777777777";

        let p_good = root.write_roadmap(id_good, "Good", "d", "active", &[]);
        // Malformed YAML: missing required fields
        let p_bad = format!("company/roadmaps/{id_bad}.yml");
        root.write_raw(
            &p_bad,
            "api_version: companyos/v1\nkind: roadmap\nbroken: :\n  - x: [",
        );
        index_roadmap_directly(&mut engine, &root, id_good, &p_good);
        index_roadmap_directly(&mut engine, &root, id_bad, &p_bad);

        let entries = engine.list_roadmaps(root.root_str(), None, None).unwrap();
        assert_eq!(entries.len(), 1, "corrupt roadmap should be skipped");
        assert_eq!(entries[0].id, id_good);
    }

    // --- summarize_roadmap tests ---

    #[test]
    fn test_summarize_by_id_basic_grouping() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();

        let id = "88888888-8888-8888-8888-888888888888";
        let p = root.write_roadmap(
            id,
            "Mixed",
            "d",
            "active",
            &[
                ("i1", "loose", "x", "past", "done"),
                ("i2", "loose", "x", "past", "done"),
                ("i3", "loose", "x", "present", "in_progress"),
                ("i4", "loose", "x", "future", "blocked"),
            ],
        );
        index_roadmap_directly(&mut engine, &root, id, &p);

        let summary = engine
            .summarize_roadmap(root.root_str(), RoadmapSelector::ById(id.into()))
            .expect("summarize ok");

        assert_eq!(summary.summary.items_total, 4);
        assert_eq!(summary.summary.blocked_count, 1);

        // by_status canonical keys present, including zero counts
        assert_eq!(summary.summary.by_status.get("done"), Some(&2));
        assert_eq!(summary.summary.by_status.get("in_progress"), Some(&1));
        assert_eq!(summary.summary.by_status.get("blocked"), Some(&1));
        assert_eq!(summary.summary.by_status.get("planned"), Some(&0));
        assert_eq!(summary.summary.by_status.get("cancelled"), Some(&0));

        // by_timeframe
        assert_eq!(summary.summary.by_timeframe.get("past"), Some(&2));
        assert_eq!(summary.summary.by_timeframe.get("present"), Some(&1));
        assert_eq!(summary.summary.by_timeframe.get("future"), Some(&1));

        // grouped items
        assert_eq!(summary.items_by_timeframe.get("past").unwrap().len(), 2);
        assert_eq!(summary.items_by_status.get("done").unwrap().len(), 2);
        assert_eq!(summary.blocked_items.len(), 1);
        assert_eq!(summary.blocked_items[0].key, "i4");
    }

    #[test]
    fn test_summarize_by_domain_picks_unique_active() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();

        let id_active = "99999999-9999-9999-9999-999999999999";
        let id_arch = "aaaaaaaa-0000-0000-0000-000000000000";
        let p_a = root.write_roadmap(id_active, "Active", "shared-dom", "active", &[]);
        let p_b = root.write_roadmap(id_arch, "Archived", "shared-dom", "archived", &[]);
        index_roadmap_directly(&mut engine, &root, id_active, &p_a);
        index_roadmap_directly(&mut engine, &root, id_arch, &p_b);

        let summary = engine
            .summarize_roadmap(
                root.root_str(),
                RoadmapSelector::ByDomain("shared-dom".into()),
            )
            .expect("should pick the active one");
        assert_eq!(summary.roadmap.id, id_active);
    }

    #[test]
    fn test_summarize_by_domain_ambiguous_returns_error() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();

        let id1 = "bbbbbbbb-0000-0000-0000-000000000001";
        let id2 = "bbbbbbbb-0000-0000-0000-000000000002";
        let p1 = root.write_roadmap(id1, "First", "ambig", "active", &[]);
        let p2 = root.write_roadmap(id2, "Second", "ambig", "active", &[]);
        index_roadmap_directly(&mut engine, &root, id1, &p1);
        index_roadmap_directly(&mut engine, &root, id2, &p2);

        let res =
            engine.summarize_roadmap(root.root_str(), RoadmapSelector::ByDomain("ambig".into()));
        match res {
            Err(OrchestratorError::RoadmapAmbiguousDomain {
                domain,
                candidate_ids,
            }) => {
                assert_eq!(domain, "ambig");
                assert_eq!(candidate_ids.len(), 2);
            }
            other => panic!("expected RoadmapAmbiguousDomain, got {other:?}"),
        }
    }

    #[test]
    fn test_summarize_blocked_items_appear_in_blocked_list_and_in_by_status() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();

        let id = "cccccccc-0000-0000-0000-000000000003";
        let p = root.write_roadmap(
            id,
            "Blockers",
            "d",
            "active",
            &[
                ("b1", "loose", "x", "present", "blocked"),
                ("b2", "loose", "x", "future", "blocked"),
                ("d1", "loose", "x", "past", "done"),
            ],
        );
        index_roadmap_directly(&mut engine, &root, id, &p);

        let summary = engine
            .summarize_roadmap(root.root_str(), RoadmapSelector::ById(id.into()))
            .expect("summarize ok");

        assert_eq!(summary.blocked_items.len(), 2);
        let by_status_blocked = summary.items_by_status.get("blocked").unwrap();
        assert_eq!(by_status_blocked.len(), 2);

        let blocked_keys: Vec<_> = summary.blocked_items.iter().map(|i| &i.key).collect();
        let by_status_keys: Vec<_> = by_status_blocked.iter().map(|i| &i.key).collect();
        for k in &blocked_keys {
            assert!(by_status_keys.contains(k));
        }

        assert_eq!(summary.items_by_status.get("done").unwrap().len(), 1);
    }

    #[test]
    fn test_summarize_not_found_returns_typed_error() {
        let root = RoadmapTestRoot::new();
        let engine = setup_engine();

        // ById on empty DB
        let by_id =
            engine.summarize_roadmap(root.root_str(), RoadmapSelector::ById("absent-id".into()));
        assert!(matches!(
            by_id,
            Err(OrchestratorError::RoadmapNotFound { .. })
        ));

        // ByDomain on empty DB
        let by_dom = engine.summarize_roadmap(
            root.root_str(),
            RoadmapSelector::ByDomain("absent-dom".into()),
        );
        assert!(matches!(
            by_dom,
            Err(OrchestratorError::RoadmapNotFound { .. })
        ));
    }

    #[test]
    fn test_summarize_id_points_to_non_roadmap_kind_returns_error() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();

        let rfc_id = "dddddddd-0000-0000-0000-000000000004";
        // Index an artifact with kind="rfc"
        index_artifact_with_kind(&mut engine, rfc_id, "rfc", "company/rfcs/x.yml");

        let res = engine.summarize_roadmap(root.root_str(), RoadmapSelector::ById(rfc_id.into()));
        match res {
            Err(OrchestratorError::RoadmapKindMismatch { id, actual_kind }) => {
                assert_eq!(id, rfc_id);
                assert_eq!(actual_kind, "rfc");
            }
            other => panic!("expected RoadmapKindMismatch, got {other:?}"),
        }
    }

    // --- set_rfc_implemented tests (RFC 1c0f2570) ---

    /// Write a minimal valid RFC YAML fixture under company/rfcs/<id>.yml.
    /// If `implemented_at` is Some, the field is emitted in metadata (used to
    /// assert preservation in the idempotent case).
    ///
    /// CONSTRAINT (lesson f3fc4a5d): built with push_str + literal \n +
    /// explicit indentation — NEVER Rust backslash line-continuation, which
    /// collapses YAML indentation.
    fn write_rfc(
        root: &RoadmapTestRoot,
        id: &str,
        status: &str,
        implemented_at: Option<&str>,
    ) -> String {
        let mut content = String::new();
        content.push_str("api_version: companyos/v1\n");
        content.push_str("kind: rfc\n");
        content.push_str("metadata:\n");
        content.push_str(&format!("  id: {id}\n"));
        content.push_str("  title: \"Test RFC\"\n");
        content.push_str("  author: architect\n");
        content.push_str(&format!("  status: {status}\n"));
        if let Some(ts) = implemented_at {
            content.push_str(&format!("  implemented_at: \"{ts}\"\n"));
        }
        content.push_str("  created_at: \"2026-06-04\"\n");
        content.push_str("spec:\n");
        content.push_str("  motivation: \"why\"\n");
        let rel_path = format!("company/rfcs/{id}.yml");
        root.write_raw(&rel_path, &content);
        rel_path
    }

    /// Index an RFC fixture so set_rfc_implemented can resolve its file_path.
    fn index_rfc(engine: &mut OrchestratorEngine, id: &str, file_path: &str) {
        index_artifact_with_kind(engine, id, "rfc", file_path);
    }

    // NOMINAL

    #[test]
    fn test_set_implemented_from_approved_succeeds() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let id = "a0000000-0000-4000-8000-000000000001";
        let path = write_rfc(&root, id, "approved", None);
        index_rfc(&mut engine, id, &path);

        let uuid = Uuid::parse_str(id).unwrap();
        let outcome = engine
            .set_rfc_implemented(uuid, root.root_str())
            .expect("transition should succeed");

        assert_eq!(outcome.previous_status, "approved");
        assert_eq!(outcome.new_status, "implemented");
        assert!(!outcome.already_implemented);
        assert!(!outcome.implemented_at.is_empty());

        // Re-read the YAML to confirm the write.
        let written = std::fs::read_to_string(root.path.join(&path)).expect("read back");
        assert!(written.contains("status: implemented"));
        assert!(written.contains("implemented_at:"));
    }

    #[test]
    fn test_set_implemented_reflects_new_status_after_write() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let id = "a0000000-0000-4000-8000-000000000002";
        let path = write_rfc(&root, id, "approved", None);
        index_rfc(&mut engine, id, &path);

        let uuid = Uuid::parse_str(id).unwrap();
        engine.set_rfc_implemented(uuid, root.root_str()).unwrap();

        let written = std::fs::read_to_string(root.path.join(&path)).expect("read back");
        let parsed: serde_yaml::Value = serde_yaml::from_str(&written).unwrap();
        let status = parsed
            .get("metadata")
            .and_then(|m| m.get("status"))
            .and_then(|s| s.as_str())
            .unwrap();
        assert_eq!(status, "implemented");
    }

    // NEGATIVE

    #[test]
    fn test_set_implemented_rfc_not_found() {
        let root = RoadmapTestRoot::new();
        let engine = setup_engine();
        let uuid = Uuid::parse_str("b0000000-0000-4000-8000-000000000001").unwrap();
        let res = engine.set_rfc_implemented(uuid, root.root_str());
        match res {
            Err(OrchestratorError::ArtifactNotFound { id }) => {
                assert_eq!(id, uuid.to_string());
            }
            other => panic!("expected ArtifactNotFound, got {other:?}"),
        }
    }

    #[test]
    fn test_set_implemented_from_draft_refused() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let id = "b0000000-0000-4000-8000-000000000002";
        let path = write_rfc(&root, id, "draft", None);
        index_rfc(&mut engine, id, &path);

        let uuid = Uuid::parse_str(id).unwrap();
        match engine.set_rfc_implemented(uuid, root.root_str()) {
            Err(OrchestratorError::ValidationFailed { errors, .. }) => {
                assert!(
                    errors.contains("draft"),
                    "message should name 'draft': {errors}"
                );
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn test_set_implemented_from_review_refused() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let id = "b0000000-0000-4000-8000-000000000003";
        let path = write_rfc(&root, id, "review", None);
        index_rfc(&mut engine, id, &path);

        let uuid = Uuid::parse_str(id).unwrap();
        match engine.set_rfc_implemented(uuid, root.root_str()) {
            Err(OrchestratorError::ValidationFailed { errors, .. }) => {
                assert!(
                    errors.contains("review"),
                    "message should name 'review': {errors}"
                );
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn test_set_implemented_from_rejected_refused() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let id = "b0000000-0000-4000-8000-000000000004";
        let path = write_rfc(&root, id, "rejected", None);
        index_rfc(&mut engine, id, &path);

        let uuid = Uuid::parse_str(id).unwrap();
        match engine.set_rfc_implemented(uuid, root.root_str()) {
            Err(OrchestratorError::ValidationFailed { errors, .. }) => {
                assert!(
                    errors.contains("rejected"),
                    "message should name 'rejected': {errors}"
                );
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn test_set_implemented_on_non_rfc_kind_refused() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let id = "b0000000-0000-4000-8000-000000000005";
        // Index as a design-doc but place a file so the path check is reachable.
        let path = format!("company/rfcs/{id}.yml");
        root.write_raw(&path, "kind: design-doc\n");
        index_artifact_with_kind(&mut engine, id, "design-doc", &path);

        let uuid = Uuid::parse_str(id).unwrap();
        match engine.set_rfc_implemented(uuid, root.root_str()) {
            Err(OrchestratorError::ValidationFailed { errors, .. }) => {
                assert!(
                    errors.contains("design-doc"),
                    "message should name actual kind: {errors}"
                );
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    // EDGE

    #[test]
    fn test_set_implemented_already_implemented_is_idempotent() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let id = "c0000000-0000-4000-8000-000000000001";
        let original_ts = "2026-06-01T10:00:00+00:00";
        let path = write_rfc(&root, id, "implemented", Some(original_ts));
        index_rfc(&mut engine, id, &path);

        let uuid = Uuid::parse_str(id).unwrap();
        let outcome = engine
            .set_rfc_implemented(uuid, root.root_str())
            .expect("idempotent success");

        assert!(outcome.already_implemented);
        assert_eq!(outcome.previous_status, "implemented");
        assert_eq!(outcome.implemented_at, original_ts);

        // Decision CEO 1: original implemented_at preserved, NO rewrite.
        let written = std::fs::read_to_string(root.path.join(&path)).expect("read back");
        assert!(
            written.contains(original_ts),
            "original implemented_at must be preserved: {written}"
        );
    }

    #[test]
    fn test_set_implemented_corrupt_yaml() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let id = "c0000000-0000-4000-8000-000000000002";
        let path = format!("company/rfcs/{id}.yml");
        // Unparsable YAML (unterminated flow mapping).
        root.write_raw(&path, "metadata: {status: approved\n  : : :\n");
        index_rfc(&mut engine, id, &path);

        let uuid = Uuid::parse_str(id).unwrap();
        match engine.set_rfc_implemented(uuid, root.root_str()) {
            Err(OrchestratorError::Yaml(_)) | Err(OrchestratorError::FileRead { .. }) => {}
            other => panic!("expected Yaml/FileRead error, got {other:?}"),
        }
    }

    #[test]
    fn test_set_implemented_twice_second_is_idempotent() {
        // The real concurrency serialization is guaranteed by the tokio Mutex
        // on the MCP side (engine.lock().await), not by the sync engine. Here
        // we assert the sequential equivalent: calling twice on the same id
        // yields a real transition then an idempotent success.
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let id = "c0000000-0000-4000-8000-000000000003";
        let path = write_rfc(&root, id, "approved", None);
        index_rfc(&mut engine, id, &path);

        let uuid = Uuid::parse_str(id).unwrap();
        let first = engine.set_rfc_implemented(uuid, root.root_str()).unwrap();
        assert!(!first.already_implemented);

        let second = engine.set_rfc_implemented(uuid, root.root_str()).unwrap();
        assert!(second.already_implemented);
        assert_eq!(second.previous_status, "implemented");
    }

    // NOTE: malformed id is rejected at the MCP deserializer level
    // (RfcSetImplementedParams.id: Uuid + with="String"), so no engine-level
    // test is possible — the method always receives a valid Uuid.

    // --- Auto-sync integration tests (RFC a5f25718) ---

    /// RFC source = implemented -> item mapped to "done" even when the YAML
    /// roadmap status is stale ("planned"). A drift warning is emitted.
    #[test]
    fn test_autosync_rfc_implemented_maps_to_done() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();

        let rfc_id = "e0000000-0000-4000-8000-000000000001";
        let rfc_path = write_rfc(
            &root,
            rfc_id,
            "implemented",
            Some("2026-06-01T00:00:00+00:00"),
        );
        index_rfc(&mut engine, rfc_id, &rfc_path);

        let rm_id = "f0000000-0000-4000-8000-000000000001";
        let p = root.write_roadmap(
            rm_id,
            "RFC implemented",
            "autosync",
            "active",
            &[("a", "rfc", rfc_id, "future", "planned")],
        );
        index_roadmap_directly(&mut engine, &root, rm_id, &p);

        let summary = engine
            .summarize_roadmap(root.root_str(), RoadmapSelector::ById(rm_id.into()))
            .expect("summarize ok");

        assert_eq!(summary.items_by_status.get("done").unwrap().len(), 1);
        assert_eq!(summary.items_by_status.get("planned").unwrap().len(), 0);
        assert_eq!(summary.drift_warnings.len(), 1);
        let w = &summary.drift_warnings[0];
        assert_eq!(w.key, "a");
        assert_eq!(w.yaml_status, "planned");
        assert_eq!(w.mapped_status, "done");
        assert!(w.cause.is_none());
    }

    /// RFC source = approved -> item mapped to "in_progress".
    #[test]
    fn test_autosync_rfc_approved_maps_to_in_progress() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();

        let rfc_id = "e0000000-0000-4000-8000-000000000002";
        let rfc_path = write_rfc(&root, rfc_id, "approved", None);
        index_rfc(&mut engine, rfc_id, &rfc_path);

        let rm_id = "f0000000-0000-4000-8000-000000000002";
        let p = root.write_roadmap(
            rm_id,
            "RFC approved",
            "autosync",
            "active",
            &[("a", "rfc", rfc_id, "present", "in_progress")],
        );
        index_roadmap_directly(&mut engine, &root, rm_id, &p);

        let summary = engine
            .summarize_roadmap(root.root_str(), RoadmapSelector::ById(rm_id.into()))
            .expect("summarize ok");

        assert_eq!(summary.items_by_status.get("in_progress").unwrap().len(), 1);
        // YAML already matched the computed status -> no drift.
        assert_eq!(summary.drift_warnings.len(), 0);
    }

    /// AC4 invariance: a YAML status=blocked item stays blocked regardless of
    /// the source (here implemented), and emits NO drift warning.
    #[test]
    fn test_autosync_blocked_short_circuits() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();

        let rfc_id = "e0000000-0000-4000-8000-000000000003";
        let rfc_path = write_rfc(
            &root,
            rfc_id,
            "implemented",
            Some("2026-06-01T00:00:00+00:00"),
        );
        index_rfc(&mut engine, rfc_id, &rfc_path);

        let rm_id = "f0000000-0000-4000-8000-000000000003";
        let p = root.write_roadmap(
            rm_id,
            "Blocked wins",
            "autosync",
            "active",
            &[("a", "rfc", rfc_id, "present", "blocked")],
        );
        index_roadmap_directly(&mut engine, &root, rm_id, &p);

        let summary = engine
            .summarize_roadmap(root.root_str(), RoadmapSelector::ById(rm_id.into()))
            .expect("summarize ok");

        assert_eq!(summary.blocked_items.len(), 1);
        assert_eq!(summary.items_by_status.get("blocked").unwrap().len(), 1);
        assert_eq!(summary.items_by_status.get("done").unwrap().len(), 0);
        assert_eq!(
            summary.drift_warnings.len(),
            0,
            "blocked never drifts (AC4)"
        );
    }

    /// AC6: an RFC ref whose id is NOT indexed -> fallback "planned" + drift
    /// warning with a cause. summarize must still succeed (non blocking).
    #[test]
    fn test_autosync_rfc_source_missing_falls_back_planned() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();

        let rm_id = "f0000000-0000-4000-8000-000000000004";
        let orphan_rfc = "e9999999-0000-4000-8000-000000000099";
        let p = root.write_roadmap(
            rm_id,
            "Orphan ref",
            "autosync",
            "active",
            &[("a", "rfc", orphan_rfc, "present", "done")],
        );
        index_roadmap_directly(&mut engine, &root, rm_id, &p);

        let summary = engine
            .summarize_roadmap(root.root_str(), RoadmapSelector::ById(rm_id.into()))
            .expect("summarize must succeed despite orphan ref");

        assert_eq!(summary.items_by_status.get("planned").unwrap().len(), 1);
        assert_eq!(summary.drift_warnings.len(), 1);
        let w = &summary.drift_warnings[0];
        assert_eq!(w.mapped_status, "planned");
        assert!(
            w.cause.as_deref().unwrap_or("").contains("introuvable"),
            "cause must mark the missing source: {:?}",
            w.cause
        );
    }

    /// Project ref whose `projects/<slug>/` exists -> "in_progress".
    #[test]
    fn test_autosync_project_dir_present_in_progress() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();

        // Create projects/my-proj/ under the test root.
        std::fs::create_dir_all(root.path.join("projects/my-proj")).expect("mkdir project");

        let rm_id = "f0000000-0000-4000-8000-000000000005";
        let p = root.write_roadmap(
            rm_id,
            "Project present",
            "autosync",
            "active",
            &[("a", "project", "my-proj", "present", "planned")],
        );
        index_roadmap_directly(&mut engine, &root, rm_id, &p);

        let summary = engine
            .summarize_roadmap(root.root_str(), RoadmapSelector::ById(rm_id.into()))
            .expect("summarize ok");

        assert_eq!(summary.items_by_status.get("in_progress").unwrap().len(), 1);
        // YAML was "planned", computed "in_progress" -> drift.
        assert_eq!(summary.drift_warnings.len(), 1);
        assert!(summary.drift_warnings[0].cause.is_none());
    }

    /// Project ref whose directory is absent -> "planned".
    #[test]
    fn test_autosync_project_dir_absent_planned() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();

        let rm_id = "f0000000-0000-4000-8000-000000000006";
        let p = root.write_roadmap(
            rm_id,
            "Project absent",
            "autosync",
            "active",
            &[("a", "project", "ghost-proj", "future", "planned")],
        );
        index_roadmap_directly(&mut engine, &root, rm_id, &p);

        let summary = engine
            .summarize_roadmap(root.root_str(), RoadmapSelector::ById(rm_id.into()))
            .expect("summarize ok");

        assert_eq!(summary.items_by_status.get("planned").unwrap().len(), 1);
        // YAML "planned" == computed "planned" -> no drift.
        assert_eq!(summary.drift_warnings.len(), 0);
    }

    /// Loose ref: status respected verbatim, never a drift warning.
    #[test]
    fn test_autosync_loose_status_respected() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();

        let rm_id = "f0000000-0000-4000-8000-000000000007";
        let p = root.write_roadmap(
            rm_id,
            "Loose item",
            "autosync",
            "active",
            &[("a", "loose", "an idea", "future", "in_progress")],
        );
        index_roadmap_directly(&mut engine, &root, rm_id, &p);

        let summary = engine
            .summarize_roadmap(root.root_str(), RoadmapSelector::ById(rm_id.into()))
            .expect("summarize ok");

        assert_eq!(summary.items_by_status.get("in_progress").unwrap().len(), 1);
        assert_eq!(summary.drift_warnings.len(), 0, "loose never drifts");
    }

    /// AC5/AC7: a manual YAML edit (status=done) on an item whose RFC is only
    /// approved -> displayed in_progress (computed wins) + drift warning.
    #[test]
    fn test_drift_warning_on_manual_yaml_edit() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();

        let rfc_id = "e0000000-0000-4000-8000-000000000008";
        let rfc_path = write_rfc(&root, rfc_id, "approved", None);
        index_rfc(&mut engine, rfc_id, &rfc_path);

        let rm_id = "f0000000-0000-4000-8000-000000000008";
        let p = root.write_roadmap(
            rm_id,
            "Manual edit",
            "autosync",
            "active",
            &[("a", "rfc", rfc_id, "past", "done")],
        );
        index_roadmap_directly(&mut engine, &root, rm_id, &p);

        let summary = engine
            .summarize_roadmap(root.root_str(), RoadmapSelector::ById(rm_id.into()))
            .expect("summarize ok");

        // Computed wins: in_progress, not done.
        assert_eq!(summary.items_by_status.get("in_progress").unwrap().len(), 1);
        assert_eq!(summary.items_by_status.get("done").unwrap().len(), 0);
        assert_eq!(summary.drift_warnings.len(), 1);
        let w = &summary.drift_warnings[0];
        assert_eq!(w.yaml_status, "done");
        assert_eq!(w.mapped_status, "in_progress");
    }

    /// list_roadmaps counters also reflect the computed status: an RFC=approved
    /// item with YAML "planned" must count as in_progress, not planned.
    #[test]
    fn test_autosync_list_roadmaps_counters_use_computed_status() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();

        let rfc_id = "e0000000-0000-4000-8000-000000000009";
        let rfc_path = write_rfc(&root, rfc_id, "approved", None);
        index_rfc(&mut engine, rfc_id, &rfc_path);

        let rm_id = "f0000000-0000-4000-8000-000000000009";
        let p = root.write_roadmap(
            rm_id,
            "List counters",
            "autosync-list",
            "active",
            &[("a", "rfc", rfc_id, "future", "planned")],
        );
        index_roadmap_directly(&mut engine, &root, rm_id, &p);

        let entries = engine
            .list_roadmaps(root.root_str(), None, Some("autosync-list"))
            .expect("list ok");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].in_progress_count, 1,
            "computed status drives the counter"
        );
    }

    // ============================================================
    // Mechanism 10: supersede_artifact  (RFC a4ee8b6a, lot 2)
    // ============================================================

    fn workspace_schemas_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("company/schemas")
    }

    fn test_validator() -> ArtifactValidator {
        let registry = companyos_validation::SchemaRegistry::load(workspace_schemas_dir())
            .expect("load schemas");
        ArtifactValidator::new(registry)
    }

    /// Write a minimal valid lesson-learned fixture under company/lessons/,
    /// optionally with a `related` block. Returns the relative path.
    fn write_lesson(
        root: &RoadmapTestRoot,
        id: &str,
        description: &str,
        related: Option<&str>,
    ) -> String {
        let mut c = String::new();
        c.push_str("api_version: companyos/v1\n");
        c.push_str("kind: lesson-learned\n");
        c.push_str("metadata:\n");
        c.push_str(&format!("  id: {id}\n"));
        c.push_str(&format!("  title: \"Lesson {id}\"\n"));
        c.push_str("  author: implementer\n");
        c.push_str("  created_at: \"2026-07-03\"\n");
        c.push_str(&format!("  description: >\n    {description}\n"));
        c.push_str("  tags:\n    - test\n");
        if let Some(r) = related {
            c.push_str(r);
        }
        c.push_str("spec:\n");
        c.push_str("  context: \"ctx\"\n");
        c.push_str("  insight: \"ins\"\n");
        c.push_str("  recommendation: \"rec\"\n");
        let rel = format!("company/lessons/{id}.yml");
        std::fs::create_dir_all(root.path.join("company/lessons")).ok();
        root.write_raw(&rel, &c);
        rel
    }

    // NOMINAL: two lessons superseded, both files updated, re-read confirms.
    #[test]
    fn test_supersede_nominal_two_lessons() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let validator = test_validator();
        let old = "c0000000-0000-4000-8000-000000000001";
        let new = "c0000000-0000-4000-8000-000000000002";
        let old_path = write_lesson(&root, old, "old lesson body", None);
        let new_path = write_lesson(&root, new, "new lesson body", None);
        index_artifact_with_kind(&mut engine, old, "lesson-learned", &old_path);
        index_artifact_with_kind(&mut engine, new, "lesson-learned", &new_path);

        let outcome = engine
            .supersede_artifact(
                old,
                new,
                Some("partie X reste valide"),
                root.root_str(),
                &validator,
            )
            .expect("supersede ok");
        assert!(outcome.old_changed && outcome.new_changed);
        assert!(!outcome.idempotent);

        let old_yaml = std::fs::read_to_string(root.path.join(&old_path)).unwrap();
        let new_yaml = std::fs::read_to_string(root.path.join(&new_path)).unwrap();
        assert!(old_yaml.contains("[SUPERSEDED-BY"), "old carries marker");
        assert!(old_yaml.contains("partie X reste valide"), "note preserved");
        assert!(old_yaml.contains("superseded-by"), "old carries link");
        assert!(new_yaml.contains("supersedes"), "new carries link");
        // Both still valid against schema.
        assert!(validator.validate_yaml_str(&old_yaml).unwrap().is_valid);
        assert!(validator.validate_yaml_str(&new_yaml).unwrap().is_valid);
    }

    // NEGATIVE: unknown id.
    #[test]
    fn test_supersede_unknown_id() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let validator = test_validator();
        let new = "c0000000-0000-4000-8000-000000000012";
        let new_path = write_lesson(&root, new, "n", None);
        index_artifact_with_kind(&mut engine, new, "lesson-learned", &new_path);
        let res = engine.supersede_artifact(
            "c0000000-0000-4000-8000-0000000000ff",
            new,
            None,
            root.root_str(),
            &validator,
        );
        assert!(matches!(
            res,
            Err(OrchestratorError::ArtifactNotFound { .. })
        ));
    }

    // NEGATIVE: self-supersession.
    #[test]
    fn test_supersede_self_refused() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let validator = test_validator();
        let id = "c0000000-0000-4000-8000-000000000021";
        let p = write_lesson(&root, id, "x", None);
        index_artifact_with_kind(&mut engine, id, "lesson-learned", &p);
        let res = engine.supersede_artifact(id, id, None, root.root_str(), &validator);
        assert!(matches!(
            res,
            Err(OrchestratorError::SelfSupersession { .. })
        ));
    }

    // NEGATIVE: target in a protected zone (persona under company/personas/).
    #[test]
    fn test_supersede_protected_zone_refused() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let validator = test_validator();
        // Copy the real protected-zones config so is_protected resolves.
        std::fs::create_dir_all(root.path.join("company/config")).ok();
        let zones = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("company/config/protected-zones.json");
        std::fs::copy(
            &zones,
            root.path.join("company/config/protected-zones.json"),
        )
        .ok();
        companyos_config::protected_zones::reload();

        let old = "c0000000-0000-4000-8000-000000000031";
        let new = "c0000000-0000-4000-8000-000000000032";
        let old_path = write_lesson(&root, old, "o", None);
        // new_path lives under company/personas/ (protected).
        std::fs::create_dir_all(root.path.join("company/personas")).ok();
        let new_path = "company/personas/c0000000.yml".to_string();
        root.write_raw(&new_path, "kind: persona\n");
        index_artifact_with_kind(&mut engine, old, "lesson-learned", &old_path);
        index_artifact_with_kind(&mut engine, new, "persona", &new_path);

        let res = engine.supersede_artifact(old, new, None, root.root_str(), &validator);
        assert!(matches!(
            res,
            Err(OrchestratorError::SupersedeProtectedZone { .. })
        ));
        companyos_config::protected_zones::reload();
    }

    // EDGE: idempotence — replaying does not duplicate links or markers.
    #[test]
    fn test_supersede_idempotent_replay() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let validator = test_validator();
        let old = "c0000000-0000-4000-8000-000000000041";
        let new = "c0000000-0000-4000-8000-000000000042";
        let old_path = write_lesson(&root, old, "o", None);
        let new_path = write_lesson(&root, new, "n", None);
        index_artifact_with_kind(&mut engine, old, "lesson-learned", &old_path);
        index_artifact_with_kind(&mut engine, new, "lesson-learned", &new_path);

        engine
            .supersede_artifact(old, new, None, root.root_str(), &validator)
            .unwrap();
        let outcome2 = engine
            .supersede_artifact(old, new, None, root.root_str(), &validator)
            .unwrap();
        assert!(outcome2.idempotent, "replay must be a no-op");

        let old_yaml = std::fs::read_to_string(root.path.join(&old_path)).unwrap();
        assert_eq!(
            old_yaml.matches("[SUPERSEDED-BY").count(),
            1,
            "marker must not be stacked"
        );
        assert_eq!(
            old_yaml.matches("superseded-by").count(),
            1,
            "link must not be duplicated"
        );
    }

    // EDGE: old artifact WITHOUT a pre-existing related block gets one.
    #[test]
    fn test_supersede_creates_related_block() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let validator = test_validator();
        let old = "c0000000-0000-4000-8000-000000000051";
        let new = "c0000000-0000-4000-8000-000000000052";
        let old_path = write_lesson(&root, old, "o", None); // no related:
        let new_path = write_lesson(&root, new, "n", None);
        index_artifact_with_kind(&mut engine, old, "lesson-learned", &old_path);
        index_artifact_with_kind(&mut engine, new, "lesson-learned", &new_path);

        engine
            .supersede_artifact(old, new, None, root.root_str(), &validator)
            .unwrap();
        let old_yaml = std::fs::read_to_string(root.path.join(&old_path)).unwrap();
        assert!(old_yaml.contains("related:"), "related block created");
        assert!(validator.validate_yaml_str(&old_yaml).unwrap().is_valid);
    }

    // EDGE: supersession_warnings emitted on a one-sided supersedes, absent
    // on a reciprocal couple.
    #[test]
    fn test_supersession_warnings_asymmetry() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let a = "c0000000-0000-4000-8000-000000000061";
        let b = "c0000000-0000-4000-8000-000000000062";
        // A supersedes B, but B does NOT carry superseded-by (unilateral).
        let a_related = format!(
            "  related:\n    - id: {b}\n      kind: lesson-learned\n      relationship: supersedes\n"
        );
        let a_path = write_lesson(&root, a, "a", Some(&a_related));
        let b_path = write_lesson(&root, b, "b", None);
        index_artifact_with_kind(&mut engine, a, "lesson-learned", &a_path);
        index_artifact_with_kind(&mut engine, b, "lesson-learned", &b_path);

        let a_art = engine.get_artifact(a).unwrap().unwrap();
        let warns = engine.supersession_warnings(root.root_str(), &a_art);
        assert_eq!(warns.len(), 1, "unilateral supersedes must warn");
        assert!(warns[0].contains(b));

        // Now make B reciprocal → no warning.
        let b_related = format!(
            "  related:\n    - id: {a}\n      kind: lesson-learned\n      relationship: superseded-by\n"
        );
        write_lesson(&root, b, "b", Some(&b_related));
        let warns2 = engine.supersession_warnings(root.root_str(), &a_art);
        assert!(warns2.is_empty(), "reciprocal couple must not warn");
    }

    // ============================================================
    // Mechanism 11: reload gate via list_active_permits
    // ============================================================

    #[test]
    fn test_list_active_permits_empty_by_default() {
        let engine = setup_engine();
        assert!(engine.list_active_permits().unwrap().is_empty());
    }

    #[test]
    fn test_list_active_permits_reflects_active_and_consumed() {
        let engine = setup_engine();
        let rfc_id = Uuid::new_v4();
        let permit = engine
            .grant_permit(
                rfc_id,
                PersonaId::Implementer,
                vec![PathPattern("crates/x.rs".into())],
            )
            .unwrap();
        assert_eq!(
            engine.list_active_permits().unwrap().len(),
            1,
            "an active permit is listed"
        );
        engine.consume_permit(permit.id).unwrap();
        assert!(
            engine.list_active_permits().unwrap().is_empty(),
            "a consumed permit is not listed"
        );
    }

    // ============================================================
    // Mechanisms 16/17/19/21 (RFC 0197fbe5): warning families.
    //
    // These engine methods read the artifact YAML from disk + query the DB;
    // they do NOT need the embedder. We therefore populate the index with
    // `upsert_on_disk` (direct db upsert + fixture on disk) rather than
    // `index_artifact` (which requires an embedder, absent in test mode).
    // The `reindex_all` end-to-end wiring is exercised over the real corpus
    // with a real embedder in tests/integration_search.rs.
    // ============================================================

    /// Upsert an artifact into the index AND write its YAML fixture to disk,
    /// with relations and created_at, without needing an embedder.
    #[allow(clippy::too_many_arguments)]
    fn upsert_on_disk(
        engine: &mut OrchestratorEngine,
        root: &RoadmapTestRoot,
        id: &str,
        kind: &str,
        author: &str,
        created_at: &str,
        file_rel: &str,
        content: &str,
        relations: &[(&str, &str)],
    ) -> IndexedArtifact {
        root.write_raw(file_rel, content);
        let artifact = crate::IndexedArtifact {
            id: id.into(),
            kind: kind.into(),
            title: format!("title-{id}"),
            description: String::new(),
            tags: vec![],
            file_path: file_rel.into(),
            indexed_at: chrono::Utc::now().to_rfc3339(),
        };
        let rels: Vec<ParsedRelation> = relations
            .iter()
            .map(|(target, rel)| ParsedRelation {
                target_id: (*target).into(),
                relationship: (*rel).into(),
            })
            .collect();
        engine
            .db
            .upsert_artifact_full(
                &artifact,
                content,
                &test_dummy_embedding(),
                &rels,
                Some(author),
                None,
                Some(created_at),
            )
            .expect("upsert");
        artifact
    }

    fn lesson_yaml(id: &str, author: &str, created_at: &str, related: Option<&str>) -> String {
        let mut c = String::new();
        c.push_str("api_version: companyos/v1\nkind: lesson-learned\nmetadata:\n");
        c.push_str(&format!(
            "  id: {id}\n  title: \"L {id}\"\n  author: {author}\n"
        ));
        c.push_str(&format!(
            "  created_at: \"{created_at}\"\n  description: d\n"
        ));
        c.push_str("  tags:\n    - t\n");
        if let Some(r) = related {
            c.push_str(r);
        }
        c.push_str("spec:\n  context: c\n  insight: i\n  recommendation: r\n");
        c
    }

    fn diag_yaml(
        id: &str,
        author: &str,
        created_at: &str,
        status: &str,
        related: Option<&str>,
    ) -> String {
        let mut c = String::new();
        c.push_str("api_version: companyos/v1\nkind: diagnostic-report\nmetadata:\n");
        c.push_str(&format!(
            "  id: {id}\n  title: \"D {id}\"\n  author: {author}\n"
        ));
        c.push_str(&format!(
            "  created_at: \"{created_at}\"\n  description: d\n"
        ));
        c.push_str("  tags:\n    - t\n");
        if let Some(r) = related {
            c.push_str(r);
        }
        c.push_str(&format!("spec:\n  symptom: s\n  status: {status}\n"));
        c
    }

    fn write_persona(root: &RoadmapTestRoot, id: &str, produces: &[&str]) {
        std::fs::create_dir_all(root.path.join("company/personas")).ok();
        let mut c = String::new();
        c.push_str("api_version: companyos/v1\nkind: persona\nmetadata:\n");
        c.push_str(&format!("  id: {id}\n  display_name: {id}\n"));
        c.push_str("identity: >\n  x\nartifacts:\n  produces:");
        if produces.is_empty() {
            c.push_str(" []\n");
        } else {
            c.push('\n');
            for p in produces {
                c.push_str(&format!("    - {p}\n"));
            }
        }
        c.push_str("  consumes:\n    - rfc\n");
        root.write_raw(&format!("company/personas/{id}.yml"), &c);
    }

    // --- Mechanism 21: ReindexOutcome struct + bulk warning assembly ---

    // EDGE: empty ReindexOutcome default is (0, []).
    #[test]
    fn test_reindex_outcome_default_empty() {
        let o = crate::ReindexOutcome::default();
        assert_eq!(o.count, 0);
        assert!(o.warnings.is_empty());
    }

    // NOMINAL: dangling_related_links bulk pass surfaces exactly the pendant.
    #[test]
    fn test_dangling_related_links_nominal() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let a = "a0000000-0000-4000-8000-000000000001";
        let dead = "dead0000-0000-4000-8000-000000000000";
        upsert_on_disk(
            &mut engine,
            &root,
            a,
            "lesson-learned",
            "implementer",
            "2026-07-03",
            &format!("company/lessons/{a}.yml"),
            &lesson_yaml(a, "implementer", "2026-07-03", None),
            &[(dead, "related")],
        );
        let pendants = engine.db.dangling_related_links().unwrap();
        assert_eq!(pendants.len(), 1);
        assert_eq!(pendants[0], (a.to_string(), dead.to_string()));
    }

    // NÉGATIF: all targets present -> no dangling link.
    #[test]
    fn test_dangling_related_links_clean() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let a = "a0000000-0000-4000-8000-000000000011";
        let b = "a0000000-0000-4000-8000-000000000012";
        upsert_on_disk(
            &mut engine,
            &root,
            b,
            "lesson-learned",
            "implementer",
            "2026-07-03",
            &format!("company/lessons/{b}.yml"),
            &lesson_yaml(b, "implementer", "2026-07-03", None),
            &[],
        );
        upsert_on_disk(
            &mut engine,
            &root,
            a,
            "lesson-learned",
            "implementer",
            "2026-07-03",
            &format!("company/lessons/{a}.yml"),
            &lesson_yaml(a, "implementer", "2026-07-03", None),
            &[(b, "related")],
        );
        assert!(engine.db.dangling_related_links().unwrap().is_empty());
    }

    // EDGE: bulk pass order-insensitive (target inserted AFTER source).
    #[test]
    fn test_dangling_related_links_order_insensitive() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let a = "a0000000-0000-4000-8000-000000000021";
        let b = "a0000000-0000-4000-8000-000000000022";
        // source first, pointing to b which does not exist yet.
        upsert_on_disk(
            &mut engine,
            &root,
            a,
            "lesson-learned",
            "implementer",
            "2026-07-03",
            &format!("company/lessons/{a}.yml"),
            &lesson_yaml(a, "implementer", "2026-07-03", None),
            &[(b, "related")],
        );
        assert_eq!(
            engine.db.dangling_related_links().unwrap().len(),
            1,
            "b missing -> dangling"
        );
        // now b arrives -> no longer dangling.
        upsert_on_disk(
            &mut engine,
            &root,
            b,
            "lesson-learned",
            "implementer",
            "2026-07-03",
            &format!("company/lessons/{b}.yml"),
            &lesson_yaml(b, "implementer", "2026-07-03", None),
            &[],
        );
        assert!(
            engine.db.dangling_related_links().unwrap().is_empty(),
            "b present -> resolved"
        );
    }

    // --- Mechanism 17: single-file related-integrity warnings ---

    // NOMINAL: resolved related -> silent.
    #[test]
    fn test_related_integrity_resolved_silent() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let a = "c1000000-0000-4000-8000-000000000001";
        let b = "c1000000-0000-4000-8000-000000000002";
        upsert_on_disk(
            &mut engine,
            &root,
            b,
            "lesson-learned",
            "implementer",
            "2026-07-03",
            &format!("company/lessons/{b}.yml"),
            &lesson_yaml(b, "implementer", "2026-07-03", None),
            &[],
        );
        let a_rel = format!("  related:\n    - id: {b}\n      relationship: related\n");
        let a_art = upsert_on_disk(
            &mut engine,
            &root,
            a,
            "lesson-learned",
            "implementer",
            "2026-07-03",
            &format!("company/lessons/{a}.yml"),
            &lesson_yaml(a, "implementer", "2026-07-03", Some(&a_rel)),
            &[(b, "related")],
        );
        assert!(
            engine
                .related_integrity_warnings(root.root_str(), &a_art)
                .is_empty()
        );
    }

    // NÉGATIF: dangling id warned; no-related silent.
    #[test]
    fn test_related_integrity_dangling_warned() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let a = "c1000000-0000-4000-8000-000000000011";
        let dead = "dead1111-0000-4000-8000-000000000000";
        let a_rel = format!("  related:\n    - id: {dead}\n      relationship: related\n");
        let a_art = upsert_on_disk(
            &mut engine,
            &root,
            a,
            "lesson-learned",
            "implementer",
            "2026-07-03",
            &format!("company/lessons/{a}.yml"),
            &lesson_yaml(a, "implementer", "2026-07-03", Some(&a_rel)),
            &[(dead, "related")],
        );
        let warns = engine.related_integrity_warnings(root.root_str(), &a_art);
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("dead1111"));

        let n = "c1000000-0000-4000-8000-000000000012";
        let n_art = upsert_on_disk(
            &mut engine,
            &root,
            n,
            "lesson-learned",
            "implementer",
            "2026-07-03",
            &format!("company/lessons/{n}.yml"),
            &lesson_yaml(n, "implementer", "2026-07-03", None),
            &[],
        );
        assert!(
            engine
                .related_integrity_warnings(root.root_str(), &n_art)
                .is_empty()
        );
    }

    // EDGE: single-file surface on a fresh artifact whose target not yet
    // indexed warns (documented transient false positive); the bulk SQL pass
    // is the order-insensitive counterpart (tested above).
    #[test]
    fn test_related_integrity_single_file_transient() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let a = "c1000000-0000-4000-8000-000000000021";
        let b = "c1000000-0000-4000-8000-000000000022";
        let a_rel = format!("  related:\n    - id: {b}\n      relationship: related\n");
        // b not yet indexed.
        let a_art = upsert_on_disk(
            &mut engine,
            &root,
            a,
            "lesson-learned",
            "implementer",
            "2026-07-03",
            &format!("company/lessons/{a}.yml"),
            &lesson_yaml(a, "implementer", "2026-07-03", Some(&a_rel)),
            &[(b, "related")],
        );
        assert_eq!(
            engine
                .related_integrity_warnings(root.root_str(), &a_art)
                .len(),
            1,
            "transient single-file false positive documented"
        );
    }

    // --- Mechanism 16: author-produces matrix ---

    // NOMINAL: legitimate author-kind pairing -> silent.
    #[test]
    fn test_author_produces_legitimate_silent() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        write_persona(
            &root,
            "implementer",
            &["lesson-learned", "diagnostic-report"],
        );
        let id = "d0000000-0000-4000-8000-000000000001";
        let art = upsert_on_disk(
            &mut engine,
            &root,
            id,
            "lesson-learned",
            "implementer",
            "2026-07-20",
            &format!("company/lessons/{id}.yml"),
            &lesson_yaml(id, "implementer", "2026-07-20", None),
            &[],
        );
        assert!(
            engine
                .author_produces_warnings(root.root_str(), &art)
                .is_empty()
        );
    }

    // NÉGATIF: kind not in produces warned; unknown author skipped silently.
    #[test]
    fn test_author_produces_violation_and_unknown_author() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        write_persona(&root, "pm", &["task-request"]);
        let d = "d0000000-0000-4000-8000-000000000011";
        let art = upsert_on_disk(
            &mut engine,
            &root,
            d,
            "diagnostic-report",
            "pm",
            "2026-07-20",
            &format!("projects/x/diagnostic-reports/{d}.yml"),
            &diag_yaml(d, "pm", "2026-07-20", "investigating", None),
            &[],
        );
        let warns = engine.author_produces_warnings(root.root_str(), &art);
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("pm") && warns[0].contains("diagnostic-report"));

        let d2 = "d0000000-0000-4000-8000-000000000012";
        let art2 = upsert_on_disk(
            &mut engine,
            &root,
            d2,
            "diagnostic-report",
            "ghost",
            "2026-07-20",
            &format!("projects/x/diagnostic-reports/{d2}.yml"),
            &diag_yaml(d2, "ghost", "2026-07-20", "investigating", None),
            &[],
        );
        assert!(
            engine
                .author_produces_warnings(root.root_str(), &art2)
                .is_empty(),
            "unknown author skipped"
        );
    }

    // EDGE: system kind exempt; pre-cutoff exempt; == cutoff NOT exempt.
    #[test]
    fn test_author_produces_edge_exemptions() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        write_persona(&root, "pm", &["task-request"]);

        // System kind (persona) -> exempt regardless of author/produces.
        let pid = "arch";
        write_persona(&root, pid, &["design-doc"]);
        let persona_art = crate::IndexedArtifact {
            id: pid.into(),
            kind: "persona".into(),
            title: "arch".into(),
            description: String::new(),
            tags: vec![],
            file_path: format!("company/personas/{pid}.yml"),
            indexed_at: chrono::Utc::now().to_rfc3339(),
        };
        assert!(
            engine
                .author_produces_warnings(root.root_str(), &persona_art)
                .is_empty(),
            "system kind persona exempt"
        );

        // Pre-cutoff diagnostic by pm (kind not produced) -> exempt.
        let pre = "d0000000-0000-4000-8000-000000000021";
        let art_pre = upsert_on_disk(
            &mut engine,
            &root,
            pre,
            "diagnostic-report",
            "pm",
            "2026-07-11",
            &format!("projects/x/diagnostic-reports/{pre}.yml"),
            &diag_yaml(pre, "pm", "2026-07-11", "investigating", None),
            &[],
        );
        assert!(
            engine
                .author_produces_warnings(root.root_str(), &art_pre)
                .is_empty(),
            "pre-cutoff exempt"
        );

        // created_at == cutoff -> NOT exempt (strict bound).
        let atc = "d0000000-0000-4000-8000-000000000022";
        let art_atc = upsert_on_disk(
            &mut engine,
            &root,
            atc,
            "diagnostic-report",
            "pm",
            AUTHOR_PRODUCES_CUTOFF,
            &format!("projects/x/diagnostic-reports/{atc}.yml"),
            &diag_yaml(atc, "pm", AUTHOR_PRODUCES_CUTOFF, "investigating", None),
            &[],
        );
        assert_eq!(
            engine
                .author_produces_warnings(root.root_str(), &art_atc)
                .len(),
            1,
            "created_at == cutoff is NOT exempt"
        );
    }

    // --- Mechanism 19: capitalization reminders + has_linked_lesson ---

    // NOMINAL: resolved diagnostic WITH linked lesson -> silent.
    #[test]
    fn test_capitalization_resolved_with_lesson_silent() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let lesson = "e0000000-0000-4000-8000-000000000001";
        let diag = "e0000000-0000-4000-8000-000000000002";
        upsert_on_disk(
            &mut engine,
            &root,
            lesson,
            "lesson-learned",
            "implementer",
            "2026-07-20",
            &format!("company/lessons/{lesson}.yml"),
            &lesson_yaml(lesson, "implementer", "2026-07-20", None),
            &[],
        );
        let d_rel = format!("  related:\n    - id: {lesson}\n      relationship: related\n");
        let d_art = upsert_on_disk(
            &mut engine,
            &root,
            diag,
            "diagnostic-report",
            "implementer",
            "2026-07-20",
            &format!("projects/x/diagnostic-reports/{diag}.yml"),
            &diag_yaml(diag, "implementer", "2026-07-20", "resolved", Some(&d_rel)),
            &[(lesson, "related")],
        );
        assert!(
            engine
                .capitalization_reminders(root.root_str(), &d_art)
                .is_empty()
        );
    }

    // NÉGATIF: resolved diagnostic WITHOUT lesson -> reminder.
    #[test]
    fn test_capitalization_resolved_without_lesson_reminded() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let diag = "e0000000-0000-4000-8000-000000000011";
        let d_art = upsert_on_disk(
            &mut engine,
            &root,
            diag,
            "diagnostic-report",
            "implementer",
            "2026-07-20",
            &format!("projects/x/diagnostic-reports/{diag}.yml"),
            &diag_yaml(diag, "implementer", "2026-07-20", "resolved", None),
            &[],
        );
        let rem = engine.capitalization_reminders(root.root_str(), &d_art);
        assert_eq!(rem.len(), 1);
        assert!(rem[0].contains("resolved sans lesson"));
    }

    // EDGE: has_linked_lesson detects BOTH directions; investigating silent.
    #[test]
    fn test_capitalization_edge_direction_and_status() {
        let root = RoadmapTestRoot::new();
        let mut engine = setup_engine();
        let lesson = "e0000000-0000-4000-8000-000000000021";
        let diag = "e0000000-0000-4000-8000-000000000022";
        // diagnostic first (no relation of its own), then lesson references it.
        let d_art = upsert_on_disk(
            &mut engine,
            &root,
            diag,
            "diagnostic-report",
            "implementer",
            "2026-07-20",
            &format!("projects/x/diagnostic-reports/{diag}.yml"),
            &diag_yaml(diag, "implementer", "2026-07-20", "resolved", None),
            &[],
        );
        let l_rel = format!("  related:\n    - id: {diag}\n      relationship: related\n");
        upsert_on_disk(
            &mut engine,
            &root,
            lesson,
            "lesson-learned",
            "implementer",
            "2026-07-20",
            &format!("company/lessons/{lesson}.yml"),
            &lesson_yaml(lesson, "implementer", "2026-07-20", Some(&l_rel)),
            &[(diag, "related")],
        );
        assert!(
            engine
                .capitalization_reminders(root.root_str(), &d_art)
                .is_empty(),
            "linked lesson in the OTHER direction still counts (has_linked_lesson bidirectional)"
        );

        // investigating status -> silent even without lesson.
        let diag2 = "e0000000-0000-4000-8000-000000000023";
        let d2_art = upsert_on_disk(
            &mut engine,
            &root,
            diag2,
            "diagnostic-report",
            "implementer",
            "2026-07-20",
            &format!("projects/x/diagnostic-reports/{diag2}.yml"),
            &diag_yaml(diag2, "implementer", "2026-07-20", "investigating", None),
            &[],
        );
        assert!(
            engine
                .capitalization_reminders(root.root_str(), &d2_art)
                .is_empty(),
            "investigating silent"
        );
    }
}
