//! Shared test-selection logic for `run` and `status`.
//!
//! Both subcommands do the same computation: list all tests via nextest, look
//! up which of the known tests cover the changed line ranges, and union that
//! with tests that are in the listing but not yet in the coverage DB (added
//! since the last `collect`). This module owns that pipeline and the output
//! format for the summary line so the two callers don't drift apart.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;

use crate::collect::nextest_list;
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

/// Compute the selection. Invokes `cargo nextest list` (which builds), so
/// callers wanting a fast no-op path should short-circuit on empty DB first.
///
/// `changed_ranges` maps each changed file to its list of changed line ranges
/// (OLD-side, in `collect_sha` coordinates). Files with no entry contribute
/// nothing — that's how we naturally handle untracked files.
pub(crate) fn compute(
    project_root: &Path,
    db: &Db,
    env_fingerprint: &str,
    changed_ranges: &BTreeMap<String, Vec<LineRange>>,
) -> Result<Selection> {
    eprintln!("checking for new tests...");
    let listing = nextest_list(project_root, None, None)?;
    let known_tests = db.all_tests(env_fingerprint)?;
    let known_count = known_tests.len();
    let new_tests: BTreeSet<TestId> = listing
        .tests
        .iter()
        .filter(|t| !known_tests.contains(*t))
        .cloned()
        .collect();

    let mut affected = BTreeSet::new();
    for (file, hunks) in changed_ranges {
        if hunks.is_empty() {
            continue;
        }
        let hits = db.tests_covering_ranges(env_fingerprint, file, hunks)?;
        affected.extend(hits);
    }

    Ok(Selection {
        affected,
        new_tests,
        known_count,
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
        Selection {
            affected: affected.iter().cloned().collect(),
            new_tests: new_tests.iter().cloned().collect(),
            known_count,
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
