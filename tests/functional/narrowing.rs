//! Function-level narrowing: editing `add`'s body must select `test_add` and
//! nothing else.
//!
//! This is the headline guarantee — without it, cargo-affected is no better
//! than a file-level cache.

use crate::{
    cargo_affected, git, init_git_with_initial_commit, replace_in_file, write_two_module_project,
};

#[test]
fn function_body_edit_selects_only_that_test() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_narrowing");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}\nstdout: {}",
        String::from_utf8_lossy(&collect.stderr),
        String::from_utf8_lossy(&collect.stdout)
    );

    // Sanity-check that the DB picked up all three tests and anchored their
    // rows to a real collect_sha. The DB is a black box for the rest of the
    // suite — this scenario doubles as smoke for `collect` so other scenarios
    // can rely on it without each re-checking the schema.
    let db_path = dir.join("target/affected/coverage.db");
    assert!(
        db_path.exists(),
        "target/affected/coverage.db should exist after collect"
    );
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let test_count: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT test_name) FROM test_regions",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(test_count, 3, "expected 3 tests in DB");
    let stored_shas: Vec<String> = conn
        .prepare("SELECT DISTINCT collect_sha FROM test_regions")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        stored_shas.len(),
        1,
        "fresh full collect should anchor every row at a single sha, got {stored_shas:?}",
    );
    assert_eq!(
        stored_shas[0].len(),
        40,
        "stored sha should be a full hex sha",
    );

    // Edit only inside `add`'s body. `test_multiply`'s stored range doesn't
    // overlap, so it must not be selected. `test_greet` lives in strings.rs
    // entirely.
    replace_in_file(&dir.join("src/math.rs"), "a + b", "a + b /* edited */");

    let status = cargo_affected(dir, &["affected", "status", "-v"]);
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);

    assert!(
        stdout.contains("test_add"),
        "status should list test_add (its function body changed), got:\n{stdout}"
    );
    assert!(
        !stdout.contains("test_multiply"),
        "status should NOT list test_multiply (multiply unchanged) — \
         function-level narrowing failed:\n{stdout}"
    );
    assert!(
        !stdout.contains("test_greet"),
        "status should NOT list test_greet (strings.rs unchanged):\n{stdout}"
    );

    // Restore the working tree so a subsequent `git` invocation in the same
    // dir wouldn't see a residual edit. Each scenario uses its own dir, so
    // this is belt-and-braces — but cheap and clarifying.
    git(dir, &["checkout", "--", "src/math.rs"]);
}
