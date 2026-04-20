//! Test selection and execution based on git changes.
//!
//! Queries git for changed files, looks up which tests cover those files in the
//! database, and runs the affected tests via `cargo nextest run` (or `cargo test`
//! as fallback).

use std::path::Path;
use std::process::{Command, ExitStatus};

use anyhow::{Context, Result};

use crate::db::{warn_untracked_rs_files, Db};
use crate::fingerprint;
use crate::project::{find_project_root, git_changed_files};

/// Entry point for `cargo difftest run`. Returns the exit code to propagate.
pub fn run(diff_base: Option<&str>, all: bool, verbose: bool) -> Result<i32> {
    let project = find_project_root()?;
    let project_root = &project.workspace_root;

    if all {
        eprintln!("running all tests (--all)");
        return run_tests(project_root, None);
    }

    let changed_files = git_changed_files(project_root, diff_base)?;
    if changed_files.is_empty() {
        eprintln!("no changed files detected");
        return Ok(0);
    }

    let env_fingerprint = fingerprint::compute(&project)?;
    let db = Db::open(project_root)?;

    let all_tests = db.test_count(&env_fingerprint)?;
    if all_tests == 0 {
        if db.has_any_coverage()? {
            eprintln!(
                "coverage database has no data for the current environment \
                 (Cargo.lock, Cargo.toml, rustc version, or build flags changed) — running all tests"
            );
            return run_tests(project_root, None);
        }
        eprintln!("no coverage data yet — run `cargo difftest collect` first");
        return Ok(0);
    }

    warn_untracked_rs_files(&db, &env_fingerprint, &changed_files)?;

    eprintln!("{} changed files:", changed_files.len());
    for f in &changed_files {
        eprintln!("  {f}");
    }

    let file_refs: Vec<&str> = changed_files.iter().map(|s| s.as_str()).collect();
    let tests = db.tests_covering(&env_fingerprint, &file_refs)?;

    if tests.is_empty() {
        eprintln!("no tests cover the changed files (run `cargo difftest collect` to update)");
        return Ok(0);
    }

    let skipped = all_tests.saturating_sub(tests.len());
    if verbose {
        eprintln!(
            "\n{} tests to run ({skipped} skipped of {all_tests} total):",
            tests.len()
        );
        for t in &tests {
            eprintln!("  {t}");
        }
    } else {
        eprintln!(
            "\n{} tests to run ({skipped} skipped of {all_tests} total) — pass -v to list",
            tests.len()
        );
    }
    eprintln!();

    let tests: Vec<String> = tests.into_iter().collect();
    run_tests(project_root, Some(&tests))
}

/// Run tests. `test_names == None` runs all tests; `Some(names)` filters to the given set.
///
/// Tries `cargo nextest run` first, falls back to `cargo test`. Returns the
/// exit code of the test runner so callers can propagate it to CI.
fn run_tests(project_root: &Path, test_names: Option<&[String]>) -> Result<i32> {
    if has_nextest(project_root) {
        let mut cmd = Command::new("cargo");
        cmd.arg("nextest").arg("run");
        match test_names {
            Some(names) => {
                let filter_expr = names
                    .iter()
                    .map(|t| format!("test(={t})"))
                    .collect::<Vec<_>>()
                    .join(" | ");
                eprintln!("running {} tests with nextest", names.len());
                cmd.arg("-E").arg(&filter_expr);
            }
            None => eprintln!("running all tests with nextest"),
        }
        let status = cmd
            .current_dir(project_root)
            .status()
            .context("failed to run cargo nextest")?;
        return Ok(exit_code(&status));
    }

    // Fallback: cargo test. For a filtered set, run each individually with --exact
    // because cargo test only accepts one filter (and interprets it as regex).
    // Runs every test even after failures so the user sees the full picture;
    // the final exit code is the worst seen.
    match test_names {
        Some(names) => {
            eprintln!("running {} tests with cargo test --exact", names.len());
            let mut worst = 0;
            for name in names {
                let status = Command::new("cargo")
                    .arg("test")
                    .arg("--")
                    .arg("--exact")
                    .arg(name)
                    .current_dir(project_root)
                    .status()
                    .context("failed to run cargo test")?;
                worst = worst.max(exit_code(&status));
            }
            Ok(worst)
        }
        None => {
            eprintln!("running all tests with cargo test");
            let status = Command::new("cargo")
                .arg("test")
                .current_dir(project_root)
                .status()
                .context("failed to run cargo test")?;
            Ok(exit_code(&status))
        }
    }
}

/// Extract the exit code from a process status. Signal kills surface as 1.
fn exit_code(status: &ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

/// Check if `cargo nextest` is available.
fn has_nextest(project_root: &Path) -> bool {
    Command::new("cargo")
        .arg("nextest")
        .arg("--version")
        .current_dir(project_root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
