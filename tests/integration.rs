//! Integration test for the full cargo-difftest pipeline.
//!
//! Creates a temp Rust project, initializes git, runs collect, then verifies
//! that status correctly identifies affected tests after a source change.
//!
//! Marked `#[ignore]` because coverage builds are slow (~30s).
//! Run with: `cargo test -- --ignored`

use std::path::Path;
use std::process::Command;

/// Run cargo-difftest binary with the given args in the given directory.
fn cargo_difftest(dir: &Path, args: &[&str]) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_cargo-difftest");
    Command::new(bin)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run cargo-difftest: {e}"))
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

    // Get the host target triple.
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

    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();

    std::fs::write(
        src.join("lib.rs"),
        r#"pub mod math;
pub mod strings;
"#,
    )
    .unwrap();

    std::fs::write(
        src.join("math.rs"),
        r#"pub fn add(a: i32, b: i32) -> i32 {
    a + b
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

#[test]
#[ignore]
fn test_full_pipeline() {
    if !llvm_tools_available() {
        eprintln!("SKIP: llvm-tools not installed (rustup component add llvm-tools)");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // Set up the sample project.
    write_sample_project(dir);

    // Initialize git and commit everything.
    git(dir, &["init"]);
    git(dir, &["config", "user.email", "test@test.com"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "initial"]);

    // Run collect.
    let output = cargo_difftest(dir, &["difftest", "collect"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "collect failed: {stderr}\nstdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("test mappings"),
        "expected 'test mappings' in collect output, got: {stderr}"
    );

    // Verify the DB was created.
    let db_path = dir.join("target").join("difftest").join("coverage.db");
    assert!(
        db_path.exists(),
        "target/difftest/coverage.db should exist after collect"
    );

    // Read the DB and verify mappings.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let test_count: i64 = conn
        .query_row("SELECT COUNT(DISTINCT test_name) FROM test_files", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(test_count, 3, "expected 3 tests in DB");

    // Verify that math tests map to math.rs and strings test maps to strings.rs.
    let math_tests: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT DISTINCT test_name FROM test_files WHERE source_file LIKE '%math.rs'")
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

    let string_tests: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT test_name FROM test_files WHERE source_file LIKE '%strings.rs'",
            )
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    assert!(
        string_tests.iter().any(|t| t.contains("test_greet")),
        "expected test_greet to cover strings.rs, got: {string_tests:?}"
    );

    // Now modify math.rs (add a comment to change git status).
    let math_path = dir.join("src/math.rs");
    let content = std::fs::read_to_string(&math_path).unwrap();
    std::fs::write(&math_path, format!("{content}\n// changed\n")).unwrap();

    // Run status and verify only math tests are listed.
    let output = cargo_difftest(dir, &["difftest", "status"]);
    assert!(
        output.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("test_add"),
        "status should list test_add, got:\n{stdout}"
    );
    assert!(
        stdout.contains("test_multiply"),
        "status should list test_multiply, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("test_greet"),
        "status should NOT list test_greet (strings.rs unchanged), got:\n{stdout}"
    );

    // Verify the skip count is reported.
    assert!(
        stdout.contains("1 skipped"),
        "status should report 1 skipped test, got:\n{stdout}"
    );
}
