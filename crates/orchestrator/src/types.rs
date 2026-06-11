use chrono::{DateTime, Utc};
use companyos_config::{ArtifactKind, PersonaId};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

// --- Artifact Index ---

/// Lightweight search result returned by `search`. No full content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactSummary {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
}

/// Full indexed artifact record (metadata from the index).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexedArtifact {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    pub file_path: String,
    pub indexed_at: String,
}

/// A relation link from the `related()` query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationLink {
    pub id: String,
    pub kind: Option<String>,
    pub title: Option<String>,
    pub relationship: String,
    pub direction: RelationDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationDirection {
    Outgoing,
    Incoming,
}

/// Parsed relation from a YAML artifact's metadata.related[].
pub struct ParsedRelation {
    pub target_id: String,
    pub relationship: String,
}

// --- Newtypes ---

/// File path to an artifact being reviewed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ArtifactPath(pub String);

impl fmt::Display for ArtifactPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A single finding or comment from a reviewer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Finding(pub String);

/// A file path or glob pattern for write permit targets.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PathPattern(pub String);

// --- Review Round ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approve,
    RequestChanges,
}

impl fmt::Display for ReviewVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Approve => f.write_str("approve"),
            Self::RequestChanges => f.write_str("request_changes"),
        }
    }
}

impl FromStr for ReviewVerdict {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "approve" => Ok(Self::Approve),
            "request_changes" => Ok(Self::RequestChanges),
            _ => Err(format!(
                "unknown verdict: '{s}'. Use 'approve' or 'request_changes'"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundStatus {
    Open,
    ConsensusReached,
    RevisionRequired,
    Escalated,
    Closed,
}

impl fmt::Display for RoundStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open => f.write_str("open"),
            Self::ConsensusReached => f.write_str("consensus_reached"),
            Self::RevisionRequired => f.write_str("revision_required"),
            Self::Escalated => f.write_str("escalated"),
            Self::Closed => f.write_str("closed"),
        }
    }
}

impl FromStr for RoundStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "open" => Ok(Self::Open),
            "consensus_reached" => Ok(Self::ConsensusReached),
            "revision_required" => Ok(Self::RevisionRequired),
            "escalated" => Ok(Self::Escalated),
            "closed" => Ok(Self::Closed),
            _ => Err(format!("unknown round status: '{s}'")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsensusResult {
    ConsensusReached,
    RevisionRequired,
    EscalationNeeded,
    WaitingForVotes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRound {
    pub id: Uuid,
    pub artifact_path: ArtifactPath,
    pub artifact_kind: ArtifactKind,
    pub author: PersonaId,
    pub required_reviewers: Vec<PersonaId>,
    pub status: RoundStatus,
    pub iteration: u32,
    pub max_iterations: u32,
    pub votes: Vec<ReviewVote>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewVote {
    pub reviewer: PersonaId,
    pub verdict: ReviewVerdict,
    pub findings: Vec<Finding>,
    /// Non-corrective observations for the record (GARDE 2b, RFC 8bf78218).
    /// `#[serde(default)]` keeps backward compatibility with votes already
    /// persisted in the DB before this field existed (they deserialize to
    /// `None`, no migration).
    #[serde(default)]
    pub notes: Option<String>,
    pub submitted_at: DateTime<Utc>,
}

// --- Write Permit ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermitStatus {
    Active,
    Consumed,
    Revoked,
}

impl fmt::Display for PermitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active => f.write_str("active"),
            Self::Consumed => f.write_str("consumed"),
            Self::Revoked => f.write_str("revoked"),
        }
    }
}

impl FromStr for PermitStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "consumed" => Ok(Self::Consumed),
            "revoked" => Ok(Self::Revoked),
            _ => Err(format!("unknown permit status: '{s}'")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WritePermit {
    pub id: Uuid,
    pub rfc_id: Uuid,
    pub granted_to: PersonaId,
    pub target_paths: Vec<PathPattern>,
    pub status: PermitStatus,
    pub granted_by: PersonaId,
    pub granted_at: DateTime<Utc>,
    pub consumed_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    // --- ReviewVerdict ---

    #[test]
    fn review_verdict_parse_approve() {
        assert_eq!(
            ReviewVerdict::from_str("approve").unwrap(),
            ReviewVerdict::Approve
        );
    }

    #[test]
    fn review_verdict_parse_request_changes() {
        assert_eq!(
            ReviewVerdict::from_str("request_changes").unwrap(),
            ReviewVerdict::RequestChanges
        );
    }

    #[test]
    fn review_verdict_parse_reject_is_err() {
        assert!(ReviewVerdict::from_str("reject").is_err());
    }

    #[test]
    fn review_verdict_display_roundtrip_approve() {
        let v = ReviewVerdict::Approve;
        assert_eq!(ReviewVerdict::from_str(&v.to_string()).unwrap(), v);
    }

    #[test]
    fn review_verdict_display_roundtrip_request_changes() {
        let v = ReviewVerdict::RequestChanges;
        assert_eq!(ReviewVerdict::from_str(&v.to_string()).unwrap(), v);
    }

    // --- RoundStatus ---

    #[test]
    fn round_status_parse_all_valid() {
        let cases = [
            ("open", RoundStatus::Open),
            ("consensus_reached", RoundStatus::ConsensusReached),
            ("revision_required", RoundStatus::RevisionRequired),
            ("escalated", RoundStatus::Escalated),
            ("closed", RoundStatus::Closed),
        ];
        for (input, expected) in cases {
            assert_eq!(RoundStatus::from_str(input).unwrap(), expected);
        }
    }

    #[test]
    fn round_status_parse_invalid_is_err() {
        assert!(RoundStatus::from_str("unknown").is_err());
    }

    #[test]
    fn round_status_display_roundtrip() {
        let variants = [
            RoundStatus::Open,
            RoundStatus::ConsensusReached,
            RoundStatus::RevisionRequired,
            RoundStatus::Escalated,
            RoundStatus::Closed,
        ];
        for v in variants {
            assert_eq!(RoundStatus::from_str(&v.to_string()).unwrap(), v);
        }
    }

    // --- PermitStatus ---

    #[test]
    fn permit_status_parse_valid() {
        assert_eq!(
            PermitStatus::from_str("active").unwrap(),
            PermitStatus::Active
        );
        assert_eq!(
            PermitStatus::from_str("consumed").unwrap(),
            PermitStatus::Consumed
        );
        assert_eq!(
            PermitStatus::from_str("revoked").unwrap(),
            PermitStatus::Revoked
        );
    }

    #[test]
    fn permit_status_parse_invalid_is_err() {
        assert!(PermitStatus::from_str("expired").is_err());
    }
}
