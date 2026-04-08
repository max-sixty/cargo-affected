//! Test selection and execution based on git changes.
//!
//! Queries git for changed files, looks up which tests cover those files in the
//! database, and runs the affected tests via `cargo nextest run` (or `cargo test`
//! as fallback).

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::db::Db;
use crate::project::{find_project_root, git_changed_files};

/// Entry point for `cargo difftest run`.
pub fn run(diff_base: Option<&str>, all: bool) -> Result<()> {
    let project = find_project_root()?;
    let project_root = &project.workspace_root;

    if all {
        eprintln!("running all tests (--all)");
        return run_all_tests(project_root);
    }

    let db = Db::open(project_root)?;

    let changed_files = git_changed_files(project_root, diff_base)?;

    warn_untracked_rs_files(&db, &changed_files)?;

    if changed_files.is_empty() {
        eprintln!("no changed files detected");
        return Ok(());
    }

    eprintln!("{} changed files:", changed_files.len());
    for f in &changed_files {
        eprintln!("  {f}");
    }

    let file_refs: Vec<&str> = changed_files.iter().map(|s| s.as_str()).collect();
    let all_tests = db.test_count()?;
    let tests = db.tests_covering(&file_refs)?;

    if tests.is_empty() {
        eprintln!("no tests cover the changed files (run `cargo difftest collect` to update)");
        return Ok(());
    }

    let skipped = all_tests.saturating_sub(tests.len());
    eprintln!(
        "\n{} tests to run ({skipped} skipped out of {all_tests} total):",
        tests.len()
    );
    for t in &tests {
        eprintln!("  {t}");
    }
    eprintln!();

    run_tests(project_root, &tests.into_iter().collect::<Vec<_>>())
}

/// Warn about changed `.rs` files that have no coverage data at all.
fn warn_untracked_rs_files(db: &Db, changed_files: &[String]) -> Result<()> {
    for file in changed_files {
        if file.ends_with(".rs") && !db.file_tracked(file)? {
            eprintln!(
                "warning: {file} has no coverage data \
                 — run `cargo difftest collect` to include it"
            );
        }
    }
    Ok(())
}

/// Run all tests without coverage-based selection.
fn run_all_tests(project_root: &Path) -> Result<()> {
    if has_nextest(project_root) {
        eprintln!("running all tests with nextest");
        let status = Command::new("cargo")
            .arg("nextest")
            .arg("run")
            .current_dir(project_root)
            .status()
            .context("failed to run cargo nextest")?;
        if !status.success() {
            bail!("some tests failed");
        }
        return Ok(());
    }

    eprintln!("running all tests with cargo test");
    let status = Command::new("cargo")
        .arg("test")
        .current_dir(project_root)
        .status()
        .context("failed to run cargo test")?;
    if !status.success() {
        bail!("some tests failed");
    }
    Ok(())
}

/// Run the specified tests.
///
/// Tries `cargo nextest run` first, falls back to `cargo test`.
fn run_tests(project_root: &Path, test_names: &[String]) -> Result<()> {
    if has_nextest(project_root) {
        let filter_expr = test_names
            .iter()
            .map(|t| format!("test(={t})"))
            .collect::<Vec<_>>()
            .join(" | ");

        eprintln!("running with nextest: -E '{filter_expr}'");

        let status = Command::new("cargo")
            .arg("nextest")
            .arg("run")
            .arg("-E")
            .arg(&filter_expr)
            .current_dir(project_root)
            .status()
            .context("failed to run cargo nextest")?;

        if !status.success() {
            bail!("some tests failed");
        }
        return Ok(());
    }

    // Fallback: cargo test with --exact to avoid regex interpretation of test names.
    // Runs each test individually since cargo test only accepts a single filter.
    eprintln!("running {} tests with cargo test --exact", test_names.len());

    for name in test_names {
        let status = Command::new("cargo")
            .arg("test")
            .arg("--")
            .arg("--exact")
            .arg(name)
            .current_dir(project_root)
            .status()
            .context("failed to run cargo test")?;

        if !status.success() {
            bail!("test {name} failed");
        }
    }
    Ok(())
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
