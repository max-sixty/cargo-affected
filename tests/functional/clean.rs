//! `cargo affected clean` wipes coverage data, after which `status` should
//! report there's nothing to compare against.

use crate::{cargo_affected, init_git_with_initial_commit, write_two_module_project};

#[test]
fn clean_then_status_reports_no_coverage() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_clean");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    // Sanity: pre-clean status should show tracked tests.
    let pre = cargo_affected(dir, &["affected", "status"]);
    assert!(pre.status.success());
    let pre_stdout = String::from_utf8_lossy(&pre.stdout);
    assert!(
        pre_stdout.contains("tests tracked"),
        "pre-clean status should show 'tests tracked', got:\n{pre_stdout}"
    );

    // clean goes through SQL DELETE rather than unlinking the file (so
    // concurrent writers don't get orphaned). The file remains on disk; the
    // tables are empty.
    let clean = cargo_affected(dir, &["affected", "clean"]);
    assert!(
        clean.status.success(),
        "clean failed: stderr={}",
        String::from_utf8_lossy(&clean.stderr)
    );
    let clean_stderr = String::from_utf8_lossy(&clean.stderr);
    assert!(
        clean_stderr.contains("cleared"),
        "clean should report what it cleared, got:\n{clean_stderr}"
    );

    let db_path = dir.join("target/affected/coverage.db");
    assert!(
        db_path.exists(),
        "DB file persists after clean (clean uses SQL DELETE, not unlink)"
    );
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let test_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM test_regions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(test_rows, 0, "test_regions should be empty after clean");

    // Post-clean status: DB exists but is empty → "no coverage data yet"
    // path, with the `cargo affected collect` hint.
    let post = cargo_affected(dir, &["affected", "status"]);
    assert!(post.status.success());
    let post_stdout = String::from_utf8_lossy(&post.stdout);
    assert!(
        post_stdout.contains("no coverage data yet"),
        "post-clean status should report no coverage, got:\n{post_stdout}"
    );
    assert!(
        post_stdout.contains("cargo affected collect"),
        "should hint at running collect, got:\n{post_stdout}"
    );
}

#[test]
fn clean_with_no_db_is_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_clean_nodb");
    init_git_with_initial_commit(dir);

    // No collect was run — there's no DB.
    let clean = cargo_affected(dir, &["affected", "clean"]);
    assert!(
        clean.status.success(),
        "clean on a fresh project should succeed: {}",
        String::from_utf8_lossy(&clean.stderr)
    );
    let stderr = String::from_utf8_lossy(&clean.stderr);
    assert!(
        stderr.contains("no coverage database found"),
        "expected 'no coverage database found' message, got:\n{stderr}"
    );
}
