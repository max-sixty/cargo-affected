//! The half of `run` and `status` that must not diverge.
//!
//! `status` is a dry run of `run`: its whole value is that what it prints is
//! what `run` would do. That promise used to be kept by hand — the two
//! commands carried their own copies of the listing/diff/config/selection
//! pipeline and their own report assembly, each annotated with a comment
//! saying it mirrored the other. It had already drifted: `run` listed tests
//! with the caller's build flags (`cargo_build_args(nextest_args)`) while
//! `status` listed with none, so on any project with feature-gated tests the
//! dry run predicted a different test set than the real one, silently and in
//! the safe-looking direction (fewer tests listed → fewer reported).
//!
//! So the pipeline lives here once and both commands call it. What genuinely
//! differs between them stays at the call sites, because it is real: `run`
//! narrates to stderr in the present tense and ends by executing nextest,
//! `status` prints a database inventory to stdout in the conditional. Those
//! are two renderings of one decision, not two decisions.
//!
//! [`plan`] is the decision. [`write_selection_report`] and
//! [`write_full_suite_report`] are the two report shapes, which differ only in
//! whether a selection was computed at all.

use std::path::Path;

use anyhow::Result;

use crate::collect::nextest_list;
use crate::config;
use crate::db::{Db, StoredFingerprintRow};
use crate::fingerprint::Fingerprint;
use crate::project::{ProjectRoot, ShaRelation};
use crate::report::{
    self, CacheStatus, CollectShaSnapshot, FullSuiteInputs, Report, SelectionInputs,
};
use crate::selection::{self, ChangedRangesBySha, DiagnosticDetail, Reachability, Selection};

/// A computed selection, plus the two by-products the report needs.
///
/// `changed_ranges` is carried rather than recomputed: building the report
/// re-derives per-file hunks, and running `git diff -U0 <sha>` a second time
/// per reachable sha is the exact waste this struct exists to avoid.
pub(crate) struct Plan {
    pub(crate) selection: Selection,
    pub(crate) status: CacheStatus,
    pub(crate) changed_ranges: ChangedRangesBySha,
}

/// List tests, diff against every reachable `collect_sha`, apply
/// `[workspace.metadata.affected]` rules, and select.
///
/// `build_args` must be the same flags the caller will hand `nextest run`
/// (via `cargo_build_args`), so new-test detection compares against the test
/// set that will actually be built rather than a feature-less one.
pub(crate) fn plan(
    project: &ProjectRoot,
    db: &Db,
    fingerprint_hex: &str,
    reach: &Reachability,
    changed_files: &[String],
    build_args: &[String],
    detail: DiagnosticDetail,
) -> Result<Plan> {
    let project_root = &project.workspace_root;
    let listing = nextest_list(project_root, None, None, build_args, None)?;
    let changed_ranges = selection::changed_ranges_per_sha(project_root, &reach.reachable)?;
    let config_hits =
        config::config_rule_hits(project, build_args, reach, &changed_ranges, changed_files)?;
    let selection = selection::select_with_precomputed_ranges(
        db,
        fingerprint_hex,
        &listing,
        reach,
        &changed_ranges,
        &config_hits,
        detail,
    )?;
    Ok(Plan {
        selection,
        status: classify_hit_status(reach),
        changed_ranges,
    })
}

/// `hit-exact` only when every reachable sha *is* HEAD.
///
/// `Reachable { commits_ahead: 0 }` — a sibling of HEAD, as after
/// `git reset --hard` to its parent — counts as divergence: the sha resolves,
/// but its tree differs from HEAD's, so stored line numbers are in a
/// different coordinate system.
fn classify_hit_status(reach: &Reachability) -> CacheStatus {
    if reach
        .per_sha
        .values()
        .all(|r| matches!(r, ShaRelation::Equal))
    {
        CacheStatus::HitExact
    } else {
        CacheStatus::HitWithDivergence
    }
}

/// Everything [`write_selection_report`] needs, bundled so the call site
/// doesn't thread nine arguments — the same shape [`SelectionInputs`] uses
/// one layer down.
pub(crate) struct SelectionReport<'a> {
    pub(crate) command: &'static str,
    pub(crate) project: &'a ProjectRoot,
    pub(crate) db: &'a Db,
    pub(crate) fingerprint: &'a Fingerprint,
    pub(crate) stored: Vec<StoredFingerprintRow>,
    pub(crate) reach: &'a Reachability,
    pub(crate) plan: &'a Plan,
    pub(crate) changed_files: &'a [String],
}

/// Write the full report for a computed selection.
///
/// Per-changed-file diagnostics and per-sha row counts are report-only — they
/// cost extra git diffs and SQLite queries that selection itself doesn't need
/// — so this is called only when `--report-json` was passed.
pub(crate) fn write_selection_report(inputs: SelectionReport, path: &Path) -> Result<()> {
    let SelectionReport {
        command,
        project,
        db,
        fingerprint,
        stored,
        reach,
        plan,
        changed_files,
    } = inputs;
    let row_counts = db.row_counts_by_sha(&fingerprint.hex)?;
    let report_inputs = SelectionInputs {
        command,
        current_fingerprint: fingerprint.hex.clone(),
        current_components: fingerprint.components.clone(),
        stored_fingerprints: report::snapshots_from(stored),
        collect_shas: report::collect_sha_snapshots(reach, &row_counts),
        status: plan.status,
        selection: &plan.selection,
        changed_files: report::build_changed_file_inputs(
            &project.workspace_root,
            db,
            &fingerprint.hex,
            reach,
            &plan.changed_ranges,
            changed_files,
        )?,
        include_changed_files: true,
    };
    Report::build_selection(report_inputs).write_json(path)
}

/// Write the partial report for the paths that never compute a selection:
/// `--all` and every cache miss.
///
/// `fingerprint` is optional because `--all` may run before any DB exists.
pub(crate) fn write_full_suite_report(
    command: &'static str,
    status: CacheStatus,
    fingerprint: Option<Fingerprint>,
    stored: Vec<StoredFingerprintRow>,
    collect_shas: Vec<CollectShaSnapshot>,
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
