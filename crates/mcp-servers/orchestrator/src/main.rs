use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Liveness flag for the file watcher tokio task. Set to true at
    /// task entry, false on exit (drop guard). Read by `index_status`.
    /// `Arc<AtomicBool>` keeps the lock-free invariant of the
    /// instrumentation (RFC bdee1af4 proposition 8).
    watcher_alive: Arc<AtomicBool>,
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
struct RfcSetImplementedParams {
    #[schemars(
        description = "metadata.id (UUID) of the approved RFC to mark as implemented",
        with = "String"
    )]
    id: Uuid,
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
    #[schemars(description = "Maximum number of results (default 10)")]
    limit: Option<usize>,
    #[schemars(description = "Optional: search mode 'lexical', 'semantic', or 'hybrid' (default)")]
    mode: Option<String>,
    #[schemars(description = "Optional: filter by tag (OR semantics across the list)")]
    tags: Option<Vec<String>>,
    #[schemars(description = "Optional: filter by metadata.id prefix")]
    id_prefix: Option<String>,
    #[schemars(description = "Optional: when true, return scores and timing trace")]
    explain: Option<bool>,
    #[schemars(
        description = "Optional: rerank top-K via Claude (requires ANTHROPIC_API_KEY; not yet \
                       wired in step 13)"
    )]
    rerank: Option<bool>,
    #[schemars(
        description = "Optional: expand the query via Claude HyDE (requires ANTHROPIC_API_KEY; \
                       not yet wired in step 13)"
    )]
    hyde: Option<bool>,
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

#[derive(Deserialize, schemars::JsonSchema)]
struct IndexStatusParams {
    #[schemars(
        description = "Optional: relative path to a specific YAML to inspect. If absent, returns global counts only."
    )]
    path: Option<String>,
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

/// Failure causes for the atomic DB seal performed by
/// [`OrchestratorServer::seal_db_commit`] (RFC 359f9162, decision CEO 1).
/// Each variant maps to a distinct MCP diagnostic reason so the CEO can
/// tell *why* a grant could not be materialized on disk.
#[derive(Debug)]
enum SealError {
    /// The `git` binary could not be spawned (absent from PATH, etc.).
    GitNotFound,
    /// `git add`/`commit` failed because the index is locked by another
    /// process (`.git/index.lock` present).
    IndexLocked,
    /// `git commit` reported "nothing to commit" — the working tree was
    /// already clean. Interpreted by the caller depending on context.
    NothingToCommit,
    /// Any other git failure; carries the captured stderr for diagnosis.
    GitFailed { stderr: String },
}

impl SealError {
    /// Human-readable cause used as the MCP diagnostic reason.
    fn diagnostic_reason(&self) -> String {
        match self {
            Self::GitNotFound => {
                "git binary not found — the orchestrator server cannot seal the permit on disk \
                 (git is a hard prerequisite, see RFC 359f9162)"
                    .to_string()
            }
            Self::IndexLocked => {
                "git index is locked by another process (.git/index.lock present) — retry once \
                 the other git operation completes"
                    .to_string()
            }
            Self::NothingToCommit => {
                "git reported nothing to commit — the DB on disk was unexpectedly identical to \
                 HEAD after inserting the permit"
                    .to_string()
            }
            Self::GitFailed { stderr } => {
                format!("git command failed while sealing the permit: {stderr}")
            }
        }
    }
}

/// Detect a git index-lock failure from a captured stderr. Matches both
/// the "index.lock" filename and the "Unable to create ... .lock"
/// phrasing git emits when another process holds the lock.
fn is_index_locked(stderr: &str) -> bool {
    stderr.contains("index.lock")
        || (stderr.contains("Unable to create") && stderr.contains(".lock"))
}

/// Detect a "nothing to commit" outcome from a git commit output (locale
/// forced to C by the caller). Covers the clean-tree phrasing as well as
/// the "no/nothing changes added to commit" phrasing git uses when only
/// untracked files (e.g. the -wal/-shm sidecars) are present.
fn is_nothing_to_commit(out: &str) -> bool {
    out.contains("nothing to commit")
        || out.contains("no changes added to commit")
        || out.contains("nothing added to commit")
}

/// Serialize a permit into the MCP response JSON, optionally adding the
/// additive `sealed_commit` field (RFC 359f9162). Backwards-compatible:
/// every original permit field is preserved; `sealed_commit` is the only
/// addition when present.
fn permit_response_json(
    permit: &companyos_orchestrator::WritePermit,
    sealed_commit: Option<&str>,
) -> String {
    let mut value = serde_json::to_value(permit).unwrap_or(serde_json::Value::Null);
    if let (Some(hash), serde_json::Value::Object(map)) = (sealed_commit, &mut value) {
        map.insert(
            "sealed_commit".to_string(),
            serde_json::Value::String(hash.to_string()),
        );
    }
    serde_json::to_string_pretty(&value).unwrap_or_default()
}

/// Best-effort rollback of a freshly-inserted permit. Logs (eprintln) on
/// failure but never panics — the defense-in-depth hook is the backstop.
fn rollback_permit(engine: &OrchestratorEngine, permit_id: Uuid, context: &str) {
    if let Err(e) = engine.delete_permit(permit_id) {
        eprintln!(
            "[orchestrator] WARNING: rollback (delete_permit) failed for permit {permit_id} \
             after {context}: {e}"
        );
    }
}

/// Build the JSON response for `rfc_set_implemented` (RFC 1c0f2570 §1).
/// Pure function — extracted for direct unit testing of the response shape
/// (RFC §7.b) without going through rmcp.
fn build_set_implemented_response(
    outcome: &companyos_orchestrator::engine::SetImplementedOutcome,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "rfc_id": outcome.rfc_id.to_string(),
        "previous_status": outcome.previous_status,
        "new_status": outcome.new_status,
        "implemented_at": outcome.implemented_at,
        "file_path": outcome.file_path,
    });
    if outcome.already_implemented
        && let serde_json::Value::Object(map) = &mut value
    {
        map.insert(
            "note".to_string(),
            serde_json::Value::String(
                "RFC was already implemented (idempotent), original implemented_at preserved"
                    .to_string(),
            ),
        );
    }
    value
}

fn ok(json: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

fn err(msg: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![Content::text(msg)]))
}

impl OrchestratorServer {
    /// Atomically seal the orchestrator DB into git after a permit
    /// insert (RFC 359f9162). Runs `git add -f` strictly scoped to the
    /// single `.db` file (NEVER `git add -A`, which would leave the
    /// VOLATILE class of the defense-in-depth hook), then `git commit`,
    /// and returns the resulting commit hash on success. Distinguishes
    /// failure causes via [`SealError`] so the caller can rollback and
    /// surface a precise diagnostic.
    fn seal_db_commit(&self, permit_id: Uuid, rfc_id: Uuid) -> Result<String, SealError> {
        use std::process::Command;

        // Path of the DB *relative to the repo root* — the literal,
        // strictly-scoped target. constants::DATA_DIR already includes
        // the "company/data" prefix.
        let db_rel_path = format!("{}/{}", constants::DATA_DIR, constants::DB_FILENAME);

        // Force the C locale on every git invocation so the stdout/stderr
        // markers we match on ("nothing to commit", "index.lock", …) are
        // stable English regardless of the server's configured locale
        // (the environment here defaults to French, which silently broke
        // the NothingToCommit detection).
        let git = || {
            let mut c = Command::new("git");
            c.current_dir(&self.root_path)
                .env("LC_ALL", "C")
                .env("LANG", "C");
            c
        };

        // (1) git add -f <db>
        let add = git().args(["add", "-f", &db_rel_path]).output();
        let add = match add {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(SealError::GitNotFound);
            }
            Err(e) => {
                return Err(SealError::GitFailed {
                    stderr: format!("git add spawn failed: {e}"),
                });
            }
        };
        if !add.status.success() {
            let stderr = String::from_utf8_lossy(&add.stderr).to_string();
            if is_index_locked(&stderr) {
                return Err(SealError::IndexLocked);
            }
            return Err(SealError::GitFailed { stderr });
        }

        // (2) git commit -m "chore: seal write permit <id> for RFC <rfc>"
        let msg = format!("chore: seal write permit {permit_id} for RFC {rfc_id}");
        let commit = git().args(["commit", "-m", &msg]).output();
        let commit = match commit {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(SealError::GitNotFound);
            }
            Err(e) => {
                return Err(SealError::GitFailed {
                    stderr: format!("git commit spawn failed: {e}"),
                });
            }
        };
        if !commit.status.success() {
            let stdout = String::from_utf8_lossy(&commit.stdout).to_string();
            let stderr = String::from_utf8_lossy(&commit.stderr).to_string();
            if is_index_locked(&stderr) {
                return Err(SealError::IndexLocked);
            }
            // git emits one of several locale-C phrasings when there is
            // nothing staged: "nothing to commit" (clean tree),
            // "no changes added to commit" / "nothing added to commit"
            // (untracked files present — e.g. the -wal/-shm sidecars).
            if is_nothing_to_commit(&stdout) || is_nothing_to_commit(&stderr) {
                return Err(SealError::NothingToCommit);
            }
            return Err(SealError::GitFailed { stderr });
        }

        // (3) git rev-parse HEAD → commit hash
        let rev = git().args(["rev-parse", "HEAD"]).output();
        let rev = match rev {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(SealError::GitNotFound);
            }
            Err(e) => {
                return Err(SealError::GitFailed {
                    stderr: format!("git rev-parse spawn failed: {e}"),
                });
            }
        };
        if !rev.status.success() {
            return Err(SealError::GitFailed {
                stderr: String::from_utf8_lossy(&rev.stderr).to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&rev.stdout).trim().to_string())
    }
}

#[tool_router]
impl OrchestratorServer {
    fn new(
        engine: Arc<Mutex<OrchestratorEngine>>,
        validator: Arc<RwLock<ArtifactValidator>>,
        root_path: String,
        watcher_alive: Arc<AtomicBool>,
    ) -> Self {
        Self {
            engine,
            validator,
            root_path,
            tokens: TokenStore::default(),
            watcher_alive,
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
        description = "Mark an approved RFC as implemented. Transitions metadata.status from 'approved' to 'implemented' and stamps implemented_at. Server-side lifecycle transition: requires NO write permit (same model as close_review_round auto-approving an RFC). Refuses any RFC not currently in 'approved' status with an explicit error. Already-implemented RFCs return an idempotent success."
    )]
    async fn rfc_set_implemented(
        &self,
        params: Parameters<RfcSetImplementedParams>,
    ) -> Result<CallToolResult, McpError> {
        let params = params.0;
        // No token check (RFC 1c0f2570 decision CEO 2): this is a lifecycle
        // transition, not a privileged operation.
        // Serialize concurrent calls on the same id via the engine mutex.
        let engine = self.engine.lock().await;
        match engine.set_rfc_implemented(params.id, &self.root_path) {
            Ok(outcome) => {
                ok(serde_json::to_string_pretty(&build_set_implemented_response(&outcome))
                    .unwrap_or_default())
            }
            Err(e) => err(Diagnostic::error(C, "Failed to mark RFC as implemented")
                .with_context(format!("rfc_set_implemented(id={})", params.id))
                .with_reason(format!("{e}"))
                .with_fix(
                    "Verify the id is an existing RFC (use search/get) and that it is currently in 'approved' status — only approved RFCs can be marked implemented",
                )
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
        // Hold the engine lock across the WHOLE sequence — idempotency
        // lookup, grant, WAL checkpoint, git seal, and any rollback — so
        // concurrent grants AND their git commits are serialized under a
        // single mutex (edge f, RFC 359f9162).
        let engine = self.engine.lock().await;

        // (a) IDEMPOTENCE (decision CEO 3): replaying the same grant key
        //     (rfc_id + granted_to + normalized target_paths) returns the
        //     existing active permit instead of creating a duplicate.
        match engine.find_permit_by_grant(params.rfc_id, &params.granted_to, &params.target_paths) {
            Ok(Some(existing)) => {
                // Permit already exists and is active. Re-seal defensively:
                // it should already be in HEAD, so NothingToCommit is the
                // expected success path here.
                match self.seal_db_commit(existing.id, params.rfc_id) {
                    Ok(hash) => return ok(permit_response_json(&existing, Some(&hash))),
                    Err(SealError::NothingToCommit) => {
                        return ok(permit_response_json(&existing, None));
                    }
                    Err(seal_err) => {
                        // Do NOT rollback — the existing permit predates
                        // this call and must survive. Surface the cause.
                        return err(Diagnostic::error(
                            C,
                            "Existing permit found but re-seal failed",
                        )
                        .with_context(format!(
                            "grant_write_permit(rfc_id={}) [idempotent replay]",
                            params.rfc_id
                        ))
                        .with_reason(seal_err.diagnostic_reason())
                        .with_fix(
                            "The permit already exists; resolve the git issue then retry. No \
                             duplicate was created.",
                        )
                        .to_string());
                    }
                }
            }
            Ok(None) => { /* fall through to the new grant */ }
            Err(e) => {
                return err(Diagnostic::error(C, "Idempotency lookup failed")
                    .with_context(format!("grant_write_permit(rfc_id={})", params.rfc_id))
                    .with_reason(format!("{e}"))
                    .with_fix("Check database connectivity and DB integrity")
                    .to_string());
            }
        }

        // (b) GRANT: insert the new permit.
        let permit =
            match engine.grant_permit(params.rfc_id, params.granted_to, params.target_paths) {
                Ok(p) => p,
                Err(e) => {
                    return err(Diagnostic::error(C, "Failed to grant write permit")
                        .with_context(format!("grant_write_permit(rfc_id={})", params.rfc_id))
                        .with_reason(format!("{e}"))
                        .with_fix(
                            "Only the CEO can grant permits. Ensure an approved RFC exists first",
                        )
                        .to_string());
                }
            };

        // (c) CHECKPOINT WAL (edge h): flush the WAL into the .db on disk
        //     BEFORE git add, so the committed file actually contains the
        //     freshly-inserted permit. On failure: rollback + error.
        if let Err(e) = engine.checkpoint_truncate() {
            rollback_permit(&engine, permit.id, "checkpoint failed");
            return err(Diagnostic::error(C, "WAL checkpoint failed before seal")
                .with_context(format!("grant_write_permit(rfc_id={})", params.rfc_id))
                .with_reason(format!("{e}"))
                .with_fix("Check DB integrity; the permit was rolled back, retry the grant")
                .to_string());
        }

        // (d) SEAL: git add -f <db> + git commit.
        match self.seal_db_commit(permit.id, params.rfc_id) {
            Ok(hash) => ok(permit_response_json(&permit, Some(&hash))),
            Err(seal_err) => {
                // (d') A brand-new grant that yields NothingToCommit is an
                //      anomaly: the insert did not materialize on disk.
                //      Treat like any seal failure → rollback + error.
                let reason = seal_err.diagnostic_reason();
                let rollback_note = match engine.delete_permit(permit.id) {
                    Ok(()) => String::new(),
                    Err(re) => {
                        // (e) edge d: rollback itself failed. Log and
                        //     surface a double error; do not panic.
                        eprintln!(
                            "[orchestrator] CRITICAL: seal failed AND rollback failed for permit \
                             {} (rfc {}): seal={reason} rollback={re}",
                            permit.id, params.rfc_id
                        );
                        format!(
                            " ADDITIONALLY the rollback (delete_permit) failed: {re} — a \
                             non-sealed permit may remain in the DB (will be handled by the \
                             defense-in-depth hook)."
                        )
                    }
                };
                err(Diagnostic::error(C, "Failed to seal write permit on disk")
                    .with_context(format!("grant_write_permit(rfc_id={})", params.rfc_id))
                    .with_reason(format!("{reason}{rollback_note}"))
                    .with_fix(
                        "git is a hard prerequisite. Resolve the git issue (binary present, \
                         index unlocked) then retry. The permit was rolled back unless noted \
                         otherwise above.",
                    )
                    .to_string())
            }
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
        let mut engine = self.engine.lock().await;
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
        let limit = params
            .limit
            .unwrap_or(constants::DEFAULT_SEARCH_LIMIT)
            .min(100);
        let mode = match params.mode.as_deref() {
            Some("lexical") => companyos_orchestrator::engine::SearchMode::Lexical,
            Some("semantic") => companyos_orchestrator::engine::SearchMode::Semantic,
            None | Some("hybrid") => companyos_orchestrator::engine::SearchMode::Hybrid,
            Some(other) => {
                return err(Diagnostic::error(C, "Invalid search mode")
                    .with_context(format!("search(mode={other})"))
                    .with_reason(format!("unknown mode '{other}'"))
                    .with_fix("Use one of: 'lexical', 'semantic', 'hybrid'")
                    .to_string());
            }
        };
        // kinds is not wired from the MCP search surface (RFC 1d3a3581):
        // filtering by kind belongs to a future list_artifacts tool. The
        // internal field is preserved for the list-mode and list_by_kind.
        let filters = companyos_orchestrator::SearchFilters {
            kinds: None,
            tags: params.tags,
            id_prefix: params.id_prefix,
        };
        let req = companyos_orchestrator::engine::SearchRequest {
            query: params.query.clone(),
            mode,
            filters,
            limit,
            rerank: params.rerank.unwrap_or(false),
            hyde: params.hyde.unwrap_or(false),
            explain: params.explain.unwrap_or(false),
        };

        let engine = self.engine.lock().await;
        match engine.search_hybrid(req) {
            Ok(resp) => {
                let count = resp.results.len();
                let mut json_out = serde_json::json!({
                    "results": resp.results,
                    "count": count,
                });
                if let Some(trace) = resp.explain {
                    json_out["explain"] = serde_json::json!({
                        "mode_applied": trace.mode_applied,
                        "candidate_set_sizes": {
                            "lexical": trace.candidate_set_sizes_lexical,
                            "semantic": trace.candidate_set_sizes_semantic,
                            "fused": trace.candidate_set_sizes_fused,
                        },
                        "latency_ms": trace.latency_ms,
                    });
                }
                ok(serde_json::to_string_pretty(&json_out).unwrap_or_default())
            }
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
        let mut engine = self.engine.lock().await;
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
        let mut engine = self.engine.lock().await;
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

    // --- Index status (RFC bdee1af4 proposition 8) ---

    #[tool(
        description = "Inspect indexation state. Without args returns global counts (artifacts/fts/vec, triplet_coherent, file_watcher_alive, last_indexed_at). With path returns per-path detail (indexed_at vs file_mtime, stale flag, presence flags). Lecture seule, latence < 10ms."
    )]
    async fn index_status(
        &self,
        params: Parameters<IndexStatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let params = params.0;
        let engine = self.engine.lock().await;

        let watcher = self.watcher_alive.load(Ordering::Acquire);
        // No intermediate mpsc channel between notify and indexer in
        // this codebase; queue size always 0 (queue_mode = direct), cf.
        // plan 640e2894 étape 14 decision.
        let queue_size = 0;

        let global = match engine.index_status_global(watcher, queue_size) {
            Ok(g) => g,
            Err(e) => {
                return err(Diagnostic::error(C, "index_status global failed")
                    .with_context("index_status")
                    .with_reason(format!("{e}"))
                    .to_string());
            }
        };

        let mut json = serde_json::json!({
            "global": {
                "artifacts_count": global.artifacts_count,
                "fts_count": global.fts_count,
                "vec_count": global.vec_count,
                "triplet_coherent": global.triplet_coherent,
                "file_watcher_alive": global.file_watcher_alive,
                "pending_index_queue_size": global.pending_index_queue_size,
                "queue_mode": "direct",
                "last_indexed_at": global.last_indexed_at,
            },
        });

        if let Some(path) = params.path {
            match engine.index_status_per_path(&self.root_path, &path) {
                Ok(per) => {
                    json["per_path"] = serde_json::json!({
                        "path": path,
                        "indexed_at": per.indexed_at,
                        "file_mtime": per.file_mtime,
                        "stale": per.stale,
                        "present_in_fts": per.present_in_fts,
                        "present_in_vec": per.present_in_vec,
                    });
                }
                Err(e) => {
                    return err(Diagnostic::error(C, "index_status per_path failed")
                        .with_context(format!("index_status(path={path})"))
                        .with_reason(format!("{e}"))
                        .to_string());
                }
            }
        }

        ok(serde_json::to_string_pretty(&json).unwrap_or_default())
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
        Some("--prefetch-embeddings") => run_prefetch_embeddings(),
        _ => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(run_server()),
    }
}

/// CLI mode: pre-fetch the embedding model weights into the local cache.
///
/// This is the ONLY entry point that performs network I/O for the
/// embedding model. The server boot path requires the cache to be
/// present (via `Embedder::load_from_cache`) and fails fast if it is
/// not (RFC bdee1af4 proposition 2, axis (i) — runtime autonomy).
///
/// Operators must run this once after install and after any change to
/// `Embedder::model_version()`. Idempotent.
fn run_prefetch_embeddings() -> anyhow::Result<()> {
    let root = std::env::var(constants::ENV_COMPANYOS_ROOT).unwrap_or_else(|_| ".".into());
    let cache = companyos_orchestrator::Embedder::prefetch_to_cache(&root)
        .map_err(|e| anyhow::anyhow!("Failed to prefetch embeddings: {e}"))?;
    println!(
        "{}",
        Diagnostic::info(
            C,
            format!(
                "Embedding model cached at '{}'. Server can now boot.",
                cache.display()
            )
        )
        .with_context("run_prefetch_embeddings")
    );
    Ok(())
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

    // RFC bdee1af4 étape 7: load embedder before any indexing. Cache must
    // be present (run --prefetch-embeddings first).
    let embedder = Arc::new(
        companyos_orchestrator::Embedder::load_from_cache(&root)
            .map_err(|e| anyhow::anyhow!("Failed to load embedder: {e}"))?,
    );
    let mut engine = OrchestratorEngine::new(db, constants::DEFAULT_MAX_ITERATIONS, embedder);
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
    // RFC bdee1af4 étape 19: if migrate() dropped+recreated artifacts_fts
    // (tokenizer drift), we must reindex_all to repopulate FTS rows.
    let fts_drift = db.fts_was_upgraded().unwrap_or(false);

    let schemas_dir = format!("{root}/{}", constants::SCHEMAS_DIR);
    let registry = SchemaRegistry::load(&schemas_dir)?;
    let validator = Arc::new(RwLock::new(ArtifactValidator::new(registry)));

    // RFC bdee1af4 étape 7: load the embedder once at boot, share via Arc.
    // Cache must be present on disk (run --prefetch-embeddings first).
    let embedder = Arc::new(
        companyos_orchestrator::Embedder::load_from_cache(&root)
            .map_err(|e| anyhow::anyhow!("Failed to load embedder: {e}"))?,
    );

    // RFC bdee1af4 étape 5+19: detect model_version drift (architecture
    // marker, model upgrade). If the stored version differs from the
    // runtime one, wipe artifacts_vec and force a reindex.
    let model_drift = {
        let tmp_db = OrchestratorDb::open(&db_file)?;
        let stored = tmp_db.get_model_version().unwrap_or(None);
        let current = companyos_orchestrator::embedding::model_version();
        match stored {
            None => true, // fresh DB: needs initial indexing anyway
            Some(v) if v != current => {
                eprintln!(
                    "[companyos:{C}] embedding model_version drift: stored='{v}' current='{current}', wiping vec table"
                );
                tmp_db.wipe_vec_table()?;
                true
            }
            _ => false,
        }
    };

    let engine = OrchestratorEngine::new(db, max_iterations, embedder.clone());

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
            let mut new_engine = OrchestratorEngine::new(new_db, max_iterations, embedder.clone());
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

    // RFC bdee1af4 étape 19: if FTS or model drift was detected, force a
    // synchronous reindex_all before serving any MCP request so the
    // index is coherent and queries don't return stale or empty results.
    let engine = if fts_drift || model_drift {
        let mut e = engine;
        let validator_guard = validator.read().await;
        let count = e
            .reindex_all(&root, &validator_guard)
            .map_err(|e| anyhow::anyhow!("post-migrate reindex_all failed: {e}"))?;
        drop(validator_guard);
        eprintln!(
            "[companyos:{C}] Post-migration reindex completed ({count} artifacts) — drift: fts={fts_drift} model={model_drift}"
        );
        e
    } else {
        engine
    };

    let engine = Arc::new(Mutex::new(engine));

    // RFC bdee1af4 étape 14: shared liveness flag for the file watcher
    // task, read by the `index_status` tool.
    let watcher_alive = Arc::new(AtomicBool::new(false));

    // RFC 062ebaa8 — shared flag set by the signal handlers below so the
    // `WatcherGuard` Drop impl can distinguish a clean shutdown (info!)
    // from an unexpected death (error!). Cf. diagnostic 9534dd33.
    let is_shutting_down = Arc::new(AtomicBool::new(false));

    let server = OrchestratorServer::new(
        engine.clone(),
        validator.clone(),
        root.clone(),
        watcher_alive.clone(),
    );

    let (stdin, stdout) = rmcp::transport::io::stdio();

    // Reindex in background after MCP handshake is ready
    let reindex_handle = tokio::spawn({
        let engine = engine.clone();
        let validator = validator.clone();
        let root = root.clone();
        async move {
            let mut engine = engine.lock().await;
            let validator = validator.read().await;
            let count = engine.reindex_all(&root, &validator).unwrap_or(0);
            if count > 0 {
                tracing::info!("Indexed {count} artifact(s) on startup");
            }
        }
    });

    // File watcher: auto-reload config/schemas/artifacts on disk changes.
    //
    // RFC 062ebaa8 — the `handle` is destructured here (NOT passed by
    // move into the closure) so that the `_guard` field can be captured
    // explicitly inside the `async move` block via `let _keep = _guard;`.
    // Without this, Rust 2024 disjoint capture for async closures would
    // leave `_guard` outside the future, dropping the RecommendedWatcher
    // at the end of this match arm and killing the inotify fd (cf.
    // diagnostic 9534dd33).
    let watcher_handle_opt =
        match watcher::spawn_watcher(&root, Duration::from_millis(500), is_shutting_down.clone()) {
            Ok(handle) => {
                let watcher::FileWatcherHandle { mut rx, _guard } = handle;
                let engine = engine.clone();
                let validator = validator.clone();
                let root = root.clone();
                let schemas_dir = schemas_dir.clone();
                let watcher_alive = watcher_alive.clone();
                let task_handle = tokio::spawn(async move {
                    // The guard (and the RecommendedWatcher it owns) MUST
                    // live as long as this consumer task. Without this
                    // binding, Rust 2024 does not capture `_guard` (the
                    // field is never read textually in the closure), it is
                    // dropped at the end of the outer match arm, and the
                    // inotify fd dies (cf. diagnostic 9534dd33). DO NOT
                    // remove this binding even if it looks unused.
                    let _keep_guard = _guard;

                    // Mark alive at task entry. The corresponding `false`
                    // write happens in a drop guard so SIGKILL of the
                    // process does not leave a stale `true` flag (the
                    // process is gone with it anyway).
                    watcher_alive.store(true, Ordering::Release);
                    struct AliveGuard(Arc<AtomicBool>);
                    impl Drop for AliveGuard {
                        fn drop(&mut self) {
                            self.0.store(false, Ordering::Release);
                        }
                    }
                    let _alive_guard = AliveGuard(watcher_alive.clone());

                    while let Some(change) = rx.recv().await {
                        match change {
                            ConfigChangeKind::Config => match CompanyConfig::load(&root) {
                                Ok(config) => {
                                    let new_max = config.flow_control.max_review_iterations;
                                    engine.lock().await.set_max_iterations(new_max);
                                    tracing::info!(
                                        "Auto-reloaded config (max_iterations={new_max})"
                                    );
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
                                let mut engine = engine.lock().await;
                                let validator = validator.read().await;
                                match engine.reindex_all(&root, &validator) {
                                    Ok(count) => {
                                        tracing::info!("Auto-reindexed {count} artifact(s)")
                                    }
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

    // If `server.serve` fails (transport error, stdin closed, etc.), it
    // is sémantiquement a clean shutdown of the service, NOT a watcher
    // bug. Arm the flag BEFORE propagating the error so the
    // `WatcherGuard` Drop emits info!, not error!.
    let service = match server.serve((stdin, stdout)).await {
        Ok(s) => s,
        Err(e) => {
            is_shutting_down.store(true, Ordering::Release);
            return Err(e.into());
        }
    };

    tokio::select! {
        result = service.waiting() => {
            // Service stopped on its own (stdin closed, transport error,
            // etc.). Sémantiquement c'est un shutdown propre : arm the
            // flag so the WatcherGuard Drop emits info!, not error!.
            is_shutting_down.store(true, Ordering::Release);
            match result {
                Ok(_) => tracing::info!("MCP service ended normally"),
                Err(e) => tracing::warn!("MCP service ended with error: {e}"),
            }
        }
        _ = sigterm.recv() => {
            // RFC 062ebaa8 — store BEFORE the info!() so a concurrent
            // drop of the watcher (e.g. consumer task ending at the
            // same instant) sees `true` and downgrades the log level.
            is_shutting_down.store(true, Ordering::Release);
            tracing::info!("SIGTERM received, initiating graceful shutdown");
        }
        _ = sigint.recv() => {
            is_shutting_down.store(true, Ordering::Release);
            tracing::info!("SIGINT received, initiating graceful shutdown");
        }
    }

    // Shutdown sequence (PILIER C):
    // (1) Drop the watcher handle: the notify thread shuts down on FD
    //     close and the consumer task exits when the channel closes.
    //     RFC 062ebaa8 — also await a brief moment to surface any panic
    //     that happened inside the consumer task (JoinHandle::abort
    //     takes &self in tokio 1.x, so we can both abort and await it).
    if let Some(h) = watcher_handle_opt {
        h.abort();
        match tokio::time::timeout(Duration::from_millis(200), h).await {
            Ok(Err(join_err)) if join_err.is_panic() => {
                tracing::error!("Watcher consumer task panicked at shutdown: {join_err}");
            }
            _ => {} // cancelled, normal exit, or timeout: silent
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use companyos_orchestrator::{OrchestratorDb, OrchestratorEngine, PathPattern};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// RAII tempdir for tests (lesson f3fc4a5d: a 10-line struct beats a
    /// tempfile dev-dependency). Removed on drop.
    struct LocalTempDir {
        path: PathBuf,
    }

    impl LocalTempDir {
        fn new(prefix: &str) -> Self {
            let path = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("create_dir_all");
            Self { path }
        }
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for LocalTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .expect("git spawn");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Initialize a git repo at `root` with a committed README so that
    /// the working tree has a HEAD to compare against.
    fn init_git_repo(root: &Path) {
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "test@companyos.local"]);
        git(root, &["config", "user.name", "Test"]);
        std::fs::write(root.join("README.md"), "test\n").unwrap();
        git(root, &["add", "README.md"]);
        git(root, &["commit", "-q", "-m", "init"]);
    }

    /// Build a test server rooted at `root`, with an on-disk DB at
    /// company/data/orchestrator.db and the CEO already authenticated.
    /// Returns (server, ceo_token).
    async fn setup_server(root: &Path) -> (OrchestratorServer, String) {
        let db_dir = root.join(constants::DATA_DIR);
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join(constants::DB_FILENAME);
        let db = OrchestratorDb::open(&db_path).unwrap();
        db.migrate().unwrap();
        let engine = OrchestratorEngine::new_without_embedder(db, 3);
        // Validator points at an empty schemas dir (never exercised by the
        // permit path).
        let registry = SchemaRegistry::load(root.join("no-schemas")).unwrap();
        let validator = ArtifactValidator::new(registry);
        let server = OrchestratorServer::new(
            Arc::new(Mutex::new(engine)),
            Arc::new(RwLock::new(validator)),
            root.to_string_lossy().to_string(),
            Arc::new(AtomicBool::new(false)),
        );
        let token = server.tokens.authenticate("ceo").await;
        (server, token)
    }

    fn result_text(result: &CallToolResult) -> String {
        result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap_or_default()
    }

    fn grant_params(token: &str, rfc: Uuid, paths: Vec<&str>) -> Parameters<GrantPermitParams> {
        Parameters(GrantPermitParams {
            token: token.to_string(),
            rfc_id: rfc,
            granted_to: PersonaId::Implementer,
            target_paths: paths.into_iter().map(|p| PathPattern(p.into())).collect(),
        })
    }

    // --- seal_db_commit unit tests (étape 4) ---

    #[test]
    fn test_seal_db_commit_ok_returns_hash() {
        let tmp = LocalTempDir::new("seal-ok");
        init_git_repo(tmp.path());
        let db_dir = tmp.path().join(constants::DATA_DIR);
        std::fs::create_dir_all(&db_dir).unwrap();
        std::fs::write(db_dir.join(constants::DB_FILENAME), b"fakedb").unwrap();

        let server = OrchestratorServer::new(
            Arc::new(Mutex::new(OrchestratorEngine::new_without_embedder(
                OrchestratorDb::open_in_memory().unwrap(),
                3,
            ))),
            Arc::new(RwLock::new(ArtifactValidator::new(
                SchemaRegistry::load(tmp.path().join("none")).unwrap(),
            ))),
            tmp.path().to_string_lossy().to_string(),
            Arc::new(AtomicBool::new(false)),
        );

        let hash = server
            .seal_db_commit(Uuid::new_v4(), Uuid::new_v4())
            .expect("seal should succeed");
        assert_eq!(hash.len(), 40, "expected a 40-char git sha, got '{hash}'");

        // The commit must touch ONLY the .db file (VOLATILE class).
        let stat = Command::new("git")
            .current_dir(tmp.path())
            .args(["show", "--stat", "--name-only", "--format=", "HEAD"])
            .output()
            .unwrap();
        let files = String::from_utf8_lossy(&stat.stdout);
        assert!(
            files.contains("orchestrator.db"),
            "commit should include the db: {files}"
        );
        assert!(
            !files.contains("README"),
            "commit must NOT touch other files: {files}"
        );
    }

    #[test]
    fn test_seal_db_commit_no_git_repo_errors() {
        let tmp = LocalTempDir::new("seal-nogit");
        // No git init → not a repo.
        let db_dir = tmp.path().join(constants::DATA_DIR);
        std::fs::create_dir_all(&db_dir).unwrap();
        std::fs::write(db_dir.join(constants::DB_FILENAME), b"fakedb").unwrap();

        let server = OrchestratorServer::new(
            Arc::new(Mutex::new(OrchestratorEngine::new_without_embedder(
                OrchestratorDb::open_in_memory().unwrap(),
                3,
            ))),
            Arc::new(RwLock::new(ArtifactValidator::new(
                SchemaRegistry::load(tmp.path().join("none")).unwrap(),
            ))),
            tmp.path().to_string_lossy().to_string(),
            Arc::new(AtomicBool::new(false)),
        );

        let res = server.seal_db_commit(Uuid::new_v4(), Uuid::new_v4());
        assert!(res.is_err(), "sealing outside a git repo must fail");
    }

    #[test]
    fn test_is_index_locked_matches() {
        assert!(is_index_locked(
            "fatal: Unable to create '/x/.git/index.lock': File exists."
        ));
        assert!(is_index_locked("another error mentioning index.lock here"));
        assert!(!is_index_locked("some unrelated git error"));
    }

    // --- grant_write_permit orchestration tests (étapes 5 & 7) ---

    // T1 (nominal): new grant → permit inserted, sealed, response carries
    // sealed_commit; commit touches only the .db.
    #[tokio::test]
    async fn test_grant_write_permit_nominal_seals() {
        let tmp = LocalTempDir::new("grant-nominal");
        init_git_repo(tmp.path());
        let (server, token) = setup_server(tmp.path()).await;
        let rfc = Uuid::new_v4();

        let res = server
            .grant_write_permit(grant_params(&token, rfc, vec!["crates/x/src/a.rs"]))
            .await
            .unwrap();
        assert_ne!(
            res.is_error,
            Some(true),
            "grant should succeed: {}",
            result_text(&res)
        );

        let text = result_text(&res);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(
            value
                .get("sealed_commit")
                .and_then(|v| v.as_str())
                .is_some(),
            "response must carry sealed_commit: {text}"
        );

        // T2: the permit is active right after the grant.
        let permit_id = Uuid::parse_str(value.get("id").unwrap().as_str().unwrap()).unwrap();
        let engine = server.engine.lock().await;
        let fetched = engine
            .check_permit(PersonaId::Implementer, "crates/x/src/a.rs")
            .unwrap();
        assert!(fetched.is_some(), "permit must be active after grant");
        assert_eq!(fetched.unwrap().id, permit_id);
        drop(engine);

        // The seal commit must touch ONLY the .db.
        let stat = Command::new("git")
            .current_dir(tmp.path())
            .args(["show", "--stat", "--name-only", "--format=", "HEAD"])
            .output()
            .unwrap();
        let files = String::from_utf8_lossy(&stat.stdout);
        assert!(
            files.contains("orchestrator.db"),
            "seal must commit the db: {files}"
        );
        assert!(
            !files.contains("README"),
            "seal must not touch other files: {files}"
        );
    }

    // T3 (negative): seal fails (no git repo) → permit rolled back + error.
    #[tokio::test]
    async fn test_grant_write_permit_seal_failure_rolls_back() {
        let tmp = LocalTempDir::new("grant-nogit");
        // No git init.
        let (server, token) = setup_server(tmp.path()).await;
        let rfc = Uuid::new_v4();

        let res = server
            .grant_write_permit(grant_params(&token, rfc, vec!["crates/x/src/a.rs"]))
            .await
            .unwrap();
        assert_eq!(
            res.is_error,
            Some(true),
            "grant must fail without a git repo"
        );

        // No permit must remain (rolled back).
        let engine = server.engine.lock().await;
        let still = engine
            .find_permit_by_grant(
                rfc,
                &PersonaId::Implementer,
                &[PathPattern("crates/x/src/a.rs".into())],
            )
            .unwrap();
        assert!(
            still.is_none(),
            "failed seal must roll back the permit (T3)"
        );
    }

    // T5 (edge/idempotence): replaying the same grant (reversed path order)
    // returns the SAME permit, no duplicate.
    #[tokio::test]
    async fn test_grant_write_permit_idempotent_replay() {
        let tmp = LocalTempDir::new("grant-idem");
        init_git_repo(tmp.path());
        let (server, token) = setup_server(tmp.path()).await;
        let rfc = Uuid::new_v4();

        let r1 = server
            .grant_write_permit(grant_params(&token, rfc, vec!["a.rs", "b.rs"]))
            .await
            .unwrap();
        let v1: serde_json::Value = serde_json::from_str(&result_text(&r1)).unwrap();
        let id1 = v1.get("id").unwrap().as_str().unwrap().to_string();

        // Replay with reversed path order.
        let r2 = server
            .grant_write_permit(grant_params(&token, rfc, vec!["b.rs", "a.rs"]))
            .await
            .unwrap();
        assert_ne!(
            r2.is_error,
            Some(true),
            "replay should succeed: {}",
            result_text(&r2)
        );
        let v2: serde_json::Value = serde_json::from_str(&result_text(&r2)).unwrap();
        let id2 = v2.get("id").unwrap().as_str().unwrap().to_string();

        assert_eq!(id1, id2, "idempotent replay must return the same permit id");

        // Exactly one permit in the DB.
        let engine = server.engine.lock().await;
        let snap = engine.snapshot_permits().unwrap();
        let count = snap.split('|').next().unwrap();
        assert_eq!(
            count, "1",
            "replay must not create a duplicate (snapshot={snap})"
        );
    }

    // T6 (edge): granting a second distinct permit does not remove the
    // first; a later rollback only touches the targeted id.
    #[tokio::test]
    async fn test_grant_write_permit_coexisting_permits() {
        let tmp = LocalTempDir::new("grant-coexist");
        init_git_repo(tmp.path());
        let (server, token) = setup_server(tmp.path()).await;

        let rfc_a = Uuid::new_v4();
        let rfc_b = Uuid::new_v4();
        server
            .grant_write_permit(grant_params(&token, rfc_a, vec!["a.rs"]))
            .await
            .unwrap();
        server
            .grant_write_permit(grant_params(&token, rfc_b, vec!["b.rs"]))
            .await
            .unwrap();

        let engine = server.engine.lock().await;
        let snap = engine.snapshot_permits().unwrap();
        let count = snap.split('|').next().unwrap();
        assert_eq!(count, "2", "both permits must coexist (edge e): {snap}");
    }

    // T8 (regression, lesson 8fd49300): the permit stays 'active' after
    // grant+seal — the auto-commit must NOT consume it (consume-last
    // invariant preserved).
    #[tokio::test]
    async fn test_grant_does_not_consume_permit() {
        let tmp = LocalTempDir::new("grant-active");
        init_git_repo(tmp.path());
        let (server, token) = setup_server(tmp.path()).await;
        let rfc = Uuid::new_v4();

        let res = server
            .grant_write_permit(grant_params(&token, rfc, vec!["a.rs"]))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&result_text(&res)).unwrap();
        assert_eq!(
            v.get("status").unwrap().as_str().unwrap(),
            "active",
            "grant+seal must leave the permit active (T8, consume-last)"
        );
        assert!(
            v.get("consumed_at").map(|c| c.is_null()).unwrap_or(true),
            "permit must not be consumed by the grant"
        );
    }

    // --- rfc_set_implemented response shape (RFC 1c0f2570 §7.b) ---

    #[test]
    fn test_build_set_implemented_response_success_shape() {
        let id = Uuid::parse_str("a0000000-0000-4000-8000-000000000010").unwrap();
        let outcome = companyos_orchestrator::engine::SetImplementedOutcome {
            rfc_id: id,
            previous_status: "approved".to_string(),
            new_status: "implemented".to_string(),
            implemented_at: "2026-06-04T12:00:00+00:00".to_string(),
            file_path: "company/rfcs/x.yml".to_string(),
            already_implemented: false,
        };
        let v = build_set_implemented_response(&outcome);
        assert_eq!(v.get("rfc_id").unwrap().as_str().unwrap(), id.to_string());
        assert_eq!(
            v.get("previous_status").unwrap().as_str().unwrap(),
            "approved"
        );
        assert_eq!(
            v.get("new_status").unwrap().as_str().unwrap(),
            "implemented"
        );
        assert_eq!(
            v.get("implemented_at").unwrap().as_str().unwrap(),
            "2026-06-04T12:00:00+00:00"
        );
        assert_eq!(
            v.get("file_path").unwrap().as_str().unwrap(),
            "company/rfcs/x.yml"
        );
        // No idempotent note on a real transition.
        assert!(v.get("note").is_none());
    }

    #[test]
    fn test_build_set_implemented_response_idempotent_shape() {
        let id = Uuid::parse_str("c0000000-0000-4000-8000-000000000010").unwrap();
        let outcome = companyos_orchestrator::engine::SetImplementedOutcome {
            rfc_id: id,
            previous_status: "implemented".to_string(),
            new_status: "implemented".to_string(),
            implemented_at: "2026-06-01T10:00:00+00:00".to_string(),
            file_path: "company/rfcs/y.yml".to_string(),
            already_implemented: true,
        };
        let v = build_set_implemented_response(&outcome);
        assert_eq!(
            v.get("new_status").unwrap().as_str().unwrap(),
            "implemented"
        );
        let note = v.get("note").expect("idempotent case must carry a note");
        assert!(note.as_str().unwrap().contains("idempotent"));
    }
}
