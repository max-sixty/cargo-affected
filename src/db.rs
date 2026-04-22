//! SQLite storage for test-to-file coverage mappings.
//!
//! Schema:
//! - `meta` — key/value pairs (e.g. last collection timestamp)
//! - `test_files` — (binary_id, test_name, source_file, env_fingerprint)
//!   tuples with index on (source_file, env_fingerprint) for fast "which tests
//!   under the current env cover this file?" queries. `binary_id` is
//!   nextest's stable package-qualified identifier (e.g.
//!   `mock-stub::builds`) — without it two tests sharing a name across
//!   binaries collide and lose coverage silently.
//! - `fingerprints` — (fingerprint, last_seen) used for LRU garbage
//!   collection. Touched on every write and on every non-empty read; `gc()`
//!   evicts the oldest fingerprints once more than `FINGERPRINT_KEEP` are
//!   tracked, never evicting the caller's current fingerprint.
//!
//! `env_fingerprint` is a SHA-256 hex of inputs that would globally invalidate
//! cached coverage (Cargo.lock, Cargo.toml files, rustc version, RUSTFLAGS,
//! CARGO_BUILD_TARGET — see `fingerprint.rs`). Every query is scoped to the
//! caller's current fingerprint, so a mismatch naturally reads as "no data"
//! without any special-case invalidation path.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use rusqlite::Connection;

/// How long to wait for a conflicting lock before giving up. Long enough to
/// ride out a concurrent `collect`'s commit phase; short enough that a
/// genuinely stuck process surfaces as an error rather than hanging forever.
const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

/// Translate `SQLITE_BUSY` (which can only surface after `BUSY_TIMEOUT` has
/// already been exhausted) into a message that points at the actual cause.
/// Non-busy rusqlite errors pass through unchanged.
fn translate_busy(err: rusqlite::Error, ctx: &'static str) -> anyhow::Error {
    if matches!(
        &err,
        rusqlite::Error::SqliteFailure(e, _) if e.code == rusqlite::ErrorCode::DatabaseBusy
    ) {
        anyhow::anyhow!(
            "another cargo-affected process appears to be holding the \
             database lock — try again in a moment"
        )
    } else {
        anyhow::Error::from(err).context(ctx)
    }
}

/// Umbrella directory for all cargo-affected artifacts (DB, profraw files).
/// Lives under `target/` so it shares the gitignore and lifecycle
/// (cargo clean wipes it) of other build artifacts.
pub fn affected_dir(project_root: &Path) -> PathBuf {
    project_root.join("target").join("affected")
}

/// Canonical DB location within the affected dir.
pub fn db_path(project_root: &Path) -> PathBuf {
    affected_dir(project_root).join("coverage.db")
}

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS test_files (
    binary_id TEXT NOT NULL,
    test_name TEXT NOT NULL,
    source_file TEXT NOT NULL,
    env_fingerprint TEXT NOT NULL,
    PRIMARY KEY (binary_id, test_name, source_file, env_fingerprint)
);
CREATE INDEX IF NOT EXISTS idx_source_file_fp ON test_files(source_file, env_fingerprint);
CREATE TABLE IF NOT EXISTS fingerprints (
    fingerprint TEXT PRIMARY KEY,
    last_seen TEXT NOT NULL
);
";

/// Identifier for a single test: nextest's stable `binary_id`
/// (e.g. `mock-stub::builds`) paired with the test name inside that binary.
///
/// Before binary_id was tracked, two tests with the same name in different
/// binaries collapsed into one DB row and one test's coverage was silently
/// overwritten. The (binary_id, test_name) tuple is nextest's actual unit of
/// test identity, so we use it everywhere — storage keys, filter expressions,
/// counts.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TestId {
    pub binary_id: String,
    pub test_name: String,
}

impl TestId {
    pub fn new(binary_id: impl Into<String>, test_name: impl Into<String>) -> Self {
        Self {
            binary_id: binary_id.into(),
            test_name: test_name.into(),
        }
    }
}

/// How many distinct fingerprints to retain. `gc()` evicts the least-recently-
/// used fingerprints beyond this cap, always keeping the caller's current one.
/// Chosen to comfortably cover typical workflows (a handful of branches plus
/// the occasional toolchain bump) while keeping the DB from accumulating
/// forever if a user rapidly cycles through many build environments.
pub const FINGERPRINT_KEEP: usize = 10;

/// Upsert `fingerprint`'s `last_seen` to now. Creates the row if absent; this
/// is safe for writes (we just inserted data under this fingerprint) but
/// callers doing bare reads of a fingerprint that has no data should gate the
/// call on actually finding rows.
fn touch_fingerprint(conn: &Connection, fingerprint: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO fingerprints (fingerprint, last_seen) VALUES (?1, ?2) \
         ON CONFLICT(fingerprint) DO UPDATE SET last_seen = excluded.last_seen",
        rusqlite::params![fingerprint, chrono_free_timestamp()],
    )?;
    Ok(())
}

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open (or create) the database at `project_root/target/affected/coverage.db`.
    ///
    /// Migrates older schemas (pre-fingerprint, pre-binary_id) by dropping the
    /// old `test_files` table — old rows can't be retroactively tagged, and
    /// `target/affected/` is cargo-clean territory, so this is a safe reset.
    pub fn open(project_root: &Path) -> Result<Self> {
        let path = db_path(project_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("failed to open database at {}", path.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT)
            .context("failed to configure SQLite busy_timeout")?;
        migrate_legacy_test_files(&conn)?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| translate_busy(e, "failed to initialize database schema"))?;
        backfill_fingerprints(&conn)?;
        Ok(Self { conn })
    }

    /// Replace coverage data for the current fingerprint with a fresh collection.
    ///
    /// Leaves rows from other fingerprints alone — they remain queryable if the
    /// user switches environments (branch with different Cargo.lock, etc.).
    pub fn store_coverage(
        &mut self,
        fingerprint: &str,
        mappings: &[(TestId, BTreeSet<Utf8PathBuf>)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM test_files WHERE env_fingerprint = ?1",
            [fingerprint],
        )?;

        {
            let mut stmt = tx.prepare(
                "INSERT INTO test_files (binary_id, test_name, source_file, env_fingerprint) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (test_id, files) in mappings {
                for file in files {
                    stmt.execute(rusqlite::params![
                        test_id.binary_id,
                        test_id.test_name,
                        file.as_str(),
                        fingerprint,
                    ])?;
                }
            }
        }

        touch_fingerprint(&tx, fingerprint)?;
        write_last_collected(&tx)?;
        tx.commit()
            .map_err(|e| translate_busy(e, "failed to commit coverage data"))?;
        Ok(())
    }

    /// Find all tests under the current fingerprint covering any of the given
    /// source files.
    ///
    /// Bumps the fingerprint's `last_seen` when the result is non-empty — a
    /// successful read counts as "the user is actively using this cache" for
    /// GC purposes. Empty reads (no matching rows, or a fingerprint that has
    /// never been collected) don't create a spurious tracking entry.
    pub fn tests_covering(&self, fingerprint: &str, files: &[&str]) -> Result<BTreeSet<TestId>> {
        if files.is_empty() {
            return Ok(BTreeSet::new());
        }

        let placeholders: Vec<&str> = files.iter().map(|_| "?").collect();
        let sql = format!(
            "SELECT DISTINCT binary_id, test_name FROM test_files \
             WHERE env_fingerprint = ? AND source_file IN ({})",
            placeholders.join(", ")
        );

        let mut params: Vec<&dyn rusqlite::types::ToSql> = Vec::with_capacity(1 + files.len());
        params.push(&fingerprint);
        for f in files {
            params.push(f);
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok(TestId::new(row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut tests = BTreeSet::new();
        for row in rows {
            tests.insert(row?);
        }
        if !tests.is_empty() {
            touch_fingerprint(&self.conn, fingerprint)?;
        }
        Ok(tests)
    }

    /// Count of distinct tests tracked under the current fingerprint.
    pub fn test_count(&self, fingerprint: &str) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM \
             (SELECT DISTINCT binary_id, test_name FROM test_files WHERE env_fingerprint = ?1)",
            [fingerprint],
            |r| r.get(0),
        )?;
        Ok(count as usize)
    }

    /// Count of (test, file) mappings under the current fingerprint.
    pub fn mapping_count(&self, fingerprint: &str) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM test_files WHERE env_fingerprint = ?1",
            [fingerprint],
            |r| r.get(0),
        )?;
        Ok(count as usize)
    }

    /// Whether a source file has coverage data under the current fingerprint.
    pub fn file_tracked(&self, fingerprint: &str, file: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM test_files \
             WHERE env_fingerprint = ?1 AND source_file = ?2",
            [fingerprint, file],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    /// All distinct (binary_id, test_name) pairs under the current fingerprint.
    pub fn all_tests(&self, fingerprint: &str) -> Result<BTreeSet<TestId>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT binary_id, test_name FROM test_files WHERE env_fingerprint = ?1",
        )?;
        let rows = stmt.query_map([fingerprint], |row| {
            Ok(TestId::new(row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut tests = BTreeSet::new();
        for row in rows {
            tests.insert(row?);
        }
        Ok(tests)
    }

    /// Whether the DB holds any coverage data at all (any fingerprint).
    ///
    /// Used to distinguish "never collected" from "collected under a different
    /// environment", which deserve different messages and different run-time
    /// behavior.
    pub fn has_any_coverage(&self) -> Result<bool> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM test_files", [], |r| r.get(0))?;
        Ok(count > 0)
    }

    /// Update coverage for specific tests under the current fingerprint,
    /// leaving other tests (and other fingerprints) untouched.
    pub fn update_coverage(
        &mut self,
        fingerprint: &str,
        mappings: &[(TestId, BTreeSet<Utf8PathBuf>)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;

        {
            let mut delete_stmt = tx.prepare(
                "DELETE FROM test_files \
                 WHERE binary_id = ?1 AND test_name = ?2 AND env_fingerprint = ?3",
            )?;
            let mut insert_stmt = tx.prepare(
                "INSERT INTO test_files (binary_id, test_name, source_file, env_fingerprint) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?;

            for (test_id, files) in mappings {
                delete_stmt.execute(rusqlite::params![
                    test_id.binary_id,
                    test_id.test_name,
                    fingerprint,
                ])?;
                for file in files {
                    insert_stmt.execute(rusqlite::params![
                        test_id.binary_id,
                        test_id.test_name,
                        file.as_str(),
                        fingerprint,
                    ])?;
                }
            }
        }

        touch_fingerprint(&tx, fingerprint)?;
        write_last_collected(&tx)?;
        tx.commit()
            .map_err(|e| translate_busy(e, "failed to commit coverage update"))?;
        Ok(())
    }

    /// Remove all coverage data (every fingerprint) and reset the `meta` table.
    ///
    /// Used by `cargo affected clean`. Going through SQL (rather than unlinking
    /// the file) means we acquire the normal write lock — so a concurrent
    /// `collect` finishes cleanly before its data is discarded, instead of
    /// being orphaned onto an unlinked inode.
    pub fn clear(&mut self) -> Result<()> {
        let tx = self
            .conn
            .transaction()
            .map_err(|e| translate_busy(e, "failed to start clear transaction"))?;
        tx.execute("DELETE FROM test_files", [])?;
        tx.execute("DELETE FROM fingerprints", [])?;
        tx.execute("DELETE FROM meta", [])?;
        tx.commit()
            .map_err(|e| translate_busy(e, "failed to commit clear"))?;
        Ok(())
    }

    /// Evict the least-recently-used fingerprints beyond `keep`, never
    /// evicting `current`. Returns the number evicted.
    ///
    /// Of all fingerprints other than `current`, the `keep - 1` most recent
    /// are retained so the total stays at most `keep`. Data and tracking rows
    /// for evicted fingerprints are removed in a single transaction.
    pub fn gc(&mut self, current: &str, keep: usize) -> Result<usize> {
        let tx = self
            .conn
            .transaction()
            .map_err(|e| translate_busy(e, "failed to start gc transaction"))?;

        let to_evict: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT fingerprint FROM fingerprints \
                 WHERE fingerprint != ?1 \
                 ORDER BY last_seen DESC, fingerprint ASC \
                 LIMIT -1 OFFSET ?2",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![current, keep.saturating_sub(1) as i64],
                |r| r.get::<_, String>(0),
            )?;
            rows.collect::<rusqlite::Result<Vec<String>>>()?
        };

        for fp in &to_evict {
            tx.execute("DELETE FROM test_files WHERE env_fingerprint = ?1", [fp])?;
            tx.execute("DELETE FROM fingerprints WHERE fingerprint = ?1", [fp])?;
        }

        tx.commit()
            .map_err(|e| translate_busy(e, "failed to commit gc"))?;
        Ok(to_evict.len())
    }

    /// Count of distinct tracked fingerprints (after any GC).
    pub fn fingerprint_count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM fingerprints", [], |r| r.get(0))?;
        Ok(count as usize)
    }

    /// Return the last collection timestamp, if any.
    pub fn last_collected(&self) -> Result<Option<String>> {
        let result = self.conn.query_row(
            "SELECT value FROM meta WHERE key = 'last_collected'",
            [],
            |r| r.get::<_, String>(0),
        );
        match result {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// Warn (to stderr) about changed `.rs` files that have no coverage data under
/// the current fingerprint.
pub fn warn_untracked_rs_files(
    db: &Db,
    fingerprint: &str,
    changed_files: &[String],
) -> Result<()> {
    for file in changed_files {
        if file.ends_with(".rs") && !db.file_tracked(fingerprint, file)? {
            eprintln!(
                "warning: {file} has no coverage data \
                 — run `cargo affected collect` to include it"
            );
        }
    }
    Ok(())
}

/// Drop `test_files` if it predates any column the current schema requires
/// (`env_fingerprint`, `binary_id`). Old rows can't be retroactively tagged
/// with missing columns, and `target/affected/` is cargo-clean territory, so
/// resetting is safe — the user re-collects.
fn migrate_legacy_test_files(conn: &Connection) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='test_files'",
        [],
        |r| r.get::<_, i64>(0).map(|n| n > 0),
    )?;
    if !exists {
        return Ok(());
    }
    let mut stmt = conn.prepare("PRAGMA table_info(test_files)")?;
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<_, _>>()?;
    let has_fingerprint = columns.iter().any(|c| c == "env_fingerprint");
    let has_binary_id = columns.iter().any(|c| c == "binary_id");
    if !has_fingerprint || !has_binary_id {
        drop(stmt);
        conn.execute("DROP TABLE test_files", [])?;
        conn.execute("DROP INDEX IF EXISTS idx_source_file", [])?;
        conn.execute("DROP INDEX IF EXISTS idx_source_file_fp", [])?;
    }
    Ok(())
}

/// Seed `fingerprints` with an entry for every distinct fingerprint in
/// `test_files` that doesn't already have one. Runs on every open so DBs
/// written by pre-GC code (which have `test_files` rows but no tracking
/// entries) get backfilled at `last_seen = now`, giving them full grace until
/// the next collect before they become eviction candidates.
fn backfill_fingerprints(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO fingerprints (fingerprint, last_seen) \
         SELECT DISTINCT env_fingerprint, ?1 FROM test_files",
        [&chrono_free_timestamp()],
    )?;
    Ok(())
}

fn write_last_collected(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let timestamp = chrono_free_timestamp();
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('last_collected', ?1)",
        [&timestamp],
    )?;
    Ok(())
}

/// ISO-8601 UTC timestamp without external dependencies.
fn chrono_free_timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    let (year, month, day) = days_to_civil(secs / 86400);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Convert days since Unix epoch to (year, month, day).
///
/// Algorithm from <http://howardhinnant.github.io/date_algorithms.html>.
fn days_to_civil(days: u64) -> (u64, u64, u64) {
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const FP_A: &str = "aaaaaaaa";
    const FP_B: &str = "bbbbbbbb";
    const BIN_A: &str = "crate_a";
    const BIN_B: &str = "crate_b";

    fn tid(binary_id: &str, test_name: &str) -> TestId {
        TestId::new(binary_id, test_name)
    }

    #[test]
    fn test_roundtrip() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let mut files_a = BTreeSet::new();
        files_a.insert(Utf8PathBuf::from("src/lib.rs"));
        files_a.insert(Utf8PathBuf::from("src/utils.rs"));

        let mut files_b = BTreeSet::new();
        files_b.insert(Utf8PathBuf::from("src/lib.rs"));

        let mappings = vec![
            (tid(BIN_A, "test_a"), files_a),
            (tid(BIN_A, "test_b"), files_b),
        ];

        db.store_coverage(FP_A, &mappings)?;

        assert_eq!(db.test_count(FP_A)?, 2);
        assert_eq!(db.mapping_count(FP_A)?, 3);

        let covering_lib = db.tests_covering(FP_A, &["src/lib.rs"])?;
        assert_eq!(covering_lib.len(), 2);
        assert!(covering_lib.contains(&tid(BIN_A, "test_a")));
        assert!(covering_lib.contains(&tid(BIN_A, "test_b")));

        let covering_utils = db.tests_covering(FP_A, &["src/utils.rs"])?;
        assert_eq!(covering_utils.len(), 1);
        assert!(covering_utils.contains(&tid(BIN_A, "test_a")));

        let covering_none = db.tests_covering(FP_A, &["src/nonexistent.rs"])?;
        assert!(covering_none.is_empty());

        assert!(db.last_collected()?.is_some());

        Ok(())
    }

    /// Two tests with the same name in different binaries must round-trip
    /// independently. Regression guard: before binary_id tracking, the second
    /// test's coverage silently overwrote the first.
    #[test]
    fn same_test_name_in_different_binaries() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let mappings = vec![
            (
                tid(BIN_A, "builds"),
                BTreeSet::from([Utf8PathBuf::from("crate_a/tests/builds.rs")]),
            ),
            (
                tid(BIN_B, "builds"),
                BTreeSet::from([Utf8PathBuf::from("crate_b/tests/builds.rs")]),
            ),
        ];
        db.store_coverage(FP_A, &mappings)?;

        assert_eq!(db.test_count(FP_A)?, 2);
        assert_eq!(db.mapping_count(FP_A)?, 2);

        // Each binary's `builds` is selected by its own source file.
        let a = db.tests_covering(FP_A, &["crate_a/tests/builds.rs"])?;
        assert_eq!(a, BTreeSet::from([tid(BIN_A, "builds")]));
        let b = db.tests_covering(FP_A, &["crate_b/tests/builds.rs"])?;
        assert_eq!(b, BTreeSet::from([tid(BIN_B, "builds")]));

        // update_coverage is scoped by (binary_id, test_name), not just name.
        let update = vec![(
            tid(BIN_A, "builds"),
            BTreeSet::from([Utf8PathBuf::from("crate_a/src/lib.rs")]),
        )];
        db.update_coverage(FP_A, &update)?;

        // BIN_A's mapping moved, BIN_B's survived intact.
        assert!(db
            .tests_covering(FP_A, &["crate_a/tests/builds.rs"])?
            .is_empty());
        assert_eq!(
            db.tests_covering(FP_A, &["crate_b/tests/builds.rs"])?,
            BTreeSet::from([tid(BIN_B, "builds")])
        );
        assert_eq!(
            db.tests_covering(FP_A, &["crate_a/src/lib.rs"])?,
            BTreeSet::from([tid(BIN_A, "builds")])
        );

        Ok(())
    }

    #[test]
    fn different_fingerprint_reads_empty() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let mappings = vec![(
            tid(BIN_A, "test_a"),
            BTreeSet::from([Utf8PathBuf::from("src/lib.rs")]),
        )];
        db.store_coverage(FP_A, &mappings)?;

        // Querying under a different fingerprint sees no rows.
        assert_eq!(db.test_count(FP_B)?, 0);
        assert_eq!(db.mapping_count(FP_B)?, 0);
        assert!(db.tests_covering(FP_B, &["src/lib.rs"])?.is_empty());
        assert!(!db.file_tracked(FP_B, "src/lib.rs")?);
        assert!(db.all_tests(FP_B)?.is_empty());

        // But the original fingerprint still sees its rows.
        assert_eq!(db.test_count(FP_A)?, 1);

        // And has_any_coverage sees rows across fingerprints.
        assert!(db.has_any_coverage()?);

        Ok(())
    }

    #[test]
    fn full_collect_preserves_other_fingerprints() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let a = vec![(
            tid(BIN_A, "test_a"),
            BTreeSet::from([Utf8PathBuf::from("src/lib.rs")]),
        )];
        let b = vec![(
            tid(BIN_A, "test_b"),
            BTreeSet::from([Utf8PathBuf::from("src/other.rs")]),
        )];
        db.store_coverage(FP_A, &a)?;
        db.store_coverage(FP_B, &b)?;

        // Rewriting FP_A's data leaves FP_B untouched.
        let a2 = vec![(
            tid(BIN_A, "test_a"),
            BTreeSet::from([Utf8PathBuf::from("src/new.rs")]),
        )];
        db.store_coverage(FP_A, &a2)?;

        assert_eq!(db.test_count(FP_A)?, 1);
        assert_eq!(db.test_count(FP_B)?, 1);
        assert!(db
            .tests_covering(FP_B, &["src/other.rs"])?
            .contains(&tid(BIN_A, "test_b")));
        assert!(db
            .tests_covering(FP_A, &["src/new.rs"])?
            .contains(&tid(BIN_A, "test_a")));
        assert!(db.tests_covering(FP_A, &["src/lib.rs"])?.is_empty());

        Ok(())
    }

    #[test]
    fn test_update_coverage() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let mappings = vec![
            (
                tid(BIN_A, "test_a"),
                BTreeSet::from([
                    Utf8PathBuf::from("src/lib.rs"),
                    Utf8PathBuf::from("src/utils.rs"),
                ]),
            ),
            (
                tid(BIN_A, "test_b"),
                BTreeSet::from([Utf8PathBuf::from("src/lib.rs")]),
            ),
        ];
        db.store_coverage(FP_A, &mappings)?;
        assert_eq!(db.test_count(FP_A)?, 2);
        assert_eq!(db.mapping_count(FP_A)?, 3);

        let update = vec![(
            tid(BIN_A, "test_a"),
            BTreeSet::from([
                Utf8PathBuf::from("src/lib.rs"),
                Utf8PathBuf::from("src/new.rs"),
            ]),
        )];
        db.update_coverage(FP_A, &update)?;

        assert_eq!(db.test_count(FP_A)?, 2);
        assert_eq!(db.mapping_count(FP_A)?, 3);

        assert!(db
            .tests_covering(FP_A, &["src/new.rs"])?
            .contains(&tid(BIN_A, "test_a")));
        assert!(db.tests_covering(FP_A, &["src/utils.rs"])?.is_empty());

        let covering_lib = db.tests_covering(FP_A, &["src/lib.rs"])?;
        assert!(covering_lib.contains(&tid(BIN_A, "test_a")));
        assert!(covering_lib.contains(&tid(BIN_A, "test_b")));

        Ok(())
    }

    #[test]
    fn test_file_tracked() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let mappings = vec![(
            tid(BIN_A, "test_a"),
            BTreeSet::from([Utf8PathBuf::from("src/lib.rs")]),
        )];
        db.store_coverage(FP_A, &mappings)?;

        assert!(db.file_tracked(FP_A, "src/lib.rs")?);
        assert!(!db.file_tracked(FP_A, "src/nonexistent.rs")?);

        Ok(())
    }

    #[test]
    fn test_all_tests() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let mappings = vec![
            (
                tid(BIN_A, "test_a"),
                BTreeSet::from([Utf8PathBuf::from("src/lib.rs")]),
            ),
            (
                tid(BIN_B, "test_a"),
                BTreeSet::from([Utf8PathBuf::from("src/lib.rs")]),
            ),
        ];
        db.store_coverage(FP_A, &mappings)?;

        let names = db.all_tests(FP_A)?;
        assert_eq!(
            names,
            BTreeSet::from([tid(BIN_A, "test_a"), tid(BIN_B, "test_a")])
        );

        Ok(())
    }

    #[test]
    fn clear_wipes_all_fingerprints() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let mappings = vec![(
            tid(BIN_A, "test_a"),
            BTreeSet::from([Utf8PathBuf::from("src/lib.rs")]),
        )];
        db.store_coverage(FP_A, &mappings)?;
        db.store_coverage(FP_B, &mappings)?;
        assert!(db.has_any_coverage()?);
        assert!(db.last_collected()?.is_some());

        db.clear()?;

        assert!(!db.has_any_coverage()?);
        assert_eq!(db.test_count(FP_A)?, 0);
        assert_eq!(db.test_count(FP_B)?, 0);
        assert!(db.last_collected()?.is_none());

        // DB still usable afterwards.
        db.store_coverage(FP_A, &mappings)?;
        assert_eq!(db.test_count(FP_A)?, 1);

        Ok(())
    }

    #[test]
    fn store_tracks_fingerprint_last_seen() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let mappings = vec![(
            tid(BIN_A, "test_a"),
            BTreeSet::from([Utf8PathBuf::from("src/lib.rs")]),
        )];
        db.store_coverage(FP_A, &mappings)?;
        assert_eq!(db.fingerprint_count()?, 1);

        db.store_coverage(FP_B, &mappings)?;
        assert_eq!(db.fingerprint_count()?, 2);

        Ok(())
    }

    #[test]
    fn empty_read_does_not_create_tracking_entry() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db = Db::open(dir.path())?;

        // No data under FP_A; reading shouldn't register it as a fingerprint.
        let hits = db.tests_covering(FP_A, &["src/lib.rs"])?;
        assert!(hits.is_empty());
        assert_eq!(db.fingerprint_count()?, 0);

        Ok(())
    }

    #[test]
    fn gc_keeps_current_and_most_recent_others() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let mappings = vec![(
            tid(BIN_A, "t"),
            BTreeSet::from([Utf8PathBuf::from("src/lib.rs")]),
        )];

        // Four fingerprints, ordered by last_seen via explicit update.
        for fp in ["fp1", "fp2", "fp3", "fp4"] {
            db.store_coverage(fp, &mappings)?;
            // Space out last_seen so ordering is unambiguous. We have
            // second-granularity timestamps; bump directly.
            db.conn.execute(
                "UPDATE fingerprints SET last_seen = ?2 WHERE fingerprint = ?1",
                rusqlite::params![fp, format!("2020-01-01T00:00:{:02}Z", fp.as_bytes()[2] - b'0')],
            )?;
        }
        assert_eq!(db.fingerprint_count()?, 4);

        // Keep 2 with fp4 current: fp4 stays + the one most-recent other = fp3.
        // fp1 and fp2 are evicted (oldest last_seen).
        let evicted = db.gc("fp4", 2)?;
        assert_eq!(evicted, 2);
        assert_eq!(db.fingerprint_count()?, 2);
        assert_eq!(db.test_count("fp1")?, 0);
        assert_eq!(db.test_count("fp2")?, 0);
        assert_eq!(db.test_count("fp3")?, 1);
        assert_eq!(db.test_count("fp4")?, 1);

        Ok(())
    }

    #[test]
    fn gc_keeps_current_even_when_oldest() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let mappings = vec![(
            tid(BIN_A, "t"),
            BTreeSet::from([Utf8PathBuf::from("src/lib.rs")]),
        )];

        for (fp, ts) in [
            ("old_current", "2020-01-01T00:00:00Z"),
            ("mid", "2020-01-02T00:00:00Z"),
            ("new", "2020-01-03T00:00:00Z"),
        ] {
            db.store_coverage(fp, &mappings)?;
            db.conn.execute(
                "UPDATE fingerprints SET last_seen = ?2 WHERE fingerprint = ?1",
                rusqlite::params![fp, ts],
            )?;
        }

        // Keep 2 with old_current as current — current always survives, and
        // the single most-recent other (new) stays. mid gets evicted.
        let evicted = db.gc("old_current", 2)?;
        assert_eq!(evicted, 1);
        assert_eq!(db.test_count("old_current")?, 1);
        assert_eq!(db.test_count("new")?, 1);
        assert_eq!(db.test_count("mid")?, 0);

        Ok(())
    }

    #[test]
    fn gc_noop_when_under_keep() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let mappings = vec![(
            tid(BIN_A, "t"),
            BTreeSet::from([Utf8PathBuf::from("src/lib.rs")]),
        )];
        db.store_coverage(FP_A, &mappings)?;
        db.store_coverage(FP_B, &mappings)?;

        let evicted = db.gc(FP_A, 10)?;
        assert_eq!(evicted, 0);
        assert_eq!(db.fingerprint_count()?, 2);

        Ok(())
    }

    #[test]
    fn gc_noop_on_empty_db() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;
        assert_eq!(db.gc("any_fp", 10)?, 0);
        Ok(())
    }

    /// A DB written by code that had the current-schema test_files (with
    /// binary_id and env_fingerprint) but no `fingerprints` tracking table
    /// yet should backfill fingerprint tracking from the existing rows on
    /// reopen, so GC has something to reason about from the first open.
    #[test]
    fn reopen_backfills_fingerprints_for_pre_gc_db() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = db_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap())?;

        {
            let conn = Connection::open(&path)?;
            conn.execute_batch(
                "\
                CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                CREATE TABLE test_files (
                    binary_id TEXT NOT NULL,
                    test_name TEXT NOT NULL,
                    source_file TEXT NOT NULL,
                    env_fingerprint TEXT NOT NULL,
                    PRIMARY KEY (binary_id, test_name, source_file, env_fingerprint)
                );
                INSERT INTO test_files VALUES ('bin1', 't1', 'src/lib.rs', 'fp_old_a');
                INSERT INTO test_files VALUES ('bin1', 't2', 'src/lib.rs', 'fp_old_b');
                ",
            )?;
        }

        let db = Db::open(dir.path())?;
        assert_eq!(db.fingerprint_count()?, 2);

        Ok(())
    }

    /// A pre-binary_id DB (fingerprinted but no binary_id column) must be
    /// dropped on open. The user re-collects — the rows can't be retroactively
    /// tagged with a binary_id.
    #[test]
    fn pre_binary_id_schema_is_dropped() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = db_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap())?;

        {
            let conn = Connection::open(&path)?;
            conn.execute_batch(
                "\
                CREATE TABLE test_files (
                    test_name TEXT NOT NULL,
                    source_file TEXT NOT NULL,
                    env_fingerprint TEXT NOT NULL,
                    PRIMARY KEY (test_name, source_file, env_fingerprint)
                );
                INSERT INTO test_files VALUES ('t1', 'src/lib.rs', 'fp_x');
                ",
            )?;
        }

        let db = Db::open(dir.path())?;
        assert!(!db.has_any_coverage()?);

        Ok(())
    }

    #[test]
    fn clear_also_wipes_fingerprints_table() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let mappings = vec![(
            tid(BIN_A, "t"),
            BTreeSet::from([Utf8PathBuf::from("src/lib.rs")]),
        )];
        db.store_coverage(FP_A, &mappings)?;
        db.store_coverage(FP_B, &mappings)?;
        assert_eq!(db.fingerprint_count()?, 2);

        db.clear()?;
        assert_eq!(db.fingerprint_count()?, 0);

        Ok(())
    }

    #[test]
    fn pre_fingerprint_schema_is_dropped() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = db_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap())?;

        // Simulate the old schema.
        {
            let conn = Connection::open(&path)?;
            conn.execute_batch(
                "\
                CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                CREATE TABLE test_files (
                    test_name TEXT NOT NULL,
                    source_file TEXT NOT NULL,
                    PRIMARY KEY (test_name, source_file)
                );
                CREATE INDEX idx_source_file ON test_files(source_file);
                INSERT INTO test_files VALUES ('old_test', 'src/lib.rs');
                ",
            )?;
        }

        // Open with the new code: old table is dropped, schema is current.
        let db = Db::open(dir.path())?;
        assert!(!db.has_any_coverage()?);
        assert_eq!(db.test_count(FP_A)?, 0);

        Ok(())
    }
}
