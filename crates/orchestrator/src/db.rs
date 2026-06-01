use std::path::Path;
use std::sync::Once;

use chrono::Utc;
use companyos_config::PersonaId;
use rusqlite::{Connection, params};
use uuid::Uuid;

use crate::error::OrchestratorError;
use crate::types::*;

/// One-shot process-wide registration of the `sqlite-vec` C extension via
/// `sqlite3_auto_extension`. Idempotent: subsequent calls are no-ops.
///
/// Rationale (RFC bdee1af4 proposition 2(c)): the orchestrator opens
/// SQLite from three sites — `run_server`, `run_index`, and tests via
/// `open_in_memory`. Registering the extension once at process startup
/// guarantees that every subsequent `Connection::open` loads vec0
/// automatically, without scattering `load_extension` calls. The
/// upstream `sqlite-vec` README and tests use the same pattern.
///
/// SAFETY: `sqlite3_vec_init` is a C function from the linked
/// `sqlite_vec0` library, with the canonical SQLite extension entry-point
/// signature. Casting through `*const ()` mirrors the documented usage in
/// the sqlite-vec crate's own test (lib.rs lines 9-13).
static SQLITE_VEC_INIT: Once = Once::new();

fn ensure_sqlite_vec_loaded() {
    SQLITE_VEC_INIT.call_once(|| {
        // The SQLite C API expects the auto-extension entry point to have
        // signature `int (*)(sqlite3*, char**, const sqlite3_api_routines*)`,
        // but `sqlite_vec::sqlite3_vec_init` is declared in the crate as
        // `extern "C" fn()` (zero-arg). Both signatures are ABI-compatible
        // when the underlying C symbol ignores extra arguments (which is
        // the documented contract for sqlite-vec). We transmute through a
        // pointer to bridge the type-system gap.
        type AutoExtensionFn = unsafe extern "C" fn(
            *mut rusqlite::ffi::sqlite3,
            *mut *mut std::os::raw::c_char,
            *const rusqlite::ffi::sqlite3_api_routines,
        ) -> std::os::raw::c_int;
        unsafe {
            let entry: AutoExtensionFn =
                std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
            rusqlite::ffi::sqlite3_auto_extension(Some(entry));
        }
    });
}

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
    content,
    tokenize = \"unicode61 remove_diacritics 2 separators '-_./'\"
);
";

/// Vector index over artifact embeddings. The first column is a metadata
/// column carrying the artifact UUID, with type TEXT — `sqlite-vec`
/// allows the metadata column to appear in the WHERE clause of a kNN
/// query, but our hybrid pipeline JOINs back to `artifacts` so the
/// column is mainly there for lookup.
///
/// Distance metric: `cosine`. fastembed normalises sentence embeddings
/// to unit L2 length, so cosine distance is the right metric (smaller
/// = more similar). Dimension is fixed at compile time via
/// `embedding::EMBEDDING_DIM` (384 for MultilingualE5Small).
///
/// `index_metadata` stores small key/value pairs that survive PILIER D
/// rebuilds (themselves repopulated by the engine boot path). Used to
/// pin the `model_version` at boot and to trigger wipe + reindex_all
/// when the runtime model_version drifts from the stored one.
const VEC_TABLE_SQL: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS artifacts_vec USING vec0(
    artifact_id text,
    embedding float[384] distance_metric=cosine
);

CREATE TABLE IF NOT EXISTS index_metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// Metadata key for the embedding model version string.
const META_MODEL_VERSION_KEY: &str = "model_version";

/// Defensive PRAGMA preamble applied at every open.
///
/// Rationale (cf. design-doc 45c04902 PILIER B):
/// - journal_mode=WAL: required for concurrent reads + single writer.
/// - foreign_keys=ON: enforce referential integrity (we don't rely on it
///   in practice yet, but kept for safety).
/// - busy_timeout=5000: tolerate contention between server + --index up
///   to 5s, avoid immediate SQLITE_BUSY failures.
/// - synchronous=NORMAL: WAL-recommended trade-off, durability preserved
///   per-COMMIT, small window of last-tx loss on crash is acceptable
///   (DB is reconstructible from YAML via PILIER D).
/// - wal_autocheckpoint=1000: explicit (default is 1000 too) so a reader
///   of the code immediately sees the checkpoint frequency.
const DEFENSIVE_PRAGMAS: &str = "\
PRAGMA journal_mode=WAL;\
PRAGMA foreign_keys=ON;\
PRAGMA busy_timeout=5000;\
PRAGMA synchronous=NORMAL;\
PRAGMA wal_autocheckpoint=1000;\
";

pub struct OrchestratorDb {
    conn: Connection,
}

impl OrchestratorDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, OrchestratorError> {
        ensure_sqlite_vec_loaded();
        let conn = Connection::open(path)?;
        conn.execute_batch(DEFENSIVE_PRAGMAS)?;
        Self::smoke_test_sqlite_vec(&conn)?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self, OrchestratorError> {
        ensure_sqlite_vec_loaded();
        let conn = Connection::open_in_memory()?;
        // In-memory DBs return "memory" for journal_mode but accept all
        // other PRAGMAs harmlessly. Apply the same preamble for uniformity.
        conn.execute_batch(DEFENSIVE_PRAGMAS)?;
        Self::smoke_test_sqlite_vec(&conn)?;
        Ok(Self { conn })
    }

    /// Smoke test the `sqlite-vec` extension at boot. Creates a temporary
    /// in-memory vec0 table with two known vectors, runs a kNN, and
    /// asserts the result matches a known-good answer (rowid 1 with
    /// distance < 1e-3, rowid 2 with distance much greater).
    ///
    /// Rationale (RFC bdee1af4 proposition 2(d.2)): sqlite-vec is pre-1.0,
    /// pinning a version is not sufficient — a silent kNN regression
    /// could pass type checks but return wrong results. This 10ms test
    /// fails fast when the wiring is broken, with a clear error message
    /// that surfaces to the operator instead of producing garbage
    /// search results downstream.
    ///
    /// The test uses a temp vec0 table that is dropped immediately to
    /// avoid polluting the live schema.
    fn smoke_test_sqlite_vec(conn: &Connection) -> Result<(), OrchestratorError> {
        // Verify the extension itself responds.
        let version: String = conn
            .query_row("SELECT vec_version()", [], |row| row.get(0))
            .map_err(|e| OrchestratorError::IntegrityFailure {
                details: format!("sqlite-vec extension not loaded: {e}"),
            })?;
        if !version.starts_with('v') {
            return Err(OrchestratorError::IntegrityFailure {
                details: format!(
                    "sqlite-vec returned unexpected version string '{version}' (expected to start with 'v')"
                ),
            });
        }

        // Create a throwaway vec0 table, insert two known vectors, run kNN.
        conn.execute_batch(
            "CREATE VIRTUAL TABLE temp.vec0_smoketest USING vec0(emb float[3]);
             INSERT INTO temp.vec0_smoketest(rowid, emb) VALUES (1, '[1.0, 0.0, 0.0]');
             INSERT INTO temp.vec0_smoketest(rowid, emb) VALUES (2, '[0.0, 1.0, 0.0]');",
        )
        .map_err(|e| OrchestratorError::IntegrityFailure {
            details: format!("sqlite-vec smoke test: vec0 create/insert failed: {e}"),
        })?;

        let mut stmt = conn
            .prepare(
                "SELECT rowid, distance FROM temp.vec0_smoketest
                 WHERE emb MATCH '[1.0, 0.0, 0.0]' AND k = 2 ORDER BY distance",
            )
            .map_err(|e| OrchestratorError::IntegrityFailure {
                details: format!("sqlite-vec smoke test: kNN prepare failed: {e}"),
            })?;

        let rows: Vec<(i64, f64)> = stmt
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?)))
            .and_then(|iter| iter.collect::<Result<Vec<_>, _>>())
            .map_err(|e| OrchestratorError::IntegrityFailure {
                details: format!("sqlite-vec smoke test: kNN query failed: {e}"),
            })?;

        drop(stmt);
        // Always drop the throwaway table even if we are about to fail.
        let _ = conn.execute_batch("DROP TABLE temp.vec0_smoketest;");

        if rows.len() != 2 {
            return Err(OrchestratorError::IntegrityFailure {
                details: format!(
                    "sqlite-vec smoke test: kNN returned {} rows, expected 2",
                    rows.len()
                ),
            });
        }
        if rows[0].0 != 1 || rows[0].1 > 1e-3 {
            return Err(OrchestratorError::IntegrityFailure {
                details: format!(
                    "sqlite-vec smoke test: closest vector should be rowid=1 with distance ~0, got rowid={} distance={}",
                    rows[0].0, rows[0].1
                ),
            });
        }
        if rows[1].0 != 2 || rows[1].1 < 0.5 {
            return Err(OrchestratorError::IntegrityFailure {
                details: format!(
                    "sqlite-vec smoke test: second vector should be rowid=2 with distance > 0.5, got rowid={} distance={}",
                    rows[1].0, rows[1].1
                ),
            });
        }

        Ok(())
    }

    /// Apply the schema migration inside a single BEGIN IMMEDIATE/COMMIT
    /// transaction. Rationale (cf. design-doc 45c04902 PILIER C and
    /// diagnostic 378c387c factor 3): if SIGKILL strikes mid-DDL, SQLite
    /// will rollback the partial transaction on the next open via WAL
    /// recovery, eliminating the FTS5 partially-materialized schema risk.
    pub fn migrate(&self) -> Result<(), OrchestratorError> {
        let migration_sql = format!("BEGIN IMMEDIATE;{SCHEMA_SQL}{FTS_SQL}{VEC_TABLE_SQL}COMMIT;");
        self.conn.execute_batch(&migration_sql)?;
        Ok(())
    }

    /// Read the embedding model version recorded in `index_metadata`.
    /// Returns `None` if the row is missing (fresh DB or after wipe).
    pub fn get_model_version(&self) -> Result<Option<String>, OrchestratorError> {
        let result = self.conn.query_row(
            "SELECT value FROM index_metadata WHERE key = ?1",
            params![META_MODEL_VERSION_KEY],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Write or replace the embedding model version row.
    pub fn set_model_version(&self, version: &str) -> Result<(), OrchestratorError> {
        self.conn.execute(
            "INSERT INTO index_metadata(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![META_MODEL_VERSION_KEY, version],
        )?;
        Ok(())
    }

    /// Wipe every row in `artifacts_vec` and clear the stored model
    /// version. Used by the boot autorepair path when the runtime
    /// `model_version()` differs from the persisted one (architecture
    /// marker change, model upgrade, etc.). After this call, the engine
    /// must call `reindex_all` to repopulate embeddings.
    pub fn wipe_vec_table(&self) -> Result<(), OrchestratorError> {
        self.conn.execute("DELETE FROM artifacts_vec", [])?;
        self.conn.execute(
            "DELETE FROM index_metadata WHERE key = ?1",
            params![META_MODEL_VERSION_KEY],
        )?;
        Ok(())
    }

    /// Run `PRAGMA integrity_check` and return true if the result is
    /// exactly "ok". Used by the boot autorepair (PILIER D) to detect
    /// corruption before serving requests.
    ///
    /// Logs the corruption details to stderr when the result is not "ok"
    /// (best-effort logging, no error propagation on log failure).
    pub fn integrity_check(&self) -> Result<bool, OrchestratorError> {
        let mut stmt = self.conn.prepare("PRAGMA integrity_check;")?;
        let rows: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;

        // SQLite returns a single row "ok" when healthy, otherwise one or
        // more rows describing the corruption.
        let is_ok = rows.len() == 1 && rows[0] == "ok";
        if !is_ok {
            eprintln!(
                "[companyos:orchestrator] integrity_check failed with {} row(s):",
                rows.len()
            );
            for row in &rows {
                eprintln!("  - {row}");
            }
        }
        Ok(is_ok)
    }

    /// Execute `PRAGMA wal_checkpoint(TRUNCATE)` to flush all WAL frames
    /// into the main DB file and truncate the WAL to zero. Used by the
    /// graceful shutdown sequence (PILIER C) to leave the DB in a clean
    /// state without -wal/-shm residuals.
    pub fn checkpoint_truncate(&self) -> Result<(), OrchestratorError> {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
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

    /// Compute an opaque blob describing the current state of
    /// `write_permits`. Format mirrors the legacy sqlite3 inline call in
    /// `defense-in-depth-core.mjs:188`:
    ///
    /// ```text
    /// "<count>|<id1>:<status1>,<id2>:<status2>,..."
    /// ```
    ///
    /// When the table is empty the blob is `"0|"`. The blob is consumed
    /// later by [`Self::restore_permits_from_snapshot`] to detect
    /// tampering and revert if necessary.
    pub fn snapshot_permits(&self) -> Result<String, OrchestratorError> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM write_permits", [], |r| r.get(0))?;

        // Concatenate id:status pairs in a stable order (ORDER BY id) so
        // two consecutive snapshots of the same state are byte-identical.
        let mut stmt = self
            .conn
            .prepare("SELECT id || ':' || status FROM write_permits ORDER BY id")?;
        let pairs: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(format!("{}|{}", count, pairs.join(",")))
    }

    /// Restore the `write_permits` table to a previous snapshot,
    /// removing any permit not present in the snapshot.
    ///
    /// - `None` (nuclear): wipe all permits. Used when the DB did not
    ///   exist before the guarded bash command but exists now.
    /// - `Some(blob)` (selective): keep only the ids found in the blob.
    ///
    /// Returns the number of rows deleted.
    pub fn restore_permits_from_snapshot(
        &self,
        snapshot: Option<&str>,
    ) -> Result<usize, OrchestratorError> {
        let snapshot = match snapshot {
            None => {
                // Nuclear: wipe everything.
                let n = self.conn.execute("DELETE FROM write_permits", [])?;
                return Ok(n);
            }
            Some(s) => s,
        };

        // Parse "<count>|<id1>:<status1>,<id2>:<status2>,..."
        // Only the ids matter for the selective delete; statuses are
        // informational (kept in the blob to detect intra-permit
        // tampering, but not enforced here — the wipe-vs-keep contract
        // is at row granularity).
        let pair_segment = snapshot.split_once('|').map(|(_, rest)| rest).unwrap_or("");
        let ids: Vec<&str> = if pair_segment.is_empty() {
            Vec::new()
        } else {
            pair_segment
                .split(',')
                .filter_map(|pair| pair.split(':').next())
                .filter(|s| !s.is_empty())
                .collect()
        };

        if ids.is_empty() {
            // Snapshot says "nothing was there" — wipe whatever is here.
            let n = self.conn.execute("DELETE FROM write_permits", [])?;
            return Ok(n);
        }

        // Build "DELETE FROM write_permits WHERE id NOT IN (?, ?, …)"
        // with the right number of placeholders.
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!("DELETE FROM write_permits WHERE id NOT IN ({placeholders})");
        let params_vec: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let n = self.conn.execute(&sql, params_vec.as_slice())?;
        Ok(n)
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

    // --- PILIER B / PILIER C / PILIER D : defensive PRAGMAs, transactional
    //     migrate, and integrity_check / checkpoint_truncate helpers.

    #[test]
    fn test_integrity_check_passes_on_fresh_db() {
        let db = setup_db();
        let ok = db.integrity_check().unwrap();
        assert!(ok, "integrity_check should return true on a fresh DB");
    }

    // RFC bdee1af4 étape 5: artifacts_vec + index_metadata.

    #[test]
    fn test_migrate_creates_artifacts_vec_table() {
        let db = setup_db();
        // A SELECT from the virtual table should succeed (returns 0 rows
        // on fresh DB but no SQL error).
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM artifacts_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_migrate_creates_index_metadata_table() {
        let db = setup_db();
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM index_metadata", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_model_version_fresh_returns_none() {
        let db = setup_db();
        assert_eq!(db.get_model_version().unwrap(), None);
    }

    #[test]
    fn test_model_version_set_then_get_roundtrip() {
        let db = setup_db();
        db.set_model_version("multilingual-e5-small-v1+x86_64")
            .unwrap();
        assert_eq!(
            db.get_model_version().unwrap(),
            Some("multilingual-e5-small-v1+x86_64".into())
        );
    }

    #[test]
    fn test_model_version_set_twice_upserts() {
        let db = setup_db();
        db.set_model_version("v1").unwrap();
        db.set_model_version("v2").unwrap();
        assert_eq!(db.get_model_version().unwrap(), Some("v2".into()));
    }

    #[test]
    fn test_wipe_vec_clears_metadata_too() {
        let db = setup_db();
        db.set_model_version("v1").unwrap();
        db.wipe_vec_table().unwrap();
        assert_eq!(db.get_model_version().unwrap(), None);
    }

    // PILIER E (RFC bdee1af4): sqlite-vec smoke test must pass on fresh DB.
    // open() and open_in_memory() both call smoke_test_sqlite_vec internally,
    // so a successful setup_db() is implicit proof; this test makes the
    // expectation explicit and would fail loudly if the wiring breaks.
    #[test]
    fn test_smoke_test_sqlite_vec_passes_on_fresh_db() {
        // setup_db() calls OrchestratorDb::open_in_memory() which itself
        // runs smoke_test_sqlite_vec — if this assertion-only test runs to
        // completion the smoke test passed.
        let db = setup_db();
        // Sanity: vec_version() is now callable on the live connection.
        let version: String = db
            .conn
            .query_row("SELECT vec_version()", [], |r| r.get(0))
            .unwrap();
        assert!(version.starts_with('v'), "vec_version() = {version}");
    }

    #[test]
    fn test_migrate_is_idempotent() {
        // The wrapping transaction must still allow multiple calls
        // (each call wraps SCHEMA_SQL + FTS_SQL in a fresh BEGIN/COMMIT,
        // and all CREATE statements are IF NOT EXISTS).
        let db = OrchestratorDb::open(":memory:").unwrap();
        db.migrate().unwrap();
        db.migrate().unwrap();
        let ok = db.integrity_check().unwrap();
        assert!(ok);
    }

    #[test]
    fn test_checkpoint_truncate_noop_on_memory_db() {
        // In-memory DBs have no WAL but the PRAGMA must still succeed.
        let db = setup_db();
        db.checkpoint_truncate().unwrap();
    }

    #[test]
    fn test_defensive_pragmas_applied() {
        // Validate that busy_timeout and synchronous are at expected values.
        let db = setup_db();
        let busy_timeout: i64 = db
            .conn
            .query_row("PRAGMA busy_timeout;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(busy_timeout, 5000);

        // synchronous=NORMAL is encoded as 1.
        let synchronous: i64 = db
            .conn
            .query_row("PRAGMA synchronous;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(synchronous, 1, "synchronous should be NORMAL (1)");
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

    // --- Snapshot / Restore permits (backing the defense-in-depth hook
    //     refactor via MCP, step 4 of the implementation-plan) ---

    #[test]
    fn test_snapshot_permits_empty() {
        let db = setup_db();
        let blob = db.snapshot_permits().unwrap();
        assert_eq!(blob, "0|");
    }

    #[test]
    fn test_snapshot_permits_format() {
        let db = setup_db();
        let p1 = make_permit(
            Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
            vec!["company/src/"],
        );
        let p2 = make_permit(
            Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap(),
            vec!["company/docs/"],
        );
        db.create_permit(&p1).unwrap();
        db.create_permit(&p2).unwrap();

        let blob = db.snapshot_permits().unwrap();
        // Ordered by id, two pairs id:status separated by ','.
        assert_eq!(
            blob,
            "2|11111111-1111-1111-1111-111111111111:active,\
             22222222-2222-2222-2222-222222222222:active"
        );
    }

    #[test]
    fn test_restore_permits_nuclear() {
        let db = setup_db();
        let p1 = make_permit(Uuid::new_v4(), vec!["a"]);
        let p2 = make_permit(Uuid::new_v4(), vec!["b"]);
        db.create_permit(&p1).unwrap();
        db.create_permit(&p2).unwrap();

        let deleted = db.restore_permits_from_snapshot(None).unwrap();
        assert_eq!(deleted, 2);

        let blob = db.snapshot_permits().unwrap();
        assert_eq!(blob, "0|");
    }

    #[test]
    fn test_restore_permits_selective_drops_only_new() {
        let db = setup_db();
        let p1 = make_permit(
            Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap(),
            vec!["a"],
        );
        let p2 = make_permit(
            Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap(),
            vec!["b"],
        );
        db.create_permit(&p1).unwrap();
        db.create_permit(&p2).unwrap();

        // Snapshot at this point ("good state" with p1+p2).
        let snapshot = db.snapshot_permits().unwrap();

        // Tampering: a third permit appears.
        let p3 = make_permit(
            Uuid::parse_str("cccccccc-cccc-cccc-cccc-cccccccccccc").unwrap(),
            vec!["c"],
        );
        db.create_permit(&p3).unwrap();

        // Revert to the snapshot.
        let deleted = db.restore_permits_from_snapshot(Some(&snapshot)).unwrap();
        assert_eq!(deleted, 1, "only p3 should be removed");

        // Verify p1 + p2 remain.
        let after = db.snapshot_permits().unwrap();
        assert_eq!(after, snapshot);
    }

    #[test]
    fn test_restore_permits_empty_snapshot_wipes_table() {
        // If the snapshot encodes "table was empty" ("0|"), restoring
        // should wipe whatever is in the table now.
        let db = setup_db();
        let p1 = make_permit(Uuid::new_v4(), vec!["a"]);
        db.create_permit(&p1).unwrap();

        let deleted = db.restore_permits_from_snapshot(Some("0|")).unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(db.snapshot_permits().unwrap(), "0|");
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
