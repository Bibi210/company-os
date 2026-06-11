use companyos_config::{ArtifactKind, PersonaId, access_level, constants, protected_zones};
use companyos_orchestrator::{
    ArtifactPath, OrchestratorDb, OrchestratorEngine, PathPattern, ReviewVerdict, RfcUpdateResult,
};
use companyos_validation::{ArtifactValidator, SchemaRegistry, ValidationError};

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn setup_validator() -> ArtifactValidator {
    let schemas_dir = workspace_root().join("company/schemas");
    let registry = SchemaRegistry::load(&schemas_dir).expect("load schemas");
    ArtifactValidator::new(registry)
}

fn setup_engine() -> OrchestratorEngine {
    let db = OrchestratorDb::open(":memory:").expect("open db");
    db.migrate().expect("migrate");
    OrchestratorEngine::new_without_embedder(db, constants::DEFAULT_MAX_ITERATIONS)
}

// =====================================================================
// 1. Double-consume a permit → second consume returns Err
// =====================================================================
#[test]
fn test_double_consume_permit() {
    let engine = setup_engine();

    // Create a round and close it so we can grant a permit
    let round = engine
        .initiate_review_round(
            ArtifactPath("company/rfcs/rfc-double.yml".into()),
            ArtifactKind::Rfc,
            PersonaId::Architect,
            vec![PersonaId::Pm],
        )
        .expect("initiate review");

    engine
        .submit_vote(
            round.id,
            PersonaId::Pm,
            ReviewVerdict::Approve,
            vec![],
            None,
        )
        .expect("pm vote");
    engine.close_round(round.id).expect("close round");

    // Grant permit
    let permit = engine
        .grant_permit(
            round.id,
            PersonaId::Implementer,
            vec![PathPattern("company/config/test.yml".into())],
        )
        .expect("grant permit");

    // First consume → Ok
    engine.consume_permit(permit.id).expect("first consume");

    // Second consume → Err
    let result = engine.consume_permit(permit.id);
    assert!(
        result.is_err(),
        "consuming an already-consumed permit must fail"
    );
}

// =====================================================================
// 2. start_revision increments iteration and clears votes
// =====================================================================
#[test]
fn test_start_revision_increments_iteration() {
    let engine = setup_engine();

    let round = engine
        .initiate_review_round(
            ArtifactPath("company/projects/design-rev.yml".into()),
            ArtifactKind::DesignDoc,
            PersonaId::Architect,
            vec![PersonaId::Implementer],
        )
        .expect("initiate review");
    assert_eq!(round.iteration, 1);

    // Submit a vote so there is something to clear
    engine
        .submit_vote(
            round.id,
            PersonaId::Implementer,
            ReviewVerdict::RequestChanges,
            vec![],
            None,
        )
        .expect("vote");

    // Start revision → iteration bumps to 2, votes cleared
    let revised = engine.start_revision(round.id).expect("start revision");
    assert_eq!(revised.iteration, 2);
    assert!(
        revised.votes.is_empty(),
        "votes should be empty after revision"
    );
}

// =====================================================================
// 3. close_round_with_rfc_update on a non-RFC → NotAnRfc
// =====================================================================
#[test]
fn test_close_round_not_rfc() {
    let engine = setup_engine();

    let round = engine
        .initiate_review_round(
            ArtifactPath("company/projects/design-not-rfc.yml".into()),
            ArtifactKind::DesignDoc,
            PersonaId::Architect,
            vec![PersonaId::Pm],
        )
        .expect("initiate review");

    engine
        .submit_vote(
            round.id,
            PersonaId::Pm,
            ReviewVerdict::Approve,
            vec![],
            None,
        )
        .expect("pm vote");

    let root = workspace_root();
    let (_closed, rfc_result) = engine
        .close_round_with_rfc_update(round.id, root.to_str().unwrap())
        .expect("close round with rfc update");

    assert_eq!(
        rfc_result,
        RfcUpdateResult::NotAnRfc,
        "design-doc round should produce NotAnRfc"
    );
}

// =====================================================================
// 4. Validate all shipped YAML artifacts under company/
// =====================================================================
#[test]
fn test_validate_all_shipped_artifacts() {
    let validator = setup_validator();
    let company_dir = workspace_root().join("company");

    let results = validator.validate_dir(&company_dir);
    assert!(
        !results.is_empty(),
        "should find at least one YAML file under company/"
    );

    let mut failures = Vec::new();
    for (path, result) in &results {
        match result {
            Ok(report) if !report.is_valid => {
                failures.push(format!("{}: {:?}", path.display(), report.errors));
            }
            // Files without api_version are not CompanyOS artifacts — skip them
            Err(ValidationError::InvalidApiVersion { .. }) => {}
            Err(e) => {
                failures.push(format!("{}: {}", path.display(), e));
            }
            _ => {}
        }
    }

    assert!(
        failures.is_empty(),
        "shipped artifacts must all be valid:\n{}",
        failures.join("\n")
    );
}

// =====================================================================
// 5. Schema registry knows at least 8 kinds
// =====================================================================
#[test]
fn test_schema_registry_has_all_kinds() {
    let schemas_dir = workspace_root().join("company/schemas");
    let registry = SchemaRegistry::load(&schemas_dir).expect("load schemas");
    let kinds = registry.kinds();
    assert!(
        kinds.len() >= 8,
        "expected at least 8 schema kinds, got {}",
        kinds.len()
    );
}

// =====================================================================
// 6. Protected zones: crates/ path is protected, README.md is not
// =====================================================================
#[test]
fn test_protected_zones_real_config() {
    let root = workspace_root();

    assert!(
        protected_zones::is_protected(&root, "crates/foo.rs"),
        "crates/foo.rs should be protected"
    );
    assert!(
        !protected_zones::is_protected(&root, "README.md"),
        "README.md should NOT be protected"
    );
}

// =====================================================================
// 7. access_level never panics for any PersonaId × ArtifactKind combo
// =====================================================================
#[test]
fn test_access_matrix_no_panics() {
    let all_kinds = [
        ArtifactKind::TaskRequest,
        ArtifactKind::DesignDoc,
        ArtifactKind::ImplementationPlan,
        ArtifactKind::ReviewReport,
        ArtifactKind::Rfc,
        ArtifactKind::LessonLearned,
        ArtifactKind::DiagnosticReport,
        ArtifactKind::Persona,
        ArtifactKind::ProjectConfig,
        ArtifactKind::FlowControl,
        ArtifactKind::ReviewProtocol,
        ArtifactKind::HumanReviewTriggers,
        ArtifactKind::Roadmap,
    ];

    for persona in PersonaId::all() {
        for kind in &all_kinds {
            // Must not panic
            let _level = access_level(*persona, *kind);
        }
    }
}

// =====================================================================
// 8. Permit prefix matching — grant for directory, check sub-path
// =====================================================================
#[test]
fn test_permit_prefix_matching() {
    let engine = setup_engine();

    let round = engine
        .initiate_review_round(
            ArtifactPath("company/rfcs/rfc-prefix.yml".into()),
            ArtifactKind::Rfc,
            PersonaId::Architect,
            vec![PersonaId::Pm],
        )
        .expect("initiate review");

    engine
        .submit_vote(
            round.id,
            PersonaId::Pm,
            ReviewVerdict::Approve,
            vec![],
            None,
        )
        .expect("pm vote");
    engine.close_round(round.id).expect("close round");

    // Grant permit for a directory prefix
    engine
        .grant_permit(
            round.id,
            PersonaId::Implementer,
            vec![PathPattern("company/config/".into())],
        )
        .expect("grant permit");

    // Check that a file under that prefix is covered
    let found = engine
        .check_permit(PersonaId::Implementer, "company/config/flow-control.yml")
        .expect("check permit");
    assert!(
        found.is_some(),
        "permit for company/config/ should match company/config/flow-control.yml"
    );
}
