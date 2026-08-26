//! `cargo affected run` actually executes only the affected tests.
//!
//! Until this scenario, the suite only exercised `status` (the dry-run view).
//! `run` invokes nextest under the hood — we capture its output and the
//! selection layer's `-v` listing to verify both that the right tests were
//! chosen AND that they actually ran successfully.

use crate::{
    cargo_affected, combined_output, git, init_git_with_initial_commit, replace_in_file,
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
    let default = cargo_affected(dir, &["affected", "run", "--", "--test-threads=1"]);
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
        &[
            "affected",
            "run",
            "--",
            "--test-threads=1",
            "--no-fail-fast",
        ],
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

/// A change *committed* since the last collect that no test covers must be
/// reported as uncovered, not as an absence of changes.
///
/// The empty-selection message used to key off `git_changed_files` — the
/// working tree alone — while selection itself diffs against `collect_sha`.
/// With a clean tree and the change one commit back, that read as "no
/// uncommitted changes … nothing to run" directly beneath the "1 commit(s)
/// since collect" notice, hiding the one fact the user needed: a file changed
/// and nothing tests it. `README.md` is the shape that makes it visible —
/// non-Rust, so it has no coverage rows to hit and no structural backstop to
/// over-select through, which is exactly the blind spot
/// `[workspace.metadata.affected]` rules exist to close.
#[test]
fn run_reports_committed_uncovered_change_as_uncovered() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_run_committed_uncovered");
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    // Commit the edit, so `git status` is clean but HEAD is one commit ahead
    // of the collect_sha.
    std::fs::write(dir.join("README.md"), "hello world\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "edit README"]);

    for (cmd, out) in [
        ("run", cargo_affected(dir, &["affected", "run"])),
        ("status", cargo_affected(dir, &["affected", "status"])),
    ] {
        assert!(
            out.status.success(),
            "{cmd} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = combined_output(&out);
        assert!(
            text.contains("no tests cover the changed lines"),
            "expected {cmd} to report the committed README change as uncovered, got:\n{text}"
        );
        assert!(
            !text.contains("no changes since the last collect"),
            "expected {cmd} not to claim nothing changed, got:\n{text}"
        );
    }
}

/// The `collect --diff` steady state reports *no* changes — the counterpart to
/// [`run_reports_committed_uncovered_change_as_uncovered`], and the reason the
/// message can't key off the changed-path union.
///
/// `collect --diff` re-anchors only the tests it reran, so the older
/// `collect_sha` stays reachable (its rows linger until `cargo affected
/// clean`) and every path touched since it stays in the union permanently.
/// Keying the message off that union told the user "no tests cover the changed
/// lines … run `cargo affected collect`" on every clean-tree `run` after a
/// `--diff` — for a file that *is* covered, by a test that had just been
/// re-collected. `since_newest` diffs against the reachable sha closest to
/// HEAD instead, which is empty here because the last collect was at HEAD.
///
/// This is CLAUDE.md's "Manual testing" sequence verbatim, so it is the state
/// an incremental user sits in between edits.
#[test]
fn run_after_diff_collect_reports_no_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_run_after_diff_collect");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    // Edit and commit a covered function, then update only the affected rows.
    // `test_greet` re-anchors at the new HEAD; `test_add` and `test_multiply`
    // stay at the initial sha, which is what leaves two shas reachable.
    //
    // `strings.rs` rather than `math.rs` because `test_greet` is the only test
    // with rows for it: once `--diff` moves those rows to the new sha, no test
    // has a `strings.rs` row at the initial sha, so the structural backstop —
    // "hunk overlaps no stored range, so select every test with rows for this
    // file at this sha" — has nothing to fire on and the selection is genuinely
    // empty. Editing `math.rs` would instead pull in `test_multiply`, still
    // anchored at the initial sha, and never reach the message under test. The
    // rewrite is behaviour-preserving so the `--diff` rerun stays green.
    replace_in_file(
        &dir.join("src/strings.rs"),
        r#"format!("hello, {name}")"#,
        r#"format!("hello, {}", name)"#,
    );
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "rewrite greet"]);
    let diff = cargo_affected(dir, &["affected", "collect", "--diff"]);
    assert!(
        diff.status.success(),
        "collect --diff failed: {}",
        String::from_utf8_lossy(&diff.stderr)
    );

    for (cmd, out) in [
        ("run", cargo_affected(dir, &["affected", "run"])),
        ("status", cargo_affected(dir, &["affected", "status"])),
    ] {
        assert!(
            out.status.success(),
            "{cmd} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = combined_output(&out);
        assert!(
            text.contains("no changes since the last collect"),
            "expected {cmd} to report the post-`--diff` tree as unchanged, got:\n{text}"
        );
        assert!(
            !text.contains("no tests cover the changed lines"),
            "expected {cmd} not to call the re-collected change uncovered, got:\n{text}"
        );
    }
}
