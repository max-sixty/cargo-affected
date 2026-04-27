//! Integration test for the full cargo-affected pipeline.
//!
//! Creates a temp Rust project, initializes git, runs collect, then verifies
//! that status correctly identifies affected tests after a source change.
//!
//! Marked `#[ignore]` because coverage builds are slow (~30s).
//! Run with: `cargo test -- --ignored`

mod common;

#[test]
#[ignore]
fn test_full_pipeline() {
    if !common::llvm_tools_available() {
        eprintln!("SKIP: llvm-tools not installed (rustup component add llvm-tools)");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    common::write_sample_project(dir);

    common::git(dir, &["init"]);
    common::git(dir, &["config", "user.email", "test@test.com"]);
    common::git(dir, &["config", "user.name", "Test"]);
    common::git(dir, &["add", "."]);
    common::git(dir, &["commit", "-m", "initial"]);

    let output = common::cargo_affected(dir, &["affected", "collect"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "collect failed: {stderr}\nstdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("storing coverage"),
        "expected 'storing coverage' in collect output, got: {stderr}"
    );

    let db_path = dir.join("target").join("affected").join("coverage.db");
    assert!(
        db_path.exists(),
        "target/affected/coverage.db should exist after collect"
    );

    // Verify mappings + collect_sha were stored.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let test_count: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT test_name) FROM test_regions",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(test_count, 3, "expected 3 tests in DB");

    let stored_sha: String = conn
        .query_row("SELECT collect_sha FROM fingerprints LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(stored_sha.len(), 40, "stored sha should be a full hex sha");

    // Each math test should map to math.rs with at least one row.
    let math_tests: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT test_name FROM test_regions WHERE source_file LIKE '%math.rs'",
            )
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    assert!(
        math_tests.iter().any(|t| t.contains("test_add")),
        "expected test_add to cover math.rs, got: {math_tests:?}"
    );
    assert!(
        math_tests.iter().any(|t| t.contains("test_multiply")),
        "expected test_multiply to cover math.rs, got: {math_tests:?}"
    );

    // Editing only `add`'s body must select test_add but not test_multiply.
    let math_path = dir.join("src/math.rs");
    common::replace_in_file(&math_path, "a + b", "a + b /* edited */");

    let output = common::cargo_affected(dir, &["affected", "status", "-v"]);
    assert!(
        output.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

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

    common::git(dir, &["checkout", "--", "src/math.rs"]);

    // Adding a derive lands outside any function body; no stored range
    // overlaps, so the file-level backstop must select every test that
    // covered math.rs.
    common::replace_in_file(
        &math_path,
        "pub struct Counter {",
        "#[derive(Debug, Clone)]\npub struct Counter {",
    );

    let output = common::cargo_affected(dir, &["affected", "status", "-v"]);
    assert!(
        output.status.success(),
        "status (structural edit) failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("test_add"),
        "backstop: test_add should run after struct-derive edit, got:\n{stdout}"
    );
    assert!(
        stdout.contains("test_multiply"),
        "backstop: test_multiply should run after struct-derive edit \
         (no function range overlaps so file-level fallback fires), got:\n{stdout}"
    );
    assert!(
        !stdout.contains("test_greet"),
        "structural edit in math.rs shouldn't pull in strings.rs tests:\n{stdout}"
    );

    common::git(dir, &["checkout", "--", "src/math.rs"]);
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(
        dir.join("tests/integration_new.rs"),
        "#[test]\nfn test_brand_new() {\n    assert_eq!(1 + 1, 2);\n}\n",
    )
    .unwrap();

    let output = common::cargo_affected(dir, &["affected", "status", "-v"]);
    assert!(
        output.status.success(),
        "status (with new test) failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("test_brand_new"),
        "status should list the new test, got:\n{stdout}"
    );
    assert!(
        stdout.contains("(new)"),
        "status should tag new tests with (new), got:\n{stdout}"
    );
    assert!(
        stdout.contains("1 new"),
        "status should report 1 new test in the summary, got:\n{stdout}"
    );
}
