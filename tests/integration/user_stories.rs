use companyos_config::{ArtifactKind, PersonaId, constants};
use companyos_orchestrator::{
    ArtifactPath, ConsensusResult, Finding, OrchestratorDb, OrchestratorEngine, PathPattern,
    ReviewVerdict,
};
use companyos_validation::{ArtifactValidator, SchemaRegistry};

fn workspace_root() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR points to tests/integration/
    // Go up 2 levels to reach the workspace root
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn setup_validator() -> ArtifactValidator {
    // Schemas live at company/schemas/ (not schemas/) relative to workspace root
    let schemas_dir = workspace_root().join("company/schemas");
    let registry = SchemaRegistry::load(&schemas_dir).expect("load schemas");
    ArtifactValidator::new(registry)
}

fn setup_engine() -> OrchestratorEngine {
    // In-memory SQLite for tests
    let db = OrchestratorDb::open(":memory:").expect("open db");
    db.migrate().expect("migrate");
    OrchestratorEngine::new_without_embedder(db, constants::DEFAULT_MAX_ITERATIONS)
}

// =====================================================================
// Story 1: Simple task lifecycle
// =====================================================================
#[test]
fn story1_simple_task_lifecycle() {
    let validator = setup_validator();
    let engine = setup_engine();

    // 1. Create and validate a task-request
    let task_yaml = r#"
api_version: "companyos/v1"
kind: task-request
metadata:
  id: a0000001-0000-4000-8000-000000000001
  title: "Build user dashboard"
  author: pm
  created_at: "2026-01-01"
spec:
  description: "Build a real-time user dashboard"
  acceptance_criteria:
    - "Shows live metrics"
    - "Responsive design"
  priority: high
"#;
    let report = validator
        .validate_yaml_str(task_yaml)
        .expect("validate task");
    assert!(
        report.is_valid,
        "task-request should be valid: {:?}",
        report.errors
    );
    assert_eq!(report.kind, ArtifactKind::TaskRequest);

    // 2. Create and validate a design-doc
    let design_yaml = r#"
api_version: "companyos/v1"
kind: design-doc
metadata:
  id: a0000004-0000-4000-8000-000000000004
  title: "Dashboard design"
  author: architect
  created_at: "2026-01-01"
spec:
  overview: "WebSocket-based real-time dashboard"
  components:
    - name: "ws-server"
      description: "WebSocket relay"
  decisions:
    - decision: "Use WebSocket over SSE"
      rationale: "Need bidirectional communication"
"#;
    let report = validator
        .validate_yaml_str(design_yaml)
        .expect("validate design-doc");
    assert!(
        report.is_valid,
        "design-doc should be valid: {:?}",
        report.errors
    );

    // 3. Initiate review round for the design-doc
    let round = engine
        .initiate_review_round(
            ArtifactPath("company/projects/design-001.yml".into()),
            ArtifactKind::DesignDoc,
            PersonaId::Architect,
            vec![PersonaId::Pm, PersonaId::Implementer],
        )
        .expect("initiate review");
    assert_eq!(round.iteration, 1);

    // 4. PM approves
    engine
        .submit_vote(round.id, PersonaId::Pm, ReviewVerdict::Approve, vec![])
        .expect("pm vote");

    // Check: not yet consensus (waiting for implementer)
    let result = engine.check_consensus(round.id).expect("check consensus");
    assert_eq!(result, ConsensusResult::WaitingForVotes);

    // 5. Implementer approves
    engine
        .submit_vote(
            round.id,
            PersonaId::Implementer,
            ReviewVerdict::Approve,
            vec![],
        )
        .expect("implementer vote");

    // 6. Consensus reached
    let result = engine.check_consensus(round.id).expect("check consensus");
    assert_eq!(result, ConsensusResult::ConsensusReached);

    // 7. Close the round
    let closed = engine.close_round(round.id).expect("close round");
    assert_eq!(closed.status.to_string(), "closed");

    // 8. Create and validate implementation-plan
    let impl_yaml = r#"
api_version: "companyos/v1"
kind: implementation-plan
metadata:
  id: a0000005-0000-4000-8000-000000000005
  title: "Dashboard implementation plan"
  author: implementer
  created_at: "2026-01-01"
spec:
  steps:
    - description: "Set up WebSocket server"
      estimated_effort: "2 days"
    - description: "Build dashboard UI"
      estimated_effort: "3 days"
  dependencies:
    - "tokio"
    - "tungstenite"
"#;
    let report = validator
        .validate_yaml_str(impl_yaml)
        .expect("validate impl-plan");
    assert!(
        report.is_valid,
        "impl-plan should be valid: {:?}",
        report.errors
    );

    // 9. Validate a lesson-learned
    let lesson_yaml = r#"
api_version: "companyos/v1"
kind: lesson-learned
metadata:
  id: a0000006-0000-4000-8000-000000000006
  title: "WebSocket vs SSE for dashboards"
  description: "WebSocket provides better bidirectional support than SSE for interactive dashboards."
  author: architect
  created_at: "2026-01-01"
  tags:
    - architecture
    - real-time
spec:
  context: "Evaluated SSE vs WebSocket for dashboard"
  insight: "WebSocket provides better bidirectional support"
  recommendation: "Use WebSocket for interactive dashboards, SSE for one-way feeds"
"#;
    let report = validator
        .validate_yaml_str(lesson_yaml)
        .expect("validate lesson");
    assert!(
        report.is_valid,
        "lesson-learned should be valid: {:?}",
        report.errors
    );

    println!("Story 1 PASSED: simple task lifecycle");
}

// =====================================================================
// Story 2: Review with revision
// =====================================================================
#[test]
fn story2_review_with_revision() {
    let engine = setup_engine();

    // 1. Initiate review
    let round = engine
        .initiate_review_round(
            ArtifactPath("company/projects/design-002.yml".into()),
            ArtifactKind::DesignDoc,
            PersonaId::Architect,
            vec![PersonaId::Pm, PersonaId::Implementer],
        )
        .expect("initiate review");

    // 2. PM approves but Implementer requests changes
    engine
        .submit_vote(round.id, PersonaId::Pm, ReviewVerdict::Approve, vec![])
        .expect("pm vote");
    engine
        .submit_vote(
            round.id,
            PersonaId::Implementer,
            ReviewVerdict::RequestChanges,
            vec![Finding("Missing error handling section".into())],
        )
        .expect("implementer vote");

    // 3. Check consensus → RevisionRequired
    let result = engine.check_consensus(round.id).expect("check consensus");
    assert_eq!(result, ConsensusResult::RevisionRequired);

    // 4. Start revision (bumps iteration, clears votes)
    let revised = engine.start_revision(round.id).expect("start revision");
    assert_eq!(revised.iteration, 2);
    assert!(revised.votes.is_empty());

    // 5. Both reviewers approve after revision
    engine
        .submit_vote(round.id, PersonaId::Pm, ReviewVerdict::Approve, vec![])
        .expect("pm re-vote");
    engine
        .submit_vote(
            round.id,
            PersonaId::Implementer,
            ReviewVerdict::Approve,
            vec![],
        )
        .expect("implementer re-vote");

    // 6. Consensus reached
    let result = engine.check_consensus(round.id).expect("check consensus");
    assert_eq!(result, ConsensusResult::ConsensusReached);

    println!("Story 2 PASSED: review with revision");
}

// =====================================================================
// Story 3: RFC + write permit
// =====================================================================
#[test]
fn story3_rfc_write_permit() {
    let validator = setup_validator();
    let engine = setup_engine();

    // 1. Validate an RFC
    let rfc_yaml = r#"
api_version: "companyos/v1"
kind: rfc
metadata:
  id: a0000002-0000-4000-8000-000000000002
  title: "Increase max review iterations"
  author: architect
  created_at: "2026-01-01"
spec:
  motivation: "Current limit of 3 is too restrictive for complex reviews"
  proposal: "Increase max_review_iterations from 3 to 5"
  impact: "Affects flow-control.yml configuration"
  rollback_plan: "Revert flow-control.yml to previous value"
"#;
    let report = validator.validate_yaml_str(rfc_yaml).expect("validate rfc");
    assert!(report.is_valid, "rfc should be valid: {:?}", report.errors);

    // 2. Review round for RFC → approved
    let round = engine
        .initiate_review_round(
            ArtifactPath("company/rfcs/rfc-001.yml".into()),
            ArtifactKind::Rfc,
            PersonaId::Architect,
            vec![PersonaId::Pm, PersonaId::Ceo],
        )
        .expect("initiate rfc review");

    engine
        .submit_vote(round.id, PersonaId::Pm, ReviewVerdict::Approve, vec![])
        .expect("pm vote");
    engine
        .submit_vote(round.id, PersonaId::Ceo, ReviewVerdict::Approve, vec![])
        .expect("ceo vote");

    let result = engine.check_consensus(round.id).expect("check");
    assert_eq!(result, ConsensusResult::ConsensusReached);
    engine.close_round(round.id).expect("close");

    // 3. Grant write permit
    let permit = engine
        .grant_permit(
            round.id, // using round.id as rfc_id for simplicity
            PersonaId::Implementer,
            vec![PathPattern("company/config/flow-control.yml".into())],
        )
        .expect("grant permit");

    // 4. Check permit exists
    let found = engine
        .check_permit(PersonaId::Implementer, "company/config/flow-control.yml")
        .expect("check permit");
    assert!(found.is_some(), "permit should exist");
    assert_eq!(found.unwrap().id, permit.id);

    // 5. Consume permit
    engine.consume_permit(permit.id).expect("consume permit");

    // 6. Check permit is now consumed
    let found = engine
        .check_permit(PersonaId::Implementer, "company/config/flow-control.yml")
        .expect("check permit after consume");
    assert!(found.is_none(), "permit should be consumed");

    println!("Story 3 PASSED: RFC + write permit");
}

// =====================================================================
// Story 4: Escalation after max iterations
// =====================================================================
#[test]
fn story4_escalation() {
    let engine = setup_engine(); // max_iterations = 3

    // 1. Initiate review
    let round = engine
        .initiate_review_round(
            ArtifactPath("company/projects/design-003.yml".into()),
            ArtifactKind::DesignDoc,
            PersonaId::Architect,
            vec![PersonaId::Implementer],
        )
        .expect("initiate review");

    // Iteration 1: request changes
    engine
        .submit_vote(
            round.id,
            PersonaId::Implementer,
            ReviewVerdict::RequestChanges,
            vec![Finding("Needs more detail".into())],
        )
        .expect("vote iter 1");
    let result = engine.check_consensus(round.id).expect("check");
    assert_eq!(result, ConsensusResult::RevisionRequired);

    // Iteration 2: request changes again
    engine.start_revision(round.id).expect("revision 2");
    engine
        .submit_vote(
            round.id,
            PersonaId::Implementer,
            ReviewVerdict::RequestChanges,
            vec![Finding("Still needs work".into())],
        )
        .expect("vote iter 2");
    let result = engine.check_consensus(round.id).expect("check");
    assert_eq!(result, ConsensusResult::RevisionRequired);

    // Iteration 3: request changes again → should trigger escalation
    engine.start_revision(round.id).expect("revision 3");
    engine
        .submit_vote(
            round.id,
            PersonaId::Implementer,
            ReviewVerdict::RequestChanges,
            vec![Finding("Fundamental design issue".into())],
        )
        .expect("vote iter 3");
    let result = engine.check_consensus(round.id).expect("check");
    assert_eq!(result, ConsensusResult::EscalationNeeded);

    println!("Story 4 PASSED: escalation after max iterations");
}

// =====================================================================
// Regression tests — RFC rfc-validator-fix-001
// allOf + required imbriqué : jsonschema 0.29+ doit enforcer les champs
// required dans spec et dans les sous-schémas de properties
// =====================================================================

/// CAS NOMINAL : task-request complet avec tous les champs required → is_valid: true
#[test]
fn test_valid_task_request_passes() {
    let validator = setup_validator();
    let yaml = r#"
api_version: "companyos/v1"
kind: task-request
metadata:
  id: a0000003-0000-4000-8000-000000000003
  title: "Valid task request"
  author: pm
  created_at: "2026-01-01"
spec:
  acceptance_criteria:
    - "Criterion A"
    - "Criterion B"
  priority: high
"#;
    let report = validator.validate_yaml_str(yaml).expect("validate");
    assert!(
        report.is_valid,
        "valid task-request should pass: {:?}",
        report.errors
    );
}

/// CAS NÉGATIF : spec.acceptance_criteria absent → is_valid: false
/// Régression directe du bug allOf+required imbriqué de jsonschema 0.28
#[test]
fn test_task_request_missing_required_spec_field() {
    let validator = setup_validator();
    let yaml = r#"
api_version: "companyos/v1"
kind: task-request
metadata:
  id: test-invalid-001
  title: "Invalid task — no acceptance_criteria"
  author: pm
  created_at: "2026-01-01"
spec:
  priority: high
"#;
    let report = validator.validate_yaml_str(yaml).expect("validate");
    assert!(
        !report.is_valid,
        "task-request without acceptance_criteria MUST fail validation"
    );
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("acceptance_criteria")),
        "error must mention 'acceptance_criteria', got: {:?}",
        report.errors
    );
}

/// CAS NÉGATIF : spec entier absent → is_valid: false
#[test]
fn test_task_request_missing_spec() {
    let validator = setup_validator();
    let yaml = r#"
api_version: "companyos/v1"
kind: task-request
metadata:
  id: test-invalid-002
  title: "Invalid task — no spec"
  author: pm
"#;
    let report = validator.validate_yaml_str(yaml).expect("validate");
    assert!(
        !report.is_valid,
        "task-request without spec MUST fail validation"
    );
}

/// CAS NÉGATIF : RFC sans spec.motivation → is_valid: false
#[test]
fn test_rfc_missing_required_spec_field() {
    let validator = setup_validator();
    let yaml = r#"
api_version: "companyos/v1"
kind: rfc
metadata:
  id: rfc-test-invalid
  title: "Incomplete RFC — no motivation"
  author: architect
  created_at: "2026-01-01"
spec:
  proposal: "Some proposal"
  impact: "Some impact"
"#;
    let report = validator.validate_yaml_str(yaml).expect("validate");
    assert!(
        !report.is_valid,
        "rfc without motivation MUST fail validation"
    );
}
