//! Workspace projects.
//!
//! cargo-affected uses `cargo metadata` to determine the workspace root and
//! stores its DB there. This scenario builds a virtual workspace with two
//! members where `math` has a path dep on `strings`, and verifies the
//! per-target sentinel scoping:
//!
//! 1. **Discovery**: `collect` enumerates tests across both members and
//!    across both target kinds in `math` (lib unit test + integration test).
//! 2. **Within-package structural guarantee + cross-member narrowing**:
//!    editing `math/src/lib.rs` pulls in every test in `math` (including the
//!    integration test that lives in a separate compilation unit), but does
//!    NOT pull in `strings`'s tests — `strings` has no dependency on `math`.
//! 3. **Per-target narrowing within a package**: editing
//!    `math/tests/integration.rs` selects the integration test only — it
//!    must NOT pull in `test_math_add` (a lib unit test in a different
//!    compilation unit).
//! 4. **Cross-package transitive dep**: editing `strings/src/lib.rs`
//!    (`math` depends on it via `path = "../strings"`) pulls in math's
//!    tests too.

use std::path::Path;

use crate::{cargo_affected, git, init_git_with_initial_commit, replace_in_file};

fn write_workspace(dir: &Path, ws_name: &str) {
    // Virtual workspace: root has only [workspace], no [package]. Two members.
    // Distinct package names per member, prefixed with the scenario name to
    // avoid cargo's shared-cache name-collision foot-gun.
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

    // Member 1: math. Path-deps on strings so we can verify cross-package
    // sentinel propagation. Unit test in lib.rs and integration test in
    // tests/integration.rs let us exercise per-target narrowing.
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

[dependencies]
{ws_name}_strings = {{ path = "../strings" }}
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
    // unit tests. Per-target narrowing means an edit here must not pull in
    // `test_math_add`, even though both belong to package `_math`.
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

    // Member 2: strings. No deps; math depends on it.
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
fn workspace_collect_finds_all_tests_across_targets() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_workspace(dir, "sample_ws_collect");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: stderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&collect.stderr),
        String::from_utf8_lossy(&collect.stdout)
    );

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
    for expected in [
        "test_math_add",
        "test_math_integration",
        "test_strings_greet",
    ] {
        assert!(
            test_names.iter().any(|t| t.contains(expected)),
            "expected {expected} in DB, got: {test_names:?}"
        );
    }
}

#[test]
fn editing_lib_in_one_member_does_not_pull_in_unrelated_member() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_workspace(dir, "sample_ws_lib_edit");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    // Edit at the top of math/src/lib.rs — above any function range, so
    // selection is driven by the per-target sentinel for math/src/lib.rs.
    // strings does NOT depend on math, so strings's tests must not be
    // pulled in.
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

    // Within-package structural guarantee: every test in math runs.
    assert!(
        stdout.contains("test_math_add"),
        "edit in math/src/lib.rs should select test_math_add, got:\n{stdout}"
    );
    assert!(
        stdout.contains("test_math_integration"),
        "edit in math/src/lib.rs should select test_math_integration \
         (within-package structural guarantee), got:\n{stdout}"
    );
    // strings doesn't depend on math, so its tests stay out.
    assert!(
        !stdout.contains("test_strings_greet"),
        "edit in math (which strings does not depend on) must NOT pull in \
         strings's tests, got:\n{stdout}"
    );

    git(dir, &["checkout", "--", "math/src/lib.rs"]);
}

#[test]
fn editing_integration_test_does_not_pull_in_lib_unit_tests() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_workspace(dir, "sample_ws_int_edit");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    // Edit the integration test file. Per-target narrowing: the lib's unit
    // test (`test_math_add`) is in a different compilation unit and does
    // NOT compile against tests/integration.rs, so it must not be selected.
    replace_in_file(
        &dir.join("math/tests/integration.rs"),
        "add(1, 1), 2",
        "add(1, 1), 2 /* edited */",
    );

    let status = cargo_affected(dir, &["affected", "status", "-v"]);
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);

    assert!(
        stdout.contains("test_math_integration"),
        "edit in tests/integration.rs should select test_math_integration, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("test_math_add"),
        "edit in tests/integration.rs must NOT pull in test_math_add \
         (per-target narrowing within a package), got:\n{stdout}"
    );
    assert!(
        !stdout.contains("test_strings_greet"),
        "edit in math's integration test must NOT pull in strings's tests, got:\n{stdout}"
    );

    git(dir, &["checkout", "--", "math/tests/integration.rs"]);
}

#[test]
fn editing_dep_lib_pulls_in_dependent_tests() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_workspace(dir, "sample_ws_dep_edit");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    // Edit at the top of strings/src/lib.rs. math depends on strings, so
    // every math test (lib unit + integration) must be re-selected — the
    // structural edit could change strings's interface (added/removed
    // module) and break math's compile. strings's own tests obviously also
    // run.
    replace_in_file(
        &dir.join("strings/src/lib.rs"),
        "pub mod fmt;",
        "// edit at the top of strings's crate root\npub mod fmt;",
    );

    let status = cargo_affected(dir, &["affected", "status", "-v"]);
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);

    assert!(
        stdout.contains("test_strings_greet"),
        "edit in strings/src/lib.rs should select test_strings_greet, got:\n{stdout}"
    );
    assert!(
        stdout.contains("test_math_add"),
        "edit in strings/src/lib.rs should pull in test_math_add \
         (math depends on strings via path dep), got:\n{stdout}"
    );
    assert!(
        stdout.contains("test_math_integration"),
        "edit in strings/src/lib.rs should pull in test_math_integration, got:\n{stdout}"
    );

    git(dir, &["checkout", "--", "strings/src/lib.rs"]);
}
