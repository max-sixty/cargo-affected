//! Status reporting: show stored coverage data and what would run for current changes.
//!
//! Mirrors `run`'s contract: reports "would run all tests" with an
//! explanation when the cache offers nothing usable (no data, fingerprint
//! mismatch, or every stored `collect_sha` unreachable). Partial divergence
//! proceeds with the reachable subset, same as `run`.
//!
//! Like `run`, can write a structured JSON diagnostic report via
//! `--report-json <PATH>`. Unlike `run`, never invokes nextest — only
//! lists tests via `nextest list` and never executes them.

use std::path::Path;

use anyhow::Result;

use crate::collect::{nextest_list, require_nextest};
use crate::db::{db_path, warn_untracked_rs_files, Db, StoredFingerprintRow};
use crate::fingerprint::{self, Fingerprint};
use crate::project::{find_project_root, git_changed_files, ShaRelation};
use crate::report::{self, CacheStatus, FullSuiteInputs, Report, SelectionInputs};
use crate::selection::{self, DiagnosticDetail};

/// Entry point for `cargo affected status`.
///
/// Reports "would run all tests" (with an explanation) when the cache
/// offers nothing usable — no coverage, fingerprint mismatch, or every
/// stored `collect_sha` unreachable. Partial divergence proceeds with the
/// reachable subset and surfaces stranded tests, mirroring `run` so the
/// dry-run accurately predicts what `run` would do.
pub fn status(
    verbose: bool,
    report_json: Option<&Path>,
    detail: DiagnosticDetail,
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
            write_full_suite(
                CacheStatus::MissNoCoverage,
                Some(fingerprint),
                vec![],
                vec![],
                report_path,
            )?;
        }
        eprintln!("{}", report::summary_line(CacheStatus::MissNoCoverage, None, 0, 0));
        return Ok(());
    }

    let fingerprint = fingerprint::compute(&project)?;
    let db = Db::open(project_root)?;
    let stored = if report_json.is_some() {
        db.stored_fingerprint_snapshots()?
    } else {
        Vec::new()
    };

    let known_count = db.test_count(&fingerprint.hex)?;
    let region_count = db.region_count(&fingerprint.hex)?;
    let last_collected = db.last_collected()?.unwrap_or_else(|| "never".to_string());
    let collect_shas = db.collect_shas(&fingerprint.hex)?;

    let rel_path = path.strip_prefix(project_root).unwrap_or(&path);

    if known_count == 0 {
        // Fetch stored snapshots on demand for the diff line if --report-json
        // didn't already force the read; cheap relative to the rest of status.
        let stored = if stored.is_empty() {
            db.stored_fingerprint_snapshots()?
        } else {
            stored
        };
        let (status, reason) = if !stored.is_empty() {
            let snapshots = report::snapshots_from(stored.clone());
            let differing =
                report::closest_stored_diff_labels(&fingerprint.components, &snapshots);
            (
                CacheStatus::MissFingerprint,
                format!(
                    "no coverage data for the current environment{}",
                    report::fingerprint_miss_clause(&differing),
                ),
            )
        } else {
            (CacheStatus::MissNoCoverage, "no coverage data yet".to_string())
        };
        println!(
            "coverage database: {}\n\
             last collected: {last_collected}\n\
             {reason} — would run all tests; run `cargo affected collect` to enable selection",
            rel_path.display(),
        );
        if let Some(report_path) = report_json {
            write_full_suite(status, Some(fingerprint), stored, vec![], report_path)?;
        }
        eprintln!("{}", report::summary_line(status, None, 0, 0));
        return Ok(());
    }

    println!(
        "coverage database: {}\n\
         last collected: {last_collected}\n\
         tests tracked: {known_count}\n\
         regions stored: {region_count}",
        rel_path.display(),
    );
    let mut sha_list: Vec<&str> = collect_shas.iter().map(String::as_str).collect();
    sha_list.sort();
    println!("collect shas: {}", sha_list.join(", "));

    let reach = selection::check_shas_reachable(project_root, &collect_shas)?;
    if !reach.missing.is_empty() {
        let stale_rows = db.region_count_at_shas(&fingerprint.hex, &reach.missing)?;
        println!(
            "\n{}\nstale rows: {stale_rows} (anchored at missing sha{})",
            selection::missing_shas_notice(&reach.missing, "would rerun as 'stranded'"),
            if reach.missing.len() == 1 { "" } else { "s" },
        );
    }
    if reach.reachable.is_empty() {
        println!(
            "\nnote: no reachable collect_sha for the current environment — \
             would run all tests; run `cargo affected collect` to re-anchor"
        );
        if let Some(report_path) = report_json {
            let row_counts = db.row_counts_by_sha(&fingerprint.hex)?;
            let collect_sha_snapshots = report::collect_sha_snapshots(&reach, &row_counts);
            write_full_suite(
                CacheStatus::MissNoReachableSha,
                Some(fingerprint),
                stored,
                collect_sha_snapshots,
                report_path,
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
        return Ok(());
    }
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
    let listing = nextest_list(project_root, None, None)?;
    let changed_ranges = selection::changed_ranges_per_sha(project_root, &reach.reachable)?;
    let sel = selection::select_with_precomputed_ranges(
        &db,
        &fingerprint.hex,
        &listing,
        &reach,
        &changed_ranges,
        detail,
    )?;

    let status = if reach.per_sha.values().all(|r| matches!(r, ShaRelation::Equal)) {
        CacheStatus::HitExact
    } else {
        CacheStatus::HitWithDivergence
    };

    // Per-changed-file diagnostics + per-sha snapshots are report-only:
    // they cost extra git diffs and SQLite queries that status itself
    // doesn't need.
    if let Some(report_path) = report_json {
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
            command: "status",
            current_fingerprint: fingerprint.hex.clone(),
            current_components: fingerprint.components.clone(),
            stored_fingerprints: report::snapshots_from(stored),
            collect_shas: collect_sha_snapshots,
            status,
            selection: &sel,
            changed_files: changed_files_input,
            include_changed_files: true,
        };
        Report::build_selection(inputs).write_json(report_path)?;
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

    if sel.selected().is_empty() {
        if changed_files.is_empty() {
            println!("\nno uncommitted changes and no new tests — nothing would run");
        } else {
            println!("\nno tests cover the changed lines and no new tests");
        }
        return Ok(());
    }

    println!("\n{}", selection::format_summary(&sel, "would run", verbose));

    Ok(())
}

/// Build and write a full-suite report from the given fingerprint and
/// stored snapshots. Mirrors run.rs's helper of the same name; both
/// callers compose the inputs identically.
fn write_full_suite(
    status: CacheStatus,
    fingerprint: Option<Fingerprint>,
    stored: Vec<StoredFingerprintRow>,
    collect_shas: Vec<crate::report::CollectShaSnapshot>,
    path: &Path,
) -> Result<()> {
    let inputs = FullSuiteInputs {
        command: "status",
        current_fingerprint: fingerprint.as_ref().map(|f| f.hex.clone()),
        current_components: fingerprint.map(|f| f.components),
        stored_fingerprints: report::snapshots_from(stored),
        collect_shas,
        status,
    };
    Report::build_full_suite(inputs).write_json(path)
}
