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

use crate::db::{HitKind, HitReason, TestId};
use crate::fingerprint::FingerprintComponent;
use crate::project::ShaRelation;
use crate::selection::{FileReasonCounts, Selection, SelectionDiagnostics};

/// JSON schema version. Bump on any incompatible field-shape change so
/// consumers can refuse to parse a too-new report.
pub const SCHEMA_VERSION: u32 = 1;

/// Top-level report structure. Field semantics in the module doc.
#[derive(Debug, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub cargo_affected_version: &'static str,
    pub command: &'static str,
    pub cache: CacheReport,
    pub selection: SelectionReport,
}

/// Cache state and per-fingerprint component info. The `status` field
/// drives consumer behavior; everything else is diagnostic detail.
#[derive(Debug, Serialize)]
pub struct CacheReport {
    pub status: CacheStatus,
    pub current_fingerprint: Option<String>,
    pub current_components: Option<Vec<ComponentEntry>>,
    pub stored_fingerprints: Vec<StoredFingerprintEntry>,
    pub collect_shas: Vec<CollectShaEntry>,
}

/// What happened on the cache lookup. Closed enum; consumers should
/// treat unknown variants as forward-compatible.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CacheStatus {
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

#[derive(Debug, Serialize)]
pub struct ComponentEntry {
    pub label: String,
    pub hash: String,
}

/// One stored fingerprint with its component-level diff against the
/// current environment. Sorted at the source so consumers don't have to.
#[derive(Debug, Serialize)]
pub struct StoredFingerprintEntry {
    pub fingerprint: String,
    pub last_seen: String,
    /// Number of components whose hash differs from the current
    /// environment. 0 == this stored fingerprint is the current one.
    pub diff_count: usize,
    /// Sorted labels of the differing components.
    pub differing_labels: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct CollectShaEntry {
    pub sha: String,
    /// `equal` | `reachable` | `missing`
    pub relation: &'static str,
    /// Number of commits between this sha and HEAD; absent for `equal`
    /// and `missing`.
    pub commits_ahead: Option<u32>,
    /// Total `test_regions` rows anchored at this sha for the current
    /// fingerprint.
    pub row_count: usize,
}

#[derive(Debug, Serialize)]
pub struct SelectionReport {
    pub summary: SelectionSummary,
    pub changed_files: Option<Vec<ChangedFileEntry>>,
    pub selected_tests: Option<Vec<SelectedTestEntry>>,
}

#[derive(Debug, Serialize)]
pub struct SelectionSummary {
    pub selected: Option<usize>,
    pub affected: Option<usize>,
    pub new: Option<usize>,
    pub stranded: Option<usize>,
    pub skipped: Option<usize>,
    pub total_reachable_known: Option<usize>,
    /// `selection` (counts populated) | `full-suite-no-listing`
    /// (counts null; nextest will run everything).
    pub mode: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ChangedFileEntry {
    pub path: String,
    /// `true` iff the file has at least one stored test_regions row at a
    /// reachable sha under the current fingerprint. Non-Rust files,
    /// freshly-added files, and files outside any tested target's
    /// dep graph all read `false`.
    pub tracked_by_coverage: bool,
    pub hunks_by_sha: Vec<HunksForSha>,
    pub tests_pulled_total: usize,
    pub tests_pulled_by_reason: ReasonCounts,
}

#[derive(Debug, Serialize)]
pub struct HunksForSha {
    pub sha: String,
    pub hunks: Vec<HunkEntry>,
}

#[derive(Debug, Serialize)]
pub struct HunkEntry {
    pub start: i64,
    pub end: i64,
}

/// Per-file counts deduplicated by strongest reason — the three values
/// sum to [`ChangedFileEntry::tests_pulled_total`].
#[derive(Debug, Serialize, Default)]
pub struct ReasonCounts {
    pub line_overlap: usize,
    pub structural_backstop: usize,
    pub crate_root_sentinel: usize,
}

#[derive(Debug, Serialize)]
pub struct SelectedTestEntry {
    pub binary_id: String,
    pub test_name: String,
    /// `affected` | `new` | `stranded`
    pub kind: &'static str,
    /// All reasons that pulled this test in, sorted
    /// `(file, kind, collect_sha)`. Empty for `new` and `stranded`.
    pub reasons: Vec<ReasonEntry>,
}

#[derive(Debug, Serialize)]
pub struct ReasonEntry {
    pub collect_sha: String,
    pub file: String,
    /// `line_overlap` | `structural_backstop` | `crate_root_sentinel`
    pub kind: &'static str,
    /// `[line_start, line_end]` of the stored row that matched. `None`
    /// for `structural_backstop` (no row matched by definition).
    pub stored_range: Option<[i64; 2]>,
    /// `[start, end]` of the diff hunk that triggered selection.
    pub matched_hunk: [i64; 2],
}

/// Inputs for [`Report::build_selection`]. Bundles the data the report
/// builder needs into one struct so the call site doesn't have to thread
/// 10+ arguments.
pub struct SelectionInputs<'a> {
    pub command: &'static str,
    pub current_fingerprint: String,
    pub current_components: Vec<FingerprintComponent>,
    pub stored_fingerprints: Vec<StoredFingerprintSnapshot>,
    pub collect_shas: Vec<CollectShaSnapshot>,
    pub status: CacheStatus,
    pub selection: &'a Selection,
    pub changed_files: Vec<ChangedFileInput>,
    /// `false` collapses to `selection.changed_files = None` (no diff
    /// anchor was usable).
    pub include_changed_files: bool,
}

/// Inputs for [`Report::build_full_suite`] — the partial-report path for
/// `--all` and cache-miss cases. `selection.summary.mode` becomes
/// `"full-suite-no-listing"` and per-test detail is omitted.
pub struct FullSuiteInputs {
    pub command: &'static str,
    pub current_fingerprint: Option<String>,
    pub current_components: Option<Vec<FingerprintComponent>>,
    pub stored_fingerprints: Vec<StoredFingerprintSnapshot>,
    pub collect_shas: Vec<CollectShaSnapshot>,
    pub status: CacheStatus,
}

/// Snapshot of one stored fingerprint as the report builder receives it
/// (before computing diff against current).
pub struct StoredFingerprintSnapshot {
    pub fingerprint: String,
    pub last_seen: String,
    pub components: Vec<FingerprintComponent>,
}

/// Snapshot of one collect_sha — what relation it has to HEAD and how
/// many rows are anchored at it.
pub struct CollectShaSnapshot {
    pub sha: String,
    pub relation: ShaRelation,
    pub row_count: usize,
}

/// Per-changed-file input. `hunks_by_sha` lists the diff hunks computed
/// against each stored sha (per-sha because a diff anchor is per-sha).
pub struct ChangedFileInput {
    pub path: String,
    pub tracked_by_coverage: bool,
    pub hunks_by_sha: BTreeMap<String, Vec<(i64, i64)>>,
}

impl Report {
    /// Build a selection-mode report. Use [`Self::build_full_suite`] for
    /// `--all` and cache-miss paths where no selection ran.
    pub fn build_selection(inputs: SelectionInputs<'_>) -> Self {
        let selection = inputs.selection;
        let summary = SelectionSummary {
            selected: Some(selection.selected().len()),
            affected: Some(selection.affected.len()),
            new: Some(selection.new_tests.len()),
            stranded: Some(selection.stranded_tests.len()),
            skipped: Some(selection.skipped()),
            total_reachable_known: Some(selection.reachable_known_count),
            mode: "selection",
        };

        let changed_files = if inputs.include_changed_files {
            Some(build_changed_files_entries(
                &inputs.changed_files,
                selection.diagnostics.per_file_counts(),
            ))
        } else {
            None
        };

        let selected_tests = match &selection.diagnostics {
            SelectionDiagnostics::Summary { .. } => None,
            SelectionDiagnostics::Full { per_test, .. } => Some(build_selected_tests(
                &selection.affected,
                &selection.new_tests,
                &selection.stranded_tests,
                per_test,
            )),
        };

        Self {
            schema_version: SCHEMA_VERSION,
            cargo_affected_version: env!("CARGO_PKG_VERSION"),
            command: inputs.command,
            cache: build_cache(
                inputs.status,
                Some(inputs.current_fingerprint),
                Some(inputs.current_components.clone()),
                inputs.stored_fingerprints,
                inputs.current_components,
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
    pub fn build_full_suite(inputs: FullSuiteInputs) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            cargo_affected_version: env!("CARGO_PKG_VERSION"),
            command: inputs.command,
            cache: build_cache(
                inputs.status,
                inputs.current_fingerprint,
                inputs.current_components.clone(),
                inputs.stored_fingerprints,
                inputs.current_components.unwrap_or_default(),
                inputs.collect_shas,
            ),
            selection: SelectionReport {
                summary: SelectionSummary {
                    selected: None,
                    affected: None,
                    new: None,
                    stranded: None,
                    skipped: None,
                    total_reachable_known: None,
                    mode: "full-suite-no-listing",
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
    pub fn write_json(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)
            .context("failed to serialize report to JSON")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| {
            format!("failed to rename {} -> {}", tmp.display(), path.display())
        })?;
        Ok(())
    }
}

/// Compose the cache section. `current_*` are `Option` so the
/// full-suite path can omit them when we never even computed a
/// fingerprint (no project root, etc.); `current_components_for_diff`
/// is the same data without the `Option` for the diff calculation.
fn build_cache(
    status: CacheStatus,
    current_fingerprint: Option<String>,
    current_components: Option<Vec<FingerprintComponent>>,
    stored_fingerprints: Vec<StoredFingerprintSnapshot>,
    current_components_for_diff: Vec<FingerprintComponent>,
    collect_shas: Vec<CollectShaSnapshot>,
) -> CacheReport {
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
        stored_fingerprints: build_stored_fingerprints(
            stored_fingerprints,
            &current_components_for_diff,
        ),
        collect_shas: build_collect_shas(collect_shas),
    }
}

fn build_stored_fingerprints(
    stored: Vec<StoredFingerprintSnapshot>,
    current: &[FingerprintComponent],
) -> Vec<StoredFingerprintEntry> {
    let current_by_label: BTreeMap<&str, &str> =
        current.iter().map(|c| (c.label.as_str(), c.hash.as_str())).collect();

    let mut entries: Vec<StoredFingerprintEntry> = stored
        .into_iter()
        .map(|snap| {
            let stored_by_label: BTreeMap<&str, &str> = snap
                .components
                .iter()
                .map(|c| (c.label.as_str(), c.hash.as_str()))
                .collect();
            // Diff in both directions: a label present in current but
            // missing from stored counts as differing, and vice versa.
            let mut differing: BTreeSet<String> = BTreeSet::new();
            for (label, hash) in &current_by_label {
                if stored_by_label.get(*label) != Some(hash) {
                    differing.insert(label.to_string());
                }
            }
            for label in stored_by_label.keys() {
                if !current_by_label.contains_key(*label) {
                    differing.insert(label.to_string());
                }
            }
            StoredFingerprintEntry {
                diff_count: differing.len(),
                differing_labels: differing.into_iter().collect(),
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

fn build_collect_shas(shas: Vec<CollectShaSnapshot>) -> Vec<CollectShaEntry> {
    let mut entries: Vec<CollectShaEntry> = shas
        .into_iter()
        .map(|s| {
            let (relation, commits_ahead) = match s.relation {
                ShaRelation::Equal => ("equal", None),
                ShaRelation::Reachable { commits_ahead } => ("reachable", Some(commits_ahead)),
                ShaRelation::Missing => ("missing", None),
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
    new_tests: &BTreeSet<TestId>,
    stranded: &BTreeSet<TestId>,
    per_test: &BTreeMap<TestId, Vec<HitReason>>,
) -> Vec<SelectedTestEntry> {
    // Union all selected tests, classify, and emit in a stable order.
    let mut out: Vec<SelectedTestEntry> = Vec::new();
    let union: BTreeSet<&TestId> = affected
        .iter()
        .chain(new_tests.iter())
        .chain(stranded.iter())
        .collect();
    for test in union {
        let kind = if new_tests.contains(test) {
            "new"
        } else if stranded.contains(test) {
            "stranded"
        } else {
            "affected"
        };
        let mut reasons: Vec<ReasonEntry> = per_test
            .get(test)
            .map(|rs| rs.iter().map(reason_entry).collect())
            .unwrap_or_default();
        // Stable sort within each test's reasons.
        reasons.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then(a.kind.cmp(b.kind))
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
        kind: hit_kind_str(r.kind),
        stored_range: r.stored_range.map(|(s, e)| [s, e]),
        matched_hunk: [r.matched_hunk.0, r.matched_hunk.1],
    }
}

fn hit_kind_str(kind: HitKind) -> &'static str {
    match kind {
        HitKind::LineOverlap => "line_overlap",
        HitKind::StructuralBackstop => "structural_backstop",
        HitKind::CrateRootSentinel => "crate_root_sentinel",
    }
}

/// `SelectionDiagnostics` accessor used by the report builder. Lives
/// here (rather than on the type) so the diagnostics type stays
/// closer to the selection algorithm.
trait DiagnosticsAccess {
    fn per_file_counts(&self) -> &BTreeMap<String, FileReasonCounts>;
}

impl DiagnosticsAccess for SelectionDiagnostics {
    fn per_file_counts(&self) -> &BTreeMap<String, FileReasonCounts> {
        match self {
            SelectionDiagnostics::Summary { per_file }
            | SelectionDiagnostics::Full { per_file, .. } => per_file,
        }
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
        assert_eq!(entries[0].relation, "equal");
        assert_eq!(entries[0].commits_ahead, None);
        assert_eq!(entries[1].relation, "reachable");
        assert_eq!(entries[1].commits_ahead, Some(6));
        assert_eq!(entries[2].relation, "missing");
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
        assert_eq!(json["selection"]["summary"]["mode"], "full-suite-no-listing");
        assert!(json["selection"]["summary"]["selected"].is_null());
        assert!(json["selection"]["changed_files"].is_null());
        assert!(json["selection"]["selected_tests"].is_null());
        assert_eq!(json["cache"]["status"], "forced-all");
    }
}
