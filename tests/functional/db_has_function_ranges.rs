//! Portability tripwire: `cargo affected collect` must produce at least one
//! `test_regions` row that is not a crate-root sentinel.
//!
//! Sentinel rows (`line_end == coverage::CRATE_ROOT_SENTINEL_END`, i.e.
//! `i64::MAX`) are the structural-edit backstop — they overlap every hunk by
//! construction. If `extract_hit_ranges` ever silently filters out every real
//! `llvm-cov` path (e.g., a path-form mismatch on a new platform causing
//! `Path::strip_prefix(canonical_root)` to discard everything), the DB would
//! contain *only* sentinels. Selection would still appear to work because
//! sentinels match unconditionally, so the failure would surface as
//! over-selection rather than a hard error. This scenario asserts the
//! invariant directly.
//!
//! Definition of `CRATE_ROOT_SENTINEL_END` lives in `src/coverage.rs`. It's
//! `pub`, but `cargo-affected` ships only a `[[bin]]` target with no library,
//! so we hard-code `i64::MAX` here.

use crate::{cargo_affected, init_git_with_initial_commit, write_two_module_project};

const CRATE_ROOT_SENTINEL_END: i64 = i64::MAX;

#[test]
fn collect_writes_non_sentinel_function_ranges() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_db_has_function_ranges");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}\nstdout: {}",
        String::from_utf8_lossy(&collect.stderr),
        String::from_utf8_lossy(&collect.stdout)
    );

    let db_path = dir.join("target/affected/coverage.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let non_sentinel_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM test_regions WHERE line_end != ?1",
            [CRATE_ROOT_SENTINEL_END],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        non_sentinel_rows > 0,
        "expected at least one test_regions row with line_end != \
         CRATE_ROOT_SENTINEL_END (i64::MAX); a sentinel-only DB means \
         extract_hit_ranges filtered every llvm-cov path out, likely a \
         path-form mismatch in Path::strip_prefix(canonical_root)"
    );
}
