//! Sibling-vs-missing collect_sha behavior in `status`. A sibling sha
//! (still in the repo, just not on HEAD's lineage) keeps coverage usable —
//! `status` should report a normal selection summary, not widen to the
//! full suite. Only a sha the repo doesn't have at all (rebased and
//! garbage-collected, beyond a shallow boundary) trips the
//! "would run all tests" path.

use crate::{
    cargo_affected, git, git_head, init_git_with_initial_commit, write_two_module_project,
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

    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        !stdout.contains("would run all tests"),
        "sibling collect_sha must not trigger full-suite widening, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("not in the repo"),
        "sibling collect_sha must not be reported as missing, got:\n{stdout}"
    );
}
