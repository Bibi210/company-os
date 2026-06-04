//! SQL schema migration registry, driven by SQLite's `PRAGMA user_version`.
//!
//! Rationale (RFC 973a5569): `OrchestratorDb::migrate()` used to be a flat
//! sequence of imperative steps whose correctness depended on a hand-wired
//! ordering (the "no such column: author" hotfix 57019e7 proved this ordering
//! was fragile and could regress). This module replaces that with a discipline:
//!
//! * `SCHEMA_VERSION_TARGET` is the schema version the binary targets.
//! * `MIGRATIONS` is an ORDERED, contiguous registry of numbered migrations,
//!   each carrying its own DDL.
//! * `migrate()` (in `db.rs`) reads `PRAGMA user_version`, refuses a DB newer
//!   than it supports, then applies in order every migration whose version is
//!   strictly greater than the stored one, stamping `user_version` to the
//!   migration's number INSIDE the same transaction (atomic schema + version).
//!
//! Each migration body stays idempotent at the DDL level (`CREATE ... IF NOT
//! EXISTS`, `PRAGMA table_info` guards) so that a legacy DB sitting at
//! `user_version = 0` with a pre-populated schema can be boot-strapped without
//! crashing. The transaction boundaries (`BEGIN IMMEDIATE` / `COMMIT`) live in
//! the orchestration loop, NOT in the migration bodies.

use rusqlite::Connection;

use crate::db::{FTS_SQL, SCHEMA_SQL, VEC_TABLE_SQL};

/// Schema version this binary targets. MUST equal `MIGRATIONS.len()` — the
/// `test_migrations_registry_is_coherent` guard fails the build otherwise.
///
/// Bump this AND append a new `Migration` to `MIGRATIONS` whenever a schema
/// change is introduced. Forgetting to bump it means the new migration is
/// never applied (caught by the coherence test); forgetting to append a
/// migration means the loop targets a version it cannot reach (also caught).
pub const SCHEMA_VERSION_TARGET: i64 = 2;

/// A single numbered schema migration. `apply` performs the DDL for this
/// version; it must NOT open its own transaction (the loop in `migrate()`
/// owns the `BEGIN IMMEDIATE` / `COMMIT`) and must NOT stamp `user_version`
/// (the loop does that, in the same transaction, after `apply` succeeds).
pub(crate) struct Migration {
    pub(crate) version: i64,
    pub(crate) apply: fn(&Connection) -> rusqlite::Result<()>,
}

/// The ordered migration registry. Order = ascending version, contiguous from
/// 1 to `SCHEMA_VERSION_TARGET`. The loop in `migrate()` relies on this order.
pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        apply: migration_01_base_schema,
    },
    Migration {
        version: 2,
        apply: migration_02_filter_columns_and_indexes,
    },
];

/// Read the current schema version from `PRAGMA user_version` (defaults to 0
/// on a brand-new DB).
pub(crate) fn read_user_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
}

/// Write the schema version into `PRAGMA user_version`.
///
/// NOTE: `PRAGMA user_version` does NOT accept a bound parameter (`?`) — SQLite
/// rejects it syntactically. We therefore interpolate the integer with
/// `format!`. This is SAFE against injection because `v` is always a `version`
/// field taken from the constant `MIGRATIONS` registry (an `i64` literal in
/// this file), never external input.
pub(crate) fn write_user_version(conn: &Connection, v: i64) -> rusqlite::Result<()> {
    conn.execute_batch(&format!("PRAGMA user_version = {v}"))
}

/// Migration 1 — the base schema. Reproduces the historical "Step 1": the
/// base tables, the FTS5 virtual table, and the vec0 virtual table, all via
/// `CREATE ... IF NOT EXISTS` so it is idempotent and tolerant of a legacy DB
/// already carrying these objects (cf. RFC boot-strapping a `user_version = 0`
/// DB). No `BEGIN`/`COMMIT` here, no `user_version` stamp here — the loop owns
/// both.
fn migration_01_base_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA_SQL)?;
    conn.execute_batch(FTS_SQL)?;
    conn.execute_batch(VEC_TABLE_SQL)?;
    Ok(())
}

/// Returns true iff `table` already has a column named `col`, determined via
/// `PRAGMA table_info(<table>)`. Deterministic and locale-independent —
/// replaces the historical, fragile string-match on the "duplicate column"
/// SQLite error message.
fn column_exists(conn: &Connection, table: &str, col: &str) -> rusqlite::Result<bool> {
    // `table` here is always a hard-coded literal ("artifacts"), never user
    // input; interpolation is safe. PRAGMA table_info does not accept a bound
    // table name.
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        // table_info columns: cid, name, type, notnull, dflt_value, pk.
        let name: String = row.get(1)?;
        if name == col {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Migration 2 — the structured filter columns (`author`, `project`,
/// `created_at`) on `artifacts`, plus their indexes. This INTEGRATES the
/// former "Step 2" (ALTER) and the hotfix 57019e7 "Step 2bis" (the three
/// CREATE INDEX) into a SINGLE migration so the CREATE-INDEX-after-ALTER order
/// is a structural property, not an implicit cross-step contract.
///
/// Idempotence: each `ADD COLUMN` is guarded by `column_exists` (SQLite has no
/// `ADD COLUMN IF NOT EXISTS`), and each index is `CREATE INDEX IF NOT EXISTS`.
/// This lets the migration boot-strap a legacy `artifacts` table that predates
/// the columns without ever failing with "no such column: author".
fn migration_02_filter_columns_and_indexes(conn: &Connection) -> rusqlite::Result<()> {
    for (col, decl) in &[
        ("author", "ALTER TABLE artifacts ADD COLUMN author TEXT"),
        ("project", "ALTER TABLE artifacts ADD COLUMN project TEXT"),
        (
            "created_at",
            "ALTER TABLE artifacts ADD COLUMN created_at TEXT",
        ),
    ] {
        if !column_exists(conn, "artifacts", col)? {
            conn.execute(decl, [])?;
        }
    }

    // MUST come after the ALTERs above (same migration => same transaction =>
    // guaranteed order). Emitting these earlier would fail with "no such
    // column: author" on a legacy artifacts table — the exact class of bug
    // hotfix 57019e7 patched, now structurally impossible.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_artifacts_author ON artifacts(author);\
         CREATE INDEX IF NOT EXISTS idx_artifacts_project ON artifacts(project);\
         CREATE INDEX IF NOT EXISTS idx_artifacts_created_at ON artifacts(created_at);",
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // TEST 6 — registry/constant coherence. Guards against (a) forgetting to
    // bump SCHEMA_VERSION_TARGET when appending a migration, and (b) a
    // non-contiguous or non-increasing version sequence.
    #[test]
    fn test_migrations_registry_is_coherent() {
        assert_eq!(
            MIGRATIONS.len() as i64,
            SCHEMA_VERSION_TARGET,
            "SCHEMA_VERSION_TARGET ({SCHEMA_VERSION_TARGET}) must equal MIGRATIONS.len() ({})",
            MIGRATIONS.len()
        );
        // Versions must be exactly 1, 2, ..., N — strictly increasing and
        // contiguous starting at 1.
        for (idx, m) in MIGRATIONS.iter().enumerate() {
            let expected = idx as i64 + 1;
            assert_eq!(
                m.version, expected,
                "MIGRATIONS[{idx}].version = {} but expected {expected} (must be 1..=N contiguous)",
                m.version
            );
        }
    }
}
