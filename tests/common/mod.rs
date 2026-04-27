//! Shared helpers for integration tests.
//!
//! Pulled in via `mod common;` from each test file under `tests/`.

use std::path::Path;
use std::process::Command;

/// Run cargo-affected binary with the given args in the given directory.
pub fn cargo_affected(dir: &Path, args: &[&str]) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_cargo-affected");
    Command::new(bin)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run cargo-affected: {e}"))
}

/// Run a git command in the given directory, panicking on failure.
pub fn git(dir: &Path, args: &[&str]) {
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
pub fn llvm_tools_available() -> bool {
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
pub fn write_sample_project(dir: &Path) {
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
pub fn replace_in_file(path: &Path, from: &str, to: &str) {
    let content = std::fs::read_to_string(path).unwrap();
    assert!(
        content.contains(from),
        "expected to find {from:?} in {} so the edit lands on the right line",
        path.display()
    );
    let modified = content.replace(from, to);
    std::fs::write(path, modified).unwrap();
}
