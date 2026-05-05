//! Test selection and execution based on git changes.
//!
//! Queries git for changed line ranges anchored at each stored `collect_sha`,
//! looks up which tests have function-range overlap in the database, and
//! runs the affected tests via `cargo nextest run`. Also lists tests via
//! nextest to catch tests added since the last `collect` — those have no
//! coverage data, so they're always selected. nextest is required — no
//! `cargo test` fallback.
//!
//! Order of operations:
//!
//! 1. Compute fingerprint and components.
//! 2. Open DB; classify cache state into a `CacheStatus` value.
//! 3. For selection-mode states (`hit-exact`, `hit-with-divergence`):
//!    list tests, compute selection, write the JSON report, emit the
//!    summary line, then invoke `nextest run` with the selection
//!    filter.
//! 4. For full-suite states (`forced-all`, `miss-*`): skip listing,
//!    write a partial report (counts null), emit the summary line,
//!    then invoke `nextest run` with no filter.
//!
//! The report writes BEFORE nextest so the artifact survives test
//! failures.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::collect::{nextest_filter_exprs, nextest_list, require_nextest};
use crate::db::{warn_untracked_rs_files, Db, TestId};
use crate::fingerprint::{self, Fingerprint};
use crate::project::{find_project_root, git_changed_files, ShaRelation};
use crate::report::{
    self, CacheStatus, FullSuiteInputs, Report, SelectionInputs,
};
use crate::selection::{self, DiagnosticDetail, Reachability};

/// Entry point for `cargo affected run`. Returns the exit code to propagate.
///
/// Runs every test (with an explanatory stderr notice) when the coverage
/// cache offers nothing usable — no coverage yet, fingerprint mismatch, or
/// every stored `collect_sha` unreachable from HEAD. Partial divergence
/// (some shas reachable, some not) proceeds with the reachable subset and
/// surfaces stranded tests as "stranded".
pub fn run(
    all: bool,
    verbose: bool,
    report_json: Option<&Path>,
    detail: DiagnosticDetail,
    nextest_args: &[String],
) -> Result<i32> {
    let project = find_project_root()?;
    let project_root = &project.workspace_root;
    require_nextest(project_root)?;

    if all {
        eprintln!("running all tests (--all)");
        // Skip the DB open + fingerprint compute entirely when no report
        // was requested — `--all` is the user explicitly bypassing
        // cache-aware selection, so an unrelated cache lock or schema
        // reset shouldn't be able to fail or mutate state on this path.
        if let Some(path) = report_json {
            let fingerprint = fingerprint::compute(&project)?;
            let stored = match open_db_if_present(project_root)? {
                Some(d) => d.stored_fingerprint_snapshots()?,
                None => vec![],
            };
            write_full_suite(
                "run",
                CacheStatus::ForcedAll,
                Some(fingerprint),
                stored,
                vec![],
                path,
            )?;
        }
        eprintln!("{}", report::summary_line(CacheStatus::ForcedAll, None, 0, 0));
        return run_tests(project_root, None, nextest_args);
    }

    let fingerprint = fingerprint::compute(&project)?;
    let db = Db::open(project_root)?;
    let stored = if report_json.is_some() {
        db.stored_fingerprint_snapshots()?
    } else {
        // Stored snapshots are diagnostic-only — skip the read when no
        // report is requested.
        Vec::new()
    };

    if db.test_count(&fingerprint.hex)? == 0 {
        let status = if db.has_any_coverage()? {
            eprintln!(
                "note: no coverage data for the current environment \
                 (Cargo.lock, Cargo.toml, rustc version, or build flags changed) — \
                 running all tests; run `cargo affected collect` to refresh"
            );
            CacheStatus::MissFingerprint
        } else {
            eprintln!(
                "note: no coverage data yet — running all tests; \
                 run `cargo affected collect` to enable selection"
            );
            CacheStatus::MissNoCoverage
        };
        if let Some(path) = report_json {
            write_full_suite("run", status, Some(fingerprint.clone()), stored, vec![], path)?;
        }
        eprintln!("{}", report::summary_line(status, None, 0, 0));
        return run_tests(project_root, None, nextest_args);
    }

    let collect_shas = db.collect_shas(&fingerprint.hex)?;
    let reach = selection::check_shas_reachable(project_root, &collect_shas)?;
    if !reach.missing.is_empty() {
        eprintln!(
            "{}",
            selection::missing_shas_notice(&reach.missing, "will rerun as 'stranded'")
        );
    }
    if reach.reachable.is_empty() {
        eprintln!(
            "note: no reachable collect_sha for the current environment — \
             running all tests; run `cargo affected collect` to re-anchor"
        );
        if let Some(path) = report_json {
            let row_counts = db.row_counts_by_sha(&fingerprint.hex)?;
            let collect_sha_snapshots = report::collect_sha_snapshots(&reach, &row_counts);
            write_full_suite(
                "run",
                CacheStatus::MissNoReachableSha,
                Some(fingerprint.clone()),
                stored,
                collect_sha_snapshots,
                path,
            )?;
        }
        eprintln!(
            "{}",
            report::summary_line(
                CacheStatus::MissNoReachableSha,
                None,
                reach.missing.len(),
                0,
            )
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
    warn_untracked_rs_files(&db, &fingerprint.hex, &changed_files)?;
    if !changed_files.is_empty() {
        eprintln!("{} changed files:", changed_files.len());
        for f in &changed_files {
            eprintln!("  {f}");
        }
    }

    eprintln!("checking for new tests...");
    let listing = nextest_list(project_root, None, None)?;
    // Compute per-sha hunks once; selection consumes them and so does
    // the report builder if --report-json is set. The previous code
    // ran `git diff -U0 <sha>` twice per reachable sha on the report
    // path.
    let changed_ranges = selection::changed_ranges_per_sha(project_root, &reach.reachable)?;
    let sel = selection::select_with_precomputed_ranges(
        &db,
        &fingerprint.hex,
        &listing,
        &reach,
        &changed_ranges,
        detail,
    )?;

    let status = classify_hit_status(&reach);

    // Per-changed-file diagnostics + per-sha snapshots are report-only:
    // they cost extra git diffs and SQLite queries that selection itself
    // doesn't need. Compute them only when --report-json is set.
    if let Some(path) = report_json {
        let row_counts = db.row_counts_by_sha(&fingerprint.hex)?;
        let collect_sha_snapshots = report::collect_sha_snapshots(&reach, &row_counts);
        let changed_files_input = report::build_changed_file_inputs(
            project_root,
            &db,
            &fingerprint.hex,
            &reach,
            &changed_ranges,
            &changed_files,
        )?;
        let inputs = SelectionInputs {
            command: "run",
            current_fingerprint: fingerprint.hex.clone(),
            current_components: fingerprint.components.clone(),
            stored_fingerprints: report::snapshots_from(stored),
            collect_shas: collect_sha_snapshots,
            status,
            selection: &sel,
            changed_files: changed_files_input,
            include_changed_files: true,
        };
        Report::build_selection(inputs).write_json(path)?;
    }
    eprintln!(
        "{}",
        report::summary_line(
            status,
            Some((sel.selected().len(), sel.reachable_known_count)),
            reach.missing.len(),
            reach.max_commits_ahead,
        )
    );

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

/// `--all` doesn't require an existing DB; gracefully skip the open if
/// none exists rather than erroring on the user's first run.
fn open_db_if_present(project_root: &Path) -> Result<Option<Db>> {
    let path = crate::db::db_path(project_root);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(Db::open(project_root)?))
}

/// True iff any reachable sha differs from HEAD. Drives the choice
/// between `hit-exact` and `hit-with-divergence`. status.rs uses the
/// same predicate. `Reachable { commits_ahead: 0 }` (sibling of HEAD
/// after `git reset --hard` to its parent) counts as divergence —
/// the sha resolves but its tree is different from HEAD's.
fn any_divergence(reach: &Reachability) -> bool {
    reach
        .per_sha
        .values()
        .any(|r| !matches!(r, ShaRelation::Equal))
}

fn classify_hit_status(reach: &Reachability) -> CacheStatus {
    if any_divergence(reach) {
        CacheStatus::HitWithDivergence
    } else {
        CacheStatus::HitExact
    }
}

/// Build and write a full-suite report. The Fingerprint is `Option`
/// because the `--all` path before any DB exists may not have one to
/// supply (currently it always does, but keep the optional shape so
/// future fast paths can stay cheap).
fn write_full_suite(
    command: &'static str,
    status: CacheStatus,
    fingerprint: Option<Fingerprint>,
    stored: Vec<crate::db::StoredFingerprintRow>,
    collect_shas: Vec<crate::report::CollectShaSnapshot>,
    path: &Path,
) -> Result<()> {
    let inputs = FullSuiteInputs {
        command,
        current_fingerprint: fingerprint.as_ref().map(|f| f.hex.clone()),
        current_components: fingerprint.map(|f| f.components),
        stored_fingerprints: report::snapshots_from(stored),
        collect_shas,
        status,
    };
    Report::build_full_suite(inputs).write_json(path)
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

