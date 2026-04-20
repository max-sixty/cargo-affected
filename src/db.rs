//! SQLite storage for test-to-file coverage mappings.
//!
//! Schema:
//! - `meta` — key/value pairs (e.g. last collection timestamp)
//! - `test_files` — (test_name, source_file) pairs with index on source_file
//!   for fast "which tests cover this file?" queries.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use rusqlite::Connection;

/// Umbrella directory for all difftest artifacts (DB, profraw files).
/// Lives under `target/` so it shares the gitignore and lifecycle
/// (cargo clean wipes it) of other build artifacts.
pub fn difftest_dir(project_root: &Path) -> PathBuf {
    project_root.join("target").join("difftest")
}

/// Canonical DB location within the difftest dir.
pub fn db_path(project_root: &Path) -> PathBuf {
    difftest_dir(project_root).join("coverage.db")
}

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS test_files (
    test_name TEXT NOT NULL,
    source_file TEXT NOT NULL,
    PRIMARY KEY (test_name, source_file)
);
CREATE INDEX IF NOT EXISTS idx_source_file ON test_files(source_file);
";

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open (or create) the database at `project_root/target/difftest/coverage.db`.
    pub fn open(project_root: &Path) -> Result<Self> {
        let path = db_path(project_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("failed to open database at {}", path.display()))?;
        conn.execute_batch(SCHEMA)
            .context("failed to initialize database schema")?;
        Ok(Self { conn })
    }

    /// Replace all coverage data with a fresh collection.
    ///
    /// Clears existing test_files rows and inserts the new mappings in a single
    /// transaction.
    pub fn store_coverage(
        &mut self,
        mappings: &[(String, BTreeSet<Utf8PathBuf>)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM test_files", [])?;

        {
            let mut stmt =
                tx.prepare("INSERT INTO test_files (test_name, source_file) VALUES (?1, ?2)")?;
            for (test_name, files) in mappings {
                for file in files {
                    stmt.execute(rusqlite::params![test_name, file.as_str()])?;
                }
            }
        }

        let timestamp = chrono_free_timestamp();
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('last_collected', ?1)",
            [&timestamp],
        )?;
        tx.commit().context("failed to commit coverage data")?;
        Ok(())
    }

    /// Find all test names that cover any of the given source files.
    pub fn tests_covering(&self, files: &[&str]) -> Result<BTreeSet<String>> {
        if files.is_empty() {
            return Ok(BTreeSet::new());
        }

        let placeholders: Vec<&str> = files.iter().map(|_| "?").collect();
        let sql = format!(
            "SELECT DISTINCT test_name FROM test_files WHERE source_file IN ({})",
            placeholders.join(", ")
        );

        let params: Vec<&dyn rusqlite::types::ToSql> =
            files.iter().map(|f| f as &dyn rusqlite::types::ToSql).collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params.as_slice(), |row| row.get::<_, String>(0))?;

        let mut tests = BTreeSet::new();
        for row in rows {
            tests.insert(row?);
        }
        Ok(tests)
    }

    /// Return total number of distinct tests tracked.
    pub fn test_count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(DISTINCT test_name) FROM test_files", [], |r| {
                r.get(0)
            })?;
        Ok(count as usize)
    }

    /// Return total number of (test, file) mappings.
    pub fn mapping_count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM test_files", [], |r| r.get(0))?;
        Ok(count as usize)
    }

    /// Check whether a source file has any coverage data.
    pub fn file_tracked(&self, file: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM test_files WHERE source_file = ?1",
            [file],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    /// Return all distinct test names in the database.
    pub fn all_test_names(&self) -> Result<BTreeSet<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT test_name FROM test_files")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut tests = BTreeSet::new();
        for row in rows {
            tests.insert(row?);
        }
        Ok(tests)
    }

    /// Update coverage for specific tests only, leaving other tests untouched.
    ///
    /// Deletes old rows for the given test names, then inserts the new mappings.
    pub fn update_coverage(
        &mut self,
        mappings: &[(String, BTreeSet<Utf8PathBuf>)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;

        {
            let mut delete_stmt =
                tx.prepare("DELETE FROM test_files WHERE test_name = ?1")?;
            let mut insert_stmt =
                tx.prepare("INSERT INTO test_files (test_name, source_file) VALUES (?1, ?2)")?;

            for (test_name, files) in mappings {
                delete_stmt.execute(rusqlite::params![test_name])?;
                for file in files {
                    insert_stmt.execute(rusqlite::params![test_name, file.as_str()])?;
                }
            }
        }

        let timestamp = chrono_free_timestamp();
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('last_collected', ?1)",
            [&timestamp],
        )?;
        tx.commit().context("failed to commit coverage update")?;
        Ok(())
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

/// Warn (to stderr) about changed `.rs` files that have no coverage data yet.
pub fn warn_untracked_rs_files(db: &Db, changed_files: &[String]) -> Result<()> {
    for file in changed_files {
        if file.ends_with(".rs") && !db.file_tracked(file)? {
            eprintln!(
                "warning: {file} has no coverage data \
                 — run `cargo difftest collect` to include it"
            );
        }
    }
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
            ("test_a".to_string(), files_a),
            ("test_b".to_string(), files_b),
        ];

        db.store_coverage(&mappings)?;

        assert_eq!(db.test_count()?, 2);
        assert_eq!(db.mapping_count()?, 3);

        let covering_lib = db.tests_covering(&["src/lib.rs"])?;
        assert_eq!(covering_lib.len(), 2);
        assert!(covering_lib.contains("test_a"));
        assert!(covering_lib.contains("test_b"));

        let covering_utils = db.tests_covering(&["src/utils.rs"])?;
        assert_eq!(covering_utils.len(), 1);
        assert!(covering_utils.contains("test_a"));

        let covering_none = db.tests_covering(&["src/nonexistent.rs"])?;
        assert!(covering_none.is_empty());

        assert!(db.last_collected()?.is_some());

        Ok(())
    }

    #[test]
    fn test_update_coverage() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        // Initial full store.
        let mappings = vec![
            ("test_a".to_string(), BTreeSet::from([Utf8PathBuf::from("src/lib.rs"), Utf8PathBuf::from("src/utils.rs")])),
            ("test_b".to_string(), BTreeSet::from([Utf8PathBuf::from("src/lib.rs")])),
        ];
        db.store_coverage(&mappings)?;
        assert_eq!(db.test_count()?, 2);
        assert_eq!(db.mapping_count()?, 3);

        // Partial update: re-collect test_a with different files, leave test_b alone.
        let update = vec![
            ("test_a".to_string(), BTreeSet::from([Utf8PathBuf::from("src/lib.rs"), Utf8PathBuf::from("src/new.rs")])),
        ];
        db.update_coverage(&update)?;

        assert_eq!(db.test_count()?, 2);
        assert_eq!(db.mapping_count()?, 3); // test_a: 2 files, test_b: 1 file

        // test_a now covers src/new.rs instead of src/utils.rs.
        let covering_new = db.tests_covering(&["src/new.rs"])?;
        assert!(covering_new.contains("test_a"));

        let covering_utils = db.tests_covering(&["src/utils.rs"])?;
        assert!(covering_utils.is_empty());

        // test_b is untouched.
        let covering_lib = db.tests_covering(&["src/lib.rs"])?;
        assert!(covering_lib.contains("test_a"));
        assert!(covering_lib.contains("test_b"));

        Ok(())
    }

    #[test]
    fn test_file_tracked() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let mappings = vec![
            ("test_a".to_string(), BTreeSet::from([Utf8PathBuf::from("src/lib.rs")])),
        ];
        db.store_coverage(&mappings)?;

        assert!(db.file_tracked("src/lib.rs")?);
        assert!(!db.file_tracked("src/nonexistent.rs")?);

        Ok(())
    }

    #[test]
    fn test_all_test_names() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;

        let mappings = vec![
            ("test_a".to_string(), BTreeSet::from([Utf8PathBuf::from("src/lib.rs")])),
            ("test_b".to_string(), BTreeSet::from([Utf8PathBuf::from("src/lib.rs")])),
        ];
        db.store_coverage(&mappings)?;

        let names = db.all_test_names()?;
        assert_eq!(names, BTreeSet::from(["test_a".to_string(), "test_b".to_string()]));

        Ok(())
    }
}
