use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use companyos_config::{ArtifactKind, PersonaId, constants};
use companyos_validation::ArtifactValidator;
use uuid::Uuid;

use crate::embedding::Embedder;

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
use crate::roadmap_summary::{
    self, ALL_STATUSES, ALL_TIMEFRAMES, RoadmapCounters, RoadmapHeader, RoadmapListEntry,
    RoadmapSelector, RoadmapSummary,
};
use crate::types::*;

/// Kind discriminator used everywhere the engine needs to scope operations
/// to roadmap artifacts. Keep in sync with crates/config/src/types.rs.
const ROADMAP_KIND: &str = "roadmap";

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

    /// Legacy single-mode search kept for direct callers that did not
    /// migrate to [`Self::search_hybrid`]. Internally calls the new
    /// hybrid pipeline with `mode = Hybrid` and falls back to lexical
    /// if the embedder is unavailable.
    pub fn search(
        &self,
        query: &str,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ArtifactSummary>, OrchestratorError> {
        let filters = crate::db::SearchFilters {
            kinds: kind.map(|k| vec![k.to_string()]),
            ..Default::default()
        };
        let req = SearchRequest {
            query: query.to_string(),
            mode: SearchMode::Hybrid,
            filters,
            limit,
            rerank: false,
            hyde: false,
            explain: false,
        };
        Ok(self.search_hybrid(req)?.results)
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
            || req.filters.author.is_some()
            || req.filters.tags.is_some()
            || req.filters.project.is_some()
            || req.filters.created_after.is_some()
            || req.filters.created_before.is_some()
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

    // --- Roadmap tools ---

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

            let items = &parsed.spec.items;
            let blocked_count = items.iter().filter(|i| i.status == "blocked").count();
            let in_progress_count = items.iter().filter(|i| i.status == "in_progress").count();

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

        // 3. Build aggregations.
        let items = parsed.spec.items;
        let blocked_items: Vec<_> = items
            .iter()
            .filter(|i| i.status == "blocked")
            .cloned()
            .collect();

        let mut by_status = roadmap_summary::count_by_status(&items);
        let mut by_timeframe = roadmap_summary::count_by_timeframe(&items);
        let mut items_by_status = roadmap_summary::group_items_by(&items, |i| i.status.clone());
        let mut items_by_timeframe =
            roadmap_summary::group_items_by(&items, |i| i.timeframe.clone());

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
                items_total: items.len(),
                blocked_count: blocked_items.len(),
                by_status,
                by_timeframe,
            },
            blocked_items,
            items_by_timeframe,
            items_by_status,
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
    ) -> Result<usize, OrchestratorError> {
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

        for rel in yaml_paths {
            if self.index_artifact(root, &rel, validator).is_ok() {
                count += 1;
            }
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
        OrchestratorEngine::new_without_embedder(db, 3)
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

    // --- Roadmap tools tests ---

    use crate::OrchestratorError;
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
                        _ => format!("    ref:\n      type: external\n      label: \"{rval}\""),
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
                ("b1", "external", "x", "present", "blocked"),
                ("b2", "external", "x", "present", "blocked"),
                ("b3", "external", "x", "past", "done"),
            ],
        );
        // C: archived, 5 blocked, title "Gamma"
        let p_c = root.write_roadmap(
            id_c,
            "Gamma",
            "d",
            "archived",
            &[
                ("c1", "external", "x", "past", "blocked"),
                ("c2", "external", "x", "past", "blocked"),
                ("c3", "external", "x", "past", "blocked"),
                ("c4", "external", "x", "past", "blocked"),
                ("c5", "external", "x", "past", "blocked"),
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
                ("i1", "external", "x", "past", "done"),
                ("i2", "external", "x", "past", "done"),
                ("i3", "external", "x", "present", "in_progress"),
                ("i4", "external", "x", "future", "blocked"),
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
                ("b1", "external", "x", "present", "blocked"),
                ("b2", "external", "x", "future", "blocked"),
                ("d1", "external", "x", "past", "done"),
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
}
