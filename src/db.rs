//! SQLite storage for test-to-function-range coverage mappings.
//!
//! Schema:
//! - `meta` — key/value pairs (e.g. last collection timestamp)
//! - `test_regions` — `(binary_id, test_name, source_file, line_start,
//!   line_end, env_fingerprint, collect_sha)` rows. One row per hit function
//!   per test, per covered file. `collect_sha` is the git HEAD at the time
//!   the row's coverage was observed; queries diff the working tree against
//!   that sha so stored line numbers line up with diff hunks. Crate roots
//!   (`lib.rs` / `main.rs` / `tests/*.rs`) are stored with sentinel range
//!   `(1, i64::MAX)` so any hunk in a crate root overlaps and selects the
//!   test — same effect as the old per-test implicit dep, no special-case
//!   branch.
//! - `fingerprints` — `(fingerprint, last_seen)`. Last-seen drives LRU
//!   eviction up to `FINGERPRINT_KEEP`.
//!
//! `collect_sha` lives per-row (rather than per-fingerprint) so `collect
//! --diff` can leave unaffected tests anchored at their original sha while
//! re-anchoring the rerun ones at the new HEAD. Multiple shas can coexist for
//! the same fingerprint; query callers iterate over distinct shas to compute
//! ranges anchored at each.
//!
//! `env_fingerprint` (see `fingerprint.rs`) is a SHA-256 hex of inputs that
//! would globally invalidate cached coverage (Cargo.lock, Cargo.toml files,
//! rustc version, RUSTFLAGS, CARGO_BUILD_TARGET). Every query is scoped to the
//! caller's current fingerprint, so a mismatch naturally reads as "no data"
//! without any special-case invalidation path.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::coverage::HitRange;
use crate::fingerprint::FingerprintComponent;
use crate::project::LineRange;

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
CREATE TABLE IF NOT EXISTS test_regions (
    binary_id TEXT NOT NULL,
    test_name TEXT NOT NULL,
    source_file TEXT NOT NULL,
    line_start INTEGER NOT NULL,
    line_end INTEGER NOT NULL,
    env_fingerprint TEXT NOT NULL,
    collect_sha TEXT NOT NULL,
    PRIMARY KEY (binary_id, test_name, source_file, line_start, line_end, env_fingerprint, collect_sha)
);
-- Range-overlap query: equality on (source_file, env_fingerprint, collect_sha)
-- plus a bound on line_start. The fifth column (line_end) lets the planner
-- skip rows once it has line_start beyond the hunk end. SQLite's composite
-- index can use equality + one range bound efficiently; the second range
-- bound is best-effort. Benchmark on real workspaces if status/run latency
-- matters.
CREATE INDEX IF NOT EXISTS idx_test_regions_lookup
    ON test_regions(source_file, env_fingerprint, collect_sha, line_start, line_end);
CREATE TABLE IF NOT EXISTS fingerprints (
    fingerprint TEXT PRIMARY KEY,
    last_seen TEXT NOT NULL
);
-- Per-fingerprint per-input component hashes. Lets a 'fingerprint
-- mismatch' miss be diagnosed at the component level: 'this PR's
-- manifest:tests/helpers/wt-perf/Cargo.toml differs from every cached
-- fingerprint, but Cargo.lock and rustc match'. Composite hash in
-- env_fingerprint already encodes the same information; this is the
-- diagnostic decomposition.
CREATE TABLE IF NOT EXISTS fingerprint_components (
    env_fingerprint TEXT NOT NULL,
    label TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    PRIMARY KEY (env_fingerprint, label)
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
///
/// Takes `&Connection`; pass `&tx` from inside a transaction (rusqlite's
/// `Transaction<'_>` derefs to `Connection`, so the write joins the open
/// transaction rather than auto-committing).
fn touch_fingerprint(conn: &Connection, fingerprint: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO fingerprints (fingerprint, last_seen) VALUES (?1, ?2) \
         ON CONFLICT(fingerprint) DO UPDATE SET last_seen = excluded.last_seen",
        rusqlite::params![fingerprint, chrono_free_timestamp()],
    )?;
    Ok(())
}

/// Build `?N, ?N+1, ...` for an `IN (...)` clause with `count` placeholders
/// starting at `start_idx`. SQLite has no parameter list for `IN`, so we
/// have to expand explicitly. Callers pair this with `params_from_iter` so
/// the values are still bound positionally — no string interpolation of
/// untrusted data.
fn in_placeholders(start_idx: usize, count: usize) -> String {
    (0..count)
        .map(|i| format!("?{}", i + start_idx))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Delete every `test_regions` row matching one of `tests`, scoped to
/// `fingerprint`. One DELETE per chunk instead of one per test, so a `--diff`
/// touching many tests doesn't hammer SQLite with N round-trips. Chunked so
/// the parameter count stays well under SQLite's default
/// `SQLITE_MAX_VARIABLE_NUMBER` (999 on older builds, 32766 on modern ones).
fn batch_delete_tests<'a>(
    tx: &rusqlite::Transaction<'_>,
    fingerprint: &str,
    tests: impl IntoIterator<Item = &'a TestId>,
) -> Result<()> {
    let tests: Vec<&TestId> = tests.into_iter().collect();
    if tests.is_empty() {
        return Ok(());
    }
    // 400 pairs → 801 bound parameters, comfortably below the conservative
    // 999 limit. SQLite's row-tuple `IN ((?, ?), …)` syntax isn't universally
    // supported across versions, so we expand into an OR predicate, which the
    // planner reduces to the same lookup.
    const CHUNK: usize = 400;
    for chunk in tests.chunks(CHUNK) {
        let predicate = (0..chunk.len())
            .map(|i| format!("(binary_id = ?{} AND test_name = ?{})", 2 + i * 2, 3 + i * 2))
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            "DELETE FROM test_regions \
             WHERE env_fingerprint = ?1 AND ({predicate})"
        );
        let params = std::iter::once(fingerprint).chain(
            chunk
                .iter()
                .flat_map(|t| [t.binary_id.as_str(), t.test_name.as_str()]),
        );
        tx.execute(&sql, rusqlite::params_from_iter(params))?;
    }
    Ok(())
}

/// Replace every `fingerprint_components` row for `fingerprint` with the
/// supplied `components`. Idempotent: a re-run with the same inputs leaves
/// the table in the same state. Called from both `store_coverage` and
/// `update_coverage_for_tests` so the components table stays in sync with
/// `test_regions` writes within the same transaction.
fn upsert_components(
    tx: &rusqlite::Transaction<'_>,
    fingerprint: &str,
    components: &[FingerprintComponent],
) -> Result<()> {
    tx.execute(
        "DELETE FROM fingerprint_components WHERE env_fingerprint = ?1",
        [fingerprint],
    )?;
    let mut stmt = tx.prepare(
        "INSERT INTO fingerprint_components (env_fingerprint, label, content_hash) \
         VALUES (?1, ?2, ?3)",
    )?;
    for component in components {
        stmt.execute(rusqlite::params![
            fingerprint,
            component.label.as_str(),
            component.hash.as_str(),
        ])?;
    }
    Ok(())
}

/// Insert every `(test_id, range)` pair as a `test_regions` row anchored at
/// `collect_sha`. Shared between [`Db::store_coverage`] and
/// [`Db::update_coverage_for_tests`] — both insert the same shape.
fn insert_mappings(
    tx: &rusqlite::Transaction<'_>,
    fingerprint: &str,
    collect_sha: &str,
    mappings: &[(TestId, BTreeSet<HitRange>)],
) -> Result<()> {
    let mut stmt = tx.prepare(
        "INSERT OR IGNORE INTO test_regions \
         (binary_id, test_name, source_file, line_start, line_end, env_fingerprint, collect_sha) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for (test_id, ranges) in mappings {
        for range in ranges {
            stmt.execute(rusqlite::params![
                test_id.binary_id,
                test_id.test_name,
                range.file.as_str(),
                range.line_start,
                range.line_end,
                fingerprint,
                collect_sha,
            ])?;
        }
    }
    Ok(())
}

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open (or create) the database at `project_root/target/affected/coverage.db`.
    ///
    /// Migrates older schemas (pre-fingerprint, pre-binary_id, pre-range,
    /// pre-per-row-collect_sha) by dropping the old `test_files` /
    /// `test_regions` tables and rebuilding `fingerprints` if its column
    /// shape is wrong. Old rows can't be retroactively tagged with missing
    /// columns, and `target/affected/` is cargo-clean territory, so the
    /// reset is safe — the user re-collects.
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
        // WAL halves insert latency on the bulk store_coverage path, which is
        // ~5–10× larger row counts under function-level granularity. NORMAL
        // sync is durable across crashes (only at risk of losing the very
        // last commit on power-loss) — fine for a coverage cache.
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("failed to set journal_mode=WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("failed to set synchronous=NORMAL")?;
        migrate_legacy_tables(&conn)?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| translate_busy(e, "failed to initialize database schema"))?;
        Ok(Self { conn })
    }

    /// Replace coverage data for the current fingerprint with a fresh
    /// collection, anchoring every new row at `collect_sha` (the git HEAD
    /// when this collection ran). `components` are the per-input hashes
    /// that make up `fingerprint`; stored in `fingerprint_components` for
    /// later "which input changed?" diagnostics.
    ///
    /// Leaves rows from other fingerprints alone — they remain queryable if
    /// the user switches environments.
    pub fn store_coverage(
        &mut self,
        fingerprint: &str,
        components: &[FingerprintComponent],
        collect_sha: &str,
        mappings: &[(TestId, BTreeSet<HitRange>)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM test_regions WHERE env_fingerprint = ?1",
            [fingerprint],
        )?;
        insert_mappings(&tx, fingerprint, collect_sha, mappings)?;
        upsert_components(&tx, fingerprint, components)?;
        touch_fingerprint(&tx, fingerprint)?;
        write_last_collected(&tx)?;
        tx.commit()
            .map_err(|e| translate_busy(e, "failed to commit coverage data"))?;
        Ok(())
    }

    /// Replace coverage data for just the tests in `mappings` (regardless of
    /// the sha they're currently anchored at) with rows anchored at
    /// `new_collect_sha`. Other tests' rows under the same fingerprint stay
    /// put — that's the whole point of `collect --diff`. Component hashes
    /// are refreshed to match `components` (the current build environment).
    pub fn update_coverage_for_tests(
        &mut self,
        fingerprint: &str,
        components: &[FingerprintComponent],
        new_collect_sha: &str,
        mappings: &[(TestId, BTreeSet<HitRange>)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        batch_delete_tests(&tx, fingerprint, mappings.iter().map(|(t, _)| t))?;
        insert_mappings(&tx, fingerprint, new_collect_sha, mappings)?;
        upsert_components(&tx, fingerprint, components)?;
        touch_fingerprint(&tx, fingerprint)?;
        write_last_collected(&tx)?;
        tx.commit()
            .map_err(|e| translate_busy(e, "failed to commit coverage update"))?;
        Ok(())
    }

    /// Drop rows for tests under `fingerprint` whose `(binary_id, test_name)`
    /// is not in `present`. Used by `collect --diff` to clear out tests that
    /// were renamed or deleted between collects. Returns the number of
    /// distinct tests pruned.
    pub fn prune_missing_tests(
        &mut self,
        fingerprint: &str,
        present: &BTreeSet<TestId>,
    ) -> Result<usize> {
        // Read with a plain `&Connection` so the no-op case (everything in
        // the listing is already in the DB) doesn't open a write
        // transaction at all. WAL gives us a snapshot read that won't see
        // mid-flight changes from another collect.
        let stored: BTreeSet<TestId> = {
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT binary_id, test_name FROM test_regions \
                 WHERE env_fingerprint = ?1",
            )?;
            let rows: BTreeSet<TestId> = stmt
                .query_map([fingerprint], |row| {
                    Ok(TestId::new(row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            rows
        };
        let to_drop: Vec<TestId> = stored.difference(present).cloned().collect();
        if to_drop.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.transaction()?;
        batch_delete_tests(&tx, fingerprint, to_drop.iter())?;
        tx.commit()
            .map_err(|e| translate_busy(e, "failed to commit prune"))?;
        Ok(to_drop.len())
    }

    /// Find tests under `(fingerprint, collect_sha)` that overlap any of the
    /// given line ranges in `file`. Per hunk: range-overlap; if a hunk
    /// overlaps no stored range, broaden to "any test with rows for this
    /// file at this sha" (the structural-edit backstop for struct-field,
    /// derive, const, use, mod edits).
    ///
    /// Scoping by `collect_sha` matters because `collect --diff` lets rows
    /// from different collect points coexist for the same fingerprint —
    /// hunks must be matched against rows whose line numbers share a
    /// coordinate system. Callers iterate over the distinct shas returned by
    /// [`Db::collect_shas`] and union the results.
    ///
    /// Pulls all rows for `(file, fingerprint, collect_sha)` once and walks
    /// them in Rust rather than running 2 queries per hunk: rows-per-file is
    /// bounded by hit functions in the file (small), and SQLite round-trips
    /// dominate the per-hunk cost.
    pub fn tests_covering_ranges(
        &self,
        fingerprint: &str,
        collect_sha: &str,
        file: &str,
        hunks: &[LineRange],
    ) -> Result<BTreeSet<TestId>> {
        let mut tests = BTreeSet::new();
        if hunks.is_empty() {
            return Ok(tests);
        }

        let mut stmt = self.conn.prepare(
            "SELECT binary_id, test_name, line_start, line_end FROM test_regions \
             WHERE source_file = ?1 AND env_fingerprint = ?2 AND collect_sha = ?3",
        )?;
        let rows: Vec<(TestId, i64, i64)> = stmt
            .query_map(rusqlite::params![file, fingerprint, collect_sha], |row| {
                Ok((
                    TestId::new(row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        if rows.is_empty() {
            return Ok(tests);
        }

        for hunk in hunks {
            // Range-overlap: a stored row [ls, le] overlaps a hunk [hs, he]
            // iff ls <= he AND le >= hs (closed intervals).
            let mut overlapped = false;
            for (test, ls, le) in &rows {
                if *ls <= hunk.end && *le >= hunk.start {
                    tests.insert(test.clone());
                    overlapped = true;
                }
            }
            if !overlapped {
                tests.extend(rows.iter().map(|(t, _, _)| t.clone()));
            }
        }

        touch_fingerprint(&self.conn, fingerprint)?;
        Ok(tests)
    }

    /// Distinct `collect_sha` values present in `test_regions` for this
    /// fingerprint. Empty when no coverage has been collected yet for the
    /// current environment.
    pub fn collect_shas(&self, fingerprint: &str) -> Result<BTreeSet<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT collect_sha FROM test_regions WHERE env_fingerprint = ?1",
        )?;
        let shas = stmt
            .query_map([fingerprint], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(shas)
    }

    /// Count of distinct tests tracked under the current fingerprint.
    pub fn test_count(&self, fingerprint: &str) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM \
             (SELECT DISTINCT binary_id, test_name FROM test_regions WHERE env_fingerprint = ?1)",
            [fingerprint],
            |r| r.get(0),
        )?;
        Ok(count as usize)
    }

    /// Count of (test, file, range) rows under the current fingerprint.
    pub fn region_count(&self, fingerprint: &str) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM test_regions WHERE env_fingerprint = ?1",
            [fingerprint],
            |r| r.get(0),
        )?;
        Ok(count as usize)
    }

    /// Whether a source file has coverage data under the current fingerprint.
    pub fn file_tracked(&self, fingerprint: &str, file: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM test_regions \
             WHERE env_fingerprint = ?1 AND source_file = ?2",
            [fingerprint, file],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    /// Distinct (binary_id, test_name) pairs under `fingerprint` whose rows
    /// are anchored at one of `shas`. Used by selection to compute "tests
    /// known to the cache *and reachable*" — tests anchored only at diverged
    /// shas appear missing here, so selection surfaces them as new and
    /// reruns them.
    pub fn all_tests_at_shas(
        &self,
        fingerprint: &str,
        shas: &BTreeSet<String>,
    ) -> Result<BTreeSet<TestId>> {
        if shas.is_empty() {
            return Ok(BTreeSet::new());
        }
        let sql = format!(
            "SELECT DISTINCT binary_id, test_name FROM test_regions \
             WHERE env_fingerprint = ?1 AND collect_sha IN ({})",
            in_placeholders(2, shas.len()),
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params = std::iter::once(fingerprint).chain(shas.iter().map(String::as_str));
        let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
            Ok(TestId::new(row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<BTreeSet<TestId>>>()
            .map_err(Into::into)
    }

    /// Count of `test_regions` rows under `fingerprint` whose `collect_sha`
    /// is one of `shas`. Used by `status` to quantify diverged-row bloat so
    /// the user knows when `cargo affected clean` is worth running.
    pub fn region_count_at_shas(
        &self,
        fingerprint: &str,
        shas: &BTreeSet<String>,
    ) -> Result<usize> {
        if shas.is_empty() {
            return Ok(0);
        }
        let sql = format!(
            "SELECT COUNT(*) FROM test_regions \
             WHERE env_fingerprint = ?1 AND collect_sha IN ({})",
            in_placeholders(2, shas.len()),
        );
        let params = std::iter::once(fingerprint).chain(shas.iter().map(String::as_str));
        let count: i64 = self
            .conn
            .query_row(&sql, rusqlite::params_from_iter(params), |r| r.get(0))?;
        Ok(count as usize)
    }

    /// Whether the DB holds any coverage data at all (any fingerprint).
    pub fn has_any_coverage(&self) -> Result<bool> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM test_regions", [], |r| r.get(0))?;
        Ok(count > 0)
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
        tx.execute("DELETE FROM test_regions", [])?;
        tx.execute("DELETE FROM fingerprints", [])?;
        tx.execute("DELETE FROM fingerprint_components", [])?;
        tx.execute("DELETE FROM meta", [])?;
        tx.commit()
            .map_err(|e| translate_busy(e, "failed to commit clear"))?;
        Ok(())
    }

    /// Evict the least-recently-used fingerprints beyond `keep`, never
    /// evicting `current`. Returns the number evicted.
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
            tx.execute("DELETE FROM test_regions WHERE env_fingerprint = ?1", [fp])?;
            tx.execute("DELETE FROM fingerprints WHERE fingerprint = ?1", [fp])?;
            tx.execute(
                "DELETE FROM fingerprint_components WHERE env_fingerprint = ?1",
                [fp],
            )?;
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

/// Drop legacy `test_files` and rebuild any table whose columns predate the
/// current schema. Old rows can't be retroactively tagged with missing
/// columns, and `target/affected/` is cargo-clean territory, so resetting
/// is safe.
fn migrate_legacy_tables(conn: &Connection) -> Result<()> {
    conn.execute("DROP TABLE IF EXISTS test_files", [])?;
    conn.execute("DROP INDEX IF EXISTS idx_source_file_fp", [])?;

    // `test_regions` must carry every required column, including the per-row
    // `collect_sha` introduced for `collect --diff`. Anything older has rows
    // we can't retroactively anchor.
    if table_exists(conn, "test_regions")? {
        let columns = table_columns(conn, "test_regions")?;
        let needed = [
            "binary_id",
            "test_name",
            "source_file",
            "line_start",
            "line_end",
            "env_fingerprint",
            "collect_sha",
        ];
        if !needed.iter().all(|c| columns.iter().any(|col| col == c)) {
            conn.execute("DROP TABLE test_regions", [])?;
            conn.execute("DROP INDEX IF EXISTS idx_test_regions_lookup", [])?;
        }
    }

    // `fingerprints` previously carried a `collect_sha` column; rows there
    // referred to a single sha for the whole fingerprint. With per-row
    // `collect_sha` on `test_regions`, the column is dead weight — drop the
    // table so the new schema (without that column) is used. Last-seen will
    // be re-populated on next collect/query.
    if table_exists(conn, "fingerprints")? {
        let columns = table_columns(conn, "fingerprints")?;
        if columns.iter().any(|c| c == "collect_sha") {
            conn.execute("DROP TABLE fingerprints", [])?;
        }
    }

    // The `fingerprint_components` table was added to support the
    // diagnostic JSON report ("which input differs?"). Existing rows in
    // `test_regions` from a pre-components binary have no component data
    // we can retroactively recover, so reset the coverage tables and let
    // the next `collect` repopulate everything. Likewise if the components
    // table itself has the wrong shape.
    let coverage_needs_reset = if table_exists(conn, "fingerprint_components")? {
        let columns = table_columns(conn, "fingerprint_components")?;
        let needed = ["env_fingerprint", "label", "content_hash"];
        !needed.iter().all(|c| columns.iter().any(|col| col == c))
    } else {
        // No components table at all: any test_regions rows reference
        // fingerprints we can't decompose. Drop them.
        table_exists(conn, "test_regions")?
    };

    if coverage_needs_reset {
        drop_coverage_tables(conn)?;
    }

    // Invariant: every fingerprint referenced by `test_regions` must have
    // matching `fingerprint_components` rows. A mismatch means an old
    // binary wrote rows after the new schema was created (or vice versa);
    // either way we can't trust the components table to answer "what
    // differs?" for those rows. Reset.
    if table_exists(conn, "test_regions")? && table_exists(conn, "fingerprint_components")? {
        let orphan: i64 = conn.query_row(
            "SELECT COUNT(*) FROM ( \
                SELECT 1 FROM test_regions tr \
                WHERE NOT EXISTS ( \
                    SELECT 1 FROM fingerprint_components fc \
                    WHERE fc.env_fingerprint = tr.env_fingerprint \
                ) LIMIT 1 \
             )",
            [],
            |r| r.get(0),
        )?;
        if orphan > 0 {
            drop_coverage_tables(conn)?;
        }
    }
    Ok(())
}

/// Drop every table that holds coverage state (test rows, fingerprints,
/// component hashes). The next collect rebuilds them from scratch via the
/// `CREATE TABLE IF NOT EXISTS` schema.
fn drop_coverage_tables(conn: &Connection) -> Result<()> {
    conn.execute("DROP TABLE IF EXISTS test_regions", [])?;
    conn.execute("DROP INDEX IF EXISTS idx_test_regions_lookup", [])?;
    conn.execute("DROP TABLE IF EXISTS fingerprints", [])?;
    conn.execute("DROP TABLE IF EXISTS fingerprint_components", [])?;
    Ok(())
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&sql)?;
    let cols: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(cols)
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
    use camino::Utf8PathBuf;

    const FP_A: &str = "aaaaaaaa";
    const FP_B: &str = "bbbbbbbb";
    const SHA_A: &str = "0000000000000000000000000000000000000000";
    const SHA_B: &str = "1111111111111111111111111111111111111111";
    const BIN_A: &str = "crate_a";
    const BIN_B: &str = "crate_b";

    fn tid(binary_id: &str, test_name: &str) -> TestId {
        TestId::new(binary_id, test_name)
    }

    fn rng(file: &str, line_start: i64, line_end: i64) -> HitRange {
        HitRange {
            file: Utf8PathBuf::from(file),
            line_start,
            line_end,
        }
    }

    fn hunk(start: i64, end: i64) -> LineRange {
        LineRange { start, end }
    }

    /// Synthetic per-fingerprint components for tests. Production uses
    /// `fingerprint::compute`; tests just need the components table to
    /// have rows so the migration's invariant check stays satisfied.
    fn comps_for(fp: &str) -> Vec<FingerprintComponent> {
        vec![FingerprintComponent {
            label: "cargo_lock".to_string(),
            hash: format!("hash-of-{fp}-cargo_lock"),
        }]
    }

    #[test]
    fn roundtrip_with_range_overlap() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let a_ranges = BTreeSet::from([
            rng("src/lib.rs", 10, 20),
            rng("src/utils.rs", 5, 15),
        ]);
        let b_ranges = BTreeSet::from([rng("src/lib.rs", 50, 60)]);
        db.store_coverage(
            FP_A,
            &comps_for(FP_A),
            SHA_A,
            &[(tid(BIN_A, "test_a"), a_ranges), (tid(BIN_A, "test_b"), b_ranges)],
        )?;

        assert_eq!(db.test_count(FP_A)?, 2);
        assert_eq!(db.region_count(FP_A)?, 3);
        assert_eq!(db.collect_shas(FP_A)?, BTreeSet::from([SHA_A.to_string()]));

        // Hunk overlapping test_a's lib.rs range.
        let hits = db.tests_covering_ranges(FP_A, SHA_A, "src/lib.rs", &[hunk(15, 18)])?;
        assert_eq!(hits, BTreeSet::from([tid(BIN_A, "test_a")]));

        // Hunk overlapping test_b's lib.rs range.
        let hits = db.tests_covering_ranges(FP_A, SHA_A, "src/lib.rs", &[hunk(55, 55)])?;
        assert_eq!(hits, BTreeSet::from([tid(BIN_A, "test_b")]));

        // Multiple hunks in same file → union.
        let hits =
            db.tests_covering_ranges(FP_A, SHA_A, "src/lib.rs", &[hunk(15, 18), hunk(55, 55)])?;
        assert_eq!(
            hits,
            BTreeSet::from([tid(BIN_A, "test_a"), tid(BIN_A, "test_b")])
        );

        Ok(())
    }

    /// Backstop: a hunk that overlaps no stored range broadens to all tests
    /// with rows for the file. Models a struct-field edit between two
    /// functions — neither function's range overlaps line 25, but both tests
    /// touched lib.rs and so both must be selected to be safe.
    #[test]
    fn structural_edit_backstop() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        // test_a covers lines 10-20; test_b covers lines 50-60. Nothing
        // touches line 25 (the "struct field" edit zone).
        db.store_coverage(
            FP_A,
            &comps_for(FP_A),
            SHA_A,
            &[
                (tid(BIN_A, "test_a"), BTreeSet::from([rng("src/lib.rs", 10, 20)])),
                (tid(BIN_A, "test_b"), BTreeSet::from([rng("src/lib.rs", 50, 60)])),
            ],
        )?;

        // No range overlaps line 25 → backstop fires → every test touching
        // lib.rs is selected.
        let hits = db.tests_covering_ranges(FP_A, SHA_A, "src/lib.rs", &[hunk(25, 25)])?;
        assert_eq!(
            hits,
            BTreeSet::from([tid(BIN_A, "test_a"), tid(BIN_A, "test_b")])
        );

        // Backstop applies per-hunk: a body-edit hunk + a structural-edit
        // hunk in the same file still selects everyone.
        let hits = db.tests_covering_ranges(
            FP_A,
            SHA_A,
            "src/lib.rs",
            &[hunk(15, 15), hunk(25, 25)],
        )?;
        assert_eq!(
            hits,
            BTreeSet::from([tid(BIN_A, "test_a"), tid(BIN_A, "test_b")])
        );

        Ok(())
    }

    /// Crate roots stored with sentinel `(1, i64::MAX)` are overlapped by
    /// any hunk in the file, so every test that "covers" the crate root via
    /// the implicit-dep path is selected.
    #[test]
    fn crate_root_sentinel_overlaps_any_hunk() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let mappings = vec![
            (
                tid(BIN_A, "test_a"),
                BTreeSet::from([HitRange::sentinel(Utf8PathBuf::from("src/lib.rs"))]),
            ),
            (
                tid(BIN_A, "test_b"),
                BTreeSet::from([HitRange::sentinel(Utf8PathBuf::from("src/lib.rs"))]),
            ),
        ];
        db.store_coverage(FP_A, &comps_for(FP_A), SHA_A, &mappings)?;

        let hits = db.tests_covering_ranges(FP_A, SHA_A, "src/lib.rs", &[hunk(7, 7)])?;
        assert_eq!(
            hits,
            BTreeSet::from([tid(BIN_A, "test_a"), tid(BIN_A, "test_b")])
        );
        Ok(())
    }

    #[test]
    fn different_fingerprint_reads_empty() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        db.store_coverage(
            FP_A,
            &comps_for(FP_A),
            SHA_A,
            &[(tid(BIN_A, "test_a"), BTreeSet::from([rng("src/lib.rs", 1, 10)]))],
        )?;

        assert_eq!(db.test_count(FP_B)?, 0);
        assert_eq!(db.region_count(FP_B)?, 0);
        assert!(db
            .tests_covering_ranges(FP_B, SHA_A, "src/lib.rs", &[hunk(1, 5)])?
            .is_empty());
        assert!(!db.file_tracked(FP_B, "src/lib.rs")?);
        assert!(db
            .all_tests_at_shas(FP_B, &BTreeSet::from([SHA_A.to_string()]))?
            .is_empty());
        assert!(db.collect_shas(FP_B)?.is_empty());
        assert!(db.has_any_coverage()?);
        Ok(())
    }

    #[test]
    fn full_collect_preserves_other_fingerprints() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        db.store_coverage(
            FP_A,
            &comps_for(FP_A),
            SHA_A,
            &[(tid(BIN_A, "test_a"), BTreeSet::from([rng("src/lib.rs", 1, 5)]))],
        )?;
        db.store_coverage(
            FP_B,
            &comps_for(FP_B),
            SHA_B,
            &[(tid(BIN_A, "test_b"), BTreeSet::from([rng("src/other.rs", 10, 15)]))],
        )?;

        // Rewriting FP_A leaves FP_B alone.
        db.store_coverage(
            FP_A,
            &comps_for(FP_A),
            SHA_A,
            &[(tid(BIN_A, "test_a"), BTreeSet::from([rng("src/new.rs", 1, 1)]))],
        )?;

        assert_eq!(db.test_count(FP_A)?, 1);
        assert_eq!(db.test_count(FP_B)?, 1);
        assert_eq!(
            db.tests_covering_ranges(FP_B, SHA_B, "src/other.rs", &[hunk(10, 12)])?,
            BTreeSet::from([tid(BIN_A, "test_b")])
        );
        assert_eq!(
            db.tests_covering_ranges(FP_A, SHA_A, "src/new.rs", &[hunk(1, 1)])?,
            BTreeSet::from([tid(BIN_A, "test_a")])
        );
        assert!(db
            .tests_covering_ranges(FP_A, SHA_A, "src/lib.rs", &[hunk(1, 5)])?
            .is_empty());
        Ok(())
    }

    /// Two tests with the same name in different binaries must round-trip
    /// independently.
    #[test]
    fn same_test_name_in_different_binaries() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        db.store_coverage(
            FP_A,
            &comps_for(FP_A),
            SHA_A,
            &[
                (tid(BIN_A, "builds"), BTreeSet::from([rng("crate_a/tests/builds.rs", 1, 5)])),
                (tid(BIN_B, "builds"), BTreeSet::from([rng("crate_b/tests/builds.rs", 1, 5)])),
            ],
        )?;

        assert_eq!(db.test_count(FP_A)?, 2);
        let a = db.tests_covering_ranges(FP_A, SHA_A, "crate_a/tests/builds.rs", &[hunk(2, 3)])?;
        assert_eq!(a, BTreeSet::from([tid(BIN_A, "builds")]));
        let b = db.tests_covering_ranges(FP_A, SHA_A, "crate_b/tests/builds.rs", &[hunk(2, 3)])?;
        assert_eq!(b, BTreeSet::from([tid(BIN_B, "builds")]));
        Ok(())
    }

    #[test]
    fn clear_wipes_all_fingerprints() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let mappings =
            vec![(tid(BIN_A, "test_a"), BTreeSet::from([rng("src/lib.rs", 1, 5)]))];
        db.store_coverage(FP_A, &comps_for(FP_A), SHA_A, &mappings)?;
        db.store_coverage(FP_B, &comps_for(FP_B), SHA_B, &mappings)?;
        assert!(db.has_any_coverage()?);

        db.clear()?;
        assert!(!db.has_any_coverage()?);
        assert_eq!(db.test_count(FP_A)?, 0);
        assert_eq!(db.test_count(FP_B)?, 0);
        assert!(db.last_collected()?.is_none());

        // Still usable.
        db.store_coverage(FP_A, &comps_for(FP_A), SHA_A, &mappings)?;
        assert_eq!(db.test_count(FP_A)?, 1);
        Ok(())
    }

    #[test]
    fn gc_keeps_current_and_most_recent_others() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let mappings =
            vec![(tid(BIN_A, "t"), BTreeSet::from([rng("src/lib.rs", 1, 5)]))];

        for fp in ["fp1", "fp2", "fp3", "fp4"] {
            db.store_coverage(fp, &comps_for(fp), SHA_A, &mappings)?;
            db.conn.execute(
                "UPDATE fingerprints SET last_seen = ?2 WHERE fingerprint = ?1",
                rusqlite::params![fp, format!("2020-01-01T00:00:{:02}Z", fp.as_bytes()[2] - b'0')],
            )?;
        }
        assert_eq!(db.fingerprint_count()?, 4);

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
    fn pre_range_schema_is_dropped() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = db_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap())?;

        // Old file-level schema with rows.
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
                INSERT INTO test_files VALUES ('bin1', 't1', 'src/lib.rs', 'fp_x');
                CREATE TABLE fingerprints (
                    fingerprint TEXT PRIMARY KEY,
                    last_seen TEXT NOT NULL
                );
                INSERT INTO fingerprints VALUES ('fp_x', '2020-01-01T00:00:00Z');
                ",
            )?;
        }

        // Open with new code: old `test_files` is gone, no rows survive.
        let db = Db::open(dir.path())?;
        assert!(!db.has_any_coverage()?);
        Ok(())
    }

    /// Pre-`--diff` schema put `collect_sha` on `fingerprints` and left it
    /// off `test_regions`. Both shapes must be dropped so the new per-row
    /// layout takes over cleanly.
    #[test]
    fn pre_diff_schema_is_dropped() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = db_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap())?;

        {
            let conn = Connection::open(&path)?;
            conn.execute_batch(
                "\
                CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                CREATE TABLE test_regions (
                    binary_id TEXT NOT NULL,
                    test_name TEXT NOT NULL,
                    source_file TEXT NOT NULL,
                    line_start INTEGER NOT NULL,
                    line_end INTEGER NOT NULL,
                    env_fingerprint TEXT NOT NULL,
                    PRIMARY KEY (binary_id, test_name, source_file, line_start, line_end, env_fingerprint)
                );
                INSERT INTO test_regions VALUES ('bin1', 't1', 'src/lib.rs', 1, 5, 'fp_x');
                CREATE TABLE fingerprints (
                    fingerprint TEXT PRIMARY KEY,
                    last_seen TEXT NOT NULL,
                    collect_sha TEXT
                );
                INSERT INTO fingerprints VALUES ('fp_x', '2020-01-01T00:00:00Z', 'deadbeef');
                ",
            )?;
        }

        let db = Db::open(dir.path())?;
        assert!(!db.has_any_coverage()?);
        assert_eq!(db.fingerprint_count()?, 0);
        Ok(())
    }

    /// `update_coverage_for_tests` replaces just the rerun tests' rows,
    /// re-anchored at the new sha; everyone else stays put with their old
    /// sha.
    #[test]
    fn diff_update_replaces_only_rerun_tests() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        db.store_coverage(
            FP_A,
            &comps_for(FP_A),
            SHA_A,
            &[
                (tid(BIN_A, "test_a"), BTreeSet::from([rng("src/a.rs", 1, 5)])),
                (tid(BIN_A, "test_b"), BTreeSet::from([rng("src/b.rs", 1, 5)])),
                (tid(BIN_A, "test_c"), BTreeSet::from([rng("src/c.rs", 1, 5)])),
            ],
        )?;
        assert_eq!(db.collect_shas(FP_A)?, BTreeSet::from([SHA_A.to_string()]));

        // Rerun only test_a, with new ranges anchored at SHA_B.
        db.update_coverage_for_tests(
            FP_A,
            &comps_for(FP_A),
            SHA_B,
            &[(tid(BIN_A, "test_a"), BTreeSet::from([rng("src/a.rs", 10, 20)]))],
        )?;

        // Both shas now coexist for FP_A.
        assert_eq!(
            db.collect_shas(FP_A)?,
            BTreeSet::from([SHA_A.to_string(), SHA_B.to_string()]),
        );

        // test_a is queryable at SHA_B with its new range.
        assert_eq!(
            db.tests_covering_ranges(FP_A, SHA_B, "src/a.rs", &[hunk(15, 15)])?,
            BTreeSet::from([tid(BIN_A, "test_a")]),
        );
        // …and gone from SHA_A: its old (1,5) row was deleted.
        assert!(db
            .tests_covering_ranges(FP_A, SHA_A, "src/a.rs", &[hunk(1, 5)])?
            .is_empty());

        // test_b/test_c untouched — still anchored at SHA_A.
        assert_eq!(
            db.tests_covering_ranges(FP_A, SHA_A, "src/b.rs", &[hunk(1, 5)])?,
            BTreeSet::from([tid(BIN_A, "test_b")]),
        );
        assert_eq!(
            db.tests_covering_ranges(FP_A, SHA_A, "src/c.rs", &[hunk(1, 5)])?,
            BTreeSet::from([tid(BIN_A, "test_c")]),
        );

        Ok(())
    }

    /// `prune_missing_tests` drops rows for tests that aren't in the current
    /// listing — renamed or deleted between collects.
    #[test]
    fn diff_prune_drops_absent_tests() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        db.store_coverage(
            FP_A,
            &comps_for(FP_A),
            SHA_A,
            &[
                (tid(BIN_A, "test_a"), BTreeSet::from([rng("src/a.rs", 1, 5)])),
                (tid(BIN_A, "test_b"), BTreeSet::from([rng("src/b.rs", 1, 5)])),
                (tid(BIN_A, "test_c"), BTreeSet::from([rng("src/c.rs", 1, 5)])),
            ],
        )?;

        let present = BTreeSet::from([tid(BIN_A, "test_a"), tid(BIN_A, "test_c")]);
        let evicted = db.prune_missing_tests(FP_A, &present)?;
        assert_eq!(evicted, 1);
        assert_eq!(db.test_count(FP_A)?, 2);
        assert!(db
            .tests_covering_ranges(FP_A, SHA_A, "src/b.rs", &[hunk(1, 5)])?
            .is_empty());
        assert_eq!(
            db.tests_covering_ranges(FP_A, SHA_A, "src/a.rs", &[hunk(1, 5)])?,
            BTreeSet::from([tid(BIN_A, "test_a")]),
        );
        Ok(())
    }

    /// `region_count_at_shas` scopes by both fingerprint and sha, used for
    /// the stale-row count `status` shows on partial divergence.
    #[test]
    fn region_count_scoped_by_shas() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        db.store_coverage(
            FP_A,
            &comps_for(FP_A),
            SHA_A,
            &[
                (tid(BIN_A, "ta"), BTreeSet::from([rng("src/a.rs", 1, 5)])),
                (tid(BIN_A, "tb"), BTreeSet::from([rng("src/b.rs", 1, 5)])),
            ],
        )?;
        db.update_coverage_for_tests(
            FP_A,
            &comps_for(FP_A),
            SHA_B,
            &[(tid(BIN_A, "ta"), BTreeSet::from([rng("src/a.rs", 10, 20)]))],
        )?;

        // FP_A now: tb at SHA_A (1 row), ta at SHA_B (1 row).
        assert_eq!(
            db.region_count_at_shas(FP_A, &BTreeSet::from([SHA_A.to_string()]))?,
            1,
        );
        assert_eq!(
            db.region_count_at_shas(FP_A, &BTreeSet::from([SHA_B.to_string()]))?,
            1,
        );
        assert_eq!(
            db.region_count_at_shas(
                FP_A,
                &BTreeSet::from([SHA_A.to_string(), SHA_B.to_string()]),
            )?,
            2,
        );
        // Empty input → 0 (and skips the IN-clause path).
        assert_eq!(db.region_count_at_shas(FP_A, &BTreeSet::new())?, 0);
        // Wrong fingerprint → 0.
        assert_eq!(
            db.region_count_at_shas(FP_B, &BTreeSet::from([SHA_A.to_string()]))?,
            0,
        );
        Ok(())
    }

    /// Reads back the (label, hash) component rows for a fingerprint via
    /// the same query the report builder will use, so we exercise the
    /// SQL shape too.
    fn read_components(db: &Db, fingerprint: &str) -> Vec<(String, String)> {
        let mut stmt = db
            .conn
            .prepare(
                "SELECT label, content_hash FROM fingerprint_components \
                 WHERE env_fingerprint = ?1 ORDER BY label",
            )
            .unwrap();
        stmt.query_map([fingerprint], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    }

    #[test]
    fn store_coverage_writes_components() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let comps = vec![
            FingerprintComponent {
                label: "cargo_lock".to_string(),
                hash: "h-lock".to_string(),
            },
            FingerprintComponent {
                label: "rustc".to_string(),
                hash: "h-rustc".to_string(),
            },
        ];
        db.store_coverage(
            FP_A,
            &comps,
            SHA_A,
            &[(tid(BIN_A, "test_a"), BTreeSet::from([rng("src/lib.rs", 1, 5)]))],
        )?;

        assert_eq!(
            read_components(&db, FP_A),
            vec![
                ("cargo_lock".to_string(), "h-lock".to_string()),
                ("rustc".to_string(), "h-rustc".to_string()),
            ],
        );
        Ok(())
    }

    /// Re-storing under the same fingerprint replaces — not duplicates —
    /// the component rows.
    #[test]
    fn store_coverage_replaces_components_idempotently() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let mappings =
            vec![(tid(BIN_A, "t"), BTreeSet::from([rng("src/lib.rs", 1, 5)]))];

        db.store_coverage(
            FP_A,
            &[FingerprintComponent {
                label: "cargo_lock".to_string(),
                hash: "first".to_string(),
            }],
            SHA_A,
            &mappings,
        )?;
        db.store_coverage(
            FP_A,
            &[FingerprintComponent {
                label: "cargo_lock".to_string(),
                hash: "second".to_string(),
            }],
            SHA_A,
            &mappings,
        )?;

        assert_eq!(
            read_components(&db, FP_A),
            vec![("cargo_lock".to_string(), "second".to_string())],
        );
        Ok(())
    }

    #[test]
    fn gc_evicts_components() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let mappings =
            vec![(tid(BIN_A, "t"), BTreeSet::from([rng("src/lib.rs", 1, 5)]))];

        for fp in ["fp1", "fp2", "fp3", "fp4"] {
            db.store_coverage(fp, &comps_for(fp), SHA_A, &mappings)?;
            db.conn.execute(
                "UPDATE fingerprints SET last_seen = ?2 WHERE fingerprint = ?1",
                rusqlite::params![fp, format!("2020-01-01T00:00:{:02}Z", fp.as_bytes()[2] - b'0')],
            )?;
        }
        // Sanity: every fingerprint has a components row.
        for fp in ["fp1", "fp2", "fp3", "fp4"] {
            assert_eq!(read_components(&db, fp).len(), 1);
        }

        db.gc("fp4", 2)?;
        // Evicted fingerprints' component rows must be gone too.
        assert!(read_components(&db, "fp1").is_empty());
        assert!(read_components(&db, "fp2").is_empty());
        // Survivors retained.
        assert_eq!(read_components(&db, "fp3").len(), 1);
        assert_eq!(read_components(&db, "fp4").len(), 1);
        Ok(())
    }

    #[test]
    fn clear_wipes_components() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        db.store_coverage(
            FP_A,
            &comps_for(FP_A),
            SHA_A,
            &[(tid(BIN_A, "t"), BTreeSet::from([rng("src/lib.rs", 1, 5)]))],
        )?;
        assert!(!read_components(&db, FP_A).is_empty());

        db.clear()?;
        assert!(read_components(&db, FP_A).is_empty());
        Ok(())
    }

    /// A pre-components DB (test_regions populated, no fingerprint_components
    /// table at all) must be reset on open. There's no way to retroactively
    /// generate component hashes for the existing rows.
    #[test]
    fn migration_resets_pre_components_db() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = db_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap())?;

        // Build the pre-components schema by hand: same shape as the
        // current `test_regions`/`fingerprints`, no `fingerprint_components`.
        {
            let conn = Connection::open(&path)?;
            conn.execute_batch(
                "\
                CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                CREATE TABLE test_regions (
                    binary_id TEXT NOT NULL,
                    test_name TEXT NOT NULL,
                    source_file TEXT NOT NULL,
                    line_start INTEGER NOT NULL,
                    line_end INTEGER NOT NULL,
                    env_fingerprint TEXT NOT NULL,
                    collect_sha TEXT NOT NULL,
                    PRIMARY KEY (binary_id, test_name, source_file, line_start, line_end, env_fingerprint, collect_sha)
                );
                INSERT INTO test_regions VALUES ('bin1', 't1', 'src/lib.rs', 1, 5, 'old_fp', 'sha');
                CREATE TABLE fingerprints (
                    fingerprint TEXT PRIMARY KEY,
                    last_seen TEXT NOT NULL
                );
                INSERT INTO fingerprints VALUES ('old_fp', '2020-01-01T00:00:00Z');
                ",
            )?;
        }

        let db = Db::open(dir.path())?;
        assert!(!db.has_any_coverage()?);
        assert_eq!(db.fingerprint_count()?, 0);
        Ok(())
    }

    /// If `fingerprint_components` exists but has the wrong column shape,
    /// drop it and reset coverage. Same destructive cutover as every other
    /// shape mismatch.
    #[test]
    fn migration_resets_on_components_column_mismatch() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = db_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap())?;

        {
            let conn = Connection::open(&path)?;
            conn.execute_batch(
                "\
                CREATE TABLE test_regions (
                    binary_id TEXT NOT NULL,
                    test_name TEXT NOT NULL,
                    source_file TEXT NOT NULL,
                    line_start INTEGER NOT NULL,
                    line_end INTEGER NOT NULL,
                    env_fingerprint TEXT NOT NULL,
                    collect_sha TEXT NOT NULL,
                    PRIMARY KEY (binary_id, test_name, source_file, line_start, line_end, env_fingerprint, collect_sha)
                );
                INSERT INTO test_regions VALUES ('bin1', 't1', 'src/lib.rs', 1, 5, 'fp', 'sha');
                CREATE TABLE fingerprint_components (
                    env_fingerprint TEXT NOT NULL,
                    label_OLD TEXT NOT NULL,    -- wrong column name
                    content_hash TEXT NOT NULL,
                    PRIMARY KEY (env_fingerprint, label_OLD)
                );
                ",
            )?;
        }

        let db = Db::open(dir.path())?;
        assert!(!db.has_any_coverage()?);
        Ok(())
    }

    /// Mixed old/new binary use can leave `test_regions` rows referencing
    /// fingerprints absent from `fingerprint_components`. The invariant
    /// check catches that and resets.
    #[test]
    fn migration_resets_on_orphan_test_regions_rows() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = db_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap())?;

        {
            let conn = Connection::open(&path)?;
            conn.execute_batch(
                "\
                CREATE TABLE test_regions (
                    binary_id TEXT NOT NULL,
                    test_name TEXT NOT NULL,
                    source_file TEXT NOT NULL,
                    line_start INTEGER NOT NULL,
                    line_end INTEGER NOT NULL,
                    env_fingerprint TEXT NOT NULL,
                    collect_sha TEXT NOT NULL,
                    PRIMARY KEY (binary_id, test_name, source_file, line_start, line_end, env_fingerprint, collect_sha)
                );
                CREATE TABLE fingerprint_components (
                    env_fingerprint TEXT NOT NULL,
                    label TEXT NOT NULL,
                    content_hash TEXT NOT NULL,
                    PRIMARY KEY (env_fingerprint, label)
                );
                INSERT INTO test_regions VALUES ('bin1', 't1', 'src/lib.rs', 1, 5, 'orphan_fp', 'sha');
                -- No matching fingerprint_components row for orphan_fp.
                INSERT INTO fingerprint_components VALUES ('different_fp', 'cargo_lock', 'h');
                ",
            )?;
        }

        let db = Db::open(dir.path())?;
        assert!(!db.has_any_coverage()?);
        Ok(())
    }
}
