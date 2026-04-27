//! Integration test for the full cargo-affected pipeline.
//!
//! Creates a temp Rust project, initializes git, runs collect, then verifies
//! that status correctly identifies affected tests after a source change.
//!
//! Marked `#[ignore]` because coverage builds are slow (~30s).
//! Run with: `cargo test -- --ignored`

use std::path::Path;
use std::process::Command;

/// Run cargo-affected binary with the given args in the given directory.
fn cargo_affected(dir: &Path, args: &[&str]) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_cargo-affected");
    Command::new(bin)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run cargo-affected: {e}"))
}

/// Run a git command in the given directory, panicking on failure.
fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run git {}: {e}", args.join(" ")));
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Check that llvm-tools are available (required for coverage collection).
fn llvm_tools_available() -> bool {
    let output = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output();
    let Ok(output) = output else { return false };
    if !output.status.success() {
        return false;
    }
    let sysroot = String::from_utf8_lossy(&output.stdout).trim().to_string();

    let target_output = Command::new("rustc").arg("-vV").output();
    let Ok(target_output) = target_output else {
        return false;
    };
    let stdout = String::from_utf8_lossy(&target_output.stdout);
    let target = stdout
        .lines()
        .find(|l| l.starts_with("host:"))
        .map(|l| l.trim_start_matches("host:").trim().to_string())
        .unwrap_or_default();

    let tool_path = std::path::PathBuf::from(&sysroot)
        .join("lib/rustlib")
        .join(&target)
        .join("bin/llvm-profdata");
    tool_path.exists()
}

/// Write a small two-module Rust project with tests into the given directory.
///
/// `math.rs` has two independently-tested functions (`add` and `multiply`) so
/// we can verify function-level narrowing — editing the body of `add` must
/// NOT select `test_multiply`.
fn write_sample_project(dir: &Path) {
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
name = "sample"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();

    // Real cargo projects ignore target/. Without this, cargo-affected's own
    // profraw dirs (which include test names) leak into `git status` as
    // untracked paths.
    std::fs::write(dir.join(".gitignore"), "/target\n").unwrap();

    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();

    std::fs::write(
        src.join("lib.rs"),
        r#"pub mod math;
pub mod strings;
"#,
    )
    .unwrap();

    // math.rs: two independent functions, each tested separately, with a
    // visible "structural zone" between them where struct/derive/use edits
    // would land. Line numbers are stable (no comments at the top) so the
    // assertions can reason about ranges.
    std::fs::write(
        src.join("math.rs"),
        r#"pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub struct Counter {
    pub n: i32,
}

pub fn multiply(a: i32, b: i32) -> i32 {
    a * b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add() {
        assert_eq!(add(2, 3), 5);
    }

    #[test]
    fn test_multiply() {
        assert_eq!(multiply(3, 4), 12);
    }
}
"#,
    )
    .unwrap();

    std::fs::write(
        src.join("strings.rs"),
        r#"pub fn greet(name: &str) -> String {
    format!("hello, {name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_greet() {
        assert_eq!(greet("world"), "hello, world");
    }
}
"#,
    )
    .unwrap();
}

/// Edit a file by replacing exactly `from` with `to`. Panics if `from` is
/// not present, so a refactor in the sample project can't silently no-op.
fn replace_in_file(path: &Path, from: &str, to: &str) {
    let content = std::fs::read_to_string(path).unwrap();
    assert!(
        content.contains(from),
        "expected to find {from:?} in {} so the edit lands on the right line",
        path.display()
    );
    let modified = content.replace(from, to);
    std::fs::write(path, modified).unwrap();
}

#[test]
#[ignore]
fn test_full_pipeline() {
    if !llvm_tools_available() {
        eprintln!("SKIP: llvm-tools not installed (rustup component add llvm-tools)");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    write_sample_project(dir);

    git(dir, &["init"]);
    git(dir, &["config", "user.email", "test@test.com"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "initial"]);

    let output = cargo_affected(dir, &["affected", "collect"]);
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
    replace_in_file(&math_path, "a + b", "a + b /* edited */");

    let output = cargo_affected(dir, &["affected", "status", "-v"]);
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

    git(dir, &["checkout", "--", "src/math.rs"]);

    // Adding a derive lands outside any function body; no stored range
    // overlaps, so the file-level backstop must select every test that
    // covered math.rs.
    replace_in_file(
        &math_path,
        "pub struct Counter {",
        "#[derive(Debug, Clone)]\npub struct Counter {",
    );

    let output = cargo_affected(dir, &["affected", "status", "-v"]);
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

    git(dir, &["checkout", "--", "src/math.rs"]);
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(
        dir.join("tests/integration_new.rs"),
        "#[test]\nfn test_brand_new() {\n    assert_eq!(1 + 1, 2);\n}\n",
    )
    .unwrap();

    let output = cargo_affected(dir, &["affected", "status", "-v"]);
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
