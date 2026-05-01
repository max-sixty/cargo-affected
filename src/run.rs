//! Test selection and execution based on git changes.
//!
//! Queries git for changed line ranges anchored at each stored `collect_sha`,
//! looks up which tests have function-range overlap in the database, and
//! runs the affected tests via `cargo nextest run`. Also lists tests via
//! nextest to catch tests added since the last `collect` — those have no
//! coverage data, so they're always selected. nextest is required — no
//! `cargo test` fallback.
//!
//! `collect --diff` lets a single fingerprint accumulate rows from several
//! distinct collect points; we enumerate those shas and compute changed-line
//! ranges per sha so each row's hunk overlap is computed in matching
//! coordinates.
//!
//! Reachability is per-sha: diverged shas are skipped and tests stranded
//! only at them surface as "new" via selection. Widening to all tests
//! happens only when the cache offers nothing usable — no coverage yet,
//! fingerprint mismatch, or every stored sha unreachable from HEAD. That
//! makes `cargo affected run` a strict superset of `cargo nextest run` —
//! always at least as safe.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::collect::{nextest_filter_exprs, nextest_list, require_nextest};
use crate::db::{warn_untracked_rs_files, Db, TestId};
use crate::fingerprint;
use crate::project::{find_project_root, git_changed_files};
use crate::selection;

/// Entry point for `cargo affected run`. Returns the exit code to propagate.
///
/// Runs every test (with an explanatory stderr notice) when the coverage
/// cache offers nothing usable — no coverage yet, fingerprint mismatch, or
/// every stored `collect_sha` unreachable from HEAD. Partial divergence
/// (some shas reachable, some not) proceeds with the reachable subset and
/// surfaces stranded tests as "new".
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

    let collect_shas = db.collect_shas(&env_fingerprint)?;
    let reach = selection::check_shas_reachable(project_root, &collect_shas)?;
    if !reach.diverged.is_empty() {
        eprintln!(
            "{}",
            selection::diverged_shas_notice(&reach.diverged, "will rerun as 'new'")
        );
    }
    if reach.reachable.is_empty() {
        // Every stored sha is unreachable — there's nothing to query against.
        eprintln!(
            "note: no reachable collect_sha for the current environment — \
             running all tests; run `cargo affected collect` to re-anchor"
        );
        return run_tests(project_root, None, nextest_args);
    }
    if reach.max_commits_ahead > 0 {
        eprintln!(
            "note: {} commit(s) since collect — \
             diff vs collect_sha is noisier than necessary; \
             run `cargo affected collect` to refresh",
            reach.max_commits_ahead,
        );
    }

    let changed_files = git_changed_files(project_root)?;
    warn_untracked_rs_files(&db, &env_fingerprint, &changed_files)?;
    if !changed_files.is_empty() {
        eprintln!("{} changed files:", changed_files.len());
        for f in &changed_files {
            eprintln!("  {f}");
        }
    }

    eprintln!("checking for new tests...");
    let listing = nextest_list(project_root, None, None)?;
    let sel = selection::select_with_reach(
        project_root,
        &db,
        &env_fingerprint,
        &listing,
        &reach,
    )?;
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
/// `Some(tests)` filters to the given set via one or more nextest `-E`
/// filterset arguments (split to stay under the OS argv-string limit).
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
            eprintln!("running {} tests with nextest", ts.len());
            for expr in nextest_filter_exprs(ts) {
                cmd.arg("-E").arg(expr);
            }
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
