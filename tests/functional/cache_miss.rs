//! Coverage-cache-miss behavior for `cargo affected run`.
//!
//! `run` aims to be a strict superset of `cargo nextest run`: when the
//! coverage cache can't anchor a precise affected-test selection (no data
//! at all, fingerprint mismatch, missing or unreachable `collect_sha`), it
//! emits a stderr notice and runs every test rather than bailing or
//! no-opping. This file pins that contract for the two cases that are easy
//! to set up integration-style — empty coverage cache, and a `collect_sha`
//! no longer reachable from HEAD.
//!
//! `status` keeps the bail behavior on drift (see `drift.rs`); only `run`
//! widens to the full suite, because `run`'s job is to actually produce
//! CI signal.

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
fn run_executes_full_suite_when_collect_sha_diverged() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_cache_miss_diverged");
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

    // Reset HEAD back to the initial commit. The collect_sha is now an
    // orphan — present in the repo but not an ancestor of HEAD —
    // matching `ShaRelation::Diverged`.
    git(dir, &["reset", "--hard", "-q", &init_sha]);

    let run = cargo_affected(dir, &["affected", "run"]);
    assert!(
        run.status.success(),
        "run on diverged collect_sha should succeed by running all: stderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&run.stderr),
        String::from_utf8_lossy(&run.stdout),
    );

    let combined = combined_output(&run);

    assert!(
        combined.contains("not reachable from HEAD") && combined.contains("running all tests"),
        "expected diverged-cache notice, got:\n{combined}"
    );
    // The whole suite ran — same three-test check as the no-coverage case.
    for t in ["test_add", "test_multiply", "test_greet"] {
        assert!(
            combined.contains("PASS") && combined.contains(t),
            "expected nextest to PASS {t}, got:\n{combined}"
        );
    }
}
