use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

use crate::constants;

// --- Newtypes for domain text ---

/// Unique identifier for an artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ArtifactId(pub String);

impl fmt::Display for ArtifactId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// DSL condition expression for human review triggers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TriggerCondition(pub String);

/// DSL action expression for human review triggers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TriggerAction(pub String);

/// Human-readable description text.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Description(pub String);

/// A single rule (must-do or never-do) for a persona.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Rule(pub String);

// --- API Version ---

/// API version marker. Always `constants::API_VERSION`.
/// Validated on deserialization, constant on serialization.
#[derive(Debug, Clone)]
pub struct ApiVersion;

impl Serialize for ApiVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(constants::API_VERSION)
    }
}

impl<'de> Deserialize<'de> for ApiVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s == constants::API_VERSION {
            Ok(ApiVersion)
        } else {
            Err(serde::de::Error::custom(format!(
                "invalid api_version: expected '{}', got '{s}'",
                constants::API_VERSION
            )))
        }
    }
}

// --- Persona ID ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PersonaId {
    Pm,
    Architect,
    Implementer,
    Ceo,
    Myself, // Special persona representing the agent itself, used for self-imposed rules and review requirements.
    All, // Special persona representing all personas, used for review protocols that apply to all personas.
}

impl PersonaId {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pm => "pm",
            Self::Architect => "architect",
            Self::Implementer => "implementer",
            Self::Ceo => "ceo",
            Self::Myself => "myself",
            Self::All => "all",
        }
    }

    pub fn all() -> &'static [PersonaId] {
        &[
            Self::Pm,
            Self::Architect,
            Self::Implementer,
            Self::Ceo,
            Self::Myself,
            Self::All,
        ]
    }
}

impl fmt::Display for PersonaId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PersonaId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pm" => Ok(Self::Pm),
            "architect" => Ok(Self::Architect),
            "implementer" => Ok(Self::Implementer),
            "ceo" => Ok(Self::Ceo),
            "myself" => Ok(Self::Myself),
            "all" => Ok(Self::All),
            _ => Err(format!("unknown persona: '{s}'")),
        }
    }
}

// --- Artifact Kind ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    TaskRequest,
    DesignDoc,
    ImplementationPlan,
    ReviewReport,
    Rfc,
    LessonLearned,
    DiagnosticReport,
    Persona,
    AgentMessage,
    ProjectConfig,
    FlowControl,
    ReviewProtocol,
    HumanReviewTriggers,
    Roadmap,
}

impl ArtifactKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TaskRequest => "task-request",
            Self::DesignDoc => "design-doc",
            Self::ImplementationPlan => "implementation-plan",
            Self::ReviewReport => "review-report",
            Self::Rfc => "rfc",
            Self::LessonLearned => "lesson-learned",
            Self::DiagnosticReport => "diagnostic-report",
            Self::Persona => "persona",
            Self::AgentMessage => "agent-message",
            Self::ProjectConfig => "project-config",
            Self::FlowControl => "flow-control",
            Self::ReviewProtocol => "review-protocol",
            Self::HumanReviewTriggers => "human-review-triggers",
            Self::Roadmap => "roadmap",
        }
    }

    pub fn schema_filename(&self) -> String {
        format!("{}{}", self.as_str(), constants::SCHEMA_EXTENSION)
    }
}

impl fmt::Display for ArtifactKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ArtifactKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "task-request" => Ok(Self::TaskRequest),
            "design-doc" => Ok(Self::DesignDoc),
            "implementation-plan" => Ok(Self::ImplementationPlan),
            "review-report" => Ok(Self::ReviewReport),
            "rfc" => Ok(Self::Rfc),
            "lesson-learned" => Ok(Self::LessonLearned),
            "diagnostic-report" => Ok(Self::DiagnosticReport),
            "persona" => Ok(Self::Persona),
            "agent-message" => Ok(Self::AgentMessage),
            "project-config" => Ok(Self::ProjectConfig),
            "flow-control" => Ok(Self::FlowControl),
            "review-protocol" => Ok(Self::ReviewProtocol),
            "human-review-triggers" => Ok(Self::HumanReviewTriggers),
            "roadmap" => Ok(Self::Roadmap),
            _ => Err(format!("unknown artifact kind: '{s}'")),
        }
    }
}

// --- Access Level ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessLevel {
    Write,
    Read,
    Permit,
}

/// Returns the access level for a given persona and artifact kind,
/// based on the access matrix defined in the design doc.
pub fn access_level(persona: PersonaId, kind: ArtifactKind) -> AccessLevel {
    use AccessLevel::*;
    use ArtifactKind::*;
    use PersonaId::*;

    match (persona, kind) {
        (Pm, TaskRequest) => Write,
        (Pm, LessonLearned) => Write,
        (Pm, Roadmap) => Write,
        (Pm, _) => Read,

        (Architect, DesignDoc) => Write,
        (Architect, Rfc) => Write,
        (Architect, LessonLearned) => Write,
        (Architect, Roadmap) => Read,
        (Architect, _) => Read,

        (Implementer, ImplementationPlan) => Write,
        (Implementer, DiagnosticReport) => Write,
        (Implementer, LessonLearned) => Write,
        (Implementer, FlowControl) => Permit,
        (Implementer, ReviewProtocol) => Permit,
        (Implementer, HumanReviewTriggers) => Permit,
        (Implementer, Persona) => Permit,
        (Implementer, Roadmap) => Read,
        (Implementer, _) => Read,

        (Myself, _) => Read,
        (All, _) => Read,
        (Ceo, _) => Read,
    }
}

// --- Config types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactEnvelope<S> {
    pub api_version: ApiVersion,
    pub kind: ArtifactKind,
    pub metadata: Metadata,
    #[serde(flatten)]
    pub spec: S,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub id: ArtifactId,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

// --- Flow Control ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowControlSpec {
    pub spec: FlowControl,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowControl {
    pub max_review_iterations: u32,
    #[serde(default)]
    pub budget_limits: Option<BudgetLimits>,
    #[serde(default)]
    pub circuit_breaker: Option<CircuitBreaker>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetLimits {
    pub max_tokens_per_task: u64,
    pub max_subagent_spawns: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreaker {
    pub consecutive_failures_threshold: u32,
    pub cooldown_seconds: u64,
}

// --- Review Protocol ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewProtocolSpec {
    pub spec: ReviewProtocol,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewProtocol {
    pub reviewers_by_artifact_type: std::collections::HashMap<ArtifactKind, Vec<PersonaId>>,
    #[serde(default)]
    pub escalation_rules: Option<EscalationRules>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscalationRules {
    pub max_iterations_before_ceo: u32,
    pub ceo_cannot_approve_alone: Vec<EscalationCategory>,
}

/// Categories of sensitive changes that the CEO cannot approve alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EscalationCategory {
    ModifyAccessMatrix,
    ModifyCeoPersona,
    BreakingSchemaChange,
    DeletePersona,
}

// --- Human Review Triggers ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HumanReviewTriggersSpec {
    pub spec: HumanReviewTriggers,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HumanReviewTriggers {
    pub triggers: Vec<HumanReviewTrigger>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HumanReviewTrigger {
    pub condition: TriggerCondition,
    pub action: TriggerAction,
    pub description: Description,
}

// --- Persona ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonaSpec {
    pub identity: Description,
    pub rules: PersonaRules,
    pub artifacts: PersonaArtifacts,
    pub review_behavior: Description,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonaRules {
    pub must: Vec<Rule>,
    pub never: Vec<Rule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonaArtifacts {
    pub produces: Vec<ArtifactKind>,
    pub consumes: Vec<ArtifactKind>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- PersonaId tests ---

    #[test]
    fn test_persona_id_parse_valid() {
        let cases = [
            ("pm", PersonaId::Pm),
            ("architect", PersonaId::Architect),
            ("implementer", PersonaId::Implementer),
            ("ceo", PersonaId::Ceo),
            ("myself", PersonaId::Myself),
            ("all", PersonaId::All),
        ];
        for (input, expected) in cases {
            assert_eq!(input.parse::<PersonaId>().unwrap(), expected);
        }
    }

    #[test]
    fn test_persona_id_parse_invalid() {
        assert!("admin".parse::<PersonaId>().is_err());
    }

    #[test]
    fn test_persona_id_display_roundtrip() {
        for &variant in PersonaId::all() {
            let s = variant.to_string();
            let parsed: PersonaId = s.parse().unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn test_persona_id_all_count() {
        assert_eq!(PersonaId::all().len(), 6);
    }

    #[test]
    fn test_persona_id_serde_roundtrip() {
        for &variant in PersonaId::all() {
            let json = serde_json::to_string(&variant).unwrap();
            let deserialized: PersonaId = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, variant);
        }
    }

    // --- ArtifactKind tests ---

    #[test]
    fn test_artifact_kind_parse_valid() {
        let cases = [
            "task-request",
            "design-doc",
            "implementation-plan",
            "review-report",
            "rfc",
            "lesson-learned",
            "diagnostic-report",
            "persona",
            "agent-message",
            "project-config",
            "flow-control",
            "review-protocol",
            "human-review-triggers",
            "roadmap",
        ];
        for input in cases {
            assert!(
                input.parse::<ArtifactKind>().is_ok(),
                "failed to parse '{input}'"
            );
        }
    }

    #[test]
    fn test_artifact_kind_parse_invalid() {
        assert!("foo".parse::<ArtifactKind>().is_err());
    }

    #[test]
    fn test_artifact_kind_display_roundtrip() {
        let all_kinds = [
            ArtifactKind::TaskRequest,
            ArtifactKind::DesignDoc,
            ArtifactKind::ImplementationPlan,
            ArtifactKind::ReviewReport,
            ArtifactKind::Rfc,
            ArtifactKind::LessonLearned,
            ArtifactKind::DiagnosticReport,
            ArtifactKind::Persona,
            ArtifactKind::AgentMessage,
            ArtifactKind::ProjectConfig,
            ArtifactKind::FlowControl,
            ArtifactKind::ReviewProtocol,
            ArtifactKind::HumanReviewTriggers,
            ArtifactKind::Roadmap,
        ];
        for variant in all_kinds {
            let s = variant.to_string();
            let parsed: ArtifactKind = s.parse().unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn test_artifact_kind_schema_filename() {
        assert_eq!(
            ArtifactKind::TaskRequest.schema_filename(),
            "task-request.schema.json"
        );
    }

    // --- ApiVersion tests ---

    #[test]
    fn test_api_version_serialize() {
        let value = serde_json::to_value(ApiVersion).unwrap();
        assert_eq!(value, serde_json::json!("companyos/v1"));
    }

    #[test]
    fn test_api_version_deserialize_invalid() {
        let result = serde_json::from_value::<ApiVersion>(serde_json::json!("companyos/v2"));
        assert!(result.is_err());
    }

    // --- access_level tests ---

    #[test]
    fn test_access_level_write_cases() {
        assert_eq!(
            access_level(PersonaId::Pm, ArtifactKind::TaskRequest),
            AccessLevel::Write
        );
        assert_eq!(
            access_level(PersonaId::Architect, ArtifactKind::DesignDoc),
            AccessLevel::Write
        );
        assert_eq!(
            access_level(PersonaId::Implementer, ArtifactKind::ImplementationPlan),
            AccessLevel::Write
        );
    }

    #[test]
    fn test_access_level_roadmap() {
        assert_eq!(
            access_level(PersonaId::Pm, ArtifactKind::Roadmap),
            AccessLevel::Write
        );
        assert_eq!(
            access_level(PersonaId::Architect, ArtifactKind::Roadmap),
            AccessLevel::Read
        );
        assert_eq!(
            access_level(PersonaId::Implementer, ArtifactKind::Roadmap),
            AccessLevel::Read
        );
        assert_eq!(
            access_level(PersonaId::Ceo, ArtifactKind::Roadmap),
            AccessLevel::Read
        );
    }

    #[test]
    fn test_access_level_permit_and_read_cases() {
        assert_eq!(
            access_level(PersonaId::Implementer, ArtifactKind::FlowControl),
            AccessLevel::Permit
        );
        assert_eq!(
            access_level(PersonaId::Ceo, ArtifactKind::TaskRequest),
            AccessLevel::Read
        );
        assert_eq!(
            access_level(PersonaId::Myself, ArtifactKind::DesignDoc),
            AccessLevel::Read
        );
        assert_eq!(
            access_level(PersonaId::All, ArtifactKind::Rfc),
            AccessLevel::Read
        );
    }
}
