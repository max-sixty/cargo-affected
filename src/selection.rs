//! Shared test-selection plumbing for `run`, `status`, and `collect --diff`.
//!
//! Reachability classification, per-sha diff collection, the selection
//! computation itself, and the human-facing notices/summaries. The three
//! callers all walk the same path: classify stored `collect_sha`s, gather
//! per-sha changed line ranges, list tests via nextest, look up overlaps for
//! known tests, and union with tests added since the last `collect`. This
//! module owns the whole flow so the callers can't drift apart.
//!
//! `collect --diff` produces rows anchored at the new HEAD while leaving
//! unaffected tests' rows at their original sha, so the DB can hold rows
//! from several distinct collect points at once for a single fingerprint.
//! Reachability is per-sha — diverged shas are skipped and tests stranded
//! only there surface as `new_tests` so they're rerun rather than silently
//! dropped.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;

use crate::collect::Listing;
use crate::db::{Db, HitKind, HitReason, TestId};
use crate::project::{
    git_added_files_since, git_changed_line_ranges, relation_to_head, LineRange, ShaRelation,
};

/// Result of the selection computation.
pub(crate) struct Selection {
    /// Known tests selected by line-range overlap with the changed hunks.
    /// Excludes `#[ignore]`d tests: their coverage rows can persist from
    /// an earlier (non-ignored) collect, but `nextest run` would skip them
    /// — same all-ignored-selection rationale as [`new_tests`].
    ///
    /// [`new_tests`]: Self::new_tests
    pub(crate) affected: BTreeSet<TestId>,
    /// Tests present in the nextest listing but absent from the DB
    /// entirely under the current fingerprint — added since the last
    /// `collect`. Always selected because we have no coverage data.
    /// Excludes `#[ignore]`d tests: `nextest run` skips them, so they
    /// never gain coverage and would otherwise read as "new" on every run.
    pub(crate) new_tests: BTreeSet<TestId>,
    /// Tests present in the nextest listing AND in the DB, but only
    /// anchored at currently-missing collect_shas. Functionally identical
    /// to "new" (rerun and re-anchor), but the diagnostic distinguishes
    /// them so consumers can tell the difference between "added in this
    /// PR" and "anchor sha got rebased away".
    pub(crate) stranded_tests: BTreeSet<TestId>,
    /// Reachable-known tests force-selected by a `[workspace.metadata.affected]` rule —
    /// a changed input (snapshot, doc, template) matched a rule's globs and
    /// coverage couldn't link it to the test. Disjoint from [`affected`]
    /// (a test pulled in by both counts as `affected`) and from
    /// `new`/`stranded` (those aren't reachable-known); these would have been
    /// *skipped* without the rule. Excludes `#[ignore]`d tests.
    ///
    /// [`affected`]: Self::affected
    pub(crate) config_tests: BTreeSet<TestId>,
    /// Distinct test count tracked under the current fingerprint at
    /// reachable shas. The "tests we could have selected from" denominator.
    pub(crate) reachable_known_count: usize,
    /// Every test currently present in the project's nextest listing —
    /// used by `collect --diff` to prune rows for tests that were renamed
    /// or removed.
    pub(crate) listed: BTreeSet<TestId>,
    /// Per-file/per-test diagnostics retained at the level requested by
    /// the caller. See [`SelectionDiagnostics`].
    pub(crate) diagnostics: SelectionDiagnostics,
}

impl Selection {
    /// Union of affected, stranded, new, and config-rule tests — what nextest
    /// will be asked to run.
    pub(crate) fn selected(&self) -> BTreeSet<TestId> {
        let mut out = self.affected.clone();
        out.extend(self.new_tests.iter().cloned());
        out.extend(self.stranded_tests.iter().cloned());
        out.extend(self.config_tests.iter().cloned());
        out
    }

    /// Known tests not selected this round. Both `affected` and `config_tests`
    /// are reachable-known and selected, so both reduce the skipped count.
    pub(crate) fn skipped(&self) -> usize {
        self.reachable_known_count
            .saturating_sub(self.affected.len() + self.config_tests.len())
    }
}

/// Detail level for retained diagnostics. Driven by the `--report-detail`
/// CLI flag (default `summary`). Memory cost on a large workspace is
/// dominated by `Full`'s per-test reason vectors, so the default is
/// bounded; `Full` is opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum DiagnosticDetail {
    /// Per-file/per-kind aggregate counters only.
    Summary,
    /// Per-test reason vectors plus the per-file aggregates.
    Full,
}

/// Diagnostics retained after selection — what the JSON report builder
/// consumes. `per_test` is `None` in summary mode (bounded regardless
/// of test-suite size); `Some` in full mode (carries per-test reason
/// vectors, can be large on big workspaces).
pub(crate) struct SelectionDiagnostics {
    pub(crate) per_file: BTreeMap<String, FileReasonCounts>,
    pub(crate) per_test: Option<BTreeMap<TestId, Vec<HitReason>>>,
}

/// Per-changed-file counts of how each test got pulled in.
///
/// Counts are deduplicated by strongest reason: a test with both a
/// LineOverlap hit and a CrateRootSentinel hit on the same file counts
/// once, classified by the strongest reason
/// (LineOverlap > StructuralBackstop > ConfigRule > CrateRootSentinel).
/// Per-file counts therefore sum to `total_unique_tests`, making the
/// diagnostic arithmetic clean.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FileReasonCounts {
    pub(crate) line_overlap: usize,
    pub(crate) structural_backstop: usize,
    pub(crate) crate_root_sentinel: usize,
    pub(crate) config_rule: usize,
    pub(crate) total_unique_tests: usize,
}

/// Strongest-reason ordering. Used to dedupe per-test reasons when
/// rolling up to per-file counts: a test counts ONCE per file, by its
/// strongest reason. ConfigRule ranks below the coverage-derived reasons
/// (its file has no coverage rows, so in practice it never co-occurs with
/// them on the same file) but above the bare sentinel.
fn strongest(a: HitKind, b: HitKind) -> HitKind {
    fn rank(k: HitKind) -> u8 {
        match k {
            HitKind::LineOverlap => 3,
            HitKind::StructuralBackstop => 2,
            HitKind::ConfigRule => 1,
            HitKind::CrateRootSentinel => 0,
        }
    }
    if rank(a) >= rank(b) {
        a
    } else {
        b
    }
}

/// Per-sha changed-line ranges: outer key is the `collect_sha` the rows are
/// anchored at, inner map mirrors the existing per-file shape produced by
/// `git_changed_line_ranges`.
pub(crate) type ChangedRangesBySha = BTreeMap<String, BTreeMap<String, Vec<LineRange>>>;

/// Per-sha reachability for the stored `collect_sha`s under one fingerprint.
///
/// Splits cleanly so callers can proceed with the reachable subset rather
/// than treating any divergence as all-or-nothing — important under `collect
/// --diff`, where rows from several shas coexist for one fingerprint and a
/// single rebase shouldn't invalidate unrelated tests' rows. Tests anchored
/// at missing shas remain in the DB; queries skip them, and selection
/// surfaces them as "new tests" so they get rerun (and re-anchored, in
/// `collect --diff`'s case). Old rows accumulate as bloat — clear with
/// `cargo affected clean`.
pub(crate) struct Reachability {
    /// Per-sha relation to HEAD for every checked sha. Lets the report
    /// builder render which shas equal HEAD vs are ahead vs are missing
    /// instead of just summarizing.
    pub(crate) per_sha: BTreeMap<String, ShaRelation>,
    /// Stored shas the repo can still resolve to a commit. Includes strict
    /// ancestors of HEAD and siblings (e.g. a recent main tip when HEAD is a
    /// PR branched off an older main). Selection runs on every sha in here.
    pub(crate) reachable: BTreeSet<String>,
    /// Stored shas the repo no longer has any object for — rebased and
    /// pruned, garbage-collected, or beyond a shallow clone boundary. Their
    /// rows are still in the DB but won't be queried.
    pub(crate) missing: BTreeSet<String>,
    /// Largest commits-ahead distance among reachable shas; 0 when every
    /// reachable sha equals HEAD.
    pub(crate) max_commits_ahead: u32,
}

/// Format the partial-divergence notice shared by `run`, `status`, and
/// `collect --diff`. `verb_phrase` slots into "tests anchored only there
/// VERB_PHRASE" — "will rerun as 'new'" for `run`/`collect --diff`, "would
/// rerun as 'new'" for `status`. Returns the body without a trailing
/// newline so callers can `eprintln!`/`println!` it directly.
pub(crate) fn missing_shas_notice(missing: &BTreeSet<String>, verb_phrase: &str) -> String {
    let plural = if missing.len() == 1 { "" } else { "s" };
    let list = missing.iter().cloned().collect::<Vec<_>>().join(", ");
    format!(
        "note: {} collect_sha{plural} not in the repo ({list}) — \
         tests anchored only there {verb_phrase}; \
         run `cargo affected clean` to clear stale rows",
        missing.len(),
    )
}

/// Classify each `collect_sha` in `shas` against HEAD. See [`Reachability`].
pub(crate) fn check_shas_reachable(
    project_root: &Path,
    shas: &BTreeSet<String>,
) -> Result<Reachability> {
    let mut per_sha = BTreeMap::new();
    let mut reachable = BTreeSet::new();
    let mut missing = BTreeSet::new();
    let mut max_commits_ahead = 0u32;
    for sha in shas {
        let relation = relation_to_head(project_root, sha)?;
        match &relation {
            ShaRelation::Equal => {
                reachable.insert(sha.clone());
            }
            ShaRelation::Reachable { commits_ahead } => {
                reachable.insert(sha.clone());
                max_commits_ahead = max_commits_ahead.max(*commits_ahead);
            }
            ShaRelation::Missing => {
                missing.insert(sha.clone());
            }
        }
        per_sha.insert(sha.clone(), relation);
    }
    Ok(Reachability {
        per_sha,
        reachable,
        missing,
        max_commits_ahead,
    })
}

/// Build the per-sha changed-ranges map: one `git diff -U0 <sha>` per
/// distinct stored `collect_sha`. With a single sha (the common case) this
/// is one diff invocation.
pub(crate) fn changed_ranges_per_sha(
    project_root: &Path,
    shas: &BTreeSet<String>,
) -> Result<ChangedRangesBySha> {
    let mut out = BTreeMap::new();
    for sha in shas {
        let ranges = git_changed_line_ranges(project_root, sha)?;
        out.insert(sha.clone(), ranges);
    }
    Ok(out)
}

/// Compute selection given a pre-built listing and a `Reachability`.
///
/// Bundles the two steps that always happen together — per-sha diff query
/// and the selection compute — so the three callers (`run`, `status`,
/// `collect --diff`) don't each open-code the pair. Caller stays
/// responsible for handling `reach.reachable.is_empty()` upstream:
/// `run`/`status` widen to all tests there, `collect --diff` bails. Those
/// policies and their notices differ, so they don't belong in here.
pub(crate) fn select_with_reach(
    project_root: &Path,
    db: &Db,
    fingerprint: &str,
    listing: &Listing,
    reach: &Reachability,
    detail: DiagnosticDetail,
) -> Result<Selection> {
    let changed_ranges_by_sha = changed_ranges_per_sha(project_root, &reach.reachable)?;
    // `collect --diff` recollects coverage for changed Rust code; config rules
    // select tests to *run* despite absent coverage, which is a `run`/`status`
    // concern. Pass no config hits here.
    compute(
        db,
        fingerprint,
        &reach.reachable,
        &changed_ranges_by_sha,
        listing,
        &BTreeMap::new(),
        detail,
    )
}

/// Like [`select_with_reach`] but accepts a pre-computed per-sha
/// changed-range map. Lets `run`/`status` compute the diffs once and
/// pass the same map to both selection and the diagnostic report,
/// avoiding a second round of `git diff` per reachable sha when
/// `--report-json` is set.
pub(crate) fn select_with_precomputed_ranges(
    db: &Db,
    fingerprint: &str,
    listing: &Listing,
    reach: &Reachability,
    changed_ranges_by_sha: &ChangedRangesBySha,
    config_hits: &BTreeMap<String, BTreeSet<TestId>>,
    detail: DiagnosticDetail,
) -> Result<Selection> {
    compute(
        db,
        fingerprint,
        &reach.reachable,
        changed_ranges_by_sha,
        listing,
        config_hits,
        detail,
    )
}

/// Union of all paths that changed between the working tree and any reachable
/// `collect_sha` — for matching against `[workspace.metadata.affected]` rule globs.
///
/// Modified files come from the per-sha diff already computed for selection;
/// added files (which `git diff -U0` omits — they have no OLD side) come from
/// [`git_added_files_since`]; working-tree changes (uncommitted, staged,
/// untracked) come from `working_tree_files`. Without the added-files source,
/// a PR that adds a brand-new `.snap`/doc with no modified sibling would slip
/// through.
pub(crate) fn changed_paths_since(
    project_root: &Path,
    reach: &Reachability,
    changed_ranges_by_sha: &ChangedRangesBySha,
    working_tree_files: &[String],
) -> Result<BTreeSet<String>> {
    let mut paths: BTreeSet<String> = working_tree_files.iter().cloned().collect();
    for by_file in changed_ranges_by_sha.values() {
        paths.extend(by_file.keys().cloned());
    }
    for sha in &reach.reachable {
        paths.extend(git_added_files_since(project_root, sha)?);
    }
    Ok(paths)
}

/// Compute the selection from a pre-built nextest listing and per-sha changed
/// ranges. Most callers should use [`select_with_reach`] instead; this is the
/// lower-level entry point for tests and any caller that already has the
/// per-sha changed-range map.
///
/// `reachable_shas` are the stored `collect_sha`s still reachable from HEAD;
/// only tests anchored at one of those shas count as "known" to the cache.
/// Listed tests get split three ways:
///
/// - `affected`: known-reachable AND pulled in by a hunk overlap.
/// - `new_tests = listed - all_db_tests` (genuinely new — never seen in
///   the DB under this fingerprint).
/// - `stranded_tests = listed ∩ (all_db_tests - reachable_known_tests)`
///   (in DB but only at currently-missing shas).
///
/// `#[ignore]`d tests are dropped from all three sets: `nextest run` skips
/// them, so a selection of nothing but ignored tests makes `nextest run` exit
/// non-zero. New/stranded would re-select an ignored test on every run only
/// for it to be skipped again; `affected` would re-select a test whose
/// coverage rows survived from a previous (non-ignored) collect after a hunk
/// happens to overlap them.
///
/// Both `new` and `stranded` get rerun (and re-anchored, in `collect
/// --diff`'s case); the split exists so the JSON report can tell them
/// apart.
pub(crate) fn compute(
    db: &Db,
    env_fingerprint: &str,
    reachable_shas: &BTreeSet<String>,
    changed_ranges_by_sha: &ChangedRangesBySha,
    listing: &Listing,
    config_hits: &BTreeMap<String, BTreeSet<TestId>>,
    detail: DiagnosticDetail,
) -> Result<Selection> {
    // Mark this fingerprint as recently used so the next collect's LRU
    // eviction respects it. One write per selection invocation, not one
    // per file-sha pair (which is what `tests_covering_ranges` used to
    // do internally).
    db.touch(env_fingerprint)?;

    let listed: BTreeSet<TestId> = listing.tests.iter().cloned().collect();
    let reachable_known = db.all_tests_at_shas(env_fingerprint, reachable_shas)?;
    let reachable_known_count = reachable_known.len();
    let all_db_tests = db.all_tests_for_fingerprint(env_fingerprint)?;

    let mut new_tests = BTreeSet::new();
    let mut stranded_tests = BTreeSet::new();
    for t in &listed {
        if listing.ignored.contains(t) {
            // Skipped by `nextest run`, so it never gains coverage — must
            // not be treated as a new/stranded test to rerun. Stays in
            // `listed` (above) so `collect --diff`'s prune keeps its rows.
            continue;
        }
        if reachable_known.contains(t) {
            continue;
        }
        if all_db_tests.contains(t) {
            stranded_tests.insert(t.clone());
        } else {
            new_tests.insert(t.clone());
        }
    }

    // Detail-aware retention. In `Summary` mode we never materialize
    // a per-test reason vector — we stream each hit directly into the
    // (file, test) → strongest-kind map and discard the reason after.
    // In `Full` mode we additionally retain the per-test vector for the
    // JSON report to consume.
    let mut affected = BTreeSet::new();
    let mut strongest_per_file_test: BTreeMap<String, BTreeMap<TestId, HitKind>> = BTreeMap::new();
    let mut per_test_reasons: BTreeMap<TestId, Vec<HitReason>> = BTreeMap::new();
    let retain_per_test = matches!(detail, DiagnosticDetail::Full);
    for (collect_sha, ranges_by_file) in changed_ranges_by_sha {
        for (file, hunks) in ranges_by_file {
            if hunks.is_empty() {
                continue;
            }
            let hits = db.tests_covering_ranges(env_fingerprint, collect_sha, file, hunks)?;
            for hit in hits {
                if listing.ignored.contains(&hit.test_id) {
                    // Coverage rows from a previous (non-ignored) collect
                    // can survive into a state where the test is now
                    // `#[ignore]`d (the `--diff` prune deliberately keeps
                    // them — see `diff_collect_keeps_ignored_test_rows`).
                    // Selecting it anyway produces the same all-ignored
                    // → nextest exit 4 we filter against above for
                    // new/stranded.
                    continue;
                }
                affected.insert(hit.test_id.clone());
                let kind = hit.reason.kind;
                let entry = strongest_per_file_test
                    .entry(hit.reason.file.clone())
                    .or_default()
                    .entry(hit.test_id.clone());
                entry
                    .and_modify(|existing| *existing = strongest(*existing, kind))
                    .or_insert(kind);
                if retain_per_test {
                    per_test_reasons
                        .entry(hit.test_id)
                        .or_default()
                        .push(hit.reason);
                }
            }
        }
    }

    // Declarative input rules: a changed (typically non-Rust) input matched a
    // `[workspace.metadata.affected]` rule. Coverage can't link these inputs to tests, so
    // the rule supplies the edge. A reachable-known test that isn't already
    // `affected` would otherwise be skipped — rescue it as a `config_test`.
    // New/stranded matches already run; ignored ones stay skipped by nextest.
    let mut config_tests = BTreeSet::new();
    for (path, tests) in config_hits {
        for test in tests {
            if listing.ignored.contains(test)
                || affected.contains(test)
                || !reachable_known.contains(test)
            {
                continue;
            }
            config_tests.insert(test.clone());
            strongest_per_file_test
                .entry(path.clone())
                .or_default()
                .entry(test.clone())
                .and_modify(|k| *k = strongest(*k, HitKind::ConfigRule))
                .or_insert(HitKind::ConfigRule);
            if retain_per_test {
                // Config reasons name the triggering input path; they have no
                // sha-anchored coverage hunk, so those fields are left empty.
                per_test_reasons
                    .entry(test.clone())
                    .or_default()
                    .push(HitReason {
                        collect_sha: String::new(),
                        file: path.clone(),
                        kind: HitKind::ConfigRule,
                        matched_hunk: (0, 0),
                        stored_range: None,
                    });
            }
        }
    }

    let per_file = aggregate_per_file_counts(&strongest_per_file_test);
    let diagnostics = SelectionDiagnostics {
        per_file,
        per_test: retain_per_test.then_some(per_test_reasons),
    };

    Ok(Selection {
        affected,
        new_tests,
        stranded_tests,
        config_tests,
        reachable_known_count,
        listed,
        diagnostics,
    })
}

/// Fold the (file → test → strongest-kind) map into per-file counts.
/// Each (file, test) contributes once, classified by its strongest kind;
/// counts within a file therefore sum to `total_unique_tests`.
fn aggregate_per_file_counts(
    strongest_per_file_test: &BTreeMap<String, BTreeMap<TestId, HitKind>>,
) -> BTreeMap<String, FileReasonCounts> {
    let mut out: BTreeMap<String, FileReasonCounts> = BTreeMap::new();
    for (file, by_test) in strongest_per_file_test {
        let mut counts = FileReasonCounts::default();
        for kind in by_test.values() {
            match kind {
                HitKind::LineOverlap => counts.line_overlap += 1,
                HitKind::StructuralBackstop => counts.structural_backstop += 1,
                HitKind::CrateRootSentinel => counts.crate_root_sentinel += 1,
                HitKind::ConfigRule => counts.config_rule += 1,
            }
            counts.total_unique_tests += 1;
        }
        out.insert(file.clone(), counts);
    }
    out
}

/// Format the summary (and verbose per-test list) for a non-empty selection.
///
/// `verb` is the tense marker — `"to run"` for `run`, `"would run"` for
/// `status`. Returns a multi-line string without trailing newline.
pub(crate) fn format_summary(sel: &Selection, verb: &str, verbose: bool) -> String {
    let selected = sel.selected();
    let mut out = format!(
        "{} tests {verb} ({} affected + {} config + {} new + {} stranded, \
         {} skipped of {} reachable-known)",
        selected.len(),
        sel.affected.len(),
        sel.config_tests.len(),
        sel.new_tests.len(),
        sel.stranded_tests.len(),
        sel.skipped(),
        sel.reachable_known_count,
    );
    if verbose {
        out.push(':');
        for t in &selected {
            let tag = if sel.new_tests.contains(t) {
                " (new)"
            } else if sel.stranded_tests.contains(t) {
                " (stranded)"
            } else if sel.config_tests.contains(t) {
                " (config)"
            } else {
                ""
            };
            let _ = write!(out, "\n  {}::{}{tag}", t.binary_id, t.test_name);
        }
    } else {
        out.push_str(" — pass -v to list");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tid(binary_id: &str, test_name: &str) -> TestId {
        TestId::new(binary_id, test_name)
    }

    fn selection_with(
        affected: &[TestId],
        new_tests: &[TestId],
        stranded_tests: &[TestId],
        config_tests: &[TestId],
        reachable_known_count: usize,
    ) -> Selection {
        let listed: BTreeSet<TestId> = affected
            .iter()
            .cloned()
            .chain(new_tests.iter().cloned())
            .chain(stranded_tests.iter().cloned())
            .chain(config_tests.iter().cloned())
            .collect();
        Selection {
            affected: affected.iter().cloned().collect(),
            new_tests: new_tests.iter().cloned().collect(),
            stranded_tests: stranded_tests.iter().cloned().collect(),
            config_tests: config_tests.iter().cloned().collect(),
            reachable_known_count,
            listed,
            diagnostics: SelectionDiagnostics {
                per_file: BTreeMap::new(),
                per_test: None,
            },
        }
    }

    #[test]
    fn summary_compact_form() {
        let sel = selection_with(
            &[tid("crate_a", "test_a"), tid("crate_a", "test_b")],
            &[tid("crate_a", "test_c")],
            &[],
            &[],
            5,
        );
        let out = format_summary(&sel, "to run", false);
        assert_eq!(
            out,
            "3 tests to run (2 affected + 0 config + 1 new + 0 stranded, \
             3 skipped of 5 reachable-known) — pass -v to list"
        );
    }

    #[test]
    fn summary_verbose_tags_categories() {
        let sel = selection_with(
            &[tid("crate_a", "test_a")],
            &[tid("crate_a", "test_b")],
            &[tid("crate_a", "test_c")],
            &[tid("crate_a", "test_d")],
            5,
        );
        let out = format_summary(&sel, "would run", true);
        assert_eq!(
            out,
            "4 tests would run (1 affected + 1 config + 1 new + 1 stranded, \
             3 skipped of 5 reachable-known):\n  \
             crate_a::test_a\n  \
             crate_a::test_b (new)\n  \
             crate_a::test_c (stranded)\n  \
             crate_a::test_d (config)"
        );
    }

    #[test]
    fn skipped_subtracts_affected_and_config() {
        // 2 affected + 1 config, all reachable-known → all 3 selected, none
        // skipped.
        let sel = selection_with(
            &[tid("crate_a", "a"), tid("crate_a", "b")],
            &[],
            &[],
            &[tid("crate_a", "c")],
            3,
        );
        assert_eq!(sel.skipped(), 0);
    }

    #[test]
    fn strongest_reason_orders_line_then_backstop_then_sentinel() {
        // Pairwise: stronger arg returned regardless of position.
        for (a, b, expected) in [
            (
                HitKind::LineOverlap,
                HitKind::CrateRootSentinel,
                HitKind::LineOverlap,
            ),
            (
                HitKind::CrateRootSentinel,
                HitKind::LineOverlap,
                HitKind::LineOverlap,
            ),
            (
                HitKind::StructuralBackstop,
                HitKind::CrateRootSentinel,
                HitKind::StructuralBackstop,
            ),
            (
                HitKind::CrateRootSentinel,
                HitKind::StructuralBackstop,
                HitKind::StructuralBackstop,
            ),
            (
                HitKind::LineOverlap,
                HitKind::StructuralBackstop,
                HitKind::LineOverlap,
            ),
            (
                HitKind::StructuralBackstop,
                HitKind::LineOverlap,
                HitKind::LineOverlap,
            ),
        ] {
            assert_eq!(strongest(a, b), expected, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn aggregate_dedupes_test_per_file_by_strongest_reason() {
        // test_a hit by LineOverlap AND CrateRootSentinel for the same
        // file should count once, classified as LineOverlap. The
        // strongest-kind map is what `compute()` builds in summary mode
        // (avoiding the per-test reason vector).
        let test_a = tid("crate_a", "a");
        let mut strongest: BTreeMap<String, BTreeMap<TestId, HitKind>> = BTreeMap::new();
        let by_test = strongest.entry("src/lib.rs".to_string()).or_default();
        // Insert sentinel first, then LineOverlap — the upgrade path is
        // what we're exercising.
        by_test.insert(test_a.clone(), HitKind::CrateRootSentinel);
        let entry = by_test.entry(test_a.clone());
        entry
            .and_modify(|k| *k = super::strongest(*k, HitKind::LineOverlap))
            .or_insert(HitKind::LineOverlap);

        let counts = aggregate_per_file_counts(&strongest);
        let lib = counts.get("src/lib.rs").expect("src/lib.rs should appear");
        assert_eq!(lib.line_overlap, 1);
        assert_eq!(lib.structural_backstop, 0);
        assert_eq!(lib.crate_root_sentinel, 0);
        assert_eq!(lib.total_unique_tests, 1);
    }
}
