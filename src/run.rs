//! Test selection and execution based on git changes.
//!
//! Queries git for changed files, looks up which tests cover those files in
//! the database, and runs the affected tests via `cargo nextest run`. Also
//! lists tests via nextest to catch tests added since the last `collect` —
//! those have no coverage data yet, so they're always selected.
//! nextest is required — no `cargo test` fallback.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::collect::{nextest_filter_expr, nextest_list, require_nextest};
use crate::db::{warn_untracked_rs_files, Db, TestId};
use crate::fingerprint;
use crate::project::{find_project_root, git_changed_files};

/// Entry point for `cargo affected run`. Returns the exit code to propagate.
pub fn run(
    diff_base: Option<&str>,
    all: bool,
    verbose: bool,
    nextest_args: &[String],
) -> Result<i32> {
    let project = find_project_root()?;
    let project_root = &project.workspace_root;
    require_nextest(project_root)?;

    if all {
        eprintln!("running all tests (--all)");
        return run_tests(project_root, None, nextest_args);
    }

    let changed_files = git_changed_files(project_root, diff_base)?;

    let env_fingerprint = fingerprint::compute(&project)?;
    let db = Db::open(project_root)?;

    let known_count = db.test_count(&env_fingerprint)?;
    if known_count == 0 {
        if db.has_any_coverage()? {
            eprintln!(
                "coverage database has no data for the current environment \
                 (Cargo.lock, Cargo.toml, rustc version, or build flags changed) — running all tests"
            );
            return run_tests(project_root, None, nextest_args);
        }
        eprintln!("no coverage data yet — run `cargo affected collect` first");
        return Ok(0);
    }

    warn_untracked_rs_files(&db, &env_fingerprint, &changed_files)?;

    // List tests first so we notice any added since the last collect. This
    // step primes the build cache for the subsequent nextest run.
    eprintln!("checking for new tests...");
    let listing = nextest_list(project_root, None, None)?;
    let known_tests = db.all_tests(&env_fingerprint)?;
    let new_tests: BTreeSet<TestId> = listing
        .tests
        .iter()
        .filter(|t| !known_tests.contains(*t))
        .cloned()
        .collect();

    let affected = if changed_files.is_empty() {
        BTreeSet::new()
    } else {
        eprintln!("{} changed files:", changed_files.len());
        for f in &changed_files {
            eprintln!("  {f}");
        }
        let file_refs: Vec<&str> = changed_files.iter().map(|s| s.as_str()).collect();
        db.tests_covering(&env_fingerprint, &file_refs)?
    };

    let selected: BTreeSet<TestId> = affected.union(&new_tests).cloned().collect();
    if selected.is_empty() {
        match (changed_files.is_empty(), diff_base) {
            (true, None) => eprintln!("no uncommitted changes and no new tests — nothing to run"),
            (true, Some(base)) => eprintln!("no changes vs {base} and no new tests"),
            (false, _) => eprintln!(
                "no tests cover the changed files and no new tests \
                 (run `cargo affected collect` to update)"
            ),
        }
        return Ok(0);
    }

    let skipped = known_count.saturating_sub(affected.len());
    if verbose {
        eprintln!(
            "\n{} tests to run ({} affected + {} new, {skipped} skipped of {known_count} known):",
            selected.len(),
            affected.len(),
            new_tests.len(),
        );
        for t in &selected {
            let tag = if new_tests.contains(t) { " (new)" } else { "" };
            eprintln!("  {}::{}{tag}", t.binary_id, t.test_name);
        }
    } else {
        eprintln!(
            "\n{} tests to run ({} affected + {} new, {skipped} skipped of {known_count} known) \
             — pass -v to list",
            selected.len(),
            affected.len(),
            new_tests.len(),
        );
    }
    eprintln!();

    let tests: Vec<TestId> = selected.into_iter().collect();
    run_tests(project_root, Some(&tests), nextest_args)
}

/// Run tests via `cargo nextest run`. `tests == None` runs all tests;
/// `Some(tests)` filters to the given set via a nextest `-E` expression of
/// the form `(binary(=X) & (test(=a) | test(=b))) | ...`.
/// Returns nextest's exit code so callers can propagate it to CI.
fn run_tests(
    project_root: &Path,
    tests: Option<&[TestId]>,
    nextest_args: &[String],
) -> Result<i32> {
    let mut cmd = Command::new("cargo");
    cmd.arg("nextest").arg("run");
    match tests {
        Some(ts) => {
            let filter_expr = nextest_filter_expr(ts);
            eprintln!("running {} tests with nextest", ts.len());
            cmd.arg("-E").arg(&filter_expr);
        }
        None => eprintln!("running all tests with nextest"),
    }
    for a in nextest_args {
        cmd.arg(a);
    }
    let status = cmd
        .current_dir(project_root)
        .status()
        .context("failed to run cargo nextest")?;
    Ok(status.code().unwrap_or(1))
}
