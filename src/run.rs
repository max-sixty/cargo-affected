//! Test selection and execution based on git changes.
//!
//! Queries git for changed files, looks up which tests cover those files in
//! the database, and runs the affected tests via `cargo nextest run`. nextest
//! is required — no `cargo test` fallback.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::collect::require_nextest;
use crate::db::{warn_untracked_rs_files, Db};
use crate::fingerprint;
use crate::project::{find_project_root, git_changed_files};

/// Entry point for `cargo difftest run`. Returns the exit code to propagate.
pub fn run(diff_base: Option<&str>, all: bool, verbose: bool) -> Result<i32> {
    let project = find_project_root()?;
    let project_root = &project.workspace_root;
    require_nextest(project_root)?;

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

/// Run tests via `cargo nextest run`. `test_names == None` runs all tests;
/// `Some(names)` filters to the given set via a nextest `-E` expression.
/// Returns nextest's exit code so callers can propagate it to CI.
fn run_tests(project_root: &Path, test_names: Option<&[String]>) -> Result<i32> {
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
    Ok(status.code().unwrap_or(1))
}
