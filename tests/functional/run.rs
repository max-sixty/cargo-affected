//! `cargo affected run` actually executes only the affected tests.
//!
//! Until this scenario, the suite only exercised `status` (the dry-run view).
//! `run` invokes nextest under the hood — we capture its output and the
//! selection layer's `-v` listing to verify both that the right tests were
//! chosen AND that they actually ran successfully.

use crate::{
    cargo_affected, combined_output, init_git_with_initial_commit, replace_in_file,
    write_two_module_project,
};

#[test]
fn run_executes_only_affected_tests() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_run");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    // Function-body edit on `add` — only test_add should be selected and run.
    replace_in_file(&dir.join("src/math.rs"), "a + b", "a + b /* edited */");

    let run = cargo_affected(dir, &["affected", "run", "-v"]);
    assert!(
        run.status.success(),
        "run failed: stderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&run.stderr),
        String::from_utf8_lossy(&run.stdout)
    );

    // The selection summary goes to stderr (selection::format_summary +
    // run.rs's `eprintln!`); nextest's own progress output also goes to
    // stderr. Concatenate both streams so assertions don't get tripped up by
    // wherever a particular line landed.
    let combined = combined_output(&run);

    // Selection summary line — verifies the run command picked exactly one
    // test (test_add) before handing off to nextest.
    assert!(
        combined.contains("1 tests to run"),
        "expected '1 tests to run' in run output, got:\n{combined}"
    );
    assert!(
        combined.contains("test_add"),
        "expected test_add in selection listing, got:\n{combined}"
    );
    assert!(
        !combined.contains("test_multiply"),
        "test_multiply must NOT appear (its range didn't overlap the edit), got:\n{combined}"
    );
    assert!(
        !combined.contains("test_greet"),
        "test_greet must NOT appear (strings.rs unchanged), got:\n{combined}"
    );

    // nextest's own line confirms the test actually executed and passed,
    // not just that selection chose it. nextest's summary format is stable
    // enough to grep on a single test's name.
    assert!(
        combined.contains("PASS") && combined.contains("test_add"),
        "expected nextest to PASS test_add, got:\n{combined}"
    );
}

/// `--no-fail-fast` and `--max-fail=N` reach nextest verbatim — cargo-affected
/// must not impose its own fail-fast policy on top of nextest's. Anchors the
/// pass-through contract so a future refactor of `run_tests` can't quietly
/// swallow these flags or substitute its own default.
///
/// Setup: collect with both tests passing, then break both function bodies so
/// each test fails AND each edit overlaps a stored range — selection picks
/// both, and they fail when nextest runs them. `--test-threads=1` serialises
/// execution so the contrast between fail-fast modes is deterministic (with
/// parallel workers both might already be in flight before the first failure
/// triggers a cancel).
///
/// Assertions look at ANSI-stable substrings (`Cancelling`, `test_multiply`)
/// rather than nextest's count summaries (`1/2 tests run`), which CI's
/// `CARGO_TERM_COLOR=always` sprays with escape sequences between digits.
#[test]
fn run_forwards_fail_fast_flags_to_nextest() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_run_fail_fast");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    // Break both function bodies — each edit overlaps the corresponding test's
    // stored range, so selection picks both, and they both fail under nextest.
    replace_in_file(&dir.join("src/math.rs"), "a + b", "a + b + 1");
    replace_in_file(&dir.join("src/math.rs"), "a * b", "a * b + 1");

    // Default fail-fast: nextest cancels after the first failure. With
    // --test-threads=1 the second test never starts.
    let default = cargo_affected(
        dir,
        &["affected", "run", "--", "--test-threads=1"],
    );
    assert!(
        !default.status.success(),
        "default run should fail when tests fail",
    );
    let default_out = combined_output(&default);
    assert!(
        default_out.contains("Cancelling"),
        "expected nextest to print its 'Cancelling due to test failure' \
         line on default fail-fast; got:\n{default_out}"
    );
    assert!(
        !default_out.contains("test_multiply"),
        "test_multiply must not run when default fail-fast cancels after \
         the first failure; got:\n{default_out}"
    );

    // --no-fail-fast: nextest must run both selected tests despite the first
    // failure. Pass-through proof — cargo-affected adds nothing on top.
    let nff = cargo_affected(
        dir,
        &["affected", "run", "--", "--test-threads=1", "--no-fail-fast"],
    );
    assert!(
        !nff.status.success(),
        "run with --no-fail-fast should still fail when tests fail",
    );
    let nff_out = combined_output(&nff);
    assert!(
        !nff_out.contains("Cancelling"),
        "expected --no-fail-fast to skip nextest's cancel path; got:\n{nff_out}"
    );
    assert!(
        nff_out.contains("test_add") && nff_out.contains("test_multiply"),
        "expected both failing tests to appear with --no-fail-fast; got:\n{nff_out}"
    );

    // --max-fail=2 reaches nextest the same way: both tests are attempted
    // because the second failure is still within budget. (nextest still
    // prints a 'Cancelling' line at the end since the budget is exactly hit
    // — that's expected, so we don't assert on its presence/absence here.)
    let mf2 = cargo_affected(
        dir,
        &["affected", "run", "--", "--test-threads=1", "--max-fail=2"],
    );
    let mf2_out = combined_output(&mf2);
    assert!(
        mf2_out.contains("test_add") && mf2_out.contains("test_multiply"),
        "expected --max-fail=2 to run both tests; got:\n{mf2_out}"
    );

    // Exit code is whatever nextest produced — nextest uses 100 for test
    // failures. cargo-affected must propagate it untouched.
    assert_eq!(
        nff.status.code(),
        Some(100),
        "expected nextest's test-failure exit code (100) to propagate"
    );
}

#[test]
fn run_with_no_changes_reports_nothing_to_do() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_run_clean");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    // Working tree is clean, no new tests added — `run` should short-circuit
    // before invoking nextest.
    let run = cargo_affected(dir, &["affected", "run"]);
    assert!(
        run.status.success(),
        "run on clean tree failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("nothing to run"),
        "expected 'nothing to run' message on clean tree, got:\n{stderr}"
    );
}
