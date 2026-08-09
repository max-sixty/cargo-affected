//! Structured JSON diagnostic report for `cargo affected run` and
//! `status`.
//!
//! Schema is versioned at v1 (see `schema_version` in [`Report`]). The
//! goal is to make selection self-explanatory at the artifact level: for
//! cache hits, which file pulled which test in by which mechanism; for
//! cache misses, which fingerprint component differs from the closest
//! stored snapshot.
//!
//! # Output rules
//!
//! - `current_fingerprint` / `current_components` are populated whenever
//!   the fingerprint was computed (always except hard early-fail paths).
//! - `stored_fingerprints` is populated whenever the DB has rows; sorted
//!   `(diff_count asc, last_seen desc)`. On `miss-fingerprint`, the
//!   first entry is the "closest" stored fingerprint.
//! - `collect_shas` is populated whenever the fingerprint matched (else
//!   meaningless).
//! - `selection.changed_files` is populated only in selection mode.
//!   Sorted `(tests_pulled_total desc, path asc)`.
//! - `selection.selected_tests` is populated only when
//!   `mode == "selection"` AND `--report-detail full`. Sorted
//!   `(binary_id, test_name)`. Each test's `reasons` sorted by
//!   `(file, kind, collect_sha)`.
//! - Counts in `selection.summary` are `null` on full-suite paths
//!   (`--all`, `miss-*`); `mode` is `"full-suite-no-listing"`.
//!
//! Writing happens via [`Report::write_json`] using a temp-file +
//! rename so a partial write never leaves a corrupted artifact at the
//! requested path.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::db::{Db, HitKind, HitReason, StoredFingerprintRow, TestId};
use crate::fingerprint::FingerprintComponent;
use crate::project::{git_added_files_since, LineRange, ShaRelation};
use crate::selection::{FileReasonCounts, Reachability, Selection};

/// JSON schema version. Bump on any incompatible field-shape change so
/// consumers can refuse to parse a too-new report.
pub(crate) const SCHEMA_VERSION: u32 = 1;

/// Top-level report structure. Field semantics in the module doc.
#[derive(Debug, Serialize)]
pub(crate) struct Report {
    pub(crate) schema_version: u32,
    pub(crate) cargo_affected_version: &'static str,
    pub(crate) command: &'static str,
    pub(crate) cache: CacheReport,
    pub(crate) selection: SelectionReport,
}

/// Cache state and per-fingerprint component info. The `status` field
/// drives consumer behavior; everything else is diagnostic detail.
#[derive(Debug, Serialize)]
pub(crate) struct CacheReport {
    pub(crate) status: CacheStatus,
    pub(crate) current_fingerprint: Option<String>,
    pub(crate) current_components: Option<Vec<ComponentEntry>>,
    pub(crate) stored_fingerprints: Vec<StoredFingerprintEntry>,
    pub(crate) collect_shas: Vec<CollectShaEntry>,
}

/// What happened on the cache lookup. Closed enum; consumers should
/// treat unknown variants as forward-compatible.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CacheStatus {
    /// Every reachable collect_sha equals HEAD; full-precision selection.
    HitExact,
    /// Fingerprint matched but at least one reachable sha is ahead of
    /// HEAD or at least one missing sha exists alongside reachable ones.
    /// Selection still runs; results are noisier than exact-match.
    HitWithDivergence,
    /// Composite fingerprint absent from DB (DB has rows under other
    /// fingerprints — a build-input changed). No selection.
    MissFingerprint,
    /// DB has no rows at all. First-ever run, or after `clean`.
    MissNoCoverage,
    /// Fingerprint matched, but every stored collect_sha is missing
    /// from the repo (rebased away). No usable diff anchor.
    MissNoReachableSha,
    /// `--all` was passed. Selection skipped intentionally.
    ForcedAll,
}

impl CacheStatus {
    /// Stable kebab-case string for the stderr summary line (reached via
    /// the [`std::fmt::Display`] impl). The serde derive independently produces the
    /// same kebab-case encoding for JSON; these are two mappings that must
    /// agree, and `cache_status_as_str_matches_serde` guards that they do.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::HitExact => "hit-exact",
            Self::HitWithDivergence => "hit-with-divergence",
            Self::MissFingerprint => "miss-fingerprint",
            Self::MissNoCoverage => "miss-no-coverage",
            Self::MissNoReachableSha => "miss-no-reachable-sha",
            Self::ForcedAll => "forced-all",
        }
    }
}

impl std::fmt::Display for CacheStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ComponentEntry {
    pub(crate) label: String,
    pub(crate) hash: String,
}

/// One stored fingerprint with its component-level diff against the
/// current environment. Sorted at the source so consumers don't have to.
#[derive(Debug, Serialize)]
pub(crate) struct StoredFingerprintEntry {
    pub(crate) fingerprint: String,
    pub(crate) last_seen: String,
    /// Number of components whose hash differs from the current
    /// environment. 0 == this stored fingerprint is the current one.
    pub(crate) diff_count: usize,
    /// Sorted labels of the differing components.
    pub(crate) differing_labels: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CollectShaEntry {
    pub(crate) sha: String,
    pub(crate) relation: ShaRelationKind,
    /// Number of commits between this sha and HEAD; absent for `equal`
    /// and `missing`. Skipped (not emitted as `null`) so consumers can
    /// use field presence to detect reachable shas — matches the
    /// documented v1 schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) commits_ahead: Option<u32>,
    /// Total `test_regions` rows anchored at this sha for the current
    /// fingerprint.
    pub(crate) row_count: usize,
}

/// JSON encoding of [`ShaRelation`]. Mirrors the variants but flattens
/// `commits_ahead` into a sibling field on [`CollectShaEntry`].
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ShaRelationKind {
    Equal,
    Reachable,
    Missing,
}

#[derive(Debug, Serialize)]
pub(crate) struct SelectionReport {
    pub(crate) summary: SelectionSummary,
    pub(crate) changed_files: Option<Vec<ChangedFileEntry>>,
    pub(crate) selected_tests: Option<Vec<SelectedTestEntry>>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SelectionSummary {
    pub(crate) selected: Option<usize>,
    pub(crate) affected: Option<usize>,
    /// Reachable-known tests force-selected by a `[workspace.metadata.affected]` rule
    /// (would otherwise have been skipped). Null on full-suite paths.
    pub(crate) config: Option<usize>,
    pub(crate) new: Option<usize>,
    pub(crate) stranded: Option<usize>,
    pub(crate) skipped: Option<usize>,
    pub(crate) total_reachable_known: Option<usize>,
    pub(crate) mode: SelectionMode,
}

/// Whether selection actually ran. `Selection` populates the count
/// fields; `FullSuiteNoListing` leaves them all null because we
/// intentionally skipped `nextest list` to keep cache-miss/`--all`
/// paths cheap.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SelectionMode {
    Selection,
    FullSuiteNoListing,
}

#[derive(Debug, Serialize)]
pub(crate) struct ChangedFileEntry {
    pub(crate) path: String,
    /// `true` iff the file has at least one stored test_regions row at a
    /// reachable sha under the current fingerprint. Non-Rust files,
    /// freshly-added files, and files outside any tested target's
    /// dep graph all read `false`.
    pub(crate) tracked_by_coverage: bool,
    pub(crate) hunks_by_sha: Vec<HunksForSha>,
    pub(crate) tests_pulled_total: usize,
    pub(crate) tests_pulled_by_reason: ReasonCounts,
}

#[derive(Debug, Serialize)]
pub(crate) struct HunksForSha {
    pub(crate) sha: String,
    pub(crate) hunks: Vec<HunkEntry>,
}

#[derive(Debug, Serialize)]
pub(crate) struct HunkEntry {
    pub(crate) start: i64,
    pub(crate) end: i64,
}

/// Per-file counts deduplicated by strongest reason — the four values
/// sum to [`ChangedFileEntry::tests_pulled_total`].
#[derive(Debug, Serialize, Default)]
pub(crate) struct ReasonCounts {
    pub(crate) line_overlap: usize,
    pub(crate) structural_backstop: usize,
    pub(crate) crate_root_sentinel: usize,
    /// Tests pulled in by a `[workspace.metadata.affected]` rule matching this path.
    /// Non-zero only for the (typically non-Rust) inputs such rules target.
    pub(crate) config_rule: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct SelectedTestEntry {
    pub(crate) binary_id: String,
    pub(crate) test_name: String,
    pub(crate) kind: SelectedTestKind,
    /// All reasons that pulled this test in, sorted
    /// `(file, kind, collect_sha)`. Empty for `new` and `stranded`.
    pub(crate) reasons: Vec<ReasonEntry>,
}

/// Why a test ended up rerun. See `selection.rs` for the formal
/// definitions of new vs stranded.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SelectedTestKind {
    Affected,
    /// Force-selected by a `[workspace.metadata.affected]` rule (reachable-known, would
    /// otherwise have been skipped). The `reasons` name the triggering inputs.
    ConfigRule,
    New,
    Stranded,
}

#[derive(Debug, Serialize)]
pub(crate) struct ReasonEntry {
    pub(crate) collect_sha: String,
    pub(crate) file: String,
    pub(crate) kind: ReasonKind,
    /// `[line_start, line_end]` of the stored row that matched. `None`
    /// for `structural_backstop` (no row matched by definition).
    pub(crate) stored_range: Option<[i64; 2]>,
    /// `[start, end]` of the diff hunk that triggered selection.
    pub(crate) matched_hunk: [i64; 2],
}

/// JSON encoding of [`HitKind`].
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReasonKind {
    LineOverlap,
    StructuralBackstop,
    CrateRootSentinel,
    /// A `[workspace.metadata.affected]` rule matched the (typically non-Rust) input
    /// named in `file`; `collect_sha` is empty and `matched_hunk` is `[0, 0]`
    /// since the selection isn't anchored to a coverage hunk.
    ConfigRule,
}

impl From<HitKind> for ReasonKind {
    fn from(kind: HitKind) -> Self {
        match kind {
            HitKind::LineOverlap => Self::LineOverlap,
            HitKind::StructuralBackstop => Self::StructuralBackstop,
            HitKind::CrateRootSentinel => Self::CrateRootSentinel,
            HitKind::ConfigRule => Self::ConfigRule,
        }
    }
}

/// Inputs for [`Report::build_selection`]. Bundles the data the report
/// builder needs into one struct so the call site doesn't have to thread
/// 10+ arguments.
pub(crate) struct SelectionInputs<'a> {
    pub(crate) command: &'static str,
    pub(crate) current_fingerprint: String,
    pub(crate) current_components: Vec<FingerprintComponent>,
    pub(crate) stored_fingerprints: Vec<StoredFingerprintSnapshot>,
    pub(crate) collect_shas: Vec<CollectShaSnapshot>,
    pub(crate) status: CacheStatus,
    pub(crate) selection: &'a Selection,
    pub(crate) changed_files: Vec<ChangedFileInput>,
    /// `false` collapses to `selection.changed_files = None` (no diff
    /// anchor was usable).
    pub(crate) include_changed_files: bool,
}

/// Inputs for [`Report::build_full_suite`] — the partial-report path for
/// `--all` and cache-miss cases. `selection.summary.mode` becomes
/// `"full-suite-no-listing"` and per-test detail is omitted.
pub(crate) struct FullSuiteInputs {
    pub(crate) command: &'static str,
    pub(crate) current_fingerprint: Option<String>,
    pub(crate) current_components: Option<Vec<FingerprintComponent>>,
    pub(crate) stored_fingerprints: Vec<StoredFingerprintSnapshot>,
    pub(crate) collect_shas: Vec<CollectShaSnapshot>,
    pub(crate) status: CacheStatus,
}

/// Snapshot of one stored fingerprint as the report builder receives it
/// (before computing diff against current).
pub(crate) struct StoredFingerprintSnapshot {
    pub(crate) fingerprint: String,
    pub(crate) last_seen: String,
    pub(crate) components: Vec<FingerprintComponent>,
}

impl From<StoredFingerprintRow> for StoredFingerprintSnapshot {
    fn from(row: StoredFingerprintRow) -> Self {
        Self {
            fingerprint: row.fingerprint,
            last_seen: row.last_seen,
            components: row.components,
        }
    }
}

/// Convert a sequence of stored fingerprint rows from the DB into the
/// snapshot shape the report builder consumes.
pub(crate) fn snapshots_from(rows: Vec<StoredFingerprintRow>) -> Vec<StoredFingerprintSnapshot> {
    rows.into_iter().map(Into::into).collect()
}

/// Snapshot of one collect_sha — what relation it has to HEAD and how
/// many rows are anchored at it.
pub(crate) struct CollectShaSnapshot {
    pub(crate) sha: String,
    pub(crate) relation: ShaRelation,
    pub(crate) row_count: usize,
}

/// Per-changed-file input. `hunks_by_sha` lists the diff hunks computed
/// against each stored sha (per-sha because a diff anchor is per-sha).
pub(crate) struct ChangedFileInput {
    pub(crate) path: String,
    pub(crate) tracked_by_coverage: bool,
    pub(crate) hunks_by_sha: BTreeMap<String, Vec<(i64, i64)>>,
}

/// Compose per-sha snapshots for the report from a `Reachability` (per-sha
/// relation) and a sha → row-count map. Rows missing from `row_counts`
/// land at 0 (the sha isn't anchoring any rows for the current
/// fingerprint — common for `Missing` shas).
pub(crate) fn collect_sha_snapshots(
    reach: &Reachability,
    row_counts: &BTreeMap<String, usize>,
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

/// Build per-changed-file input entries for the JSON report.
///
/// Inputs:
///   - `changed_ranges_by_sha`: the per-sha hunk maps already computed
///     by selection (avoids re-diffing each reachable sha — selection
///     and the report consume the same data).
///   - `working_tree_files`: working-tree changes (uncommitted /
///     staged / untracked) so files that selection considered but
///     produced no hunks are still in the report.
///   - committed-added files (via `git_added_files_since(sha)` per
///     reachable sha) — these have no OLD-side and the unified-diff
///     parser skips them, but the report should surface them.
///
/// `tracked_by_coverage` is set in one DB query
/// ([`Db::tracked_files_at_shas`]) instead of one query per file.
pub(crate) fn build_changed_file_inputs(
    project_root: &std::path::Path,
    db: &Db,
    fingerprint: &str,
    reach: &Reachability,
    changed_ranges_by_sha: &BTreeMap<String, BTreeMap<String, Vec<LineRange>>>,
    working_tree_files: &[String],
) -> Result<Vec<ChangedFileInput>> {
    let mut all_files: BTreeSet<String> = working_tree_files.iter().cloned().collect();
    for by_file in changed_ranges_by_sha.values() {
        for path in by_file.keys() {
            all_files.insert(path.clone());
        }
    }
    for sha in &reach.reachable {
        for added in git_added_files_since(project_root, sha)? {
            all_files.insert(added);
        }
    }

    // One query for "which files does the cache know about?", indexed
    // by path lookup below.
    let tracked = db.tracked_files_at_shas(fingerprint, &reach.reachable)?;

    let mut out = Vec::new();
    for path in all_files {
        let mut hunks_by_sha: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();
        for (sha, by_file) in changed_ranges_by_sha {
            if let Some(hunks) = by_file.get(&path) {
                hunks_by_sha.insert(
                    sha.clone(),
                    hunks.iter().map(|h| (h.start, h.end)).collect(),
                );
            }
        }
        out.push(ChangedFileInput {
            tracked_by_coverage: tracked.contains(&path),
            path,
            hunks_by_sha,
        });
    }
    Ok(out)
}

/// Final stderr line a CI consumer can grep to track selection ratios
/// over time. Format depends on cache status; per the design doc, miss
/// statuses get `mode=full-suite`; hit statuses get the selection ratio
/// plus divergence detail when any sha is non-Equal.
///
/// `selection` is `(selected_count, total_reachable_known)` — `None` on
/// full-suite paths where no listing happened.
pub(crate) fn summary_line(
    status: CacheStatus,
    selection: Option<(usize, usize)>,
    missing_shas: usize,
    max_commits_ahead: u32,
) -> String {
    match status {
        CacheStatus::HitExact | CacheStatus::HitWithDivergence => {
            let (selected, total) = selection.unwrap_or((0, 0));
            // Empty cache → vacuous 100% (we ran nothing of nothing).
            let pct = (selected * 100).checked_div(total).unwrap_or(100);
            let mut line =
                format!("cargo-affected: cache={status} selection={selected}/{total} ({pct}%)");
            if matches!(status, CacheStatus::HitWithDivergence) {
                if missing_shas > 0 {
                    line.push_str(&format!(" missing_shas={missing_shas}"));
                }
                if max_commits_ahead > 0 {
                    line.push_str(&format!(" max_commits_ahead={max_commits_ahead}"));
                }
            }
            line
        }
        CacheStatus::MissNoReachableSha => {
            format!("cargo-affected: cache={status} mode=full-suite missing_shas={missing_shas}")
        }
        // Named rather than `_`: these three share a rendering today, and a
        // new status should have to say whether it joins them.
        CacheStatus::MissFingerprint | CacheStatus::MissNoCoverage | CacheStatus::ForcedAll => {
            format!("cargo-affected: cache={status} mode=full-suite")
        }
    }
}

impl Report {
    /// Build a selection-mode report. Use [`Self::build_full_suite`] for
    /// `--all` and cache-miss paths where no selection ran.
    pub(crate) fn build_selection(inputs: SelectionInputs<'_>) -> Self {
        let selection = inputs.selection;
        let summary = SelectionSummary {
            selected: Some(selection.selected().len()),
            affected: Some(selection.affected.len()),
            config: Some(selection.config_tests.len()),
            new: Some(selection.new_tests.len()),
            stranded: Some(selection.stranded_tests.len()),
            skipped: Some(selection.skipped()),
            total_reachable_known: Some(selection.reachable_known_count),
            mode: SelectionMode::Selection,
        };

        let changed_files = if inputs.include_changed_files {
            Some(build_changed_files_entries(
                &inputs.changed_files,
                &selection.diagnostics.per_file,
            ))
        } else {
            None
        };

        let selected_tests = selection.diagnostics.per_test.as_ref().map(|per_test| {
            build_selected_tests(
                &selection.affected,
                &selection.config_tests,
                &selection.new_tests,
                &selection.stranded_tests,
                per_test,
            )
        });

        Self {
            schema_version: SCHEMA_VERSION,
            cargo_affected_version: env!("CARGO_PKG_VERSION"),
            command: inputs.command,
            cache: build_cache(
                inputs.status,
                Some(inputs.current_fingerprint),
                Some(inputs.current_components),
                inputs.stored_fingerprints,
                inputs.collect_shas,
            ),
            selection: SelectionReport {
                summary,
                changed_files,
                selected_tests,
            },
        }
    }

    /// Build a full-suite-no-listing report. Used when `--all` was
    /// passed or the cache miss made selection impossible. Counts are
    /// `null` and `selected_tests` / `changed_files` are omitted to
    /// avoid forcing an expensive `nextest list` we wouldn't otherwise
    /// run.
    pub(crate) fn build_full_suite(inputs: FullSuiteInputs) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            cargo_affected_version: env!("CARGO_PKG_VERSION"),
            command: inputs.command,
            cache: build_cache(
                inputs.status,
                inputs.current_fingerprint,
                inputs.current_components,
                inputs.stored_fingerprints,
                inputs.collect_shas,
            ),
            selection: SelectionReport {
                summary: SelectionSummary {
                    selected: None,
                    affected: None,
                    config: None,
                    new: None,
                    stranded: None,
                    skipped: None,
                    total_reachable_known: None,
                    mode: SelectionMode::FullSuiteNoListing,
                },
                changed_files: None,
                selected_tests: None,
            },
        }
    }

    /// Serialize and write to `path` atomically: write to
    /// `<path>.tmp`, then rename. A partial write (process killed,
    /// disk full) leaves the previous artifact intact rather than a
    /// truncated JSON file.
    pub(crate) fn write_json(&self, path: &Path) -> Result<()> {
        let json =
            serde_json::to_string_pretty(self).context("failed to serialize report to JSON")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).with_context(|| format!("failed to write {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("failed to rename {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

/// Compose the cache section. `current_components` is `Option` so the
/// full-suite path can omit them when we never even computed a
/// fingerprint; absent components imply an empty diff against every
/// stored snapshot (every label diffs).
fn build_cache(
    status: CacheStatus,
    current_fingerprint: Option<String>,
    current_components: Option<Vec<FingerprintComponent>>,
    stored_fingerprints: Vec<StoredFingerprintSnapshot>,
    collect_shas: Vec<CollectShaSnapshot>,
) -> CacheReport {
    let current_for_diff: &[FingerprintComponent] = current_components.as_deref().unwrap_or(&[]);
    let stored = build_stored_fingerprints(stored_fingerprints, current_for_diff);
    CacheReport {
        status,
        current_fingerprint,
        current_components: current_components.map(|cs| {
            cs.into_iter()
                .map(|c| ComponentEntry {
                    label: c.label,
                    hash: c.hash,
                })
                .collect()
        }),
        stored_fingerprints: stored,
        collect_shas: build_collect_shas(collect_shas),
    }
}

fn build_stored_fingerprints(
    stored: Vec<StoredFingerprintSnapshot>,
    current: &[FingerprintComponent],
) -> Vec<StoredFingerprintEntry> {
    let mut entries: Vec<StoredFingerprintEntry> = stored
        .into_iter()
        .map(|snap| {
            let differing_labels = diff_labels(current, &snap.components);
            StoredFingerprintEntry {
                diff_count: differing_labels.len(),
                differing_labels,
                fingerprint: snap.fingerprint,
                last_seen: snap.last_seen,
            }
        })
        .collect();

    // (diff_count asc, last_seen desc) — closest first, then most recent
    // among equally-close entries.
    entries.sort_by(|a, b| {
        a.diff_count
            .cmp(&b.diff_count)
            .then(b.last_seen.cmp(&a.last_seen))
    });
    entries
}

/// Sorted symmetric difference of `(label, hash)` pairs between two
/// fingerprint component lists. Used by both the JSON report's per-stored
/// `differing_labels` and the human-facing cache-miss explanation. A label
/// present in one side but missing from the other counts as differing.
fn diff_labels(current: &[FingerprintComponent], stored: &[FingerprintComponent]) -> Vec<String> {
    let current_by_label: BTreeMap<&str, &str> = current
        .iter()
        .map(|c| (c.label.as_str(), c.hash.as_str()))
        .collect();
    let stored_by_label: BTreeMap<&str, &str> = stored
        .iter()
        .map(|c| (c.label.as_str(), c.hash.as_str()))
        .collect();
    let mut differing: BTreeSet<String> = BTreeSet::new();
    for (label, hash) in &current_by_label {
        if stored_by_label.get(*label) != Some(hash) {
            differing.insert((*label).to_string());
        }
    }
    for label in stored_by_label.keys() {
        if !current_by_label.contains_key(*label) {
            differing.insert((*label).to_string());
        }
    }
    differing.into_iter().collect()
}

/// Differing component labels for the *closest* stored fingerprint —
/// the one whose component set differs from `current` in the fewest
/// labels. Returns an empty vec if `stored` is empty (no fingerprints to
/// compare against — i.e. `MissNoCoverage`, not `MissFingerprint`).
///
/// Powers the human-facing cache-miss line in `run` / `status` so the
/// user sees *which* build input changed without parsing the JSON
/// report. Reuses [`diff_labels`] verbatim — no host-OS-specific logic;
/// the rustc host triple is just one of the labels that can appear.
pub(crate) fn closest_stored_diff_labels(
    current: &[FingerprintComponent],
    stored: &[StoredFingerprintSnapshot],
) -> Vec<String> {
    stored
        .iter()
        .map(|snap| diff_labels(current, &snap.components))
        .min_by_key(Vec::len)
        .unwrap_or_default()
}

/// Format the parenthetical body of a fingerprint-miss message, listing
/// the labels that differ from the closest stored fingerprint. Empty
/// `labels` yields an empty string so callers can concatenate
/// unconditionally; the leading space is part of the returned text so
/// the slot reads cleanly inside a sentence ("for the current
/// environment{clause} — running …").
pub(crate) fn fingerprint_miss_clause(labels: &[String]) -> String {
    if labels.is_empty() {
        String::new()
    } else {
        format!(
            " (differs from closest stored fingerprint in: {})",
            labels.join(", ")
        )
    }
}

fn build_collect_shas(shas: Vec<CollectShaSnapshot>) -> Vec<CollectShaEntry> {
    let mut entries: Vec<CollectShaEntry> = shas
        .into_iter()
        .map(|s| {
            let (relation, commits_ahead) = match s.relation {
                ShaRelation::Equal => (ShaRelationKind::Equal, None),
                ShaRelation::Reachable { commits_ahead } => {
                    (ShaRelationKind::Reachable, Some(commits_ahead))
                }
                ShaRelation::Missing => (ShaRelationKind::Missing, None),
            };
            CollectShaEntry {
                sha: s.sha,
                relation,
                commits_ahead,
                row_count: s.row_count,
            }
        })
        .collect();
    entries.sort_by(|a, b| a.sha.cmp(&b.sha));
    entries
}

fn build_changed_files_entries(
    inputs: &[ChangedFileInput],
    per_file_counts: &BTreeMap<String, FileReasonCounts>,
) -> Vec<ChangedFileEntry> {
    let mut entries: Vec<ChangedFileEntry> = inputs
        .iter()
        .map(|f| {
            let counts = per_file_counts.get(&f.path).cloned().unwrap_or_default();
            let hunks_by_sha = f
                .hunks_by_sha
                .iter()
                .map(|(sha, hunks)| HunksForSha {
                    sha: sha.clone(),
                    hunks: hunks
                        .iter()
                        .map(|(s, e)| HunkEntry { start: *s, end: *e })
                        .collect(),
                })
                .collect();
            ChangedFileEntry {
                path: f.path.clone(),
                tracked_by_coverage: f.tracked_by_coverage,
                hunks_by_sha,
                tests_pulled_total: counts.total_unique_tests,
                tests_pulled_by_reason: ReasonCounts {
                    line_overlap: counts.line_overlap,
                    structural_backstop: counts.structural_backstop,
                    crate_root_sentinel: counts.crate_root_sentinel,
                    config_rule: counts.config_rule,
                },
            }
        })
        .collect();
    entries.sort_by(|a, b| {
        b.tests_pulled_total
            .cmp(&a.tests_pulled_total)
            .then(a.path.cmp(&b.path))
    });
    entries
}

fn build_selected_tests(
    affected: &BTreeSet<TestId>,
    config_tests: &BTreeSet<TestId>,
    new_tests: &BTreeSet<TestId>,
    stranded: &BTreeSet<TestId>,
    per_test: &BTreeMap<TestId, Vec<HitReason>>,
) -> Vec<SelectedTestEntry> {
    // Union all selected tests, classify, and emit in a stable order. The four
    // sets are disjoint, so the classification order only needs to be exhaustive.
    let mut out: Vec<SelectedTestEntry> = Vec::new();
    let union: BTreeSet<&TestId> = affected
        .iter()
        .chain(config_tests.iter())
        .chain(new_tests.iter())
        .chain(stranded.iter())
        .collect();
    for test in union {
        let kind = if new_tests.contains(test) {
            SelectedTestKind::New
        } else if stranded.contains(test) {
            SelectedTestKind::Stranded
        } else if config_tests.contains(test) {
            SelectedTestKind::ConfigRule
        } else {
            SelectedTestKind::Affected
        };
        let mut reasons: Vec<ReasonEntry> = per_test
            .get(test)
            .map(|rs| rs.iter().map(reason_entry).collect())
            .unwrap_or_default();
        // Stable sort within each test's reasons.
        reasons.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then(a.kind.cmp(&b.kind))
                .then(a.collect_sha.cmp(&b.collect_sha))
        });
        out.push(SelectedTestEntry {
            binary_id: test.binary_id.clone(),
            test_name: test.test_name.clone(),
            kind,
            reasons,
        });
    }
    // Already in (binary_id, test_name) order via BTreeSet iteration.
    out
}

fn reason_entry(r: &HitReason) -> ReasonEntry {
    ReasonEntry {
        collect_sha: r.collect_sha.clone(),
        file: r.file.clone(),
        kind: r.kind.into(),
        stored_range: r.stored_range.map(|(s, e)| [s, e]),
        matched_hunk: [r.matched_hunk.0, r.matched_hunk.1],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ShaRelation;

    fn fp_component(label: &str, hash: &str) -> FingerprintComponent {
        FingerprintComponent {
            label: label.to_string(),
            hash: hash.to_string(),
        }
    }

    #[test]
    fn cache_status_serializes_kebab_case() {
        let json = serde_json::to_string(&CacheStatus::HitWithDivergence).unwrap();
        assert_eq!(json, "\"hit-with-divergence\"");
    }

    /// `as_str` (stderr summary, via `Display`) and the serde derive (JSON)
    /// are two independent kebab-case mappings that must produce the same
    /// string for every variant. Guard the whole set so renaming one
    /// encoding without the other fails loudly instead of silently drifting.
    #[test]
    fn cache_status_as_str_matches_serde() {
        for status in [
            CacheStatus::HitExact,
            CacheStatus::HitWithDivergence,
            CacheStatus::MissFingerprint,
            CacheStatus::MissNoCoverage,
            CacheStatus::MissNoReachableSha,
            CacheStatus::ForcedAll,
        ] {
            let serde = serde_json::to_string(&status).unwrap();
            assert_eq!(
                serde,
                format!("\"{}\"", status.as_str()),
                "as_str and serde disagree for {status:?}",
            );
            // Display routes through as_str — keep it in the same guard.
            assert_eq!(status.to_string(), status.as_str());
        }
    }

    /// Stored fingerprint diff against current: differing labels are
    /// the symmetric difference of (label, hash) sets.
    #[test]
    fn stored_fingerprint_diff_is_symmetric() {
        let current = vec![
            fp_component("cargo_lock", "h-current-lock"),
            fp_component("rustc", "h-rustc"),
        ];
        let stored = vec![StoredFingerprintSnapshot {
            fingerprint: "old".to_string(),
            last_seen: "2026-01-01T00:00:00Z".to_string(),
            components: vec![
                fp_component("cargo_lock", "h-old-lock"),
                fp_component("rustc", "h-rustc"),
                fp_component("manifest:Cargo.toml", "h-old-manifest"),
            ],
        }];

        let entries = build_stored_fingerprints(stored, &current);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].diff_count, 2);
        assert_eq!(
            entries[0].differing_labels,
            vec!["cargo_lock", "manifest:Cargo.toml"],
        );
    }

    /// `closest_stored_diff_labels` picks the stored fingerprint with the
    /// smallest symmetric diff against current, regardless of input order.
    /// Empty stored set → empty labels (drives the message branch in
    /// `run`/`status` that falls back to the no-coverage phrasing).
    #[test]
    fn closest_stored_diff_labels_picks_min_diff() {
        let current = vec![
            fp_component("rustc", "host-A"),
            fp_component("CARGO_BUILD_TARGET", "tgt-A"),
        ];
        let stored = vec![
            // 2 labels differ
            StoredFingerprintSnapshot {
                fingerprint: "far".to_string(),
                last_seen: "2026-01-01T00:00:00Z".to_string(),
                components: vec![
                    fp_component("rustc", "host-X"),
                    fp_component("CARGO_BUILD_TARGET", "tgt-X"),
                ],
            },
            // 1 label differs — should win
            StoredFingerprintSnapshot {
                fingerprint: "close".to_string(),
                last_seen: "2026-04-01T00:00:00Z".to_string(),
                components: vec![
                    fp_component("rustc", "host-X"),
                    fp_component("CARGO_BUILD_TARGET", "tgt-A"),
                ],
            },
        ];
        let labels = closest_stored_diff_labels(&current, &stored);
        assert_eq!(labels, vec!["rustc"]);

        assert!(closest_stored_diff_labels(&current, &[]).is_empty());
    }

    /// Closest stored fingerprint comes first; ties broken by `last_seen` desc.
    #[test]
    fn stored_fingerprints_sorted_by_diff_then_recency() {
        let current = vec![fp_component("rustc", "h")];
        let stored = vec![
            StoredFingerprintSnapshot {
                fingerprint: "older-far".to_string(),
                last_seen: "2026-01-01T00:00:00Z".to_string(),
                components: vec![fp_component("rustc", "DIFF")],
            },
            StoredFingerprintSnapshot {
                fingerprint: "newer-close".to_string(),
                last_seen: "2026-05-01T00:00:00Z".to_string(),
                components: vec![fp_component("rustc", "h")],
            },
            StoredFingerprintSnapshot {
                fingerprint: "older-close".to_string(),
                last_seen: "2026-02-01T00:00:00Z".to_string(),
                components: vec![fp_component("rustc", "h")],
            },
        ];
        let entries = build_stored_fingerprints(stored, &current);
        let order: Vec<&str> = entries.iter().map(|e| e.fingerprint.as_str()).collect();
        assert_eq!(order, vec!["newer-close", "older-close", "older-far"]);
    }

    #[test]
    fn collect_shas_render_relation_and_commits_ahead() {
        let entries = build_collect_shas(vec![
            CollectShaSnapshot {
                sha: "aaa".to_string(),
                relation: ShaRelation::Equal,
                row_count: 10,
            },
            CollectShaSnapshot {
                sha: "bbb".to_string(),
                relation: ShaRelation::Reachable { commits_ahead: 6 },
                row_count: 100,
            },
            CollectShaSnapshot {
                sha: "ccc".to_string(),
                relation: ShaRelation::Missing,
                row_count: 5,
            },
        ]);
        assert_eq!(entries[0].relation, ShaRelationKind::Equal);
        assert_eq!(entries[0].commits_ahead, None);
        assert_eq!(entries[1].relation, ShaRelationKind::Reachable);
        assert_eq!(entries[1].commits_ahead, Some(6));
        assert_eq!(entries[2].relation, ShaRelationKind::Missing);
        assert_eq!(entries[2].commits_ahead, None);
    }

    /// Build a minimal full-suite report and verify the shape callers
    /// expect — null counts, omitted changed_files/selected_tests, mode
    /// = "full-suite-no-listing".
    #[test]
    fn full_suite_report_has_null_counts_and_omitted_arrays() {
        let report = Report::build_full_suite(FullSuiteInputs {
            command: "run",
            current_fingerprint: Some("abc".to_string()),
            current_components: Some(vec![fp_component("cargo_lock", "h")]),
            stored_fingerprints: vec![],
            collect_shas: vec![],
            status: CacheStatus::ForcedAll,
        });
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(
            json["selection"]["summary"]["mode"],
            "full-suite-no-listing"
        );
        assert!(json["selection"]["summary"]["selected"].is_null());
        assert!(json["selection"]["changed_files"].is_null());
        assert!(json["selection"]["selected_tests"].is_null());
        assert_eq!(json["cache"]["status"], "forced-all");
    }
}
