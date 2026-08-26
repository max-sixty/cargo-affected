//! Status reporting: show stored coverage data and what would run for current changes.
//!
//! This is the conditional-tense rendering of [`crate::plan`]; `run` is the
//! present-tense one. Both reach the selection through the same call, so
//! "would run all tests" here and "running all tests" there answer to one
//! decision rather than two implementations that have to be kept agreeing.
//!
//! What's genuinely this command's own: a database inventory on stdout (path,
//! last collect, tracked tests, stored regions, collect shas) and the fact
//! that it stops at `nextest list` and never executes anything.

use std::path::Path;

use anyhow::Result;

use crate::collect::{cargo_build_args, require_nextest};
use crate::db::{db_path, warn_untracked_rs_files, Db};
use crate::fingerprint;
use crate::plan::{self, Assessment, CacheMiss, CacheState, SelectionReport};
use crate::project::{find_project_root, git_changed_files};
use crate::report::{self, CacheStatus};
use crate::selection::{self, DiagnosticDetail};

/// Entry point for `cargo affected status`.
///
/// Reports "would run all tests" (with an explanation) when the cache
/// offers nothing usable — no coverage, fingerprint mismatch, or every
/// stored `collect_sha` unreachable. Partial divergence proceeds with the
/// reachable subset and surfaces stranded tests.
///
/// The prediction comes from [`crate::plan::plan`], the same call `run`
/// makes, so "what would run" can't drift from what runs. `nextest_args`
/// therefore matters here as much as it does for `run`: it decides which
/// tests get built, and a dry run listing a feature-less build would
/// under-report against a `run -- --features …`.
pub(crate) fn status(
    verbose: bool,
    report_json: Option<&Path>,
    detail: DiagnosticDetail,
    nextest_args: &[String],
) -> Result<()> {
    let project = find_project_root()?;
    let project_root = &project.workspace_root;

    let path = db_path(project_root);
    if !path.exists() {
        println!(
            "no coverage data found — would run all tests; \
             run `cargo affected collect` to enable selection"
        );
        if let Some(report_path) = report_json {
            // No DB at all → still emit a partial report naming
            // MissNoCoverage so consumers get a parseable artifact.
            let fingerprint = fingerprint::compute(&project)?;
            plan::write_full_suite_report(
                "status",
                CacheStatus::MissNoCoverage,
                Some(fingerprint),
                vec![],
                vec![],
                report_path,
            )?;
        }
        eprintln!(
            "{}",
            report::summary_line(CacheStatus::MissNoCoverage, None, 0, 0)
        );
        return Ok(());
    }

    let fingerprint = fingerprint::compute(&project)?;
    let db = Db::open(project_root)?;
    let Assessment {
        test_count,
        collect_shas,
        stored,
        state,
    } = plan::assess(project_root, &db, &fingerprint, report_json.is_some())?;

    let last_collected = db.last_collected()?.unwrap_or_else(|| "never".to_string());
    let rel_path = path.strip_prefix(project_root).unwrap_or(&path);

    // The inventory heads the output either way; what follows it depends on
    // whether any of it turned out to be usable. Nothing tracked means there
    // are no regions or shas to count, so those queries don't run.
    if test_count == 0 {
        println!(
            "coverage database: {}\nlast collected: {last_collected}",
            rel_path.display(),
        );
    } else {
        println!(
            "coverage database: {}\n\
             last collected: {last_collected}\n\
             tests tracked: {test_count}\n\
             regions stored: {}",
            rel_path.display(),
            db.region_count(&fingerprint.hex)?,
        );
        let mut sha_list: Vec<&str> = collect_shas.iter().map(String::as_str).collect();
        sha_list.sort();
        println!("collect shas: {}", sha_list.join(", "));
    }

    if let Some(reach) = state.reachability() {
        if !reach.missing.is_empty() {
            let stale_rows = db.region_count_at_shas(&fingerprint.hex, &reach.missing)?;
            println!(
                "\n{}\nstale rows: {stale_rows} (anchored at missing sha{})",
                selection::missing_shas_notice(&reach.missing, "would rerun as 'stranded'"),
                if reach.missing.len() == 1 { "" } else { "s" },
            );
        }
    }

    let reach = match state {
        CacheState::Usable(reach) => reach,
        CacheState::Miss(miss) => {
            let status = miss.status();
            let (collect_sha_snapshots, missing) = match &miss {
                CacheMiss::NoCoverage => {
                    println!(
                        "no coverage data yet — would run all tests; \
                         run `cargo affected collect` to enable selection"
                    );
                    (Vec::new(), 0)
                }
                CacheMiss::Fingerprint { differing } => {
                    println!(
                        "no coverage data for the current environment{} — would run \
                         all tests; run `cargo affected collect` to enable selection",
                        report::fingerprint_miss_clause(differing),
                    );
                    (Vec::new(), 0)
                }
                CacheMiss::NoReachableSha(reach) => {
                    println!(
                        "\nnote: no reachable collect_sha for the current environment — \
                         would run all tests; run `cargo affected collect` to re-anchor"
                    );
                    let row_counts = db.row_counts_by_sha(&fingerprint.hex)?;
                    (
                        report::collect_sha_snapshots(reach, &row_counts),
                        reach.missing.len(),
                    )
                }
            };
            if let Some(report_path) = report_json {
                plan::write_full_suite_report(
                    "status",
                    status,
                    Some(fingerprint),
                    stored,
                    collect_sha_snapshots,
                    report_path,
                )?;
            }
            eprintln!("{}", report::summary_line(status, None, missing, 0));
            return Ok(());
        }
    };

    if reach.max_commits_ahead > 0 {
        println!(
            "\nnote: {} commit(s) since collect — \
             diff vs collect_sha is noisier than necessary; \
             run `cargo affected collect` to refresh",
            reach.max_commits_ahead,
        );
    }

    let changed_files = git_changed_files(project_root)?;
    warn_untracked_rs_files(&db, &fingerprint.hex, &changed_files)?;

    if !changed_files.is_empty() {
        println!("\nchanged files ({}):", changed_files.len());
        for f in &changed_files {
            println!("  {f}");
        }
    }

    require_nextest(project_root)?;
    eprintln!("checking for new tests...");
    let plan = plan::plan(
        &project,
        &db,
        &fingerprint.hex,
        &reach,
        &changed_files,
        &cargo_build_args(nextest_args),
        detail,
    )?;
    let sel = &plan.selection;

    if let Some(report_path) = report_json {
        plan::write_selection_report(
            SelectionReport {
                command: "status",
                db: &db,
                fingerprint: &fingerprint,
                stored,
                reach: &reach,
                plan: &plan,
            },
            report_path,
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

    if sel.selected().is_empty() {
        // See the matching arm in `run` — `changed_files` is the working tree
        // alone, which would call a committed-but-uncollected change "no
        // changes" one line under the "N commit(s) since collect" notice.
        if plan.changed_paths.is_empty() {
            println!("\nno changes since the last collect and no new tests — nothing would run");
        } else {
            println!("\nno tests cover the changed lines and no new tests");
        }
        return Ok(());
    }

    println!("\n{}", selection::format_summary(sel, "would run", verbose));

    Ok(())
}
