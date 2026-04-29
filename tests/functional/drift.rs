//! collect_sha drift: when the recorded `collect_sha` is no longer reachable
//! from HEAD (rebased away, branch switched, history rewritten), stored line
//! numbers no longer share a coordinate system with the working tree.
//! `status` mirrors `run`'s widening — it reports "would run all tests" with
//! a re-anchor hint, so the dry-run accurately predicts what `run` would do.

use crate::{
    cargo_affected, git, git_head, init_git_with_initial_commit, write_two_module_project,
};

#[test]
fn unreachable_collect_sha_status_reports_full_suite() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_drift");
    init_git_with_initial_commit(dir);
    let init_sha = git_head(dir);

    // Make a second commit so we have something to collect against and
    // something to reset back from. After collect, we'll reset HEAD back to
    // the initial commit — the second commit's sha (where collect ran) will
    // become unreachable.
    std::fs::write(
        dir.join("src/extra.rs"),
        "pub fn extra() -> i32 { 1 }\n",
    )
    .unwrap();
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

    // Reset HEAD back to the initial commit. The collect_sha (collect_commit)
    // is now an orphaned object — present in the repo, but not an ancestor
    // of HEAD. relation_to_head will return Diverged.
    git(dir, &["reset", "--hard", "-q", &init_sha]);

    let status = cargo_affected(dir, &["affected", "status"]);
    assert!(
        status.status.success(),
        "status should succeed and report full-suite widening, got failure:\nstdout=\n{}\nstderr=\n{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );

    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("not reachable from HEAD"),
        "expected 'not reachable from HEAD' in notice, got:\n{stdout}"
    );
    assert!(
        stdout.contains("would run all tests"),
        "expected 'would run all tests' in notice, got:\n{stdout}"
    );
    assert!(
        stdout.contains("re-anchor") || stdout.contains("collect"),
        "expected re-anchor / collect hint in notice, got:\n{stdout}"
    );
}
