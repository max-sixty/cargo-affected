//! Sibling-vs-missing collect_sha behavior in `status`. A sibling sha
//! (still in the repo, just not on HEAD's lineage) keeps coverage usable —
//! `status` should report a normal selection summary, not widen to the
//! full suite. Only a sha the repo doesn't have at all (rebased and
//! garbage-collected, beyond a shallow boundary) trips the
//! "would run all tests" path.

use crate::{
    cargo_affected, combined_output, git, git_head, init_git_with_initial_commit,
    write_two_module_project,
};

#[test]
fn sibling_collect_sha_status_uses_selection() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_drift");
    init_git_with_initial_commit(dir);
    let init_sha = git_head(dir);

    // Make a second commit so we have something to collect against and
    // something to reset back from. After collect, we'll reset HEAD back to
    // the initial commit — the second commit's sha (where collect ran)
    // becomes a sibling: still in the repo, not an ancestor of HEAD.
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

    git(dir, &["reset", "--hard", "-q", &init_sha]);

    let status = cargo_affected(dir, &["affected", "status"]);
    assert!(
        status.status.success(),
        "status with sibling collect_sha should succeed, got failure:\nstdout=\n{}\nstderr=\n{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );

    let combined = combined_output(&status);
    assert!(
        !combined.contains("would run all tests"),
        "sibling collect_sha must not trigger full-suite widening, got:\n{combined}"
    );
    assert!(
        !combined.contains("not in the repo"),
        "sibling collect_sha must not be reported as missing, got:\n{combined}"
    );
    // Those two negatives hold just as well on an *empty* selection: that
    // path prints "no tests cover the changed lines and no new tests" and
    // neither forbidden string, so on its own the scenario would stay green
    // through a sibling sha that stopped anchoring the diff at all — silent
    // under-selection, the failure mode `tests/CLAUDE.md` calls out as
    // undetectable downstream. Pin the two halves the name claims
    // positively: the cache stayed usable, and it produced a selection.
    assert!(
        combined.contains("cache=hit-with-divergence"),
        "sibling collect_sha must leave the cache usable and be classified \
         as diverged, got:\n{combined}"
    );
    assert!(
        combined.contains("tests would run"),
        "sibling collect_sha must still select the tests covering the diff, \
         got:\n{combined}"
    );
}

/// The other half of the pair. The missing-sha notice itself is already pinned
/// positively by `run_unions_affected_and_stranded_when_sha_is_missing` in
/// `diff_collect.rs` — but only on the partial-divergence path, where a second
/// sha survives and the lost one's tests come back as `stranded`. The branch
/// where the *only* collect_sha is gone is what nothing covers: it reaches
/// `CacheMiss::NoReachableSha` and widens to the full suite, and
/// `"would run all tests"` is asserted positively nowhere else in the suite.
#[test]
fn missing_collect_sha_status_widens_to_all_tests() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_drift_missing");
    init_git_with_initial_commit(dir);
    let init_sha = git_head(dir);

    std::fs::write(dir.join("src/extra.rs"), "pub fn extra() -> i32 { 1 }\n").unwrap();
    let lib_path = dir.join("src/lib.rs");
    let lib = std::fs::read_to_string(&lib_path).unwrap();
    std::fs::write(&lib_path, format!("{lib}pub mod extra;\n")).unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "add extra module"]);
    let collect_commit = git_head(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    // Resetting alone only orphans the commit — the reflog still holds it, so
    // `git cat-file -e` succeeds and `relation_to_head` reports Reachable (the
    // sibling case above). Expiring the reflog and pruning is what actually
    // deletes the object, which is the "rebased and garbage-collected" shape
    // the missing branch is written for.
    git(dir, &["reset", "--hard", "-q", &init_sha]);
    git(dir, &["reflog", "expire", "--expire=now", "--all"]);
    git(dir, &["gc", "--prune=now", "--quiet"]);
    let gone = std::process::Command::new("git")
        .args(["cat-file", "-e", &format!("{collect_commit}^{{commit}}")])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        !gone.status.success(),
        "collect_sha {collect_commit} survived the prune, so this scenario \
         would exercise the sibling path instead of the missing one"
    );

    let status = cargo_affected(dir, &["affected", "status"]);
    assert!(
        status.status.success(),
        "status with a missing collect_sha should succeed, got failure:\nstdout=\n{}\nstderr=\n{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );

    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("not in the repo"),
        "a pruned collect_sha must be reported as missing, got:\n{stdout}"
    );
    assert!(
        stdout.contains("would run all tests"),
        "the only collect_sha being missing must widen to the full suite, got:\n{stdout}"
    );
    // The missing-sha notice is shared with the case where *some* sha still
    // anchors a diff, and there its tests do come back as the `stranded`
    // selection category. Here there is no selection at all, so promising
    // `stranded` contradicts the widening notice printed right after it.
    assert!(
        !stdout.contains("stranded"),
        "with no reachable sha nothing is selected, so the missing-sha \
         notice must not promise the 'stranded' category, got:\n{stdout}"
    );
    assert!(
        stdout.contains("would rerun as part of the full suite"),
        "the missing-sha notice should say the full suite covers those \
         tests, got:\n{stdout}"
    );
}
