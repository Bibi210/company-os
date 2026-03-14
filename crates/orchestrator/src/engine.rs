use std::path::Path;

use chrono::Utc;
use companyos_config::{ArtifactKind, PersonaId, constants};
use companyos_validation::ArtifactValidator;
use uuid::Uuid;

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

use crate::db::OrchestratorDb;
use crate::error::OrchestratorError;
use crate::types::*;

pub struct OrchestratorEngine {
    db: OrchestratorDb,
    max_iterations: u32,
}

impl OrchestratorEngine {
    pub fn new(db: OrchestratorDb, max_iterations: u32) -> Self {
        Self { db, max_iterations }
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
    ) -> Result<ReviewRound, OrchestratorError> {
        let mut round = self
            .db
            .get_round(round_id)?
            .ok_or_else(|| OrchestratorError::RoundNotFound { id: round_id })?;

        if round.status != RoundStatus::Open && round.status != RoundStatus::RevisionRequired {
            return Err(OrchestratorError::RoundNotOpen {
                id: round_id,
                status: round.status,
            });
        }

        if !round.required_reviewers.contains(&reviewer) {
            return Err(OrchestratorError::NotRequiredReviewer { reviewer, round_id });
        }

        round.votes.retain(|v| v.reviewer != reviewer);

        round.votes.push(ReviewVote {
            reviewer,
            verdict,
            findings,
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
        let timestamp_field = if new_status == "approved" {
            format!("  approved_at: \"{now_iso}\"")
        } else {
            format!("  rejected_at: \"{now_iso}\"")
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

    pub fn consume_permit(&self, permit_id: Uuid) -> Result<(), OrchestratorError> {
        self.db.consume_permit(permit_id)
    }

    // --- Artifact Index Operations ---

    /// Index a single artifact file. Reads YAML, validates, extracts metadata, upserts into index.
    pub fn index_artifact(
        &self,
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

        self.db
            .upsert_artifact(&artifact, &searchable, &relations)?;
        Ok(artifact)
    }

    /// Search the artifact index. Returns lightweight summaries.
    pub fn search(
        &self,
        query: &str,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ArtifactSummary>, OrchestratorError> {
        self.db.search_artifacts(query, kind, limit)
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

    /// Reindex all artifacts under the given directory.
    pub fn reindex_all(
        &self,
        root: &str,
        validator: &ArtifactValidator,
    ) -> Result<usize, OrchestratorError> {
        self.db.delete_all_artifacts()?;

        let mut count = 0;
        let scan_roots = [constants::ARTIFACTS_DIR, constants::PROJECTS_DIR];

        for dir_name in &scan_roots {
            let scan_dir = format!("{root}/{dir_name}");
            walk_yaml_files(Path::new(&scan_dir), &mut |path| {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string();
                // Strip leading / if present
                let rel = rel.trim_start_matches('/');

                if self.index_artifact(root, rel, validator).is_ok() {
                    count += 1;
                }
            });
        }

        Ok(count)
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
    use crate::{
        ArtifactPath, ConsensusResult, Finding, OrchestratorDb, OrchestratorEngine, ReviewVerdict,
    };
    use companyos_config::{ArtifactKind, PersonaId};

    fn setup_engine() -> OrchestratorEngine {
        let db = OrchestratorDb::open_in_memory().expect("open in-memory db");
        db.migrate().expect("migrate");
        OrchestratorEngine::new(db, 3)
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
            )
            .unwrap();
        engine
            .submit_vote(round.id, PersonaId::Ceo, ReviewVerdict::Approve, vec![])
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
            )
            .unwrap();

        // Same reviewer votes again — should replace, not accumulate
        let updated = engine
            .submit_vote(
                round.id,
                PersonaId::Architect,
                ReviewVerdict::Approve,
                vec![],
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
        let result = engine.submit_vote(round.id, PersonaId::Ceo, ReviewVerdict::Approve, vec![]);
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
        );
        assert!(result.is_err());
    }
}
