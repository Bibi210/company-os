use std::path::Path;

use chrono::Utc;
use companyos_config::PersonaId;
use rusqlite::{Connection, params};
use uuid::Uuid;

use crate::error::OrchestratorError;
use crate::types::*;

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS review_rounds (
    id TEXT PRIMARY KEY,
    artifact_path TEXT NOT NULL,
    artifact_kind TEXT NOT NULL,
    author TEXT NOT NULL,
    required_reviewers TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'open',
    iteration INTEGER NOT NULL DEFAULT 1,
    max_iterations INTEGER NOT NULL DEFAULT 3,
    votes TEXT NOT NULL DEFAULT '[]',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS write_permits (
    id TEXT PRIMARY KEY,
    rfc_id TEXT NOT NULL,
    granted_to TEXT NOT NULL,
    target_paths TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    granted_by TEXT NOT NULL,
    granted_at TEXT NOT NULL,
    consumed_at TEXT
);

CREATE TABLE IF NOT EXISTS artifacts (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    description TEXT NOT NULL DEFAULT '',
    tags TEXT NOT NULL DEFAULT '[]',
    file_path TEXT NOT NULL,
    indexed_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS artifact_relations (
    source_id TEXT NOT NULL,
    target_id TEXT NOT NULL,
    relationship TEXT NOT NULL,
    PRIMARY KEY (source_id, target_id, relationship)
);

CREATE INDEX IF NOT EXISTS idx_rounds_status ON review_rounds(status);
CREATE INDEX IF NOT EXISTS idx_permits_status ON write_permits(status);
CREATE INDEX IF NOT EXISTS idx_permits_granted_to ON write_permits(granted_to);
CREATE INDEX IF NOT EXISTS idx_artifacts_kind ON artifacts(kind);
CREATE INDEX IF NOT EXISTS idx_relations_target ON artifact_relations(target_id);
";

const FTS_SQL: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS artifacts_fts USING fts5(
    id UNINDEXED,
    kind,
    title,
    description,
    tags,
    content
);
";

pub struct OrchestratorDb {
    conn: Connection,
}

impl OrchestratorDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, OrchestratorError> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self, OrchestratorError> {
        let conn = Connection::open_in_memory()?;
        Ok(Self { conn })
    }

    pub fn migrate(&self) -> Result<(), OrchestratorError> {
        self.conn.execute_batch(SCHEMA_SQL)?;
        self.conn.execute_batch(FTS_SQL)?;
        Ok(())
    }

    // --- Review Rounds ---

    pub fn create_round(&self, round: &ReviewRound) -> Result<(), OrchestratorError> {
        let reviewers_json = serde_json::to_string(&round.required_reviewers)?;
        let votes_json = serde_json::to_string(&round.votes)?;

        self.conn.execute(
            "INSERT INTO review_rounds (id, artifact_path, artifact_kind, author, required_reviewers, status, iteration, max_iterations, votes, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                round.id.to_string(),
                round.artifact_path.to_string(),
                round.artifact_kind.as_str(),
                round.author.as_str(),
                reviewers_json,
                round.status.to_string(),
                round.iteration,
                round.max_iterations,
                votes_json,
                round.created_at.to_rfc3339(),
                round.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn get_round(&self, id: Uuid) -> Result<Option<ReviewRound>, OrchestratorError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, artifact_path, artifact_kind, author, required_reviewers, status, iteration, max_iterations, votes, created_at, updated_at FROM review_rounds WHERE id = ?1")?;

        let result = stmt.query_row(params![id.to_string()], |row| {
            Ok(RoundRow {
                id: row.get(0)?,
                artifact_path: row.get(1)?,
                artifact_kind: row.get(2)?,
                author: row.get(3)?,
                required_reviewers: row.get(4)?,
                status: row.get(5)?,
                iteration: row.get(6)?,
                max_iterations: row.get(7)?,
                votes: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        });

        match result {
            Ok(row) => Ok(Some(row.into_round()?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn update_round(&self, round: &ReviewRound) -> Result<(), OrchestratorError> {
        let votes_json = serde_json::to_string(&round.votes)?;

        self.conn.execute(
            "UPDATE review_rounds SET status = ?1, iteration = ?2, votes = ?3, updated_at = ?4 WHERE id = ?5",
            params![
                round.status.to_string(),
                round.iteration,
                votes_json,
                Utc::now().to_rfc3339(),
                round.id.to_string(),
            ],
        )?;
        Ok(())
    }

    // --- Write Permits ---

    pub fn create_permit(&self, permit: &WritePermit) -> Result<(), OrchestratorError> {
        let paths_json = serde_json::to_string(&permit.target_paths)?;

        self.conn.execute(
            "INSERT INTO write_permits (id, rfc_id, granted_to, target_paths, status, granted_by, granted_at, consumed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                permit.id.to_string(),
                permit.rfc_id.to_string(),
                permit.granted_to.as_str(),
                paths_json,
                permit.status.to_string(),
                permit.granted_by.as_str(),
                permit.granted_at.to_rfc3339(),
                permit.consumed_at.map(|t| t.to_rfc3339()),
            ],
        )?;
        Ok(())
    }

    pub fn get_permit(&self, id: Uuid) -> Result<Option<WritePermit>, OrchestratorError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rfc_id, granted_to, target_paths, status, granted_by, granted_at, consumed_at FROM write_permits WHERE id = ?1"
        )?;

        let result = stmt.query_row(params![id.to_string()], |row| {
            Ok(PermitRow {
                id: row.get(0)?,
                rfc_id: row.get(1)?,
                granted_to: row.get(2)?,
                target_paths: row.get(3)?,
                status: row.get(4)?,
                granted_by: row.get(5)?,
                granted_at: row.get(6)?,
                consumed_at: row.get(7)?,
            })
        });

        match result {
            Ok(row) => Ok(Some(row.into_permit()?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn check_permit(
        &self,
        persona: PersonaId,
        path: &str,
    ) -> Result<Option<WritePermit>, OrchestratorError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rfc_id, granted_to, target_paths, status, granted_by, granted_at, consumed_at FROM write_permits WHERE granted_to = ?1 AND status = ?2"
        )?;

        let rows: Vec<PermitRow> = stmt
            .query_map(
                params![persona.as_str(), PermitStatus::Active.to_string()],
                |row| {
                    Ok(PermitRow {
                        id: row.get(0)?,
                        rfc_id: row.get(1)?,
                        granted_to: row.get(2)?,
                        target_paths: row.get(3)?,
                        status: row.get(4)?,
                        granted_by: row.get(5)?,
                        granted_at: row.get(6)?,
                        consumed_at: row.get(7)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;

        for row in rows {
            let permit = row.into_permit()?;
            if permit.target_paths.iter().any(|p| path_matches(p, path)) {
                return Ok(Some(permit));
            }
        }

        Ok(None)
    }

    pub fn consume_permit(&self, id: Uuid) -> Result<(), OrchestratorError> {
        let now = Utc::now().to_rfc3339();
        let updated = self.conn.execute(
            "UPDATE write_permits SET status = ?1, consumed_at = ?2 WHERE id = ?3 AND status = ?4",
            params![
                PermitStatus::Consumed.to_string(),
                now,
                id.to_string(),
                PermitStatus::Active.to_string(),
            ],
        )?;

        if updated == 0 {
            let exists: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM write_permits WHERE id = ?1)",
                params![id.to_string()],
                |row| row.get(0),
            )?;

            if exists {
                return Err(OrchestratorError::PermitAlreadyConsumed { id });
            } else {
                return Err(OrchestratorError::PermitNotFound { id });
            }
        }

        Ok(())
    }

    // --- Artifact Index ---

    pub fn upsert_artifact(
        &self,
        artifact: &IndexedArtifact,
        content: &str,
        relations: &[ParsedRelation],
    ) -> Result<(), OrchestratorError> {
        // Delete old data
        self.conn
            .execute("DELETE FROM artifacts WHERE id = ?1", params![artifact.id])?;
        self.conn.execute(
            "DELETE FROM artifacts_fts WHERE id = ?1",
            params![artifact.id],
        )?;
        self.conn.execute(
            "DELETE FROM artifact_relations WHERE source_id = ?1",
            params![artifact.id],
        )?;

        // Insert artifact
        let tags_json = serde_json::to_string(&artifact.tags)?;
        self.conn.execute(
            "INSERT INTO artifacts (id, kind, title, description, tags, file_path, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                artifact.id,
                artifact.kind,
                artifact.title,
                artifact.description,
                tags_json,
                artifact.file_path,
                artifact.indexed_at,
            ],
        )?;

        // Insert FTS
        self.conn.execute(
            "INSERT INTO artifacts_fts (id, kind, title, description, tags, content)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                artifact.id,
                artifact.kind,
                artifact.title,
                artifact.description,
                artifact.tags.join(" "),
                content,
            ],
        )?;

        // Insert relations
        for rel in relations {
            self.conn.execute(
                "INSERT OR IGNORE INTO artifact_relations (source_id, target_id, relationship)
                 VALUES (?1, ?2, ?3)",
                params![artifact.id, rel.target_id, rel.relationship],
            )?;
        }

        Ok(())
    }

    pub fn search_artifacts(
        &self,
        query: &str,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ArtifactSummary>, OrchestratorError> {
        let sql = if kind.is_some() {
            "SELECT a.id, a.kind, a.title, a.description, a.tags
             FROM artifacts_fts f
             JOIN artifacts a ON a.id = f.id
             WHERE artifacts_fts MATCH ?1 AND a.kind = ?2
             LIMIT ?3"
        } else {
            "SELECT a.id, a.kind, a.title, a.description, a.tags
             FROM artifacts_fts f
             JOIN artifacts a ON a.id = f.id
             WHERE artifacts_fts MATCH ?1
             LIMIT ?2"
        };

        let mut stmt = self.conn.prepare(sql)?;

        let rows = if let Some(k) = kind {
            stmt.query_map(params![query, k, limit as i64], |row| {
                Ok(ArtifactRow {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    title: row.get(2)?,
                    description: row.get(3)?,
                    tags: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![query, limit as i64], |row| {
                Ok(ArtifactRow {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    title: row.get(2)?,
                    description: row.get(3)?,
                    tags: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
        };

        let mut results = Vec::new();
        for row in rows {
            let tags: Vec<String> = serde_json::from_str(&row.tags).unwrap_or_default();
            results.push(ArtifactSummary {
                id: row.id,
                kind: row.kind,
                title: row.title,
                description: row.description,
                tags,
            });
        }
        Ok(results)
    }

    /// List all indexed artifacts of a given kind, ordered by title.
    /// Unlike `search_artifacts`, this does NOT require an FTS query term —
    /// it scans the base `artifacts` table directly. Use it when you need an
    /// exhaustive listing of artifacts of a specific kind (e.g., all roadmaps).
    pub fn list_by_kind(&self, kind: &str) -> Result<Vec<IndexedArtifact>, OrchestratorError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, title, description, tags, file_path, indexed_at
             FROM artifacts
             WHERE kind = ?1
             ORDER BY title",
        )?;

        let rows = stmt
            .query_map(params![kind], |row| {
                Ok(ArtifactFullRow {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    title: row.get(2)?,
                    description: row.get(3)?,
                    tags: row.get(4)?,
                    file_path: row.get(5)?,
                    indexed_at: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            let tags: Vec<String> = serde_json::from_str(&row.tags).unwrap_or_default();
            results.push(IndexedArtifact {
                id: row.id,
                kind: row.kind,
                title: row.title,
                description: row.description,
                tags,
                file_path: row.file_path,
                indexed_at: row.indexed_at,
            });
        }
        Ok(results)
    }

    pub fn get_artifact(&self, id: &str) -> Result<Option<IndexedArtifact>, OrchestratorError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, title, description, tags, file_path, indexed_at FROM artifacts WHERE id = ?1",
        )?;

        let result = stmt.query_row(params![id], |row| {
            Ok(ArtifactFullRow {
                id: row.get(0)?,
                kind: row.get(1)?,
                title: row.get(2)?,
                description: row.get(3)?,
                tags: row.get(4)?,
                file_path: row.get(5)?,
                indexed_at: row.get(6)?,
            })
        });

        match result {
            Ok(row) => {
                let tags: Vec<String> = serde_json::from_str(&row.tags).unwrap_or_default();
                Ok(Some(IndexedArtifact {
                    id: row.id,
                    kind: row.kind,
                    title: row.title,
                    description: row.description,
                    tags,
                    file_path: row.file_path,
                    indexed_at: row.indexed_at,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn get_relations(&self, id: &str) -> Result<Vec<RelationLink>, OrchestratorError> {
        let mut results = Vec::new();

        // Outgoing: this artifact references others
        let mut stmt = self.conn.prepare(
            "SELECT r.target_id, r.relationship, a.kind, a.title
             FROM artifact_relations r
             LEFT JOIN artifacts a ON a.id = r.target_id
             WHERE r.source_id = ?1",
        )?;
        let outgoing = stmt.query_map(params![id], |row| {
            Ok(RelationLink {
                id: row.get(0)?,
                relationship: row.get(1)?,
                kind: row.get(2)?,
                title: row.get(3)?,
                direction: RelationDirection::Outgoing,
            })
        })?;
        for link in outgoing {
            results.push(link?);
        }

        // Incoming: other artifacts reference this one
        let mut stmt = self.conn.prepare(
            "SELECT r.source_id, r.relationship, a.kind, a.title
             FROM artifact_relations r
             LEFT JOIN artifacts a ON a.id = r.source_id
             WHERE r.target_id = ?1",
        )?;
        let incoming = stmt.query_map(params![id], |row| {
            Ok(RelationLink {
                id: row.get(0)?,
                relationship: row.get(1)?,
                kind: row.get(2)?,
                title: row.get(3)?,
                direction: RelationDirection::Incoming,
            })
        })?;
        for link in incoming {
            results.push(link?);
        }

        Ok(results)
    }

    pub fn delete_all_artifacts(&self) -> Result<(), OrchestratorError> {
        self.conn.execute("DELETE FROM artifact_relations", [])?;
        self.conn.execute("DELETE FROM artifacts_fts", [])?;
        self.conn.execute("DELETE FROM artifacts", [])?;
        Ok(())
    }
}

/// Simple glob-like path matching (supports prefix matching with trailing /).
fn path_matches(pattern: &PathPattern, path: &str) -> bool {
    let pat = &pattern.0;
    if pat == path {
        return true;
    }
    if pat.ends_with('/') {
        return path.starts_with(pat.as_str());
    }
    glob::Pattern::new(pat)
        .map(|p| p.matches(path))
        .unwrap_or(false)
}

// --- Internal row types for SQLite deserialization ---

struct RoundRow {
    id: String,
    artifact_path: String,
    artifact_kind: String,
    author: String,
    required_reviewers: String,
    status: String,
    iteration: u32,
    max_iterations: u32,
    votes: String,
    created_at: String,
    updated_at: String,
}

impl RoundRow {
    fn into_round(self) -> Result<ReviewRound, OrchestratorError> {
        let required_reviewers: Vec<PersonaId> = serde_json::from_str(&self.required_reviewers)?;
        let votes: Vec<ReviewVote> = serde_json::from_str(&self.votes)?;
        let status: RoundStatus = self
            .status
            .parse()
            .map_err(OrchestratorError::InvalidEnumValue)?;
        let artifact_kind = self
            .artifact_kind
            .parse()
            .map_err(OrchestratorError::InvalidEnumValue)?;
        let author = self
            .author
            .parse()
            .map_err(OrchestratorError::InvalidEnumValue)?;
        let created_at = chrono::DateTime::parse_from_rfc3339(&self.created_at)
            .unwrap_or_default()
            .with_timezone(&Utc);
        let updated_at = chrono::DateTime::parse_from_rfc3339(&self.updated_at)
            .unwrap_or_default()
            .with_timezone(&Utc);

        Ok(ReviewRound {
            id: Uuid::parse_str(&self.id).unwrap_or_default(),
            artifact_path: ArtifactPath(self.artifact_path),
            artifact_kind,
            author,
            required_reviewers,
            status,
            iteration: self.iteration,
            max_iterations: self.max_iterations,
            votes,
            created_at,
            updated_at,
        })
    }
}

struct PermitRow {
    id: String,
    rfc_id: String,
    granted_to: String,
    target_paths: String,
    status: String,
    granted_by: String,
    granted_at: String,
    consumed_at: Option<String>,
}

impl PermitRow {
    fn into_permit(self) -> Result<WritePermit, OrchestratorError> {
        let target_paths: Vec<PathPattern> =
            serde_json::from_str::<Vec<String>>(&self.target_paths)?
                .into_iter()
                .map(PathPattern)
                .collect();
        let status: PermitStatus = self
            .status
            .parse()
            .map_err(OrchestratorError::InvalidEnumValue)?;
        let granted_to: PersonaId = self
            .granted_to
            .parse()
            .map_err(OrchestratorError::InvalidEnumValue)?;
        let granted_by: PersonaId = self
            .granted_by
            .parse()
            .map_err(OrchestratorError::InvalidEnumValue)?;
        let granted_at = chrono::DateTime::parse_from_rfc3339(&self.granted_at)
            .unwrap_or_default()
            .with_timezone(&Utc);
        let consumed_at = self.consumed_at.and_then(|s| {
            chrono::DateTime::parse_from_rfc3339(&s)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        });

        Ok(WritePermit {
            id: Uuid::parse_str(&self.id).unwrap_or_default(),
            rfc_id: Uuid::parse_str(&self.rfc_id).unwrap_or_default(),
            granted_to,
            target_paths,
            status,
            granted_by,
            granted_at,
            consumed_at,
        })
    }
}

struct ArtifactRow {
    id: String,
    kind: String,
    title: String,
    description: String,
    tags: String,
}

struct ArtifactFullRow {
    id: String,
    kind: String,
    title: String,
    description: String,
    tags: String,
    file_path: String,
    indexed_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use companyos_config::{ArtifactKind, PersonaId};
    use uuid::Uuid;

    fn setup_db() -> OrchestratorDb {
        let db = OrchestratorDb::open(":memory:").unwrap();
        db.migrate().unwrap();
        db
    }

    fn make_round(id: Uuid) -> ReviewRound {
        let now = Utc::now();
        ReviewRound {
            id,
            artifact_path: ArtifactPath("company/docs/design.md".into()),
            artifact_kind: ArtifactKind::DesignDoc,
            author: PersonaId::Architect,
            required_reviewers: vec![PersonaId::Pm, PersonaId::Implementer],
            status: RoundStatus::Open,
            iteration: 1,
            max_iterations: 3,
            votes: vec![],
            created_at: now,
            updated_at: now,
        }
    }

    fn make_permit(id: Uuid, paths: Vec<&str>) -> WritePermit {
        WritePermit {
            id,
            rfc_id: Uuid::new_v4(),
            granted_to: PersonaId::Implementer,
            target_paths: paths.into_iter().map(|s| PathPattern(s.into())).collect(),
            status: PermitStatus::Active,
            granted_by: PersonaId::Architect,
            granted_at: Utc::now(),
            consumed_at: None,
        }
    }

    // --- Review Rounds ---

    #[test]
    fn test_create_and_get_round() {
        let db = setup_db();
        let id = Uuid::new_v4();
        let round = make_round(id);
        db.create_round(&round).unwrap();

        let fetched = db.get_round(id).unwrap().expect("round should exist");
        assert_eq!(fetched.id, id);
        assert_eq!(fetched.artifact_path.0, "company/docs/design.md");
        assert_eq!(fetched.status, RoundStatus::Open);
        assert_eq!(fetched.iteration, 1);
        assert_eq!(fetched.required_reviewers.len(), 2);
    }

    #[test]
    fn test_get_nonexistent_round() {
        let db = setup_db();
        let result = db.get_round(Uuid::new_v4()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_update_round() {
        let db = setup_db();
        let id = Uuid::new_v4();
        let mut round = make_round(id);
        db.create_round(&round).unwrap();

        round.status = RoundStatus::ConsensusReached;
        round.votes.push(ReviewVote {
            reviewer: PersonaId::Pm,
            verdict: ReviewVerdict::Approve,
            findings: vec![Finding("Looks good".into())],
            submitted_at: Utc::now(),
        });
        db.update_round(&round).unwrap();

        let fetched = db.get_round(id).unwrap().unwrap();
        assert_eq!(fetched.status, RoundStatus::ConsensusReached);
        assert_eq!(fetched.votes.len(), 1);
        assert_eq!(fetched.votes[0].verdict, ReviewVerdict::Approve);
    }

    #[test]
    fn test_round_json_fields_roundtrip() {
        let db = setup_db();
        let id = Uuid::new_v4();
        let mut round = make_round(id);
        round.required_reviewers = vec![PersonaId::Pm, PersonaId::Ceo, PersonaId::Implementer];
        round.votes.push(ReviewVote {
            reviewer: PersonaId::Pm,
            verdict: ReviewVerdict::RequestChanges,
            findings: vec![Finding("Needs work".into()), Finding("Fix naming".into())],
            submitted_at: Utc::now(),
        });
        db.create_round(&round).unwrap();

        let fetched = db.get_round(id).unwrap().unwrap();
        assert_eq!(fetched.required_reviewers.len(), 3);
        assert_eq!(fetched.required_reviewers[2], PersonaId::Implementer);
        assert_eq!(fetched.votes[0].findings.len(), 2);
        assert_eq!(fetched.votes[0].findings[1].0, "Fix naming");
    }

    // --- Write Permits ---

    #[test]
    fn test_create_and_get_permit() {
        let db = setup_db();
        let id = Uuid::new_v4();
        let permit = make_permit(id, vec!["company/src/main.rs"]);
        db.create_permit(&permit).unwrap();

        let fetched = db.get_permit(id).unwrap().expect("permit should exist");
        assert_eq!(fetched.id, id);
        assert_eq!(fetched.status, PermitStatus::Active);
        assert_eq!(fetched.target_paths.len(), 1);
    }

    #[test]
    fn test_check_permit_exact_match() {
        let db = setup_db();
        let permit = make_permit(Uuid::new_v4(), vec!["company/config/foo.yml"]);
        db.create_permit(&permit).unwrap();

        let found = db
            .check_permit(PersonaId::Implementer, "company/config/foo.yml")
            .unwrap();
        assert!(found.is_some());
    }

    #[test]
    fn test_check_permit_prefix_match() {
        let db = setup_db();
        let permit = make_permit(Uuid::new_v4(), vec!["company/config/"]);
        db.create_permit(&permit).unwrap();

        let found = db
            .check_permit(PersonaId::Implementer, "company/config/foo.yml")
            .unwrap();
        assert!(found.is_some());
    }

    #[test]
    fn test_check_permit_no_match() {
        let db = setup_db();
        let permit = make_permit(Uuid::new_v4(), vec!["company/src/"]);
        db.create_permit(&permit).unwrap();

        let found = db
            .check_permit(PersonaId::Implementer, "company/docs/readme.md")
            .unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn test_check_permit_only_active() {
        let db = setup_db();
        let id = Uuid::new_v4();
        let permit = make_permit(id, vec!["company/src/"]);
        db.create_permit(&permit).unwrap();
        db.consume_permit(id).unwrap();

        let found = db
            .check_permit(PersonaId::Implementer, "company/src/lib.rs")
            .unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn test_consume_permit_success() {
        let db = setup_db();
        let id = Uuid::new_v4();
        let permit = make_permit(id, vec!["company/src/"]);
        db.create_permit(&permit).unwrap();

        db.consume_permit(id).unwrap();

        let fetched = db.get_permit(id).unwrap().unwrap();
        assert_eq!(fetched.status, PermitStatus::Consumed);
        assert!(fetched.consumed_at.is_some());
    }

    #[test]
    fn test_consume_already_consumed() {
        let db = setup_db();
        let id = Uuid::new_v4();
        let permit = make_permit(id, vec!["company/src/"]);
        db.create_permit(&permit).unwrap();
        db.consume_permit(id).unwrap();

        let result = db.consume_permit(id);
        assert!(result.is_err());
    }

    #[test]
    fn test_consume_not_found() {
        let db = setup_db();
        let result = db.consume_permit(Uuid::new_v4());
        assert!(result.is_err());
    }

    // --- Artifact Index ---

    #[test]
    fn test_upsert_and_search() {
        let db = setup_db();
        let artifact = IndexedArtifact {
            id: "rfc-001".into(),
            kind: "rfc".into(),
            title: "Implement caching layer".into(),
            description: "A proposal to add Redis caching".into(),
            tags: vec!["performance".into(), "infrastructure".into()],
            file_path: "company/rfcs/rfc-001.md".into(),
            indexed_at: Utc::now().to_rfc3339(),
        };
        db.upsert_artifact(
            &artifact,
            "Full content about caching and Redis integration",
            &[],
        )
        .unwrap();

        let results = db.search_artifacts("caching", None, 10).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].id, "rfc-001");
        assert_eq!(results[0].title, "Implement caching layer");
    }

    fn make_indexed(id: &str, kind: &str, title: &str) -> IndexedArtifact {
        IndexedArtifact {
            id: id.into(),
            kind: kind.into(),
            title: title.into(),
            description: String::new(),
            tags: vec![],
            file_path: format!("company/fixtures/{id}.yml"),
            indexed_at: Utc::now().to_rfc3339(),
        }
    }

    #[test]
    fn test_list_by_kind_returns_only_matching_kind() {
        let db = setup_db();
        let r1 = make_indexed("road-001", "roadmap", "Beta Roadmap");
        let r2 = make_indexed("road-002", "roadmap", "Alpha Roadmap");
        let other = make_indexed("rfc-001", "rfc", "Some RFC");
        db.upsert_artifact(&r1, "content", &[]).unwrap();
        db.upsert_artifact(&r2, "content", &[]).unwrap();
        db.upsert_artifact(&other, "content", &[]).unwrap();

        let results = db.list_by_kind("roadmap").unwrap();
        assert_eq!(results.len(), 2);
        // Ordered by title: "Alpha" < "Beta"
        assert_eq!(results[0].id, "road-002");
        assert_eq!(results[1].id, "road-001");
        assert!(results.iter().all(|a| a.kind == "roadmap"));
    }

    #[test]
    fn test_list_by_kind_empty_when_no_match() {
        let db = setup_db();
        let rfc = make_indexed("rfc-only", "rfc", "Lonely RFC");
        db.upsert_artifact(&rfc, "content", &[]).unwrap();

        let results = db.list_by_kind("roadmap").unwrap();
        assert!(results.is_empty());
    }
}
