//! Test selection and execution based on git changes.
//!
//! Queries git for changed line ranges (vs. `collect_sha`), looks up which
//! tests have function-range overlap in the database, and runs the affected
//! tests via `cargo nextest run`. Also lists tests via nextest to catch tests
//! added since the last `collect` — those have no coverage data, so they're
//! always selected. nextest is required — no `cargo test` fallback.
//!
//! When selection can't be computed precisely (no coverage yet, fingerprint
//! changed, missing `collect_sha`, or `collect_sha` not reachable from HEAD),
//! `run` falls back to running every test with an explanatory stderr notice.
//! That makes `cargo affected run` a strict superset of `cargo nextest run` —
//! always at least as safe.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::collect::{nextest_filter_expr, require_nextest};
use crate::db::{warn_untracked_rs_files, Db, TestId};
use crate::fingerprint;
use crate::project::{
    find_project_root, git_changed_files, git_changed_line_ranges, relation_to_head, ShaRelation,
};
use crate::selection;

/// Entry point for `cargo affected run`. Returns the exit code to propagate.
///
/// Falls back to running all tests (with an explanatory stderr notice) in
/// every case where a precise affected-test selection can't be computed —
/// no coverage data, fingerprint mismatch, missing `collect_sha`, or
/// `collect_sha` not reachable from HEAD.
pub fn run(all: bool, verbose: bool, nextest_args: &[String]) -> Result<i32> {
    let project = find_project_root()?;
    let project_root = &project.workspace_root;
    require_nextest(project_root)?;

    if all {
        eprintln!("running all tests (--all)");
        return run_tests(project_root, None, nextest_args);
    }

    let env_fingerprint = fingerprint::compute(&project)?;
    let db = Db::open(project_root)?;

    if db.test_count(&env_fingerprint)? == 0 {
        if db.has_any_coverage()? {
            eprintln!(
                "note: no coverage data for the current environment \
                 (Cargo.lock, Cargo.toml, rustc version, or build flags changed) — \
                 running all tests; run `cargo affected collect` to refresh"
            );
        } else {
            eprintln!(
                "note: no coverage data yet — running all tests; \
                 run `cargo affected collect` to enable selection"
            );
        }
        return run_tests(project_root, None, nextest_args);
    }

    let Some(collect_sha) = db.collect_sha(&env_fingerprint)? else {
        eprintln!(
            "note: coverage data exists but is missing collect_sha — running all tests; \
             run `cargo affected collect` to re-anchor"
        );
        return run_tests(project_root, None, nextest_args);
    };

    match relation_to_head(project_root, &collect_sha)? {
        ShaRelation::Equal => {}
        ShaRelation::Ancestor { commits_ahead } => {
            eprintln!(
                "note: {commits_ahead} commit(s) since collect — \
                 diff vs collect_sha is noisier than necessary; \
                 run `cargo affected collect` to refresh"
            );
        }
        ShaRelation::Diverged => {
            eprintln!(
                "note: collect_sha {collect_sha} not reachable from HEAD \
                 (rebased or branch switched) — running all tests; \
                 run `cargo affected collect` to re-anchor"
            );
            return run_tests(project_root, None, nextest_args);
        }
    }

    let changed_files = git_changed_files(project_root)?;
    warn_untracked_rs_files(&db, &env_fingerprint, &changed_files)?;
    if !changed_files.is_empty() {
        eprintln!("{} changed files:", changed_files.len());
        for f in &changed_files {
            eprintln!("  {f}");
        }
    }

    let changed_ranges = git_changed_line_ranges(project_root, &collect_sha)?;
    let sel = selection::compute(project_root, &db, &env_fingerprint, &changed_ranges)?;
    let selected = sel.selected();
    if selected.is_empty() {
        if changed_files.is_empty() {
            eprintln!("no uncommitted changes and no new tests — nothing to run");
        } else {
            eprintln!(
                "no tests cover the changed lines and no new tests \
                 (run `cargo affected collect` to update)"
            );
        }
        return Ok(0);
    }

    eprintln!("\n{}\n", selection::format_summary(&sel, "to run", verbose));

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
