//! Portability tripwire: `cargo affected collect` must produce at least one
//! `test_regions` row that is not a crate-root sentinel.
//!
//! Sentinel rows (`line_end == coverage::CRATE_ROOT_SENTINEL_END`, i.e.
//! `i64::MAX`) are the structural-edit backstop — they cover a target's crate
//! roots and overlap every hunk in *those* files by construction. If the real
//! function ranges ever silently vanished — a path-form mismatch on a new
//! platform making `Path::strip_prefix(canonical_root)` discard every
//! function, or a symbol-name form the profile-to-map join doesn't recognise —
//! the DB would hold *only* sentinels, and nothing about that looks like an
//! error: collect exits 0 and the row count is plausible. But a sentinel
//! covers a crate root and nothing else, so every edit elsewhere would select
//! no test at all — silent *under*-selection, the one failure this tool can't
//! detect downstream.
//!
//! `build_function_map` and `hit_ranges` each refuse their own half of that at
//! runtime; this scenario is the end-to-end check that real ranges come out
//! the far end.
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
         CRATE_ROOT_SENTINEL_END (i64::MAX); a sentinel-only DB means the \
         function ranges were lost between llvm-cov and the DB — likely a \
         path-form mismatch in Path::strip_prefix(canonical_root), or a \
         profile-to-map join that matched nothing"
    );
}
