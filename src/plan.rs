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
//! Two decisions live here, in the order the commands make them.
//! [`assess`] classifies the cache — usable, or one of three ways of missing —
//! and [`plan`] turns a usable one into a selection. [`write_selection_report`]
//! and [`write_full_suite_report`] are the two report shapes, which differ only
//! in whether a selection was computed at all.
//!
//! Splitting the classification out is what makes the guarantee hold in both
//! directions. A new cache state is a non-exhaustive `match` in `run` *and*
//! `status`, so the compiler asks for the second rendering rather than leaving
//! it to whoever remembers. What stays at the call sites is what genuinely
//! varies: the wording, the stream, and — for `status` — a database inventory
//! `run` has no reason to print.

use std::collections::BTreeSet;
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

/// Where the coverage cache stands for this environment, and what each
/// standing carries.
///
/// The classification is the decision; the wording is not. `run` and `status`
/// each `match` this and write their own prose, so a new state is a compile
/// error in both rather than something to remember in the second file.
pub(crate) enum CacheState {
    /// At least one `collect_sha` resolves, so a diff can be anchored.
    Usable(Reachability),
    /// Nothing to anchor against, for one of three reasons.
    Miss(CacheMiss),
}

/// Why the cache couldn't anchor a diff. Each variant is a different thing to
/// tell the user and a different `CacheStatus` in the report, but all three
/// end the same way: every test runs.
pub(crate) enum CacheMiss {
    /// No rows under any fingerprint — nothing has ever been collected.
    NoCoverage,
    /// Rows exist, but none for this environment. `differing` names the
    /// fingerprint components that diverge from the closest stored snapshot.
    Fingerprint { differing: Vec<String> },
    /// Rows for this environment, but every `collect_sha` is gone from the
    /// repo.
    NoReachableSha(Reachability),
}

impl CacheState {
    /// The reachability result, on the two states that got far enough to
    /// compute one. Lets a caller emit the missing-sha notice once rather
    /// than in each arm that could reach it.
    pub(crate) fn reachability(&self) -> Option<&Reachability> {
        match self {
            Self::Usable(reach) | Self::Miss(CacheMiss::NoReachableSha(reach)) => Some(reach),
            Self::Miss(CacheMiss::NoCoverage | CacheMiss::Fingerprint { .. }) => None,
        }
    }
}

impl CacheMiss {
    /// The report's name for this miss.
    pub(crate) fn status(&self) -> CacheStatus {
        match self {
            Self::NoCoverage => CacheStatus::MissNoCoverage,
            Self::Fingerprint { .. } => CacheStatus::MissFingerprint,
            Self::NoReachableSha(_) => CacheStatus::MissNoReachableSha,
        }
    }
}

/// [`assess`]'s result: the cache's standing plus the values both commands
/// display or feed to the report.
pub(crate) struct Assessment {
    /// Distinct tests recorded under this fingerprint.
    pub(crate) test_count: usize,
    /// Every `collect_sha` under this fingerprint, reachable or not. Empty
    /// when the fingerprint matched nothing.
    pub(crate) collect_shas: BTreeSet<String>,
    /// Stored fingerprint snapshots — always populated on a miss (they decide
    /// [`CacheMiss::Fingerprint`] vs [`CacheMiss::NoCoverage`]),
    /// otherwise only when a report will consume them.
    pub(crate) stored: Vec<StoredFingerprintRow>,
    pub(crate) state: CacheState,
}

/// Classify the cache once, for whichever command asked.
///
/// `want_stored` requests the diagnostic snapshots on the paths that don't
/// otherwise need them — they're report-only there, and reading them costs a
/// query that a plain `run` shouldn't pay.
pub(crate) fn assess(
    project_root: &Path,
    db: &Db,
    fingerprint: &Fingerprint,
    want_stored: bool,
) -> Result<Assessment> {
    let test_count = db.test_count(&fingerprint.hex)?;
    if test_count == 0 {
        // Unconditional here regardless of `want_stored`: whether anything is
        // stored under *another* fingerprint is exactly what separates "you
        // changed environments" from "you never collected".
        let stored = db.stored_fingerprint_snapshots()?;
        let state = CacheState::Miss(if stored.is_empty() {
            CacheMiss::NoCoverage
        } else {
            let snapshots = report::snapshots_from(stored.clone());
            CacheMiss::Fingerprint {
                differing: report::closest_stored_diff_labels(&fingerprint.components, &snapshots),
            }
        });
        return Ok(Assessment {
            test_count,
            collect_shas: BTreeSet::new(),
            stored,
            state,
        });
    }

    let stored = if want_stored {
        db.stored_fingerprint_snapshots()?
    } else {
        Vec::new()
    };
    let collect_shas = db.collect_shas(&fingerprint.hex)?;
    let reach = selection::check_shas_reachable(project_root, &collect_shas)?;
    let state = if reach.reachable.is_empty() {
        CacheState::Miss(CacheMiss::NoReachableSha(reach))
    } else {
        CacheState::Usable(reach)
    };
    Ok(Assessment {
        test_count,
        collect_shas,
        stored,
        state,
    })
}

/// A computed selection, plus the by-products its consumers need.
///
/// `changed_ranges` is carried rather than recomputed: building the report
/// re-derives per-file hunks, and running `git diff -U0 <sha>` a second time
/// per reachable sha is the exact waste this struct exists to avoid.
///
/// `changed_paths` is the same idea one level up — every path that differs
/// from any reachable `collect_sha` or from HEAD, unioned once (see
/// [`selection::changed_paths_since`]). Three consumers want it and each used
/// to derive its own: config rules built it internally, the report rebuilt it
/// while assembling per-file entries, and `run`/`status` approximated it with
/// the working-tree list alone — which is why a committed-but-uncollected
/// change used to be reported as "no uncommitted changes … nothing to run".
pub(crate) struct Plan {
    pub(crate) selection: Selection,
    pub(crate) status: CacheStatus,
    pub(crate) changed_ranges: ChangedRangesBySha,
    pub(crate) changed_paths: BTreeSet<String>,
}

/// List tests, diff against every reachable `collect_sha`, apply
/// `[workspace.metadata.affected]` rules, and select.
///
/// `build_args` must be the same flags the caller will hand `nextest run`
/// (via `cargo_build_args`), so new-test detection compares against the test
/// set that will actually be built rather than a feature-less one.
///
/// `changed_files` is the working tree alone; the full changed-path set is
/// derived here once and carried on the [`Plan`].
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
    let changed_paths =
        selection::changed_paths_since(project_root, reach, &changed_ranges, changed_files)?;
    let config_hits = config::config_rule_hits(project, build_args, &changed_paths)?;
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
        changed_paths,
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
    pub(crate) db: &'a Db,
    pub(crate) fingerprint: &'a Fingerprint,
    pub(crate) stored: Vec<StoredFingerprintRow>,
    pub(crate) reach: &'a Reachability,
    pub(crate) plan: &'a Plan,
}

/// Write the full report for a computed selection.
///
/// Per-changed-file diagnostics and per-sha row counts are report-only — they
/// cost extra git diffs and SQLite queries that selection itself doesn't need
/// — so this is called only when `--report-json` was passed.
pub(crate) fn write_selection_report(inputs: SelectionReport, path: &Path) -> Result<()> {
    let SelectionReport {
        command,
        db,
        fingerprint,
        stored,
        reach,
        plan,
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
            db,
            &fingerprint.hex,
            reach,
            &plan.changed_ranges,
            &plan.changed_paths,
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
