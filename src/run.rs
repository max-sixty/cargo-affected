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

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::collect::{nextest_filter_expr, nextest_list, require_nextest};
use crate::db::{warn_untracked_rs_files, Db, TestId};
use crate::fingerprint;
use crate::project::{
    find_project_root, git_changed_files, git_changed_line_ranges, relation_to_head, ShaRelation,
};
use crate::selection::{self, ChangedRangesBySha};

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
    let reach = check_shas_reachable(project_root, &collect_shas)?;
    if !reach.diverged.is_empty() {
        eprintln!("{}", diverged_shas_notice(&reach.diverged, "will rerun as 'new'"));
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

    let changed_ranges_by_sha = changed_ranges_per_sha(project_root, &reach.reachable)?;
    eprintln!("checking for new tests...");
    let listing = nextest_list(project_root, None, None)?;
    let sel = selection::compute(
        &db,
        &env_fingerprint,
        &reach.reachable,
        &changed_ranges_by_sha,
        &listing,
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

/// Per-sha reachability for the stored `collect_sha`s under one fingerprint.
///
/// Splits cleanly so callers can proceed with the reachable subset rather
/// than treating any divergence as all-or-nothing — important under `collect
/// --diff`, where rows from several shas coexist for one fingerprint and a
/// single rebase shouldn't invalidate unrelated tests' rows. Tests anchored
/// at diverged shas remain in the DB; queries skip them, and selection
/// surfaces them as "new tests" so they get rerun (and re-anchored, in
/// `collect --diff`'s case). Old rows accumulate as bloat — clear with
/// `cargo affected clean`.
pub(crate) struct Reachability {
    /// Stored shas reachable from HEAD (Equal or Ancestor).
    pub(crate) reachable: BTreeSet<String>,
    /// Stored shas no longer reachable (rebased away, garbage-collected,
    /// beyond shallow boundary). Their rows are still in the DB but won't
    /// be queried by `run`/`status`/`collect --diff`.
    pub(crate) diverged: BTreeSet<String>,
    /// Largest commits-ahead distance among reachable shas; 0 when every
    /// reachable sha equals HEAD.
    pub(crate) max_commits_ahead: u32,
}

/// Format the partial-divergence notice shared by `run`, `status`, and
/// `collect --diff`. `verb_phrase` slots into "tests anchored only there
/// VERB_PHRASE" — "will rerun as 'new'" for `run`/`collect --diff`, "would
/// rerun as 'new'" for `status`. Returns the body without a trailing
/// newline so callers can `eprintln!`/`println!` it directly.
pub(crate) fn diverged_shas_notice(diverged: &BTreeSet<String>, verb_phrase: &str) -> String {
    let plural = if diverged.len() == 1 { "" } else { "s" };
    let list = diverged.iter().cloned().collect::<Vec<_>>().join(", ");
    format!(
        "note: {} collect_sha{plural} not reachable from HEAD ({list}) — \
         tests anchored only there {verb_phrase}; \
         run `cargo affected clean` to clear stale rows",
        diverged.len(),
    )
}

/// Classify each `collect_sha` in `shas` against HEAD. See [`Reachability`].
pub(crate) fn check_shas_reachable(
    project_root: &Path,
    shas: &BTreeSet<String>,
) -> Result<Reachability> {
    let mut reachable = BTreeSet::new();
    let mut diverged = BTreeSet::new();
    let mut max_commits_ahead = 0u32;
    for sha in shas {
        match relation_to_head(project_root, sha)? {
            ShaRelation::Equal => {
                reachable.insert(sha.clone());
            }
            ShaRelation::Ancestor { commits_ahead } => {
                reachable.insert(sha.clone());
                max_commits_ahead = max_commits_ahead.max(commits_ahead);
            }
            ShaRelation::Diverged => {
                diverged.insert(sha.clone());
            }
        }
    }
    Ok(Reachability {
        reachable,
        diverged,
        max_commits_ahead,
    })
}

/// Build the per-sha changed-ranges map: one `git diff -U0 <sha>` per
/// distinct stored `collect_sha`. With a single sha (the common case) this
/// is one diff invocation.
pub(crate) fn changed_ranges_per_sha(
    project_root: &Path,
    shas: &BTreeSet<String>,
) -> Result<ChangedRangesBySha> {
    let mut out = BTreeMap::new();
    for sha in shas {
        let ranges = git_changed_line_ranges(project_root, sha)?;
        out.insert(sha.clone(), ranges);
    }
    Ok(out)
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
