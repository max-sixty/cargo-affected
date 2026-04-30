//! `cargo affected collect` refuses to run on a dirty working tree by
//! default. The captured `collect_sha` points at HEAD, so stored ranges
//! filed under that sha must reflect HEAD's line numbers — not whatever the
//! working tree happened to look like at compile time.

use crate::{
    cargo_affected, init_git_with_initial_commit, replace_in_file, write_two_module_project,
};

#[test]
fn collect_refuses_dirty_tree_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_dirty_refuse");
    init_git_with_initial_commit(dir);

    // Modify a tracked source file without committing.
    replace_in_file(&dir.join("src/math.rs"), "a + b", "a + b + 0");

    let out = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        !out.status.success(),
        "collect should refuse a dirty tree, but exited 0; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("uncommitted changes") && stderr.contains("--allow-dirty"),
        "error should explain the refusal and point at --allow-dirty, got:\n{stderr}"
    );

    // No DB should have been created — we bailed before any cargo work.
    assert!(
        !dir.join("target/affected/coverage.db").exists(),
        "collect should bail before touching the DB on a dirty tree"
    );
}

#[test]
fn collect_with_allow_dirty_proceeds_with_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_dirty_allow");
    init_git_with_initial_commit(dir);

    replace_in_file(&dir.join("src/math.rs"), "a + b", "a + b + 0");

    let out = cargo_affected(dir, &["affected", "collect", "--allow-dirty"]);
    assert!(
        out.status.success(),
        "collect --allow-dirty should succeed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("dirty working tree"),
        "should warn about the dirty tree, got:\n{stderr}"
    );
}
