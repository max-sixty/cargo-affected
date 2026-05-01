//! Coverage-cache-miss behavior for `cargo affected run`.
//!
//! `run` aims to be a strict superset of `cargo nextest run`: when the
//! coverage cache can't anchor a precise affected-test selection (no data
//! at all, fingerprint mismatch, every stored `collect_sha` missing from
//! the repo), it emits a stderr notice and runs every test rather than
//! bailing or no-opping. This file pins that contract for the empty-cache
//! case, and the inverse: when the `collect_sha` is a sibling on a
//! different lineage (CI's PR-vs-main-tip shape, or a local
//! `git reset --hard`), the cached coverage is still usable — diff vs
//! collect_sha works in either direction, and selection runs as normal.

use crate::{
    cargo_affected, combined_output, git, git_head, init_git_with_initial_commit,
    write_two_module_project,
};

#[test]
fn run_executes_full_suite_when_no_coverage_data() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_cache_miss_no_coverage");
    init_git_with_initial_commit(dir);

    // No `cargo affected collect` — DB is empty (or absent). `run` must
    // still succeed by running every test, with a stderr notice explaining
    // why selection wasn't computed.
    let run = cargo_affected(dir, &["affected", "run"]);
    assert!(
        run.status.success(),
        "run with no coverage should succeed by running all: stderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&run.stderr),
        String::from_utf8_lossy(&run.stdout),
    );

    let combined = combined_output(&run);

    // The "running all tests" notice — phrased as "note: ..." per the
    // existing notice style in run.rs. Match on the "no coverage data yet"
    // key phrase.
    assert!(
        combined.contains("no coverage data yet") && combined.contains("running all tests"),
        "expected cache-miss notice, got:\n{combined}"
    );
    // nextest actually ran every test in the sample project (PASS lines for
    // each of test_add / test_multiply / test_greet). One PASS would be
    // ambiguous (could be the old narrow-selection path), so check all
    // three to confirm we really ran the full suite.
    for t in ["test_add", "test_multiply", "test_greet"] {
        assert!(
            combined.contains("PASS") && combined.contains(t),
            "expected nextest to PASS {t}, got:\n{combined}"
        );
    }
}

#[test]
fn run_uses_selection_when_collect_sha_is_sibling() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_cache_miss_sibling");
    init_git_with_initial_commit(dir);
    let init_sha = git_head(dir);

    // Add a second commit so we have something to collect against and a
    // commit to reset back from. `collect` runs at this second commit, so
    // its sha becomes the recorded `collect_sha`.
    std::fs::write(dir.join("src/extra.rs"), "pub fn extra() -> i32 { 1 }\n").unwrap();
    let lib_path = dir.join("src/lib.rs");
    let lib = std::fs::read_to_string(&lib_path).unwrap();
    std::fs::write(&lib_path, format!("{lib}pub mod extra;\n")).unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "add extra module"]);
    let collect_commit = git_head(dir);
    assert_ne!(init_sha, collect_commit);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    // Reset HEAD back to the initial commit. The collect_sha is still in the
    // repo (loose object via reflog), just on a sibling lineage — which is
    // the same shape as the CI PR-vs-main-tip case.
    git(dir, &["reset", "--hard", "-q", &init_sha]);

    let run = cargo_affected(dir, &["affected", "run"]);
    assert!(
        run.status.success(),
        "run on sibling collect_sha should succeed: stderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&run.stderr),
        String::from_utf8_lossy(&run.stdout),
    );

    let combined = combined_output(&run);

    // Must NOT widen to the full suite — a sibling sha is reachable, so the
    // diff feeds normal selection. The "tests to run (N affected + M new ...
    // skipped of K known)" summary line is unique to the selection path; the
    // full-suite path emits "running all tests with nextest" and skips it.
    assert!(
        !combined.contains("running all tests"),
        "sibling collect_sha must not trigger full-suite widening, got:\n{combined}"
    );
    assert!(
        !combined.contains("not reachable from HEAD")
            && !combined.contains("not in the repo"),
        "sibling collect_sha is reachable; should not emit the missing-sha notice, got:\n{combined}"
    );
    assert!(
        combined.contains("tests to run") && combined.contains("affected"),
        "expected the selection summary (tests to run / affected), got:\n{combined}"
    );
}
