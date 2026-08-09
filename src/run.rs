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

use crate::collect::{
    args_for_listing, nextest_filter_expr, require_nextest, write_nextest_config,
};
use crate::db::{warn_untracked_rs_files, Db, TestId};
use crate::fingerprint;
use crate::plan::{self, Assessment, CacheMiss, CacheState, SelectionReport};
use crate::project::{find_project_root, git_changed_files};
use crate::report::{self, CacheStatus};
use crate::selection::{self, DiagnosticDetail};

/// Entry point for `cargo affected run`. Returns the exit code to propagate.
///
/// Runs every test (with an explanatory stderr notice) when the coverage
/// cache offers nothing usable — no coverage yet, fingerprint mismatch, or
/// every stored `collect_sha` unreachable from HEAD. Partial divergence
/// (some shas reachable, some not) proceeds with the reachable subset and
/// surfaces stranded tests as "stranded".
pub(crate) fn run(
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
            plan::write_full_suite_report(
                "run",
                CacheStatus::ForcedAll,
                Some(fingerprint),
                stored,
                vec![],
                path,
            )?;
        }
        eprintln!(
            "{}",
            report::summary_line(CacheStatus::ForcedAll, None, 0, 0)
        );
        return run_tests(project_root, None, nextest_args);
    }

    let fingerprint = fingerprint::compute(&project)?;
    let db = Db::open(project_root)?;
    let assessment = plan::assess(project_root, &db, &fingerprint, report_json.is_some())?;
    let Assessment { stored, state, .. } = assessment;

    // Stranded shas are worth saying whether or not any sha survived, so it
    // goes here rather than inside the two arms that could emit it.
    if let Some(reach) = state.reachability() {
        if !reach.missing.is_empty() {
            eprintln!(
                "{}",
                selection::missing_shas_notice(&reach.missing, "will rerun as 'stranded'")
            );
        }
    }

    let reach = match state {
        CacheState::Usable(reach) => reach,
        // Every miss narrates itself, then they share one exit: report the
        // full suite, print the summary, run everything.
        CacheState::Miss(miss) => {
            let status = miss.status();
            let (collect_shas, missing) = match &miss {
                CacheMiss::NoCoverage => {
                    eprintln!(
                        "note: no coverage data yet — running all tests; \
                         run `cargo affected collect` to enable selection"
                    );
                    (Vec::new(), 0)
                }
                CacheMiss::Fingerprint { differing } => {
                    eprintln!(
                        "note: no coverage data for the current environment{} — \
                         running all tests; run `cargo affected collect` to refresh",
                        report::fingerprint_miss_clause(differing),
                    );
                    (Vec::new(), 0)
                }
                CacheMiss::NoReachableSha(reach) => {
                    eprintln!(
                        "note: no reachable collect_sha for the current environment — \
                         running all tests; run `cargo affected collect` to re-anchor"
                    );
                    let row_counts = db.row_counts_by_sha(&fingerprint.hex)?;
                    (
                        report::collect_sha_snapshots(reach, &row_counts),
                        reach.missing.len(),
                    )
                }
            };
            if let Some(path) = report_json {
                plan::write_full_suite_report(
                    "run",
                    status,
                    Some(fingerprint),
                    stored,
                    collect_shas,
                    path,
                )?;
            }
            eprintln!("{}", report::summary_line(status, None, missing, 0));
            return run_tests(project_root, None, nextest_args);
        }
    };

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
    // List with the same args `run_tests` hands to `nextest run`, so
    // new-test detection compares against the test set the run actually
    // builds and admits — not a feature-less, filter-less one. Run-only
    // flags (`--retries`, `--no-fail-fast`, …) are dropped because `list`
    // rejects them; positional substring filters and `-E` filtersets pass
    // through so each testcase is tagged with the same `filter-match`
    // status `nextest run` will apply.
    let build_args = args_for_listing(nextest_args);
    let plan = plan::plan(
        &project,
        &db,
        &fingerprint.hex,
        &reach,
        &changed_files,
        &build_args,
        detail,
    )?;
    let sel = &plan.selection;

    if let Some(path) = report_json {
        plan::write_selection_report(
            SelectionReport {
                command: "run",
                project: &project,
                db: &db,
                fingerprint: &fingerprint,
                stored,
                reach: &reach,
                plan: &plan,
                changed_files: &changed_files,
            },
            path,
        )?;
    }
    eprintln!(
        "{}",
        report::summary_line(
            plan.status,
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

    eprintln!("\n{}\n", selection::format_summary(sel, "to run", verbose));

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

/// Run tests via `cargo nextest run`. `tests == None` runs all tests;
/// `Some(tests)` filters to the given set by handing nextest a generated
/// config file whose `default-filter` names exactly those tests (see
/// [`write_nextest_config`]). Returns nextest's exit code so callers can
/// propagate it to CI.
///
/// The filterset lives in a file rather than an inline `-E` argument so an
/// arbitrarily large affected set can't overflow the OS command-line limit
/// — Windows' ~32 KB `CreateProcess` cap raised `os error 206` here.
///
/// `nextest_args` reach nextest verbatim — this is deliberate for the
/// failure-handling flags (`--no-fail-fast`, `--max-fail=N`, `--retries`):
/// nextest's own semantics govern when the run stops, and cargo-affected
/// adds no fail-fast policy of its own. The functional suite's
/// `run_forwards_fail_fast_flags_to_nextest` anchors that contract.
fn run_tests(
    project_root: &Path,
    tests: Option<&[TestId]>,
    nextest_args: &[String],
) -> Result<i32> {
    let mut cmd = Command::new("cargo");
    cmd.arg("nextest").arg("run");
    let filter_config = match tests {
        Some(ts) => {
            eprintln!("running {} tests with nextest", ts.len());
            let config = write_nextest_config(project_root, &nextest_filter_expr(ts))?;
            cmd.arg("--config-file").arg(&config);
            Some(config)
        }
        None => {
            eprintln!("running all tests with nextest");
            None
        }
    };
    for a in nextest_args {
        cmd.arg(a);
    }
    let status = cmd
        .current_dir(project_root)
        .status()
        .context("failed to run cargo nextest")?;
    if let Some(config) = &filter_config {
        // Best-effort cleanup; a stale file in gitignored target/ is harmless.
        let _ = std::fs::remove_file(config);
    }
    Ok(status.code().unwrap_or(1))
}
