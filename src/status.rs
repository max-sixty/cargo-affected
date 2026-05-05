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
use crate::db::{db_path, warn_untracked_rs_files, Db};
use crate::fingerprint::{self, Fingerprint};
use crate::project::{find_project_root, git_changed_files};
use crate::report::{
    CacheStatus, FullSuiteInputs, Report, SelectionInputs, StoredFingerprintSnapshot,
};
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
            // No DB at all → no fingerprint we can compute against the cache.
            // Still emit a partial report naming MissNoCoverage so consumers
            // get a parseable artifact.
            let fingerprint = fingerprint::compute(&project)?;
            write_full_suite_report(
                "status",
                CacheStatus::MissNoCoverage,
                Some(fingerprint.hex.clone()),
                Some(fingerprint.components.clone()),
                vec![],
                vec![],
                report_path,
            )?;
        }
        eprintln!("cargo-affected: cache=miss-no-coverage mode=full-suite");
        return Ok(());
    }

    let fingerprint = fingerprint::compute(&project)?;
    let db = Db::open(project_root)?;
    // Stored snapshots and per-sha row counts are diagnostic-only;
    // skip the reads when no report was requested.
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
        let (status, reason) = if db.has_any_coverage()? {
            (
                CacheStatus::MissFingerprint,
                "no coverage data for the current environment \
                 (Cargo.lock, Cargo.toml, rustc version, or build flags changed since last collect)",
            )
        } else {
            (CacheStatus::MissNoCoverage, "no coverage data yet")
        };
        println!(
            "coverage database: {}\n\
             last collected: {last_collected}\n\
             {reason} — would run all tests; run `cargo affected collect` to enable selection",
            rel_path.display(),
        );
        if let Some(report_path) = report_json {
            write_full_suite_report(
                "status",
                status,
                Some(fingerprint.hex.clone()),
                Some(fingerprint.components.clone()),
                stored,
                vec![],
                report_path,
            )?;
        }
        eprintln!(
            "cargo-affected: cache={} mode=full-suite",
            cache_status_str(status)
        );
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
            let collect_sha_snapshots = collect_sha_snapshots_from_reach(&reach, &row_counts);
            write_full_suite_report(
                "status",
                CacheStatus::MissNoReachableSha,
                Some(fingerprint.hex.clone()),
                Some(fingerprint.components.clone()),
                stored,
                collect_sha_snapshots,
                report_path,
            )?;
        }
        eprintln!(
            "cargo-affected: cache=miss-no-reachable-sha mode=full-suite missing_shas={}",
            reach.missing.len()
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
    let sel = selection::select_with_reach(
        project_root,
        &db,
        &fingerprint.hex,
        &listing,
        &reach,
        detail,
    )?;

    let status = if reach.missing.is_empty()
        && reach
            .per_sha
            .values()
            .all(|r| matches!(r, crate::project::ShaRelation::Equal))
    {
        CacheStatus::HitExact
    } else {
        CacheStatus::HitWithDivergence
    };

    // Per-changed-file diagnostics + per-sha snapshots are report-only:
    // they cost extra git diffs and SQLite queries that status itself
    // doesn't need.
    if let Some(report_path) = report_json {
        let row_counts = db.row_counts_by_sha(&fingerprint.hex)?;
        let collect_sha_snapshots = collect_sha_snapshots_from_reach(&reach, &row_counts);
        let changed_files_input = build_changed_file_inputs(
            project_root,
            &db,
            &fingerprint.hex,
            &reach,
            &changed_files,
        )?;
        let inputs = SelectionInputs {
            command: "status",
            current_fingerprint: fingerprint.hex.clone(),
            current_components: fingerprint.components.clone(),
            stored_fingerprints: to_snapshots(stored),
            collect_shas: collect_sha_snapshots,
            status,
            selection: &sel,
            changed_files: changed_files_input,
            include_changed_files: true,
        };
        Report::build_selection(inputs).write_json(report_path)?;
    }

    let selected_count = sel.selected().len();
    let total = sel.reachable_known_count;
    let pct = (selected_count * 100).checked_div(total).unwrap_or(100);
    let mut summary = format!(
        "cargo-affected: cache={} selection={selected_count}/{total} ({pct}%)",
        cache_status_str(status)
    );
    if matches!(status, CacheStatus::HitWithDivergence) {
        if !reach.missing.is_empty() {
            summary.push_str(&format!(" missing_shas={}", reach.missing.len()));
        }
        if reach.max_commits_ahead > 0 {
            summary.push_str(&format!(" max_commits_ahead={}", reach.max_commits_ahead));
        }
    }
    eprintln!("{summary}");

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

fn collect_sha_snapshots_from_reach(
    reach: &selection::Reachability,
    row_counts: &std::collections::BTreeMap<String, usize>,
) -> Vec<crate::report::CollectShaSnapshot> {
    reach
        .per_sha
        .iter()
        .map(|(sha, relation)| crate::report::CollectShaSnapshot {
            sha: sha.clone(),
            relation: relation.clone(),
            row_count: row_counts.get(sha).copied().unwrap_or(0),
        })
        .collect()
}

/// Mirrors `run.rs::build_changed_file_inputs` — the file set is the
/// UNION of files in any per-sha diff plus working-tree changes, so
/// the report doesn't drop committed-diff files that selection
/// actually consumed.
fn build_changed_file_inputs(
    project_root: &Path,
    db: &Db,
    fingerprint: &str,
    reach: &selection::Reachability,
    working_tree_files: &[String],
) -> Result<Vec<crate::report::ChangedFileInput>> {
    use std::collections::BTreeMap;
    let mut hunks_per_sha: BTreeMap<String, BTreeMap<String, Vec<crate::project::LineRange>>> =
        BTreeMap::new();
    for sha in &reach.reachable {
        let map = crate::project::git_changed_line_ranges(project_root, sha)?;
        hunks_per_sha.insert(sha.clone(), map);
    }

    let mut all_files: std::collections::BTreeSet<String> =
        working_tree_files.iter().cloned().collect();
    for by_file in hunks_per_sha.values() {
        for path in by_file.keys() {
            all_files.insert(path.clone());
        }
    }
    for sha in &reach.reachable {
        for added in crate::project::git_added_files_since(project_root, sha)? {
            all_files.insert(added);
        }
    }

    let mut out = Vec::new();
    for path in all_files {
        let mut hunks_by_sha: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();
        for (sha, by_file) in &hunks_per_sha {
            if let Some(hunks) = by_file.get(&path) {
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
                &path,
                &[crate::project::LineRange { start: 1, end: i64::MAX }],
            )?;
            if !n.is_empty() {
                tracked = true;
                break;
            }
        }
        out.push(crate::report::ChangedFileInput {
            path,
            tracked_by_coverage: tracked,
            hunks_by_sha,
        });
    }
    Ok(out)
}

fn cache_status_str(status: CacheStatus) -> &'static str {
    match status {
        CacheStatus::HitExact => "hit-exact",
        CacheStatus::HitWithDivergence => "hit-with-divergence",
        CacheStatus::MissFingerprint => "miss-fingerprint",
        CacheStatus::MissNoCoverage => "miss-no-coverage",
        CacheStatus::MissNoReachableSha => "miss-no-reachable-sha",
        CacheStatus::ForcedAll => "forced-all",
    }
}

#[allow(clippy::too_many_arguments)]
fn write_full_suite_report(
    command: &'static str,
    status: CacheStatus,
    current_fingerprint: Option<String>,
    current_components: Option<Vec<crate::fingerprint::FingerprintComponent>>,
    stored: Vec<crate::db::StoredFingerprintRow>,
    collect_shas: Vec<crate::report::CollectShaSnapshot>,
    path: &Path,
) -> Result<()> {
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

// Reference Fingerprint via path use; the type is alive through
// fingerprint::compute().
const _: Option<Fingerprint> = None;
