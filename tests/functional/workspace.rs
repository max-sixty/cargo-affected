//! Workspace projects.
//!
//! cargo-affected uses `cargo metadata` to determine the workspace root and
//! stores its DB there. This scenario builds a virtual workspace with two
//! members, each with its own tests, and verifies:
//!
//! 1. `collect` enumerates tests across both members.
//! 2. **Cross-member narrowing**: editing `math/src/lib.rs` (a crate root in
//!    `math`) does NOT pull in `strings`'s tests. The crate-root sentinel
//!    `(1, i64::MAX)` is scoped per-package — `strings`'s tests can't observe
//!    a change in another crate's compilation unit, so they must not be
//!    selected.
//! 3. **Within-package structural guarantee**: editing `math/src/lib.rs`
//!    *does* pull in every test in `math`, including tests in
//!    `math/tests/integration.rs` that live in a separate compilation unit —
//!    a structural edit (`mod foo;`, `use ...;`) in a crate root must
//!    re-select every test that builds against that crate.

use std::path::Path;

use crate::{cargo_affected, git, init_git_with_initial_commit, replace_in_file};

fn write_workspace(dir: &Path, ws_name: &str) {
    // Virtual workspace: root has only [workspace], no [package]. Two members,
    // each a tiny lib. Distinct package names per member, prefixed with the
    // scenario name to avoid cargo's shared-cache name-collision foot-gun.
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"[workspace]
resolver = "2"
members = ["math", "strings"]
"#,
    )
    .unwrap();

    // /target and /Cargo.lock — see write_two_module_project for the
    // rationale; same trade-off applies here.
    std::fs::write(dir.join(".gitignore"), "/target\n/Cargo.lock\n").unwrap();

    // Member 1: math. The unit test in lib.rs and the integration test in
    // tests/integration.rs both build against math's crate roots — both must
    // be re-selected by an edit to math/src/lib.rs (within-package
    // structural guarantee).
    let math = dir.join("math");
    std::fs::create_dir_all(math.join("src")).unwrap();
    std::fs::create_dir_all(math.join("tests")).unwrap();
    std::fs::write(
        math.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{ws_name}_math"
version = "0.1.0"
edition = "2021"
"#
        ),
    )
    .unwrap();
    std::fs::write(
        math.join("src/lib.rs"),
        r#"pub mod algo;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_math_add() {
        assert_eq!(algo::add(2, 3), 5);
    }
}
"#,
    )
    .unwrap();
    std::fs::write(
        math.join("src/algo.rs"),
        r#"pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
"#,
    )
    .unwrap();
    // Integration test in math: a separate compilation unit from the lib's
    // unit tests, but still bound to math/src/lib.rs's crate root.
    std::fs::write(
        math.join("tests/integration.rs"),
        format!(
            r#"#[test]
fn test_math_integration() {{
    assert_eq!({ws_name}_math::algo::add(1, 1), 2);
}}
"#
        ),
    )
    .unwrap();

    // Member 2: strings. Same shape — submodule for the function so its tests
    // can reach it from outside the crate root.
    let strings = dir.join("strings");
    std::fs::create_dir_all(strings.join("src")).unwrap();
    std::fs::write(
        strings.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{ws_name}_strings"
version = "0.1.0"
edition = "2021"
"#
        ),
    )
    .unwrap();
    std::fs::write(
        strings.join("src/lib.rs"),
        r#"pub mod fmt;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strings_greet() {
        assert_eq!(fmt::greet("world"), "hello, world");
    }
}
"#,
    )
    .unwrap();
    std::fs::write(
        strings.join("src/fmt.rs"),
        r#"pub fn greet(name: &str) -> String {
    format!("hello, {name}")
}
"#,
    )
    .unwrap();
}

#[test]
fn workspace_edit_in_one_member_doesnt_pull_in_other() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_workspace(dir, "sample_workspace");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: stderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&collect.stderr),
        String::from_utf8_lossy(&collect.stdout)
    );

    // All three tests should be tracked. The DB lives at the workspace root
    // regardless of which member is active.
    let db_path = dir.join("target/affected/coverage.db");
    assert!(
        db_path.exists(),
        "DB should live at workspace root (target/affected/coverage.db)"
    );
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let test_names: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT DISTINCT test_name FROM test_regions")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    for expected in ["test_math_add", "test_math_integration", "test_strings_greet"] {
        assert!(
            test_names.iter().any(|t| t.contains(expected)),
            "expected {expected} in DB, got: {test_names:?}"
        );
    }

    // Direct sqlite check: no test should carry a row for a source file in
    // another package. This is the per-package sentinel guarantee at the
    // storage layer — without the fix, math/src/lib.rs sentinels would land
    // on strings's tests and vice versa.
    let cross_member_rows: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT binary_id, source_file FROM test_regions \
                 WHERE (binary_id LIKE '%_math%' AND source_file LIKE 'strings/%') \
                    OR (binary_id LIKE '%_strings%' AND source_file LIKE 'math/%')",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    assert!(
        cross_member_rows.is_empty(),
        "no test should carry rows for files in another package, got: {cross_member_rows:?}"
    );

    // Edit directly in math/src/lib.rs — a crate root. The edit sits above
    // any function range, so only the per-package sentinel for math's
    // lib.rs can match. This is exactly where the old workspace-wide
    // sentinel would have over-selected and pulled in `strings`'s tests.
    replace_in_file(
        &dir.join("math/src/lib.rs"),
        "pub mod algo;",
        "// edit at the top of the crate root\npub mod algo;",
    );

    let status = cargo_affected(dir, &["affected", "status", "-v"]);
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);

    // Within-package structural guarantee: the edit to math/src/lib.rs (a
    // crate root) re-selects every test in math, including the integration
    // test that lives in a separate compilation unit.
    assert!(
        stdout.contains("test_math_add"),
        "edit in math/src/lib.rs should select test_math_add, got:\n{stdout}"
    );
    assert!(
        stdout.contains("test_math_integration"),
        "edit in math/src/lib.rs should select test_math_integration \
         (within-package structural guarantee), got:\n{stdout}"
    );
    // Cross-member narrowing: strings's tests must NOT be pulled in.
    assert!(
        !stdout.contains("test_strings_greet"),
        "edit in math member must NOT pull in strings member's tests, got:\n{stdout}"
    );

    git(dir, &["checkout", "--", "math/src/lib.rs"]);
}
