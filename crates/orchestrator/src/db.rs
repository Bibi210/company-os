use std::path::Path;
use std::sync::Once;

use chrono::Utc;
use companyos_config::PersonaId;
use rusqlite::{Connection, params};
use uuid::Uuid;

use crate::error::OrchestratorError;
use crate::migrations;
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

pub(crate) const SCHEMA_SQL: &str = "
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
    indexed_at TEXT NOT NULL,
    author TEXT,
    project TEXT,
    created_at TEXT
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

pub(crate) const FTS_SQL: &str = "
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
pub(crate) const VEC_TABLE_SQL: &str = "
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

/// Structured filters for `search_lexical` / `search_semantic`. All
/// fields are optional and combined with AND in the SQL WHERE clause.
/// `tags` and `kinds` use IN semantics (OR within a single field).
///
/// `kinds` is retained for the internal list-mode (and `list_by_kind`
/// for roadmaps) but is no longer wired from the MCP `search` surface
/// (RFC 1d3a3581). `author`, `project` and the `created_*` date filters
/// were removed entirely: they had zero callers and were cargo-cult API
/// (diagnostic 655f74d7).
#[derive(Debug, Clone, Default)]
pub struct SearchFilters {
    pub kinds: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
    pub id_prefix: Option<String>,
}

/// Append the WHERE-clause conjuncts and bind parameters for a
/// [`SearchFilters`]. Used by both lexical and semantic SQL paths so
/// the filter semantics are kept identical.
fn append_filter_clauses(
    filters: &SearchFilters,
    sql: &mut String,
    params_vec: &mut Vec<Box<dyn rusqlite::ToSql>>,
) {
    if let Some(kinds) = &filters.kinds
        && !kinds.is_empty()
    {
        sql.push_str(" AND a.kind IN (");
        for (i, k) in kinds.iter().enumerate() {
            if i > 0 {
                sql.push(',');
            }
            sql.push('?');
            params_vec.push(Box::new(k.clone()));
        }
        sql.push(')');
    }
    if let Some(prefix) = &filters.id_prefix {
        sql.push_str(" AND a.id LIKE ?");
        params_vec.push(Box::new(format!("{prefix}%")));
    }
    if let Some(tags) = &filters.tags
        && !tags.is_empty()
    {
        // tags are stored as a JSON array string in `artifacts.tags`.
        // EXISTS over json_each gives an OR-of-tags semantic.
        sql.push_str(" AND EXISTS (SELECT 1 FROM json_each(a.tags) WHERE json_each.value IN (");
        for (i, t) in tags.iter().enumerate() {
            if i > 0 {
                sql.push(',');
            }
            sql.push('?');
            params_vec.push(Box::new(t.clone()));
        }
        sql.push_str("))");
    }
}

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

    /// Apply the schema migrations driven by `PRAGMA user_version`, then
    /// perform the (non-versioned) FTS5 tokenizer-drift repair.
    ///
    /// Rationale (RFC 973a5569): the schema is versioned via the
    /// `crate::migrations` registry. We read the stored `user_version`,
    /// refuse a DB newer than this binary supports (the DB is left
    /// untouched in that case), then apply, IN ORDER, every migration whose
    /// number is strictly greater than the stored version. Each migration
    /// runs inside its own `BEGIN IMMEDIATE`/`COMMIT`, and `user_version`
    /// is stamped to the migration's number INSIDE that same transaction —
    /// so the schema change and its version bump are atomic. If a migration
    /// fails, we `ROLLBACK` (best-effort) and propagate, leaving the DB on
    /// the previous version (cf. design-doc 45c04902 PILIER C: a SIGKILL
    /// mid-DDL is recovered by WAL rollback on the next open).
    ///
    /// The FTS5 tokenizer-drift repair (former "Step 3") is kept verbatim
    /// AFTER the loop: it is a conditional drift repair, not a schema
    /// version transition, so it stays out of the numbered registry.
    pub fn migrate(&self) -> Result<(), OrchestratorError> {
        let current = migrations::read_user_version(&self.conn)?;

        // Refuse a DB created by a newer binary. The DB is NOT touched.
        if current > migrations::SCHEMA_VERSION_TARGET {
            return Err(OrchestratorError::SchemaVersionTooNew {
                found: current,
                supported: migrations::SCHEMA_VERSION_TARGET,
            });
        }

        // Apply, in registry order, every migration newer than `current`.
        for m in migrations::MIGRATIONS
            .iter()
            .filter(|m| m.version > current)
        {
            self.apply_migration_in_tx(m)?;
        }

        // Non-versioned drift repair: detect tokenizer drift on artifacts_fts. If the live
        // DDL does not include our explicit unicode61 tokenizer, drop +
        // recreate the FTS5 table. Caller (run_server) is responsible
        // for triggering a reindex_all afterwards (we don't do it here
        // to keep migrate() cheap and let the caller decide when to
        // pay the cost).
        let needs_fts_upgrade = self.fts_tokenizer_needs_upgrade()?;
        if needs_fts_upgrade {
            self.conn.execute_batch(
                "BEGIN IMMEDIATE;\
                 DROP TABLE IF EXISTS artifacts_fts;\
                 COMMIT;",
            )?;
            self.conn
                .execute_batch(&format!("BEGIN IMMEDIATE;{FTS_SQL}COMMIT;"))?;
        }

        Ok(())
    }

    /// Apply a single migration inside its own `BEGIN IMMEDIATE`/`COMMIT`
    /// transaction, stamping `PRAGMA user_version` to the migration's
    /// number in the SAME transaction so the schema change and the version
    /// bump are atomic. On any error, a best-effort `ROLLBACK` is issued so
    /// no transaction is ever left open (which would otherwise make the
    /// next operation fail with "cannot start a transaction within a
    /// transaction") and the DB stays on the previous version.
    fn apply_migration_in_tx(&self, m: &migrations::Migration) -> Result<(), OrchestratorError> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;

        let result = (m.apply)(&self.conn)
            .and_then(|()| migrations::write_user_version(&self.conn, m.version));

        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                // Best-effort rollback: ignore its own error, surface the
                // original migration failure.
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(OrchestratorError::Database(e))
            }
        }
    }

    /// Inspect the live DDL of `artifacts_fts` and return true if the
    /// tokenizer is not the explicit unicode61 + extra separators that
    /// RFC bdee1af4 mandates.
    fn fts_tokenizer_needs_upgrade(&self) -> Result<bool, OrchestratorError> {
        let result = self.conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='artifacts_fts'",
            [],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(sql) => {
                // Heuristic: presence of "unicode61" AND "separators".
                Ok(!sql.contains("unicode61") || !sql.contains("separators"))
            }
            // Table missing => caller will create it through migrate(),
            // no upgrade needed.
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Public flag: returns true iff the previous migrate() call detected
    /// a stale FTS5 schema and dropped+recreated it. Callers (run_server)
    /// can use this to trigger a reindex_all afterwards.
    ///
    /// Inspect using the same heuristic: if the live DDL contains
    /// unicode61+separators, no upgrade was needed.
    pub fn fts_was_upgraded(&self) -> Result<bool, OrchestratorError> {
        // Counts as "upgraded" only if the table is currently empty
        // (just dropped+recreated). On steady state we expect non-zero.
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM artifacts_fts", [], |r| r.get(0))?;
        // If empty AND artifacts has rows, FTS was just upgraded.
        let art_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM artifacts", [], |r| r.get(0))?;
        Ok(count == 0 && art_count > 0)
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

    /// List every permit in the table, ordered by id ascending, whatever
    /// its status (RFC cde13417 A1.3). Deterministic order so the canonical
    /// JSON seal exported by [`crate::engine::OrchestratorEngine::write_permits_seal`]
    /// is byte-stable for identical table states. Unlike
    /// [`Self::list_active_permits`], consumed and revoked permits are
    /// included: the seal is a full mirror of the authoritative state.
    pub fn list_all_permits(&self) -> Result<Vec<WritePermit>, OrchestratorError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rfc_id, granted_to, target_paths, status, granted_by, granted_at, consumed_at \
             FROM write_permits ORDER BY id ASC",
        )?;

        let rows: Vec<PermitRow> = stmt
            .query_map([], |row| {
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
            })?
            .collect::<Result<Vec<_>, _>>()?;

        rows.into_iter().map(|r| r.into_permit()).collect()
    }

    /// Insert a permit row verbatim, preserving its status and timestamps
    /// (RFC cde13417 A1.3). Used by the boot reseed
    /// ([`crate::engine::OrchestratorEngine::reseed_permits_from_seal`]) to
    /// reconstruct the table from the HEAD seal. Distinct from
    /// [`Self::create_permit`] only in intent (reconstruction vs fresh
    /// grant); both perform the same full-column INSERT so status,
    /// `granted_at` and `consumed_at` are taken from the row as-is (no new
    /// timestamp is generated).
    pub fn insert_permit_row(&self, permit: &WritePermit) -> Result<(), OrchestratorError> {
        self.create_permit(permit)
    }

    /// Revert a consumed permit back to `active`, clearing `consumed_at`
    /// (RFC cde13417 A1.3). Rollback of a consume whose seal commit failed,
    /// symmetric to the grant rollback (`delete_permit`). Only a row that is
    /// currently `consumed` matches; returns `PermitNotFound` otherwise so a
    /// caller never silently believes a non-consumed permit was reverted.
    pub fn unconsume_permit(&self, id: Uuid) -> Result<(), OrchestratorError> {
        let updated = self.conn.execute(
            "UPDATE write_permits SET status = ?1, consumed_at = NULL WHERE id = ?2 AND status = ?3",
            params![
                PermitStatus::Active.to_string(),
                id.to_string(),
                PermitStatus::Consumed.to_string(),
            ],
        )?;

        if updated == 0 {
            return Err(OrchestratorError::PermitNotFound { id });
        }
        Ok(())
    }

    /// Delete a single permit by id. Used for the targeted rollback of a
    /// freshly-inserted permit when the atomic seal (checkpoint + git
    /// commit) fails in `grant_write_permit` (RFC 359f9162). Only ever
    /// touches the one row identified by `id`, so the rollback of one
    /// grant never collaterally wipes other active permits (edge e).
    /// Returns `Ok(())` even when no row matched (defensively idempotent).
    pub fn delete_permit(&self, id: Uuid) -> Result<(), OrchestratorError> {
        self.conn.execute(
            "DELETE FROM write_permits WHERE id = ?1",
            params![id.to_string()],
        )?;
        Ok(())
    }

    /// Idempotency lookup for `grant_write_permit` (RFC 359f9162,
    /// decision CEO 3). Returns the first **active** permit whose
    /// `(rfc_id, granted_to)` match and whose `target_paths` form the
    /// same normalized set (order-insensitive, deduplicated) as the
    /// requested paths. A consumed or revoked permit never matches, so a
    /// legitimate new grant after consumption is not short-circuited.
    /// Returns `None` when no active permit matches the grant key.
    pub fn find_permit_by_grant(
        &self,
        rfc_id: Uuid,
        granted_to: &PersonaId,
        target_paths: &[PathPattern],
    ) -> Result<Option<WritePermit>, OrchestratorError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rfc_id, granted_to, target_paths, status, granted_by, granted_at, consumed_at \
             FROM write_permits WHERE rfc_id = ?1 AND granted_to = ?2 AND status = ?3",
        )?;

        let rows: Vec<PermitRow> = stmt
            .query_map(
                params![
                    rfc_id.to_string(),
                    granted_to.as_str(),
                    PermitStatus::Active.to_string(),
                ],
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

        // Normalize the requested paths into a sorted, deduplicated set so
        // that the order of paths in the request JSON never breaks
        // idempotence (the semantic key is the *set* of paths).
        let wanted = normalized_path_set(target_paths);

        for row in rows {
            let permit = row.into_permit()?;
            if normalized_path_set(&permit.target_paths) == wanted {
                return Ok(Some(permit));
            }
        }

        Ok(None)
    }

    /// List every permit currently in `active` status (mechanism 11, RFC
    /// a4ee8b6a). Used by `reload_config` to refuse a hot-reload while a
    /// write-permit window is still open. Consumed or revoked permits are
    /// excluded.
    pub fn list_active_permits(&self) -> Result<Vec<WritePermit>, OrchestratorError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rfc_id, granted_to, target_paths, status, granted_by, granted_at, consumed_at \
             FROM write_permits WHERE status = ?1",
        )?;

        let rows: Vec<PermitRow> = stmt
            .query_map(params![PermitStatus::Active.to_string()], |row| {
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
            })?
            .collect::<Result<Vec<_>, _>>()?;

        rows.into_iter().map(|r| r.into_permit()).collect()
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

    /// Upsert an artifact in all four index tables (`artifacts`,
    /// `artifacts_fts`, `artifacts_vec`, `artifact_relations`) inside a
    /// single SQLite transaction.
    ///
    /// Rationale (RFC bdee1af4 proposition 7, invariant 1): a single
    /// artifact is either present in all four tables or in none. If
    /// any insert fails, the transaction is rolled back atomically.
    ///
    /// The `embedding` slice must have exactly [`embedding::EMBEDDING_DIM`]
    /// (= 384) f32 components; sqlite-vec accepts a JSON array literal
    /// `'[f1, f2, ...]'` as a textual representation of the vector.
    ///
    /// Structured filter columns (`author`, `project`, `created_at`) are
    /// passed as `Option<&str>` so a YAML without those fields stores
    /// NULL — the SQL filters in `search_lexical` / `search_semantic`
    /// treat NULL as "does not match".
    pub fn upsert_artifact(
        &mut self,
        artifact: &IndexedArtifact,
        content: &str,
        embedding: &[f32],
        relations: &[ParsedRelation],
    ) -> Result<(), OrchestratorError> {
        self.upsert_artifact_full(artifact, content, embedding, relations, None, None, None)
    }

    /// Same as `upsert_artifact` but with the structured filter columns
    /// explicit. Production code paths (engine.index_artifact) extract
    /// these from the YAML; tests use the simpler form which stores
    /// NULL for all three.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_artifact_full(
        &mut self,
        artifact: &IndexedArtifact,
        content: &str,
        embedding: &[f32],
        relations: &[ParsedRelation],
        author: Option<&str>,
        project: Option<&str>,
        created_at: Option<&str>,
    ) -> Result<(), OrchestratorError> {
        if embedding.len() != crate::embedding::EMBEDDING_DIM {
            return Err(OrchestratorError::EmbeddingFailed {
                reason: format!(
                    "upsert_artifact: embedding has dim {} but EMBEDDING_DIM = {}",
                    embedding.len(),
                    crate::embedding::EMBEDDING_DIM
                ),
            });
        }
        let tags_json = serde_json::to_string(&artifact.tags)?;
        let embedding_json = encode_embedding_as_json(embedding);

        let tx = self.conn.transaction()?;

        // Delete old data — keep the four tables aligned.
        tx.execute("DELETE FROM artifacts WHERE id = ?1", params![artifact.id])?;
        tx.execute(
            "DELETE FROM artifacts_fts WHERE id = ?1",
            params![artifact.id],
        )?;
        tx.execute(
            "DELETE FROM artifacts_vec WHERE artifact_id = ?1",
            params![artifact.id],
        )?;
        tx.execute(
            "DELETE FROM artifact_relations WHERE source_id = ?1",
            params![artifact.id],
        )?;

        // Insert artifact (full row including filter columns).
        tx.execute(
            "INSERT INTO artifacts
               (id, kind, title, description, tags, file_path, indexed_at,
                author, project, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                artifact.id,
                artifact.kind,
                artifact.title,
                artifact.description,
                tags_json,
                artifact.file_path,
                artifact.indexed_at,
                author,
                project,
                created_at,
            ],
        )?;

        // Insert FTS row
        tx.execute(
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

        // Insert vector row.
        tx.execute(
            "INSERT INTO artifacts_vec (artifact_id, embedding) VALUES (?1, ?2)",
            params![artifact.id, embedding_json],
        )?;

        // Insert relations
        for rel in relations {
            tx.execute(
                "INSERT OR IGNORE INTO artifact_relations (source_id, target_id, relationship)
                 VALUES (?1, ?2, ?3)",
                params![artifact.id, rel.target_id, rel.relationship],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Lexical-only search via FTS5 + BM25 + structured filter push-down.
    /// Returns ranked ids (1-indexed) for downstream fusion.
    ///
    /// `fts_query` must already be sanitised by
    /// [`crate::query::sanitize_fts_query`]; if empty, the lexical path
    /// is short-circuited and returns an empty Vec.
    pub fn search_lexical(
        &self,
        fts_query: &str,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<Vec<crate::fusion::RankedResult>, OrchestratorError> {
        if fts_query.trim().is_empty() {
            return Ok(Vec::new());
        }

        // Build dynamic WHERE + params. The base is always:
        //   WHERE artifacts_fts MATCH ?
        let mut sql = String::from(
            "SELECT a.id FROM artifacts_fts f \
             JOIN artifacts a ON a.id = f.id \
             WHERE artifacts_fts MATCH ?",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        params_vec.push(Box::new(fts_query.to_string()));

        append_filter_clauses(filters, &mut sql, &mut params_vec);

        // BM25 ranking. Five indexed columns in artifacts_fts (kind,
        // title, description, tags, content — id is UNINDEXED). Weights:
        //   kind=0 (filter only), title=10, description=3, tags=5, content=1.
        sql.push_str(" ORDER BY bm25(artifacts_fts, 0.0, 10.0, 3.0, 5.0, 1.0) LIMIT ?");
        params_vec.push(Box::new(limit as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|b| b.as_ref()).collect();

        let ids: Vec<String> = stmt
            .query_map(params_refs.as_slice(), |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(ids
            .into_iter()
            .enumerate()
            .map(|(i, id)| crate::fusion::RankedResult { id, rank: i + 1 })
            .collect())
    }

    /// Semantic search via sqlite-vec kNN over `artifacts_vec`. Returns
    /// ranked ids (1-indexed). Filters are applied as a post-filter on
    /// the JOIN; for our corpus (< 1000 rows) brute-force kNN + JOIN
    /// is sub-50ms.
    pub fn search_semantic(
        &self,
        embedding: &[f32],
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<Vec<crate::fusion::RankedResult>, OrchestratorError> {
        if embedding.len() != crate::embedding::EMBEDDING_DIM {
            return Err(OrchestratorError::EmbeddingFailed {
                reason: format!(
                    "search_semantic: query embedding has dim {} but EMBEDDING_DIM = {}",
                    embedding.len(),
                    crate::embedding::EMBEDDING_DIM
                ),
            });
        }
        let q_json = encode_embedding_as_json(embedding);

        // Inner kNN over vec0; outer JOIN applies structured filters.
        // We over-fetch (limit * 3, capped at 200) to leave headroom for
        // the post-filter to still produce `limit` results in the
        // common case.
        let knn_k = (limit * 3).clamp(limit, 200);
        let mut sql = format!(
            "SELECT v.artifact_id, v.distance \
             FROM artifacts_vec v \
             JOIN artifacts a ON a.id = v.artifact_id \
             WHERE v.embedding MATCH ? AND v.k = {knn_k}"
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        params_vec.push(Box::new(q_json));

        append_filter_clauses(filters, &mut sql, &mut params_vec);

        sql.push_str(" ORDER BY v.distance LIMIT ?");
        params_vec.push(Box::new(limit as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|b| b.as_ref()).collect();

        let ids: Vec<String> = stmt
            .query_map(params_refs.as_slice(), |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(ids
            .into_iter()
            .enumerate()
            .map(|(i, id)| crate::fusion::RankedResult { id, rank: i + 1 })
            .collect())
    }

    /// List artifacts matching the given filters ordered by created_at
    /// desc (recent first, NULLs last). Used for the empty-query +
    /// filters mode of `search`.
    pub fn list_with_filters(
        &self,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<Vec<ArtifactSummary>, OrchestratorError> {
        let mut sql = String::from(
            "SELECT a.id, a.kind, a.title, a.description, a.tags \
             FROM artifacts a WHERE 1=1",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        append_filter_clauses(filters, &mut sql, &mut params_vec);
        sql.push_str(" ORDER BY COALESCE(a.created_at, a.indexed_at) DESC LIMIT ?");
        params_vec.push(Box::new(limit as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|b| b.as_ref()).collect();
        let rows: Vec<(String, String, String, String, String)> = stmt
            .query_map(params_refs.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows
            .into_iter()
            .map(|(id, kind, title, description, tags_json)| {
                let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
                ArtifactSummary {
                    id,
                    kind,
                    title,
                    description,
                    tags,
                }
            })
            .collect())
    }

    /// Legacy lexical search used by callers that have not yet migrated
    /// to the (filters, mode) API. Kept for backwards compat during the
    /// transition; new callers should use `search_lexical` or the engine
    /// `search` entry point.
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

    /// Mechanism 17b (RFC 0197fbe5) — bulk dangling-related detection.
    /// One SQL pass over the fully-populated index: every relation whose
    /// `target_id` is not itself an indexed artifact is a dangling link.
    /// Order-insensitive by construction (the whole corpus is indexed before
    /// this runs), unlike the single-file surface which can produce a
    /// transient false positive for two mutually-referencing new artifacts.
    /// Returns `(source_id, target_id)` pairs. Uses `idx_relations_target`.
    pub fn dangling_related_links(&self) -> Result<Vec<(String, String)>, OrchestratorError> {
        let mut stmt = self.conn.prepare(
            "SELECT source_id, target_id FROM artifact_relations \
             WHERE target_id NOT IN (SELECT id FROM artifacts)",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Mechanism 19a (RFC 0197fbe5) — does artifact `id` have any relation
    /// (either direction) to an artifact of kind `lesson-learned`? Backs the
    /// non-blocking capitalization reminders on `rfc_set_implemented` and on a
    /// resolved diagnostic-report. Uses the bidirectional relation table with
    /// a join on the artifacts kind.
    pub fn has_linked_lesson(&self, id: &str) -> Result<bool, OrchestratorError> {
        let found: bool = self.conn.query_row(
            "SELECT EXISTS(\
               SELECT 1 FROM artifact_relations r \
               JOIN artifacts a \
                 ON a.id = CASE WHEN r.source_id = ?1 THEN r.target_id ELSE r.source_id END \
               WHERE (r.source_id = ?1 OR r.target_id = ?1) \
                 AND a.kind = 'lesson-learned')",
            params![id],
            |row| row.get(0),
        )?;
        Ok(found)
    }

    /// Mechanism 18a (RFC 0197fbe5) — list every review round whose
    /// `artifact_path` matches `path` (relative form, same shape as the index
    /// `file_path`). Backs `write_permit_gate`: given an implementation-plan's
    /// file_path, find its rounds to check for a Closed + consensus one.
    pub fn list_rounds_by_artifact_path(
        &self,
        path: &str,
    ) -> Result<Vec<ReviewRound>, OrchestratorError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, artifact_path, artifact_kind, author, required_reviewers, status, \
             iteration, max_iterations, votes, created_at, updated_at \
             FROM review_rounds WHERE artifact_path = ?1",
        )?;
        let rows = stmt.query_map(params![path], |row| {
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
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?.into_round()?);
        }
        Ok(out)
    }

    /// Return the three table counts (`artifacts`, `artifacts_fts`,
    /// `artifacts_vec`) for [`crate::engine::IndexStatusGlobal`].
    pub fn index_table_counts(&self) -> Result<(usize, usize, usize), OrchestratorError> {
        let a: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM artifacts", [], |r| r.get(0))?;
        let f: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM artifacts_fts", [], |r| r.get(0))?;
        let v: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM artifacts_vec", [], |r| r.get(0))?;
        Ok((a as usize, f as usize, v as usize))
    }

    /// Return MAX(indexed_at) across the artifacts table, or None if
    /// the table is empty.
    pub fn last_indexed_at(&self) -> Result<Option<String>, OrchestratorError> {
        let result = self
            .conn
            .query_row("SELECT MAX(indexed_at) FROM artifacts", [], |row| {
                row.get::<_, Option<String>>(0)
            })?;
        Ok(result)
    }

    /// Return the row in artifacts matching `id`, if any (file_path +
    /// indexed_at). Used by index_status per_path lookups.
    pub fn artifact_by_id_status(
        &self,
        id: &str,
    ) -> Result<Option<(String, String, bool, bool)>, OrchestratorError> {
        // file_path, indexed_at, present_in_fts, present_in_vec
        let result = self.conn.query_row(
            "SELECT a.file_path, a.indexed_at,
                    EXISTS(SELECT 1 FROM artifacts_fts WHERE id = ?1),
                    EXISTS(SELECT 1 FROM artifacts_vec WHERE artifact_id = ?1)
             FROM artifacts a WHERE a.id = ?1",
            params![id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            },
        );
        match result {
            Ok(t) => Ok(Some(t)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Return the artifact id corresponding to a given file_path, if any.
    pub fn artifact_id_by_path(&self, path: &str) -> Result<Option<String>, OrchestratorError> {
        let result = self.conn.query_row(
            "SELECT id FROM artifacts WHERE file_path = ?1",
            params![path],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(id) => Ok(Some(id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn delete_all_artifacts(&self) -> Result<(), OrchestratorError> {
        self.conn.execute("DELETE FROM artifact_relations", [])?;
        self.conn.execute("DELETE FROM artifacts_fts", [])?;
        self.conn.execute("DELETE FROM artifacts_vec", [])?;
        self.conn.execute("DELETE FROM artifacts", [])?;
        Ok(())
    }
}

/// Encode a float32 embedding vector as the JSON-array textual format
/// accepted by `sqlite-vec`'s vec0 INSERT (e.g. `"[0.1, -0.2, ...]"`).
///
/// Round-trips through `Vec<f64>` is intentionally avoided: keeping the
/// f32 precision matches the storage layout (`float[384]`) and avoids
/// silent precision inflation in the JSON.
fn encode_embedding_as_json(embedding: &[f32]) -> String {
    let mut out = String::with_capacity(embedding.len() * 12 + 2);
    out.push('[');
    for (i, v) in embedding.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        // Use `{:?}` so NaN / inf are reported verbatim and SQLite-vec
        // will reject them deterministically rather than silently coerce.
        use std::fmt::Write;
        let _ = write!(out, "{v:?}");
    }
    out.push(']');
    out
}

/// Simple glob-like path matching (supports prefix matching with trailing /).
/// Normalize a list of `PathPattern` into a sorted, deduplicated set of
/// the underlying strings. Used by `find_permit_by_grant` so that the
/// idempotency key compares path *sets* rather than ordered lists
/// (RFC 359f9162, decision CEO 3).
fn normalized_path_set(paths: &[PathPattern]) -> Vec<String> {
    let mut set: Vec<String> = paths.iter().map(|p| p.0.clone()).collect();
    set.sort();
    set.dedup();
    set
}

/// Match a permit path pattern against a concrete path (exact, `dir/` prefix,
/// or glob). Made `pub(crate)` (mechanism 14, RFC 0197fbe5) so the engine's
/// `rfc_scope_warnings` and the trigger evaluation reuse the SAME semantics as
/// the permit checker — NOT a 4th independent implementation (the full
/// unification is reserved for v1-harness-refactor). Single internal caller
/// besides these: `check_permit` (L778).
pub(crate) fn path_matches(pattern: &PathPattern, path: &str) -> bool {
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

    /// Dummy embedding of the correct dimension for tests that just want
    /// to exercise the SQL pipeline without invoking fastembed. The
    /// vector is unit-L2 (cosine-friendly) and varies a little per id
    /// (first byte tweak) so that kNN tests are not degenerate.
    fn dummy_embedding(seed: u8) -> Vec<f32> {
        let mut v = vec![0.0_f32; crate::embedding::EMBEDDING_DIM];
        v[0] = 1.0;
        if seed != 0 {
            v[1] = (seed as f32) * 1e-3;
        }
        v
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

    // Helper: does column `col` exist on `table`? Mirrors the production
    // `column_exists` (PRAGMA table_info) but lives in the test module so we
    // can assert on schema shape without exposing the private helper.
    fn test_column_exists(db: &OrchestratorDb, table: &str, col: &str) -> bool {
        let mut stmt = db
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        names.iter().any(|n| n == col)
    }

    // Helper: list index names on `table`.
    fn test_index_names(db: &OrchestratorDb, table: &str) -> Vec<String> {
        let mut stmt = db
            .conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = ?1 \
                 ORDER BY name",
            )
            .unwrap();
        stmt.query_map([table], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    // Helper: does a table/virtual-table named `name` exist?
    fn test_table_exists(db: &OrchestratorDb, name: &str) -> bool {
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1 \
                 AND type IN ('table', 'view')",
                [name],
                |r| r.get(0),
            )
            .unwrap();
        count > 0
    }

    fn user_version(db: &OrchestratorDb) -> i64 {
        db.conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    // TEST 1 — NOMINAL: a fresh in-memory DB migrates to the target version
    // with the complete schema (tables, filter columns, indexes) and passes
    // integrity_check.
    #[test]
    fn test_migrate_fresh_db_reaches_target_version() {
        let db = OrchestratorDb::open(":memory:").unwrap();
        db.migrate().unwrap();

        assert_eq!(
            user_version(&db),
            crate::migrations::SCHEMA_VERSION_TARGET,
            "fresh DB must end at SCHEMA_VERSION_TARGET"
        );

        for t in [
            "artifacts",
            "artifacts_fts",
            "artifacts_vec",
            "index_metadata",
        ] {
            assert!(test_table_exists(&db, t), "missing table {t}");
        }

        for c in ["author", "project", "created_at"] {
            assert!(
                test_column_exists(&db, "artifacts", c),
                "missing column {c} on artifacts"
            );
        }

        let idx = test_index_names(&db, "artifacts");
        for expected in [
            "idx_artifacts_author",
            "idx_artifacts_created_at",
            "idx_artifacts_project",
        ] {
            assert!(
                idx.iter().any(|n| n == expected),
                "missing index {expected}; got {idx:?}"
            );
        }

        assert!(db.integrity_check().unwrap());
    }

    // TEST 3 — EDGE: a DB already at the target version is a no-op on a second
    // migrate() (user_version stays put, no migration re-applied). Evolution
    // of the former test_migrate_is_idempotent.
    #[test]
    fn test_migrate_is_idempotent() {
        let db = OrchestratorDb::open(":memory:").unwrap();
        db.migrate().unwrap();
        let v_after_first = user_version(&db);
        assert_eq!(v_after_first, crate::migrations::SCHEMA_VERSION_TARGET);

        // Snapshot the schema object set to prove the second call is a no-op.
        let count_before: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0))
            .unwrap();

        db.migrate().unwrap();

        assert_eq!(
            user_version(&db),
            v_after_first,
            "second migrate() must not change user_version"
        );
        let count_after: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count_before, count_after,
            "second migrate() must not create/drop schema objects"
        );
        assert!(db.integrity_check().unwrap());
    }

    // TEST 4 — EDGE: resume after interruption. We simulate a DB that stopped
    // after migration 1 (schema v1 applied, user_version = 1) and assert that
    // migrate() applies ONLY migration 2, finishing at the target version.
    #[test]
    fn test_migrate_resumes_from_intermediate_version() {
        let db = OrchestratorDb::open(":memory:").unwrap();

        // Simulate a DB whose schema reached version 1 BEFORE migration 2
        // existed: a legacy `artifacts` table without the filter columns,
        // plus the FTS/vec tables, stamped user_version = 1 as if the process
        // committed migration 1 and then stopped.
        db.conn
            .execute_batch(
                "CREATE TABLE artifacts (
                    id TEXT PRIMARY KEY,
                    kind TEXT NOT NULL,
                    title TEXT NOT NULL DEFAULT '',
                    description TEXT NOT NULL DEFAULT '',
                    tags TEXT NOT NULL DEFAULT '[]',
                    file_path TEXT NOT NULL,
                    indexed_at TEXT NOT NULL
                );",
            )
            .unwrap();
        db.conn.execute_batch(crate::db::FTS_SQL).unwrap();
        db.conn.execute_batch(crate::db::VEC_TABLE_SQL).unwrap();
        db.conn.execute_batch("PRAGMA user_version = 1").unwrap();

        // At v1 the filter columns/indexes must NOT exist yet.
        assert!(!test_column_exists(&db, "artifacts", "author"));

        db.migrate().unwrap();

        assert_eq!(
            user_version(&db),
            crate::migrations::SCHEMA_VERSION_TARGET,
            "resume must finish at SCHEMA_VERSION_TARGET"
        );
        for c in ["author", "project", "created_at"] {
            assert!(
                test_column_exists(&db, "artifacts", c),
                "migration 2 column {c} must be present after resume"
            );
        }
        let idx = test_index_names(&db, "artifacts");
        for expected in [
            "idx_artifacts_author",
            "idx_artifacts_created_at",
            "idx_artifacts_project",
        ] {
            assert!(idx.iter().any(|n| n == expected), "missing {expected}");
        }
    }

    // TEST 5 — EDGE/NEGATIVE: a DB whose user_version is newer than this
    // binary supports is refused with SchemaVersionTooNew and is left
    // untouched.
    // TEST 4bis — EDGE: the exact divergent state of the PRODUCTION DB at the
    // time of RFC 973a5569: user_version is still 0 (it predates the
    // user_version discipline) but the artifacts table ALREADY has the filter
    // columns AND their indexes (the old imperative migrate() added them on a
    // previous boot). migrate() must treat this as a full run (current = 0),
    // find every ALTER/index already satisfied (no-op via column_exists /
    // IF NOT EXISTS), and stamp user_version to the target WITHOUT crashing.
    #[test]
    fn test_migrate_legacy_v0_with_columns_already_present_boots() {
        let db = OrchestratorDb::open(":memory:").unwrap();

        // Materialize the full current schema, then explicitly reset
        // user_version to 0 to mirror a DB that reached today's shape under
        // the OLD migrate() (which never set user_version).
        db.migrate().unwrap();
        db.conn.execute_batch("PRAGMA user_version = 0").unwrap();
        // Sanity: columns + indexes already there, version artificially 0.
        assert!(test_column_exists(&db, "artifacts", "author"));
        assert_eq!(user_version(&db), 0);

        // Re-running migrate() must not crash on "duplicate column" and must
        // re-stamp the version to the target.
        db.migrate()
            .expect("legacy v0 DB with columns already present must boot");
        assert_eq!(
            user_version(&db),
            crate::migrations::SCHEMA_VERSION_TARGET,
            "divergent legacy DB must be re-stamped to SCHEMA_VERSION_TARGET"
        );
        assert!(db.integrity_check().unwrap());
    }

    #[test]
    fn test_migrate_refuses_future_version_untouched() {
        let db = OrchestratorDb::open(":memory:").unwrap();
        db.migrate().unwrap();
        let future = crate::migrations::SCHEMA_VERSION_TARGET + 1;
        db.conn
            .execute_batch(&format!("PRAGMA user_version = {future}"))
            .unwrap();

        let err = db.migrate().unwrap_err();
        assert!(
            matches!(
                err,
                OrchestratorError::SchemaVersionTooNew { found, supported }
                    if found == future && supported == crate::migrations::SCHEMA_VERSION_TARGET
            ),
            "expected SchemaVersionTooNew, got {err:?}"
        );
        // DB must be untouched: user_version still the future value.
        assert_eq!(user_version(&db), future, "refused DB must not be modified");
    }

    // Hotfix RFC bdee1af4: a legacy DB whose `artifacts` table predates
    // the (author, project, created_at) columns must still migrate
    // successfully. The bug was: SCHEMA_SQL emitted CREATE INDEX on
    // those columns before step 2's ALTER TABLE could add them, so
    // migrate() failed with "no such column: author" on boot.
    #[test]
    fn test_migrate_legacy_db_without_filter_columns_succeeds() {
        let db = OrchestratorDb::open(":memory:").unwrap();

        // Hand-craft a pre-migration `artifacts` table that mirrors the
        // legacy schema (no author / project / created_at columns).
        db.conn
            .execute_batch(
                "CREATE TABLE artifacts (
                    id TEXT PRIMARY KEY,
                    kind TEXT NOT NULL,
                    title TEXT NOT NULL DEFAULT '',
                    description TEXT NOT NULL DEFAULT '',
                    tags TEXT NOT NULL DEFAULT '[]',
                    file_path TEXT NOT NULL,
                    indexed_at TEXT NOT NULL
                );",
            )
            .unwrap();

        // migrate() must NOT fail with "no such column: author".
        db.migrate()
            .expect("migrate() must succeed on a legacy DB lacking the filter columns");

        // Verify the three indexes now exist on the artifacts table.
        let mut stmt = db
            .conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'artifacts' \
                 ORDER BY name",
            )
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        for expected in [
            "idx_artifacts_author",
            "idx_artifacts_created_at",
            "idx_artifacts_kind",
            "idx_artifacts_project",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing expected index {expected} on artifacts; got {names:?}"
            );
        }

        // A legacy DB starts at user_version = 0 and must be boot-strapped to
        // the target version (RFC 973a5569 amorçage current=0).
        assert_eq!(
            user_version(&db),
            crate::migrations::SCHEMA_VERSION_TARGET,
            "legacy DB must be boot-strapped to SCHEMA_VERSION_TARGET"
        );
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
            notes: None,
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
            notes: None,
            submitted_at: Utc::now(),
        });
        db.create_round(&round).unwrap();

        let fetched = db.get_round(id).unwrap().unwrap();
        assert_eq!(fetched.required_reviewers.len(), 3);
        assert_eq!(fetched.required_reviewers[2], PersonaId::Implementer);
        assert_eq!(fetched.votes[0].findings.len(), 2);
        assert_eq!(fetched.votes[0].findings[1].0, "Fix naming");
    }

    #[test]
    fn test_review_vote_notes_roundtrip() {
        // GARDE 2b (RFC 8bf78218): the optional `notes` field survives the
        // JSON serialization roundtrip through the votes_json column.
        let db = setup_db();
        let id = Uuid::new_v4();
        let mut round = make_round(id);
        round.votes.push(ReviewVote {
            reviewer: PersonaId::Pm,
            verdict: ReviewVerdict::Approve,
            findings: vec![],
            notes: Some("non-corrective observation".into()),
            submitted_at: Utc::now(),
        });
        db.create_round(&round).unwrap();

        let fetched = db.get_round(id).unwrap().unwrap();
        assert_eq!(
            fetched.votes[0].notes.as_deref(),
            Some("non-corrective observation")
        );
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

    // --- delete_permit (targeted rollback, RFC 359f9162 étape 1) ---

    #[test]
    fn test_delete_permit_removes_only_target() {
        let db = setup_db();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let p1 = make_permit(id1, vec!["a"]);
        let p2 = make_permit(id2, vec!["b"]);
        db.create_permit(&p1).unwrap();
        db.create_permit(&p2).unwrap();

        db.delete_permit(id1).unwrap();

        assert!(
            db.get_permit(id1).unwrap().is_none(),
            "deleted permit must be gone"
        );
        assert!(
            db.get_permit(id2).unwrap().is_some(),
            "other permit must survive (edge e)"
        );
    }

    #[test]
    fn test_delete_permit_idempotent_when_absent() {
        let db = setup_db();
        // Deleting a non-existent id is a no-op Ok (defensively idempotent).
        assert!(db.delete_permit(Uuid::new_v4()).is_ok());
    }

    // --- find_permit_by_grant (idempotency lookup, RFC 359f9162 étape 2) ---

    /// Build a permit with explicit rfc_id and status for the idempotency
    /// lookup tests.
    fn make_permit_full(
        id: Uuid,
        rfc_id: Uuid,
        status: PermitStatus,
        paths: Vec<&str>,
    ) -> WritePermit {
        WritePermit {
            id,
            rfc_id,
            granted_to: PersonaId::Implementer,
            target_paths: paths.into_iter().map(|s| PathPattern(s.into())).collect(),
            status,
            granted_by: PersonaId::Ceo,
            granted_at: Utc::now(),
            consumed_at: None,
        }
    }

    #[test]
    fn test_find_permit_by_grant_order_insensitive_match() {
        let db = setup_db();
        let rfc = Uuid::new_v4();
        let id = Uuid::new_v4();
        let permit = make_permit_full(id, rfc, PermitStatus::Active, vec!["a", "b"]);
        db.create_permit(&permit).unwrap();

        // Same rfc + persona, paths in reversed order → matches.
        let reversed = vec![PathPattern("b".into()), PathPattern("a".into())];
        let found = db
            .find_permit_by_grant(rfc, &PersonaId::Implementer, &reversed)
            .unwrap();
        assert!(found.is_some(), "reversed path order should still match");
        assert_eq!(found.unwrap().id, id);
    }

    #[test]
    fn test_find_permit_by_grant_different_paths_no_match() {
        let db = setup_db();
        let rfc = Uuid::new_v4();
        let permit = make_permit_full(Uuid::new_v4(), rfc, PermitStatus::Active, vec!["a", "b"]);
        db.create_permit(&permit).unwrap();

        let other = vec![PathPattern("a".into()), PathPattern("c".into())];
        let found = db
            .find_permit_by_grant(rfc, &PersonaId::Implementer, &other)
            .unwrap();
        assert!(found.is_none(), "different path set must not match");
    }

    #[test]
    fn test_find_permit_by_grant_different_rfc_no_match() {
        let db = setup_db();
        let rfc = Uuid::new_v4();
        let permit = make_permit_full(Uuid::new_v4(), rfc, PermitStatus::Active, vec!["a", "b"]);
        db.create_permit(&permit).unwrap();

        let paths = vec![PathPattern("a".into()), PathPattern("b".into())];
        let found = db
            .find_permit_by_grant(Uuid::new_v4(), &PersonaId::Implementer, &paths)
            .unwrap();
        assert!(found.is_none(), "different rfc_id must not match");
    }

    #[test]
    fn test_find_permit_by_grant_consumed_no_match() {
        let db = setup_db();
        let rfc = Uuid::new_v4();
        let id = Uuid::new_v4();
        let permit = make_permit_full(id, rfc, PermitStatus::Active, vec!["a", "b"]);
        db.create_permit(&permit).unwrap();
        db.consume_permit(id).unwrap();

        let paths = vec![PathPattern("a".into()), PathPattern("b".into())];
        let found = db
            .find_permit_by_grant(rfc, &PersonaId::Implementer, &paths)
            .unwrap();
        assert!(
            found.is_none(),
            "a consumed permit must not short-circuit a new legitimate grant"
        );
    }

    #[test]
    fn test_find_permit_by_grant_dedup_normalization() {
        let db = setup_db();
        let rfc = Uuid::new_v4();
        let id = Uuid::new_v4();
        // Stored with a duplicate path; lookup with the deduplicated set
        // should still match (normalization dedups both sides).
        let permit = make_permit_full(id, rfc, PermitStatus::Active, vec!["a", "a", "b"]);
        db.create_permit(&permit).unwrap();

        let paths = vec![PathPattern("b".into()), PathPattern("a".into())];
        let found = db
            .find_permit_by_grant(rfc, &PersonaId::Implementer, &paths)
            .unwrap();
        assert!(found.is_some(), "dedup-normalized set should match");
        assert_eq!(found.unwrap().id, id);
    }

    // --- list_all_permits / insert_permit_row / unconsume_permit
    //     (RFC cde13417 A1.3) ---

    // NOMINAL: list_all_permits returns every status, ordered by id asc.
    #[test]
    fn test_list_all_permits_ordered_all_statuses() {
        let db = setup_db();
        let id_b = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
        let id_a = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        db.create_permit(&make_permit_full(
            id_b,
            Uuid::new_v4(),
            PermitStatus::Active,
            vec!["b"],
        ))
        .unwrap();
        db.create_permit(&make_permit_full(
            id_a,
            Uuid::new_v4(),
            PermitStatus::Active,
            vec!["a"],
        ))
        .unwrap();
        db.consume_permit(id_a).unwrap(); // one consumed, must still be listed
        let all = db.list_all_permits().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, id_a, "ordered by id asc");
        assert_eq!(all[0].status, PermitStatus::Consumed);
        assert_eq!(all[1].id, id_b);
    }

    // EDGE: list_all_permits on an empty table is an empty vec (not an error).
    #[test]
    fn test_list_all_permits_empty() {
        let db = setup_db();
        assert!(db.list_all_permits().unwrap().is_empty());
    }

    // NOMINAL: insert_permit_row preserves status and timestamps verbatim
    // (reseed reconstruction, no new timestamp generated).
    #[test]
    fn test_insert_permit_row_preserves_state() {
        let db = setup_db();
        let id = Uuid::new_v4();
        let mut permit = make_permit_full(id, Uuid::new_v4(), PermitStatus::Consumed, vec!["x"]);
        let consumed_at = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05+00:00")
            .unwrap()
            .with_timezone(&Utc);
        permit.consumed_at = Some(consumed_at);
        db.insert_permit_row(&permit).unwrap();
        let fetched = db.get_permit(id).unwrap().unwrap();
        assert_eq!(fetched.status, PermitStatus::Consumed);
        assert_eq!(fetched.consumed_at, Some(consumed_at));
    }

    // NOMINAL: unconsume_permit reverts consumed -> active and clears consumed_at.
    #[test]
    fn test_unconsume_permit_reverts() {
        let db = setup_db();
        let id = Uuid::new_v4();
        db.create_permit(&make_permit_full(
            id,
            Uuid::new_v4(),
            PermitStatus::Active,
            vec!["x"],
        ))
        .unwrap();
        db.consume_permit(id).unwrap();
        db.unconsume_permit(id).unwrap();
        let fetched = db.get_permit(id).unwrap().unwrap();
        assert_eq!(fetched.status, PermitStatus::Active);
        assert!(fetched.consumed_at.is_none());
    }

    // NÉGATIF: unconsume on a non-consumed (active) permit is PermitNotFound.
    #[test]
    fn test_unconsume_permit_not_consumed_errors() {
        let db = setup_db();
        let id = Uuid::new_v4();
        db.create_permit(&make_permit_full(
            id,
            Uuid::new_v4(),
            PermitStatus::Active,
            vec!["x"],
        ))
        .unwrap();
        let res = db.unconsume_permit(id);
        assert!(matches!(res, Err(OrchestratorError::PermitNotFound { .. })));
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
        let mut db = setup_db();
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
            &dummy_embedding(1),
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
        let mut db = setup_db();
        let r1 = make_indexed("road-001", "roadmap", "Beta Roadmap");
        let r2 = make_indexed("road-002", "roadmap", "Alpha Roadmap");
        let other = make_indexed("rfc-001", "rfc", "Some RFC");
        db.upsert_artifact(&r1, "content", &dummy_embedding(1), &[])
            .unwrap();
        db.upsert_artifact(&r2, "content", &dummy_embedding(2), &[])
            .unwrap();
        db.upsert_artifact(&other, "content", &dummy_embedding(3), &[])
            .unwrap();

        let results = db.list_by_kind("roadmap").unwrap();
        assert_eq!(results.len(), 2);
        // Ordered by title: "Alpha" < "Beta"
        assert_eq!(results[0].id, "road-002");
        assert_eq!(results[1].id, "road-001");
        assert!(results.iter().all(|a| a.kind == "roadmap"));
    }

    #[test]
    fn test_list_by_kind_empty_when_no_match() {
        let mut db = setup_db();
        let rfc = make_indexed("rfc-only", "rfc", "Lonely RFC");
        db.upsert_artifact(&rfc, "content", &dummy_embedding(1), &[])
            .unwrap();

        let results = db.list_by_kind("roadmap").unwrap();
        assert!(results.is_empty());
    }

    // RFC bdee1af4 étape 6: transactional upsert writes all four tables.

    #[test]
    fn test_upsert_writes_artifacts_vec_too() {
        let mut db = setup_db();
        let art = make_indexed("vec-1", "rfc", "T");
        db.upsert_artifact(&art, "content", &dummy_embedding(7), &[])
            .unwrap();
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM artifacts_vec WHERE artifact_id = ?1",
                params!["vec-1"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "artifacts_vec must contain one row for vec-1");
    }

    #[test]
    fn test_upsert_rejects_wrong_dimension() {
        let mut db = setup_db();
        let art = make_indexed("vec-1", "rfc", "T");
        let too_short = vec![1.0_f32; 100]; // not 384
        let res = db.upsert_artifact(&art, "content", &too_short, &[]);
        assert!(res.is_err(), "wrong-dim embedding must be rejected");
    }

    // RFC bdee1af4 étape 10: lexical search via search_lexical + filters.

    fn insert_test_artifact(db: &mut OrchestratorDb, id: &str, kind: &str, title: &str) {
        let art = IndexedArtifact {
            id: id.into(),
            kind: kind.into(),
            title: title.into(),
            description: String::new(),
            tags: vec![],
            file_path: format!("fixtures/{id}.yml"),
            indexed_at: Utc::now().to_rfc3339(),
        };
        db.upsert_artifact(&art, title, &dummy_embedding(id.as_bytes()[0]), &[])
            .unwrap();
    }

    #[test]
    fn test_search_lexical_returns_ranked_results() {
        let mut db = setup_db();
        insert_test_artifact(&mut db, "a", "rfc", "embeddings tutorial");
        insert_test_artifact(&mut db, "b", "design-doc", "unrelated topic");

        let q = crate::query::sanitize_fts_query("embeddings", crate::query::QueryMode::Natural);
        let results = db
            .search_lexical(&q, &SearchFilters::default(), 10)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "a");
        assert_eq!(results[0].rank, 1);
    }

    #[test]
    fn test_search_lexical_filter_kind_excludes_others() {
        let mut db = setup_db();
        insert_test_artifact(&mut db, "rfc1", "rfc", "foo bar");
        insert_test_artifact(&mut db, "dd1", "design-doc", "foo bar");

        let q = crate::query::sanitize_fts_query("foo", crate::query::QueryMode::Natural);
        let filters = SearchFilters {
            kinds: Some(vec!["rfc".into()]),
            ..Default::default()
        };
        let results = db.search_lexical(&q, &filters, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "rfc1");
    }

    #[test]
    fn test_search_lexical_empty_query_returns_empty() {
        let db = setup_db();
        let results = db
            .search_lexical("", &SearchFilters::default(), 10)
            .unwrap();
        assert!(results.is_empty());
    }

    // RFC 1d3a3581: contract test for the surviving structured filters.
    // After removing kind/author/project/dates from the MCP surface, only
    // tags, id_prefix (and the internal kinds) remain. This pins that the
    // two agent-facing survivors still push down correctly.

    #[test]
    fn test_search_lexical_filter_id_prefix_survives() {
        let mut db = setup_db();
        insert_test_artifact(&mut db, "abc12345", "rfc", "foo bar");
        insert_test_artifact(&mut db, "xyz98765", "rfc", "foo bar");

        let q = crate::query::sanitize_fts_query("foo", crate::query::QueryMode::Natural);
        let filters = SearchFilters {
            id_prefix: Some("abc".into()),
            ..Default::default()
        };
        let results = db.search_lexical(&q, &filters, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "abc12345");
    }

    #[test]
    fn test_search_lexical_filter_tags_survives() {
        let mut db = setup_db();
        let tagged = IndexedArtifact {
            id: "tagged".into(),
            kind: "rfc".into(),
            title: "foo bar".into(),
            description: String::new(),
            tags: vec!["search".into(), "retrieval".into()],
            file_path: "fixtures/tagged.yml".into(),
            indexed_at: Utc::now().to_rfc3339(),
        };
        let untagged = IndexedArtifact {
            id: "untagged".into(),
            kind: "rfc".into(),
            title: "foo bar".into(),
            description: String::new(),
            tags: vec!["unrelated".into()],
            file_path: "fixtures/untagged.yml".into(),
            indexed_at: Utc::now().to_rfc3339(),
        };
        db.upsert_artifact(&tagged, "foo bar", &dummy_embedding(1), &[])
            .unwrap();
        db.upsert_artifact(&untagged, "foo bar", &dummy_embedding(2), &[])
            .unwrap();

        let q = crate::query::sanitize_fts_query("foo", crate::query::QueryMode::Natural);
        let filters = SearchFilters {
            tags: Some(vec!["search".into()]),
            ..Default::default()
        };
        let results = db.search_lexical(&q, &filters, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "tagged");
    }

    #[test]
    fn test_search_lexical_bm25_orders_title_above_body() {
        let mut db = setup_db();
        // a has the term in the title, b has it in the content only.
        let art_a = IndexedArtifact {
            id: "a".into(),
            kind: "rfc".into(),
            title: "caching layer overview".into(),
            description: String::new(),
            tags: vec![],
            file_path: "a.yml".into(),
            indexed_at: Utc::now().to_rfc3339(),
        };
        let art_b = IndexedArtifact {
            id: "b".into(),
            kind: "rfc".into(),
            title: "unrelated".into(),
            description: String::new(),
            tags: vec![],
            file_path: "b.yml".into(),
            indexed_at: Utc::now().to_rfc3339(),
        };
        db.upsert_artifact(&art_a, "body without keyword", &dummy_embedding(1), &[])
            .unwrap();
        db.upsert_artifact(
            &art_b,
            "long body that mentions caching once and that is it",
            &dummy_embedding(2),
            &[],
        )
        .unwrap();

        let q = crate::query::sanitize_fts_query("caching", crate::query::QueryMode::Natural);
        let results = db
            .search_lexical(&q, &SearchFilters::default(), 10)
            .unwrap();
        assert_eq!(results.len(), 2);
        // 'a' has title match (weight 10), should rank above 'b'.
        assert_eq!(results[0].id, "a");
    }

    #[test]
    fn test_search_semantic_returns_nearest_first() {
        let mut db = setup_db();
        // We craft three deterministic vectors and ensure the closest
        // one to the query vector comes first.
        let mut v_close = vec![0.0_f32; crate::embedding::EMBEDDING_DIM];
        v_close[0] = 1.0;
        let mut v_far = vec![0.0_f32; crate::embedding::EMBEDDING_DIM];
        v_far[1] = 1.0;

        let art_close = IndexedArtifact {
            id: "close".into(),
            kind: "rfc".into(),
            title: "close".into(),
            description: String::new(),
            tags: vec![],
            file_path: "close.yml".into(),
            indexed_at: Utc::now().to_rfc3339(),
        };
        let art_far = IndexedArtifact {
            id: "far".into(),
            kind: "rfc".into(),
            title: "far".into(),
            description: String::new(),
            tags: vec![],
            file_path: "far.yml".into(),
            indexed_at: Utc::now().to_rfc3339(),
        };
        db.upsert_artifact(&art_close, "c", &v_close, &[]).unwrap();
        db.upsert_artifact(&art_far, "f", &v_far, &[]).unwrap();

        let mut query = vec![0.0_f32; crate::embedding::EMBEDDING_DIM];
        query[0] = 1.0; // identical to v_close

        let results = db
            .search_semantic(&query, &SearchFilters::default(), 5)
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "close");
        assert_eq!(results[1].id, "far");
    }

    #[test]
    fn test_search_semantic_filter_kind() {
        let mut db = setup_db();
        let mut v = vec![0.0_f32; crate::embedding::EMBEDDING_DIM];
        v[0] = 1.0;
        let make = |id: &str, kind: &str| IndexedArtifact {
            id: id.to_string(),
            kind: kind.to_string(),
            title: id.to_string(),
            description: String::new(),
            tags: vec![],
            file_path: format!("{id}.yml"),
            indexed_at: Utc::now().to_rfc3339(),
        };
        db.upsert_artifact(&make("rfc1", "rfc"), "c", &v, &[])
            .unwrap();
        db.upsert_artifact(&make("dd1", "design-doc"), "c", &v, &[])
            .unwrap();
        let filters = SearchFilters {
            kinds: Some(vec!["rfc".into()]),
            ..Default::default()
        };
        let results = db.search_semantic(&v, &filters, 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "rfc1");
    }

    #[test]
    fn test_upsert_replaces_old_vector() {
        let mut db = setup_db();
        let art = make_indexed("v", "rfc", "T");
        db.upsert_artifact(&art, "c1", &dummy_embedding(1), &[])
            .unwrap();
        db.upsert_artifact(&art, "c2", &dummy_embedding(2), &[])
            .unwrap();
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM artifacts_vec WHERE artifact_id = ?1",
                params!["v"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "re-upsert must keep exactly one vector row");
    }
}
