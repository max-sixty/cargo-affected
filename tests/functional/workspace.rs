//! Workspace projects.
//!
//! cargo-affected uses `cargo metadata` to determine the workspace root and
//! stores its DB there. This scenario builds a virtual workspace with two
//! members, each with its own tests, and verifies that:
//!
//! 1. `collect` enumerates tests across both members.
//! 2. Editing a function inside a non-crate-root submodule of one member
//!    selects only that member's tests — the other member's tests must not
//!    be pulled in.
//!
//! The "non-crate-root" qualifier matters: the binary intentionally adds a
//! sentinel `(1, i64::MAX)` range for every workspace target's crate root
//! (`*/src/lib.rs`) to every test, so that structural edits in any crate root
//! pull in everyone. That means an edit directly in `math/src/lib.rs` would
//! over-select across members today — and verifying narrowing requires
//! editing a submodule that's not itself a crate root.

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

    // Member 1: math. lib.rs re-exports algo so its function is reachable from
    // tests; the actual `add` function lives in algo.rs (a non-crate-root file)
    // so an edit inside it doesn't trigger the crate-root sentinel.
    let math = dir.join("math");
    std::fs::create_dir_all(math.join("src")).unwrap();
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

    // Member 2: strings. Same shape — submodule for the function so its tests
    // can reach the function but the file is not a crate root.
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

    // Both members' tests should be tracked. The DB lives at the workspace
    // root regardless of which member is active.
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
    assert!(
        test_names.iter().any(|t| t.contains("test_math_add")),
        "expected test_math_add in DB, got: {test_names:?}"
    );
    assert!(
        test_names.iter().any(|t| t.contains("test_strings_greet")),
        "expected test_strings_greet in DB, got: {test_names:?}"
    );

    // Edit a non-crate-root file in math. Only the math member's tests should
    // be selected; the strings member's tests have no rows for math/src/algo.rs.
    replace_in_file(&dir.join("math/src/algo.rs"), "a + b", "a + b /* edited */");

    let status = cargo_affected(dir, &["affected", "status", "-v"]);
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);

    assert!(
        stdout.contains("test_math_add"),
        "edit in math should select test_math_add, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("test_strings_greet"),
        "edit in math member must NOT pull in strings member's tests, got:\n{stdout}"
    );

    git(dir, &["checkout", "--", "math/src/algo.rs"]);
}
