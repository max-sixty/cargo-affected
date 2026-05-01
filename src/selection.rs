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
use crate::db::{Db, TestId};
use crate::project::{git_changed_line_ranges, relation_to_head, LineRange, ShaRelation};

/// Result of the selection computation.
pub(crate) struct Selection {
    /// Known tests selected by line-range overlap with the changed hunks.
    pub(crate) affected: BTreeSet<TestId>,
    /// Tests present in the nextest listing but missing from the DB —
    /// added since the last `collect`. Always selected because we have no
    /// coverage data to decide otherwise.
    pub(crate) new_tests: BTreeSet<TestId>,
    /// Distinct tests tracked in the DB under the current fingerprint.
    pub(crate) known_count: usize,
    /// Every test currently present in the project's nextest listing —
    /// used by `collect --diff` to prune rows for tests that were renamed
    /// or removed.
    pub(crate) listed: BTreeSet<TestId>,
}

impl Selection {
    /// Union of affected and new tests — what nextest will be asked to run.
    pub(crate) fn selected(&self) -> BTreeSet<TestId> {
        self.affected.union(&self.new_tests).cloned().collect()
    }

    /// Known tests not selected this round.
    pub(crate) fn skipped(&self) -> usize {
        self.known_count.saturating_sub(self.affected.len())
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
    let mut reachable = BTreeSet::new();
    let mut missing = BTreeSet::new();
    let mut max_commits_ahead = 0u32;
    for sha in shas {
        match relation_to_head(project_root, sha)? {
            ShaRelation::Equal => {
                reachable.insert(sha.clone());
            }
            ShaRelation::Reachable { commits_ahead } => {
                reachable.insert(sha.clone());
                max_commits_ahead = max_commits_ahead.max(commits_ahead);
            }
            ShaRelation::Missing => {
                missing.insert(sha.clone());
            }
        }
    }
    Ok(Reachability {
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
) -> Result<Selection> {
    let changed_ranges_by_sha = changed_ranges_per_sha(project_root, &reach.reachable)?;
    compute(db, fingerprint, &reach.reachable, &changed_ranges_by_sha, listing)
}

/// Compute the selection from a pre-built nextest listing and per-sha changed
/// ranges. Most callers should use [`select_with_reach`] instead; this is the
/// lower-level entry point for tests and any caller that already has the
/// per-sha changed-range map.
///
/// `reachable_shas` are the stored `collect_sha`s still reachable from HEAD;
/// only tests anchored at one of those shas count as "known" to the cache.
/// Tests anchored exclusively at diverged shas surface as `new_tests` so
/// they're rerun (and, in `collect --diff`, re-anchored at the new HEAD)
/// rather than silently skipped.
pub(crate) fn compute(
    db: &Db,
    env_fingerprint: &str,
    reachable_shas: &BTreeSet<String>,
    changed_ranges_by_sha: &ChangedRangesBySha,
    listing: &Listing,
) -> Result<Selection> {
    let listed: BTreeSet<TestId> = listing.tests.iter().cloned().collect();
    let known_tests = db.all_tests_at_shas(env_fingerprint, reachable_shas)?;
    let known_count = known_tests.len();
    let new_tests: BTreeSet<TestId> = listed
        .iter()
        .filter(|t| !known_tests.contains(*t))
        .cloned()
        .collect();

    let mut affected = BTreeSet::new();
    for (collect_sha, ranges_by_file) in changed_ranges_by_sha {
        for (file, hunks) in ranges_by_file {
            if hunks.is_empty() {
                continue;
            }
            let hits =
                db.tests_covering_ranges(env_fingerprint, collect_sha, file, hunks)?;
            affected.extend(hits);
        }
    }

    Ok(Selection {
        affected,
        new_tests,
        known_count,
        listed,
    })
}

/// Format the summary (and verbose per-test list) for a non-empty selection.
///
/// `verb` is the tense marker — `"to run"` for `run`, `"would run"` for
/// `status`. Returns a multi-line string without trailing newline.
pub(crate) fn format_summary(sel: &Selection, verb: &str, verbose: bool) -> String {
    let selected = sel.selected();
    let mut out = format!(
        "{} tests {verb} ({} affected + {} new, {} skipped of {} known)",
        selected.len(),
        sel.affected.len(),
        sel.new_tests.len(),
        sel.skipped(),
        sel.known_count,
    );
    if verbose {
        out.push(':');
        for t in &selected {
            let tag = if sel.new_tests.contains(t) { " (new)" } else { "" };
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

    fn selection_with(affected: &[TestId], new_tests: &[TestId], known_count: usize) -> Selection {
        let listed: BTreeSet<TestId> = affected
            .iter()
            .cloned()
            .chain(new_tests.iter().cloned())
            .collect();
        Selection {
            affected: affected.iter().cloned().collect(),
            new_tests: new_tests.iter().cloned().collect(),
            known_count,
            listed,
        }
    }

    #[test]
    fn summary_compact_form() {
        let sel = selection_with(
            &[tid("crate_a", "test_a"), tid("crate_a", "test_b")],
            &[tid("crate_a", "test_c")],
            5,
        );
        let out = format_summary(&sel, "to run", false);
        assert_eq!(
            out,
            "3 tests to run (2 affected + 1 new, 3 skipped of 5 known) — pass -v to list"
        );
    }

    #[test]
    fn summary_verbose_tags_new_tests() {
        let sel = selection_with(
            &[tid("crate_a", "test_a")],
            &[tid("crate_a", "test_b")],
            3,
        );
        let out = format_summary(&sel, "would run", true);
        assert_eq!(
            out,
            "2 tests would run (1 affected + 1 new, 2 skipped of 3 known):\n  \
             crate_a::test_a\n  \
             crate_a::test_b (new)"
        );
    }

    #[test]
    fn skipped_saturates_when_all_known_selected() {
        let sel = selection_with(
            &[tid("crate_a", "a"), tid("crate_a", "b")],
            &[],
            2,
        );
        assert_eq!(sel.skipped(), 0);
    }
}
