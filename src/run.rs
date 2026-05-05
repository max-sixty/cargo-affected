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
use crate::project::{find_project_root, git_changed_files, git_changed_line_ranges};
use crate::report::{
    CacheStatus, ChangedFileInput, CollectShaSnapshot, FullSuiteInputs, Report,
    SelectionInputs, StoredFingerprintSnapshot,
};
use crate::selection::{self, DiagnosticDetail, Reachability, Selection};

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

    let fingerprint = fingerprint::compute(&project)?;

    if all {
        eprintln!("running all tests (--all)");
        let db_opt = open_db_if_present(project_root)?;
        emit_full_suite(
            "run",
            CacheStatus::ForcedAll,
            &fingerprint,
            db_opt.as_ref(),
            report_json,
        )?;
        return run_tests(project_root, None, nextest_args);
    }

    let db = Db::open(project_root)?;
    let stored = db.stored_fingerprint_snapshots()?;
    let row_counts = db.row_counts_by_sha(&fingerprint.hex)?;

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
        write_full_suite_report(
            "run",
            status,
            Some(fingerprint.hex.clone()),
            Some(fingerprint.components.clone()),
            stored,
            vec![],
            report_json,
        )?;
        emit_summary_line(status, None, None, None);
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
        let collect_sha_snapshots =
            collect_sha_snapshots_from(&reach, &row_counts);
        write_full_suite_report(
            "run",
            CacheStatus::MissNoReachableSha,
            Some(fingerprint.hex.clone()),
            Some(fingerprint.components.clone()),
            stored,
            collect_sha_snapshots,
            report_json,
        )?;
        emit_summary_line(
            CacheStatus::MissNoReachableSha,
            None,
            Some(reach.missing.len()),
            None,
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
    let sel = selection::select_with_reach(
        project_root,
        &db,
        &fingerprint.hex,
        &listing,
        &reach,
        detail,
    )?;

    let collect_sha_snapshots = collect_sha_snapshots_from(&reach, &row_counts);
    let changed_files_input = build_changed_file_inputs(
        project_root,
        &db,
        &fingerprint.hex,
        &reach,
        &changed_files,
    )?;
    let status = classify_hit_status(&reach);

    write_selection_report(
        "run",
        status,
        &fingerprint,
        stored,
        collect_sha_snapshots,
        &sel,
        changed_files_input,
        report_json,
    )?;
    emit_summary_line(
        status,
        Some((sel.selected().len(), sel.reachable_known_count)),
        Some(reach.missing.len()),
        Some(reach.max_commits_ahead),
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

/// Compose per-sha snapshots for the report from a `Reachability`
/// (per-sha relation) and a sha → row-count map.
fn collect_sha_snapshots_from(
    reach: &Reachability,
    row_counts: &std::collections::BTreeMap<String, usize>,
) -> Vec<CollectShaSnapshot> {
    reach
        .per_sha
        .iter()
        .map(|(sha, relation)| CollectShaSnapshot {
            sha: sha.clone(),
            relation: relation.clone(),
            row_count: row_counts.get(sha).copied().unwrap_or(0),
        })
        .collect()
}

/// Build per-changed-file input entries: for each file, compute hunks
/// against every reachable sha and decide whether the file is tracked
/// by coverage (has at least one stored row at any reachable sha).
fn build_changed_file_inputs(
    project_root: &Path,
    db: &Db,
    fingerprint: &str,
    reach: &Reachability,
    changed_files: &[String],
) -> Result<Vec<ChangedFileInput>> {
    use std::collections::BTreeMap;

    // Cache hunks per sha so we don't re-diff for each file.
    let mut hunks_per_sha: BTreeMap<String, BTreeMap<String, Vec<crate::project::LineRange>>> =
        BTreeMap::new();
    for sha in &reach.reachable {
        let map = git_changed_line_ranges(project_root, sha)?;
        hunks_per_sha.insert(sha.clone(), map);
    }

    let mut out = Vec::new();
    for path in changed_files {
        let mut hunks_by_sha: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();
        for (sha, by_file) in &hunks_per_sha {
            if let Some(hunks) = by_file.get(path) {
                hunks_by_sha.insert(
                    sha.clone(),
                    hunks.iter().map(|h| (h.start, h.end)).collect(),
                );
            }
        }
        let mut tracked = false;
        for sha in &reach.reachable {
            let n = db.tests_covering_ranges(
                fingerprint,
                sha,
                path,
                &[crate::project::LineRange { start: 1, end: i64::MAX }],
            )?;
            if !n.is_empty() {
                tracked = true;
                break;
            }
        }
        out.push(ChangedFileInput {
            path: path.clone(),
            tracked_by_coverage: tracked,
            hunks_by_sha,
        });
    }
    Ok(out)
}

fn classify_hit_status(reach: &Reachability) -> CacheStatus {
    let any_divergence = !reach.missing.is_empty()
        || reach
            .per_sha
            .values()
            .any(|r| matches!(r, crate::project::ShaRelation::Reachable { commits_ahead } if *commits_ahead > 0));
    if any_divergence {
        CacheStatus::HitWithDivergence
    } else {
        CacheStatus::HitExact
    }
}

#[allow(clippy::too_many_arguments)]
fn write_selection_report(
    command: &'static str,
    status: CacheStatus,
    fingerprint: &Fingerprint,
    stored: Vec<crate::db::StoredFingerprintRow>,
    collect_shas: Vec<CollectShaSnapshot>,
    selection: &Selection,
    changed_files: Vec<ChangedFileInput>,
    report_json: Option<&Path>,
) -> Result<()> {
    let Some(path) = report_json else {
        return Ok(());
    };
    let inputs = SelectionInputs {
        command,
        current_fingerprint: fingerprint.hex.clone(),
        current_components: fingerprint.components.clone(),
        stored_fingerprints: to_snapshots(stored),
        collect_shas,
        status,
        selection,
        changed_files,
        include_changed_files: true,
    };
    Report::build_selection(inputs).write_json(path)
}

#[allow(clippy::too_many_arguments)]
fn write_full_suite_report(
    command: &'static str,
    status: CacheStatus,
    current_fingerprint: Option<String>,
    current_components: Option<Vec<crate::fingerprint::FingerprintComponent>>,
    stored: Vec<crate::db::StoredFingerprintRow>,
    collect_shas: Vec<CollectShaSnapshot>,
    report_json: Option<&Path>,
) -> Result<()> {
    let Some(path) = report_json else {
        return Ok(());
    };
    let inputs = FullSuiteInputs {
        command,
        current_fingerprint,
        current_components,
        stored_fingerprints: to_snapshots(stored),
        collect_shas,
        status,
    };
    Report::build_full_suite(inputs).write_json(path)
}

fn to_snapshots(rows: Vec<crate::db::StoredFingerprintRow>) -> Vec<StoredFingerprintSnapshot> {
    rows.into_iter()
        .map(|r| StoredFingerprintSnapshot {
            fingerprint: r.fingerprint,
            last_seen: r.last_seen,
            components: r.components,
        })
        .collect()
}

/// Helper for the `--all` path which doesn't open the DB unless one
/// exists. Builds the report from whatever's available (empty stored /
/// collect arrays are fine).
fn emit_full_suite(
    command: &'static str,
    status: CacheStatus,
    fingerprint: &Fingerprint,
    db: Option<&Db>,
    report_json: Option<&Path>,
) -> Result<()> {
    let stored = match db {
        Some(d) => d.stored_fingerprint_snapshots()?,
        None => vec![],
    };
    write_full_suite_report(
        command,
        status,
        Some(fingerprint.hex.clone()),
        Some(fingerprint.components.clone()),
        stored,
        vec![],
        report_json,
    )?;
    emit_summary_line(status, None, None, None);
    Ok(())
}

/// Final stderr line a CI consumer can grep to track selection ratios
/// over time. Format depends on cache status; per the design doc, miss
/// statuses get `mode=full-suite` and the closest-component label as
/// `reason=`; hit statuses get the selection ratio.
fn emit_summary_line(
    status: CacheStatus,
    selection: Option<(usize, usize)>,
    missing_shas: Option<usize>,
    max_commits_ahead: Option<u32>,
) {
    let status_str = match status {
        CacheStatus::HitExact => "hit-exact",
        CacheStatus::HitWithDivergence => "hit-with-divergence",
        CacheStatus::MissFingerprint => "miss-fingerprint",
        CacheStatus::MissNoCoverage => "miss-no-coverage",
        CacheStatus::MissNoReachableSha => "miss-no-reachable-sha",
        CacheStatus::ForcedAll => "forced-all",
    };
    match status {
        CacheStatus::HitExact | CacheStatus::HitWithDivergence => {
            let (selected, total) = selection.unwrap_or((0, 0));
            // Empty cache → vacuous 100% (we ran nothing of nothing).
            let pct = (selected * 100).checked_div(total).unwrap_or(100);
            let mut line = format!(
                "cargo-affected: cache={status_str} selection={selected}/{total} ({pct}%)"
            );
            if matches!(status, CacheStatus::HitWithDivergence) {
                if let Some(m) = missing_shas {
                    if m > 0 {
                        line.push_str(&format!(" missing_shas={m}"));
                    }
                }
                if let Some(c) = max_commits_ahead {
                    if c > 0 {
                        line.push_str(&format!(" max_commits_ahead={c}"));
                    }
                }
            }
            eprintln!("{line}");
        }
        CacheStatus::MissNoReachableSha => {
            let m = missing_shas.unwrap_or(0);
            eprintln!(
                "cargo-affected: cache={status_str} mode=full-suite missing_shas={m}"
            );
        }
        _ => {
            eprintln!("cargo-affected: cache={status_str} mode=full-suite");
        }
    }
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

