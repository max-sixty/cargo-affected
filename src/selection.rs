//! Shared test-selection logic for `run`, `status`, and `collect --diff`.
//!
//! All three callers do the same computation: list all tests via nextest, look
//! up which of the known tests cover the changed line ranges, and union that
//! with tests that are in the listing but not yet in the coverage DB (added
//! since the last `collect`). This module owns that pipeline and the output
//! format for the summary line so the callers don't drift apart.
//!
//! `collect --diff` produces rows anchored at the new HEAD while leaving
//! unaffected tests' rows at their original sha, so the DB can hold rows
//! from several distinct collect points at once for a single fingerprint.
//! Callers iterate over those shas and pass per-sha changed-line ranges in;
//! selection looks up overlaps in each sha's row set and unions the results.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use anyhow::Result;

use crate::collect::Listing;
use crate::db::{Db, TestId};
use crate::project::LineRange;

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

/// Compute the selection from a pre-built nextest listing and per-sha changed
/// ranges. Callers do their own `nextest list` so they can pass the listing
/// they already need for the binary-map dance (collect) or to control the
/// build flags (run/status invoke a non-instrumented list).
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
