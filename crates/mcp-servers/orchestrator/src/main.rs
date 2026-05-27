use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use companyos_config::watcher::{self, ConfigChangeKind};
use companyos_config::{ArtifactKind, CompanyConfig, Diagnostic, PersonaId, constants};
use companyos_orchestrator::{
    ArtifactPath, Finding, OrchestratorDb, OrchestratorEngine, PathPattern, ReviewVerdict,
    RfcUpdateResult, RoadmapSelector,
};
use companyos_validation::{ArtifactValidator, SchemaRegistry};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
};
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

const C: &str = constants::COMPONENT_ORCHESTRATOR;

/// In-memory token store — maps persona_id → secret token.
/// Tokens are generated on first `authenticate` call and never written to disk.
#[derive(Clone, Default)]
struct TokenStore {
    tokens: Arc<Mutex<HashMap<String, String>>>,
}

impl TokenStore {
    /// Authenticate a persona: generate a token on first call, return it.
    async fn authenticate(&self, persona: &str) -> String {
        let mut map = self.tokens.lock().await;
        if let Some(token) = map.get(persona) {
            return token.clone();
        }
        let token = Uuid::new_v4().to_string();
        map.insert(persona.to_string(), token.clone());
        token
    }

    /// Verify that a token matches the claimed persona.
    async fn verify(&self, persona: &str, token: &str) -> bool {
        let map = self.tokens.lock().await;
        map.get(persona).is_some_and(|t| t == token)
    }
}

#[derive(Clone)]
struct OrchestratorServer {
    engine: Arc<Mutex<OrchestratorEngine>>,
    validator: Arc<RwLock<ArtifactValidator>>,
    root_path: String,
    tokens: TokenStore,
    tool_router: ToolRouter<Self>,
}

// --- Review & Permit params ---

#[derive(Deserialize, schemars::JsonSchema)]
struct InitiateReviewParams {
    #[schemars(description = "Path to the artifact being reviewed", with = "String")]
    artifact_path: ArtifactPath,
    #[schemars(description = "Artifact kind (e.g., design-doc, rfc)", with = "String")]
    artifact_kind: ArtifactKind,
    #[schemars(description = "Persona ID of the author", with = "String")]
    author: PersonaId,
    #[schemars(
        description = "List of persona IDs required to review",
        with = "Vec<String>"
    )]
    required_reviewers: Vec<PersonaId>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SubmitVoteParams {
    #[schemars(description = "Auth token obtained from authenticate()")]
    token: String,
    #[schemars(description = "UUID of the review round", with = "String")]
    round_id: Uuid,
    #[schemars(description = "Persona ID of the reviewer", with = "String")]
    reviewer: PersonaId,
    #[schemars(description = "Vote: 'approve' or 'request_changes'", with = "String")]
    verdict: ReviewVerdict,
    #[schemars(description = "List of findings or comments", with = "Vec<String>")]
    findings: Vec<Finding>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct RoundIdParams {
    #[schemars(description = "UUID of the review round", with = "String")]
    round_id: Uuid,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct GrantPermitParams {
    #[schemars(description = "Auth token obtained from authenticate() — must be CEO's token")]
    token: String,
    #[schemars(
        description = "UUID of the RFC that justifies this permit",
        with = "String"
    )]
    rfc_id: Uuid,
    #[schemars(description = "Persona ID receiving the permit", with = "String")]
    granted_to: PersonaId,
    #[schemars(
        description = "List of file paths or glob patterns the permit covers",
        with = "Vec<String>"
    )]
    target_paths: Vec<PathPattern>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CheckPermitParams {
    #[schemars(description = "Persona ID to check", with = "String")]
    persona: PersonaId,
    #[schemars(description = "File path to check")]
    path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ConsumePermitParams {
    #[schemars(description = "Auth token obtained from authenticate()")]
    token: String,
    #[schemars(description = "Persona ID consuming the permit", with = "String")]
    persona: String,
    #[schemars(description = "UUID of the permit to consume", with = "String")]
    permit_id: Uuid,
}

// --- Auth params ---

#[derive(Deserialize, schemars::JsonSchema)]
struct AuthenticateParams {
    #[schemars(
        description = "Persona ID to authenticate as (e.g., pm, architect, implementer, ceo)"
    )]
    persona: String,
}

// --- Artifact Index params ---

#[derive(Deserialize, schemars::JsonSchema)]
struct SearchParams {
    #[schemars(description = "Full-text search query")]
    query: String,
    #[schemars(
        description = "Optional: filter by artifact kind (e.g., lesson-learned, design-doc)"
    )]
    kind: Option<String>,
    #[schemars(description = "Maximum number of results (default 10)")]
    limit: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct GetParams {
    #[schemars(description = "metadata.id of the artifact to retrieve")]
    id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct RelatedParams {
    #[schemars(description = "metadata.id of the artifact to get relations for")]
    id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct IndexArtifactParams {
    #[schemars(description = "Relative path to the YAML artifact file (from project root)")]
    path: String,
}

// --- Defense-in-depth coordination params (PILIER A / hook refactor) ---

#[derive(Deserialize, schemars::JsonSchema)]
struct RevertPermitsToSnapshotParams {
    #[schemars(
        description = "Snapshot blob obtained from snapshot_permits_state, or null to wipe all permits"
    )]
    snapshot: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct IndexNowParams {
    #[schemars(description = "Relative path (from project root) to the YAML artifact to index")]
    path: String,
}

// --- Roadmap tools params ---

#[derive(Deserialize, schemars::JsonSchema)]
struct ListRoadmapsParams {
    #[schemars(description = "Optional: filter by status ('active' or 'archived')")]
    status: Option<String>,
    #[schemars(description = "Optional: filter by domain (exact match on kebab-case)")]
    domain: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SummarizeRoadmapParams {
    #[schemars(
        description = "metadata.id of the roadmap (UUID). Mutually exclusive with 'domain'."
    )]
    id: Option<String>,
    #[schemars(
        description = "spec.domain of the roadmap. Mutually exclusive with 'id'. If multiple active roadmaps share this domain, returns an error."
    )]
    domain: Option<String>,
}

fn ok(json: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

fn err(msg: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![Content::text(msg)]))
}

#[tool_router]
impl OrchestratorServer {
    fn new(
        engine: Arc<Mutex<OrchestratorEngine>>,
        validator: Arc<RwLock<ArtifactValidator>>,
        root_path: String,
    ) -> Self {
        Self {
            engine,
            validator,
            root_path,
            tokens: TokenStore::default(),
            tool_router: Self::tool_router(),
        }
    }

    // --- Authentication ---

    #[tool(
        description = "Authenticate as a persona. Returns a secret token required for privileged operations (vote, grant_permit, consume_permit). Call this once at the start of your session. Keep the token secret — do NOT share it or write it to any file."
    )]
    async fn authenticate(
        &self,
        params: Parameters<AuthenticateParams>,
    ) -> Result<CallToolResult, McpError> {
        let params = params.0;
        let token = self.tokens.authenticate(&params.persona).await;
        ok(serde_json::to_string_pretty(&serde_json::json!({
            "authenticated": true,
            "persona": params.persona,
            "token": token,
            "warning": "This token is secret. Never write it to a file, never share it with other agents."
        }))
        .unwrap_or_default())
    }

    // --- Review Round tools ---

    #[tool(description = "Initiate a new review round for an artifact")]
    async fn initiate_review_round(
        &self,
        params: Parameters<InitiateReviewParams>,
    ) -> Result<CallToolResult, McpError> {
        let params = params.0;
        let engine = self.engine.lock().await;
        match engine.initiate_review_round(
            params.artifact_path,
            params.artifact_kind,
            params.author,
            params.required_reviewers,
        ) {
            Ok(round) => ok(serde_json::to_string_pretty(&round).unwrap_or_default()),
            Err(e) => err(Diagnostic::error(C, "Failed to initiate review round")
                .with_context("initiate_review_round")
                .with_reason(format!("{e}"))
                .with_fix(
                    "Check that artifact_path, artifact_kind, and required_reviewers are valid",
                )
                .to_string()),
        }
    }

    #[tool(
        description = "Submit a review vote (approve or request_changes) with findings. Requires auth token."
    )]
    async fn submit_review_vote(
        &self,
        params: Parameters<SubmitVoteParams>,
    ) -> Result<CallToolResult, McpError> {
        let params = params.0;
        let reviewer_str = format!("{}", params.reviewer);
        if !self.tokens.verify(&reviewer_str, &params.token).await {
            return err(Diagnostic::error(C, "Authentication failed")
                .with_context(format!("submit_review_vote(reviewer={})", reviewer_str))
                .with_reason("Invalid or missing auth token for this persona")
                .with_fix("Call authenticate(persona=...) first to get a valid token")
                .to_string());
        }
        let engine = self.engine.lock().await;
        match engine.submit_vote(
            params.round_id,
            params.reviewer,
            params.verdict,
            params.findings,
        ) {
            Ok(round) => ok(serde_json::to_string_pretty(&round).unwrap_or_default()),
            Err(e) => err(Diagnostic::error(C, "Failed to submit review vote")
                .with_context(format!("submit_review_vote(round_id={})", params.round_id))
                .with_reason(format!("{e}"))
                .with_fix("Verify the round_id exists and is open, and that the reviewer is in the required_reviewers list")
                .to_string()),
        }
    }

    #[tool(
        description = "Check consensus status of a review round. Returns: consensus_reached, revision_required, escalation_needed, or waiting_for_votes"
    )]
    async fn check_consensus(
        &self,
        params: Parameters<RoundIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let params = params.0;
        let engine = self.engine.lock().await;
        match engine.check_consensus(params.round_id) {
            Ok(result) => ok(serde_json::to_string_pretty(
                &serde_json::json!({ "result": result }),
            )
            .unwrap_or_default()),
            Err(e) => err(Diagnostic::error(C, "Failed to check consensus")
                .with_context(format!("check_consensus(round_id={})", params.round_id))
                .with_reason(format!("{e}"))
                .with_fix("Verify the round_id exists. Use initiate_review_round to create one")
                .to_string()),
        }
    }

    #[tool(description = "Close a review round after consensus or CEO decision")]
    async fn close_review_round(
        &self,
        params: Parameters<RoundIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let params = params.0;
        let engine = self.engine.lock().await;
        match engine.close_round_with_rfc_update(params.round_id, &self.root_path) {
            Ok((round, rfc_update)) => {
                // Build response including RFC update info (rfc-auto-approve-001)
                let rfc_info = match &rfc_update {
                    RfcUpdateResult::Updated { new_status } => serde_json::json!({
                        "rfc_auto_updated": true,
                        "rfc_new_status": new_status,
                    }),
                    RfcUpdateResult::AlreadyUpToDate => serde_json::json!({
                        "rfc_auto_updated": false,
                        "rfc_note": "RFC status already up to date (idempotent)",
                    }),
                    RfcUpdateResult::NotAnRfc => serde_json::json!({
                        "rfc_auto_updated": false,
                    }),
                    RfcUpdateResult::Failed(reason) => serde_json::json!({
                        "rfc_auto_updated": false,
                        "rfc_update_warning": reason,
                    }),
                };

                let mut response = serde_json::to_value(&round).unwrap_or_default();
                if let serde_json::Value::Object(ref mut map) = response
                    && let serde_json::Value::Object(rfc_map) = rfc_info
                {
                    map.extend(rfc_map);
                }
                ok(serde_json::to_string_pretty(&response).unwrap_or_default())
            }
            Err(e) => err(Diagnostic::error(C, "Failed to close review round")
                .with_context(format!("close_review_round(round_id={})", params.round_id))
                .with_reason(format!("{e}"))
                .with_fix("Ensure the round exists and is still open before closing")
                .to_string()),
        }
    }

    #[tool(
        description = "Grant a write permit for protected zones (CEO only, after RFC approval). Requires CEO's auth token."
    )]
    async fn grant_write_permit(
        &self,
        params: Parameters<GrantPermitParams>,
    ) -> Result<CallToolResult, McpError> {
        let params = params.0;
        // Only CEO can grant permits — verify CEO token
        if !self.tokens.verify("ceo", &params.token).await {
            return err(Diagnostic::error(C, "Authentication failed — CEO token required")
                .with_context("grant_write_permit")
                .with_reason("Only the CEO can grant write permits. Invalid or missing CEO auth token.")
                .with_fix("The CEO must call authenticate(persona='ceo') first, then pass the token here")
                .to_string());
        }
        let engine = self.engine.lock().await;
        match engine.grant_permit(params.rfc_id, params.granted_to, params.target_paths) {
            Ok(permit) => ok(serde_json::to_string_pretty(&permit).unwrap_or_default()),
            Err(e) => err(Diagnostic::error(C, "Failed to grant write permit")
                .with_context(format!("grant_write_permit(rfc_id={})", params.rfc_id))
                .with_reason(format!("{e}"))
                .with_fix("Only the CEO can grant permits. Ensure an approved RFC exists first")
                .to_string()),
        }
    }

    #[tool(description = "Check if a persona has an active write permit for a path")]
    async fn check_write_permit(
        &self,
        params: Parameters<CheckPermitParams>,
    ) -> Result<CallToolResult, McpError> {
        let params = params.0;
        let engine = self.engine.lock().await;
        match engine.check_permit(params.persona, &params.path) {
            Ok(Some(permit)) => ok(serde_json::to_string_pretty(&permit).unwrap_or_default()),
            Ok(None) => ok(r#"{"has_permit": false}"#.to_string()),
            Err(e) => err(Diagnostic::error(C, "Failed to check write permit")
                .with_context(format!("check_write_permit(path={})", params.path))
                .with_reason(format!("{e}"))
                .with_fix("Check database connectivity and that the persona ID is valid")
                .to_string()),
        }
    }

    #[tool(
        description = "Consume a write permit after successful write to protected zone. Requires auth token of the persona consuming it."
    )]
    async fn consume_write_permit(
        &self,
        params: Parameters<ConsumePermitParams>,
    ) -> Result<CallToolResult, McpError> {
        let params = params.0;
        if !self.tokens.verify(&params.persona, &params.token).await {
            return err(Diagnostic::error(C, "Authentication failed")
                .with_context(format!("consume_write_permit(persona={})", params.persona))
                .with_reason("Invalid or missing auth token for this persona")
                .with_fix("Call authenticate(persona=...) first to get a valid token")
                .to_string());
        }
        let engine = self.engine.lock().await;
        match engine.consume_permit(params.permit_id) {
            Ok(()) => ok(r#"{"consumed": true}"#.to_string()),
            Err(e) => err(Diagnostic::error(C, "Failed to consume write permit")
                .with_context(format!(
                    "consume_write_permit(permit_id={})",
                    params.permit_id
                ))
                .with_reason(format!("{e}"))
                .with_fix("Verify the permit_id is valid and has not already been consumed")
                .to_string()),
        }
    }

    // --- Defense-in-depth coordination tools (RFC cdbfee72
    //     PROPOSITION 5 + finding B index_now) ---

    #[tool(
        description = "Return an opaque blob describing the current state of write_permits. Used by the defense-in-depth hook to detect permit tampering before/after bash commands. Read-only."
    )]
    async fn snapshot_permits_state(&self) -> Result<CallToolResult, McpError> {
        let engine = self.engine.lock().await;
        match engine.snapshot_permits() {
            Ok(blob) => ok(serde_json::to_string_pretty(&serde_json::json!({
                "snapshot": blob,
            }))
            .unwrap_or_default()),
            Err(e) => err(Diagnostic::error(C, "Failed to snapshot write_permits")
                .with_context("snapshot_permits_state")
                .with_reason(format!("{e}"))
                .with_fix("Check database connectivity and DB integrity")
                .to_string()),
        }
    }

    #[tool(
        description = "Restore the write_permits table to a previous snapshot, removing any permit not present in the snapshot. Used by the defense-in-depth hook to revert tampering detected after a bash command. Pass snapshot=null to wipe all permits (used when the DB did not exist before the bash command). Returns the number of permits deleted."
    )]
    async fn revert_permits_to_snapshot(
        &self,
        params: Parameters<RevertPermitsToSnapshotParams>,
    ) -> Result<CallToolResult, McpError> {
        let params = params.0;
        let engine = self.engine.lock().await;
        match engine.restore_permits_from_snapshot(params.snapshot.as_deref()) {
            Ok(deleted) => ok(serde_json::to_string_pretty(&serde_json::json!({
                "deleted": deleted,
            }))
            .unwrap_or_default()),
            Err(e) => err(Diagnostic::error(C, "Failed to revert write_permits")
                .with_context("revert_permits_to_snapshot")
                .with_reason(format!("{e}"))
                .with_fix("Check database connectivity and DB integrity")
                .to_string()),
        }
    }

    #[tool(
        description = "Index a single YAML artifact file immediately, bypassing the file watcher's 500ms debounce. Used by the orchestrator CLI --index mode as a fallback when the server holds the lock (PILIER A) but the file watcher is unavailable, and by tests that need deterministic indexing latency. Idempotent: INSERT OR REPLACE."
    )]
    async fn index_now(
        &self,
        params: Parameters<IndexNowParams>,
    ) -> Result<CallToolResult, McpError> {
        let params = params.0;
        let engine = self.engine.lock().await;
        let validator = self.validator.read().await;
        match engine.index_artifact(&self.root_path, &params.path, &validator) {
            Ok(artifact) => ok(serde_json::to_string_pretty(&artifact).unwrap_or_default()),
            Err(e) => err(Diagnostic::error(C, "Failed to index artifact")
                .with_context(format!("index_now(path={})", params.path))
                .with_reason(format!("{e}"))
                .with_fix("Verify the file exists, is a valid YAML artifact, and passes schema validation")
                .to_string()),
        }
    }

    #[tool(
        description = "Reload company config from disk (picks up changes to flow-control.yml, etc.)"
    )]
    async fn reload_config(&self) -> Result<CallToolResult, McpError> {
        match CompanyConfig::load(&self.root_path) {
            Ok(config) => {
                let new_max = config.flow_control.max_review_iterations;
                self.engine.lock().await.set_max_iterations(new_max);
                ok(serde_json::to_string_pretty(&serde_json::json!({
                    "reloaded": true,
                    "max_iterations": new_max
                }))
                .unwrap_or_default())
            }
            Err(e) => err(Diagnostic::error(C, "Failed to reload config")
                .with_context("reload_config")
                .with_reason(format!("{e}"))
                .with_fix("Check that company/config/flow-control.yml exists and is valid YAML")
                .to_string()),
        }
    }

    // --- ID Generation ---

    #[tool(
        description = "Generate a new UUID v4 to use as metadata.id when creating a new artifact. Always call this BEFORE writing a YAML artifact — never invent an ID manually."
    )]
    async fn generate_id(&self) -> Result<CallToolResult, McpError> {
        let engine = self.engine.lock().await;
        let mut id = Uuid::new_v4().to_string();
        // Ensure no collision with existing artifacts in the index
        for _ in 0..10 {
            if engine.get(&id, &self.root_path).is_err() {
                break;
            }
            id = Uuid::new_v4().to_string();
        }
        ok(serde_json::to_string_pretty(&serde_json::json!({
            "id": id,
        }))
        .unwrap_or_default())
    }

    // --- Artifact Index tools ---

    #[tool(
        description = "Search the artifact index. Returns lightweight summaries: [{id, kind, title, description, tags}]. Use 'get' to retrieve full content."
    )]
    async fn search(&self, params: Parameters<SearchParams>) -> Result<CallToolResult, McpError> {
        let params = params.0;
        let limit = params.limit.unwrap_or(constants::DEFAULT_SEARCH_LIMIT);
        let engine = self.engine.lock().await;
        match engine.search(&params.query, params.kind.as_deref(), limit) {
            Ok(results) => ok(serde_json::to_string_pretty(&serde_json::json!({
                "results": results,
                "count": results.len(),
            }))
            .unwrap_or_default()),
            Err(e) => err(Diagnostic::error(C, "Search failed")
                .with_context(format!("search(query={})", params.query))
                .with_reason(format!("{e}"))
                .with_fix("Try simpler search terms or run reindex_all to rebuild the index")
                .to_string()),
        }
    }

    #[tool(
        description = "Get full artifact content by its metadata.id. Returns the complete YAML content."
    )]
    async fn get(&self, params: Parameters<GetParams>) -> Result<CallToolResult, McpError> {
        let params = params.0;
        let engine = self.engine.lock().await;
        match engine.get(&params.id, &self.root_path) {
            Ok(content) => ok(content),
            Err(e) => err(Diagnostic::error(C, "Failed to get artifact")
                .with_context(format!("get(id={})", params.id))
                .with_reason(format!("{e}"))
                .with_fix(
                    "Verify the ID exists (use search first). If the file was moved, run reindex_all",
                )
                .to_string()),
        }
    }

    #[tool(
        description = "Get all artifacts related to a given artifact ID (bidirectional). Returns [{id, kind, title, relationship, direction}]."
    )]
    async fn related(&self, params: Parameters<RelatedParams>) -> Result<CallToolResult, McpError> {
        let params = params.0;
        let engine = self.engine.lock().await;
        match engine.related(&params.id) {
            Ok(links) => ok(serde_json::to_string_pretty(&serde_json::json!({
                "id": params.id,
                "relations": links,
                "count": links.len(),
            }))
            .unwrap_or_default()),
            Err(e) => err(Diagnostic::error(C, "Failed to get relations")
                .with_context(format!("related(id={})", params.id))
                .with_reason(format!("{e}"))
                .with_fix("Verify the ID exists in the index")
                .to_string()),
        }
    }

    #[tool(
        description = "Index a single artifact file. Validates it first, then adds to the searchable index."
    )]
    async fn index_artifact(
        &self,
        params: Parameters<IndexArtifactParams>,
    ) -> Result<CallToolResult, McpError> {
        let params = params.0;
        let engine = self.engine.lock().await;
        let validator = self.validator.read().await;
        match engine.index_artifact(&self.root_path, &params.path, &validator) {
            Ok(artifact) => ok(serde_json::to_string_pretty(&serde_json::json!({
                "indexed": true,
                "id": artifact.id,
                "kind": artifact.kind,
            }))
            .unwrap_or_default()),
            Err(e) => err(Diagnostic::error(C, "Failed to index artifact")
                .with_context(format!("index_artifact(path={})", params.path))
                .with_reason(format!("{e}"))
                .with_fix(
                    "Ensure the file is a valid YAML artifact with api_version/kind/metadata.id",
                )
                .to_string()),
        }
    }

    #[tool(
        description = "Rebuild the entire artifact index from all YAML files under company/. Use after bulk file changes."
    )]
    async fn reindex_all(&self) -> Result<CallToolResult, McpError> {
        let engine = self.engine.lock().await;
        let validator = self.validator.read().await;
        match engine.reindex_all(&self.root_path, &validator) {
            Ok(count) => ok(serde_json::to_string_pretty(&serde_json::json!({
                "reindexed": true,
                "count": count,
            }))
            .unwrap_or_default()),
            Err(e) => err(Diagnostic::error(C, "Failed to reindex")
                .with_context("reindex_all")
                .with_reason(format!("{e}"))
                .with_fix(
                    "Check that the company/ directory exists and contains valid YAML artifacts",
                )
                .to_string()),
        }
    }

    // --- Roadmap tools ---

    #[tool(
        description = "List indexed roadmaps. Returns lightweight entries (id, title, domain, status, items_count, blocked_count, in_progress_count). Filterable by status and/or domain. Use this BEFORE summarize_roadmap to discover what exists."
    )]
    async fn list_roadmaps(
        &self,
        params: Parameters<ListRoadmapsParams>,
    ) -> Result<CallToolResult, McpError> {
        let params = params.0;
        // Tool-level validation: status must be one of the two allowed values when provided.
        if let Some(ref s) = params.status
            && s != "active"
            && s != "archived"
        {
            return err(Diagnostic::error(C, "Invalid status filter")
                .with_context(format!("list_roadmaps(status={s})"))
                .with_reason("status must be 'active' or 'archived'")
                .with_fix("Omit status to list all roadmaps, or pass 'active' / 'archived'")
                .to_string());
        }
        let engine = self.engine.lock().await;
        match engine.list_roadmaps(
            &self.root_path,
            params.status.as_deref(),
            params.domain.as_deref(),
        ) {
            Ok(entries) => {
                let count = entries.len();
                ok(serde_json::to_string_pretty(&serde_json::json!({
                    "roadmaps": entries,
                    "count": count,
                }))
                .unwrap_or_default())
            }
            Err(e) => err(Diagnostic::error(C, "Failed to list roadmaps")
                .with_context("list_roadmaps")
                .with_reason(format!("{e}"))
                .with_fix("Verify the roadmap index is healthy (try reindex_all)")
                .to_string()),
        }
    }

    #[tool(
        description = "Summarize a roadmap's state by id or by domain. Returns narrative + items grouped by timeframe AND by status, with blocked items highlighted. Read-only, no state change."
    )]
    async fn summarize_roadmap(
        &self,
        params: Parameters<SummarizeRoadmapParams>,
    ) -> Result<CallToolResult, McpError> {
        let params = params.0;
        // Validation: exactly ONE of id/domain must be provided.
        let selector = match (params.id, params.domain) {
            (Some(id), None) => RoadmapSelector::ById(id),
            (None, Some(domain)) => RoadmapSelector::ByDomain(domain),
            (Some(_), Some(_)) => {
                return err(Diagnostic::error(C, "Ambiguous selector")
                    .with_context("summarize_roadmap")
                    .with_reason("Provide exactly ONE of 'id' or 'domain', not both")
                    .with_fix("Drop one of the two parameters")
                    .to_string());
            }
            (None, None) => {
                return err(Diagnostic::error(C, "Missing selector")
                    .with_context("summarize_roadmap")
                    .with_reason("Provide exactly ONE of 'id' or 'domain'")
                    .with_fix("Call list_roadmaps first to find the id or domain")
                    .to_string());
            }
        };
        let engine = self.engine.lock().await;
        match engine.summarize_roadmap(&self.root_path, selector) {
            Ok(summary) => ok(serde_json::to_string_pretty(&summary).unwrap_or_default()),
            Err(e) => err(Diagnostic::error(C, "Failed to summarize roadmap")
                .with_context("summarize_roadmap")
                .with_reason(format!("{e}"))
                .with_fix(
                    "Use list_roadmaps to see what exists. If a YAML is corrupt, the error message points to the file.",
                )
                .to_string()),
        }
    }
}

#[tool_handler]
impl ServerHandler for OrchestratorServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("companyos-orchestrator", env!("CARGO_PKG_VERSION")))
            .with_instructions("Manages review rounds, write permits, and the unified artifact index for Company OS. Use 'search' to find artifacts, 'get' to read full content, 'related' to navigate the knowledge graph.")
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(|s| s.as_str()) {
        Some("--index") => {
            let path = args
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("usage: --index <path>"))?;
            run_index(path)
        }
        _ => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(run_server()),
    }
}

// --- PILIER A constants (file-lock coordination, RFC cdbfee72) ---
const LOCK_FILENAME: &str = "orchestrator.lock";
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(200);
const LOCK_RETRY_TIMEOUT: Duration = Duration::from_secs(10);

/// CLI mode: index a single artifact file (called by defense-in-depth plugin).
///
/// Implements the NO-OP-IF-SERVER semantics tranchée by the
/// implementation-plan étape 5.2 (option (b) of design-doc finding 2):
///
/// - if the file lock is free → acquire it, run the indexing work, release
///   it. Normal off-server CLI path.
/// - if the lock is held by another writer (server) → no-op + log + exit 0.
///   The server's file watcher will pick up the YAML change within ~500ms.
///
/// The RFC step 5.2 originally specified an MCP call to `index_now` here,
/// but this is technically impossible cross-process given the
/// orchestrator's stdio transport. An amendment of the RFC documenting
/// this constraint is engagé in étape 10.0 of the plan.
fn run_index(file_path: &str) -> anyhow::Result<()> {
    let root = std::env::var(constants::ENV_COMPANYOS_ROOT).unwrap_or_else(|_| ".".into());

    let db_dir = format!("{root}/{}", constants::DATA_DIR);
    std::fs::create_dir_all(&db_dir)?;

    let lock_path = format!("{db_dir}/{LOCK_FILENAME}");

    // Try to acquire the lock non-blocking. If another writer (server)
    // holds it, we become a no-op: the watcher will index.
    let _lock_guard = match companyos_orchestrator::lock::try_acquire_exclusive(
        std::path::Path::new(&lock_path),
    ) {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            println!(
                "{}",
                Diagnostic::info(
                    C,
                    format!(
                        "Server detected via flock, indexing of {file_path} delegated to file watcher"
                    ),
                )
                .with_context(format!("run_index(path={file_path})"))
            );
            return Ok(());
        }
        Err(e) => {
            eprintln!(
                "{}",
                Diagnostic::warning(C, format!("Lock probe failed for {lock_path}"))
                    .with_context(format!("run_index(path={file_path})"))
                    .with_reason(format!("{e}"))
            );
            // Lock probe failed for a non-EWOULDBLOCK reason (permissions,
            // missing parent dir, etc.). Exit non-zero so the caller can
            // surface the issue.
            return Err(anyhow::anyhow!("Failed to probe DB lock: {e}"));
        }
    };

    let db = OrchestratorDb::open(format!("{db_dir}/{}", constants::DB_FILENAME))?;
    db.migrate()?;

    let schemas_dir = format!("{root}/{}", constants::SCHEMAS_DIR);
    let registry = SchemaRegistry::load(&schemas_dir)?;
    let validator = ArtifactValidator::new(registry);

    let engine = OrchestratorEngine::new(db, constants::DEFAULT_MAX_ITERATIONS);
    match engine.index_artifact(&root, file_path, &validator) {
        Ok(artifact) => {
            println!(
                "{}",
                Diagnostic::info(C, format!("Indexed: {} ({})", artifact.id, artifact.kind))
                    .with_context(format!("index_artifact(path={file_path})"))
            );
            Ok(())
        }
        Err(e) => {
            eprintln!(
                "{}",
                Diagnostic::warning(C, format!("Skip indexing: {file_path}"))
                    .with_context(format!("index_artifact(path={file_path})"))
                    .with_reason(format!("{e}"))
            );
            Ok(())
        }
    }
    // _lock_guard drops here on the happy path too → lock released.
}

async fn run_server() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter("info")
        .init();

    let root = std::env::var(constants::ENV_COMPANYOS_ROOT).unwrap_or_else(|_| ".".into());

    let config = CompanyConfig::load(&root)?;
    let max_iterations = config.flow_control.max_review_iterations;

    let db_dir = format!("{root}/{}", constants::DATA_DIR);
    std::fs::create_dir_all(&db_dir)?;

    // PILIER A — acquire the exclusive lock with bounded retry. Absorbs
    // transient --index holders during proxy-driven restarts (finding A
    // round 2 of design-doc 45c04902). On timeout: error propagated up
    // to main, exit non-zero with the LockBusy message.
    let lock_path = format!("{db_dir}/{LOCK_FILENAME}");
    let _lock_guard = companyos_orchestrator::lock::acquire_exclusive_blocking(
        std::path::Path::new(&lock_path),
        LOCK_RETRY_INTERVAL,
        LOCK_RETRY_TIMEOUT,
    )?;
    tracing::info!("Acquired exclusive DB lock on {lock_path}");

    let db_file = format!("{db_dir}/{}", constants::DB_FILENAME);
    let db = OrchestratorDb::open(&db_file)?;
    db.migrate()?;

    let schemas_dir = format!("{root}/{}", constants::SCHEMAS_DIR);
    let registry = SchemaRegistry::load(&schemas_dir)?;
    let validator = Arc::new(RwLock::new(ArtifactValidator::new(registry)));

    let engine = OrchestratorEngine::new(db, max_iterations);

    // PILIER D — autorepair at boot. If integrity_check reports
    // corruption, rebuild the DB from the YAML index source of truth.
    let engine = match engine.integrity_check() {
        Ok(true) => engine,
        Ok(false) => {
            eprintln!(
                "[companyos:{C}] Database corruption detected at boot — rebuilding from YAML"
            );
            let db = engine.into_db_for_rebuild();
            drop(db);
            // Remove the corrupted DB + its WAL/SHM siblings. Errors are
            // logged but not fatal: missing files are fine, real IO
            // errors are recovered when the new open() succeeds anyway.
            let _ = std::fs::remove_file(&db_file);
            let _ = std::fs::remove_file(format!("{db_file}-wal"));
            let _ = std::fs::remove_file(format!("{db_file}-shm"));

            let new_db = OrchestratorDb::open(&db_file)?;
            new_db.migrate()?;
            let new_engine = OrchestratorEngine::new(new_db, max_iterations);
            // Rebuild synchronously before the server starts serving
            // requests. read() the validator under its RwLock once.
            let validator_guard = validator.read().await;
            let count = new_engine
                .reindex_all(&root, &validator_guard)
                .map_err(|e| anyhow::anyhow!("rebuild reindex_all failed: {e}"))?;
            drop(validator_guard);
            eprintln!(
                "[companyos:{C}] Database rebuilt from YAML index — {count} artifacts re-indexed"
            );
            new_engine
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "integrity_check failed before any work: {e}"
            ));
        }
    };

    let engine = Arc::new(Mutex::new(engine));

    let server = OrchestratorServer::new(engine.clone(), validator.clone(), root.clone());

    let (stdin, stdout) = rmcp::transport::io::stdio();

    // Reindex in background after MCP handshake is ready
    let reindex_handle = tokio::spawn({
        let engine = engine.clone();
        let validator = validator.clone();
        let root = root.clone();
        async move {
            let engine = engine.lock().await;
            let validator = validator.read().await;
            let count = engine.reindex_all(&root, &validator).unwrap_or(0);
            if count > 0 {
                tracing::info!("Indexed {count} artifact(s) on startup");
            }
        }
    });

    // File watcher: auto-reload config/schemas/artifacts on disk changes
    let watcher_handle_opt = match watcher::spawn_watcher(&root, Duration::from_millis(500)) {
        Ok(mut handle) => {
            let engine = engine.clone();
            let validator = validator.clone();
            let root = root.clone();
            let schemas_dir = schemas_dir.clone();
            let task_handle = tokio::spawn(async move {
                while let Some(change) = handle.rx.recv().await {
                    match change {
                        ConfigChangeKind::Config => match CompanyConfig::load(&root) {
                            Ok(config) => {
                                let new_max = config.flow_control.max_review_iterations;
                                engine.lock().await.set_max_iterations(new_max);
                                tracing::info!("Auto-reloaded config (max_iterations={new_max})");
                            }
                            Err(e) => tracing::warn!("Auto-reload config failed: {e}"),
                        },
                        ConfigChangeKind::Schemas => match SchemaRegistry::load(&schemas_dir) {
                            Ok(registry) => {
                                let count = registry.kinds().len();
                                *validator.write().await = ArtifactValidator::new(registry);
                                tracing::info!("Auto-reloaded {count} schema(s)");
                            }
                            Err(e) => tracing::warn!("Auto-reload schemas failed: {e}"),
                        },
                        ConfigChangeKind::Artifacts => {
                            let engine = engine.lock().await;
                            let validator = validator.read().await;
                            match engine.reindex_all(&root, &validator) {
                                Ok(count) => tracing::info!("Auto-reindexed {count} artifact(s)"),
                                Err(e) => tracing::warn!("Auto-reindex failed: {e}"),
                            }
                        }
                    }
                }
            });
            Some(task_handle)
        }
        Err(e) => {
            tracing::warn!("File watcher unavailable, manual reload_config still works: {e}");
            None
        }
    };

    // PILIER C — graceful shutdown signal-driven. Install SIGTERM and
    // SIGINT handlers, then race them with the MCP service.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    let service = server.serve((stdin, stdout)).await?;

    tokio::select! {
        result = service.waiting() => {
            match result {
                Ok(_) => tracing::info!("MCP service ended normally"),
                Err(e) => tracing::warn!("MCP service ended with error: {e}"),
            }
        }
        _ = sigterm.recv() => {
            tracing::info!("SIGTERM received, initiating graceful shutdown");
        }
        _ = sigint.recv() => {
            tracing::info!("SIGINT received, initiating graceful shutdown");
        }
    }

    // Shutdown sequence (PILIER C):
    // (1) Drop the watcher handle: the notify thread shuts down on FD
    //     close and the consumer task exits when the channel closes.
    if let Some(h) = watcher_handle_opt {
        h.abort();
    }
    // (2) Await reindex background with a bounded timeout. If it doesn't
    //     finish in time, abort and continue (data loss acceptable: a
    //     reindex is idempotent and will rerun at next boot).
    if let Err(_e) = tokio::time::timeout(Duration::from_secs(5), reindex_handle).await {
        tracing::warn!("reindex_all background did not finish in 5s — aborting");
    }
    // (3) Explicit WAL checkpoint TRUNCATE on the main connection so the
    //     DB is left in a clean state without -wal/-shm residuals.
    {
        let engine = engine.lock().await;
        match engine.checkpoint_truncate() {
            Ok(()) => tracing::info!("PRAGMA wal_checkpoint(TRUNCATE) done at shutdown"),
            Err(e) => tracing::warn!("Checkpoint TRUNCATE failed at shutdown: {e}"),
        }
    }
    // (4) Drop the engine + lock_guard happens at scope exit.

    Ok(())
}
