//! Structural-edit backstop: an edit that lands outside any tracked function
//! range (e.g. adding a `#[derive]` to a struct) must select every test that
//! covered the file. Otherwise we'd silently miss tests that depend on the
//! struct's layout or trait impls.

use crate::{
    cargo_affected, init_git_with_initial_commit, replace_in_file, write_two_module_project,
};

#[test]
fn added_derive_pulls_in_all_file_tests() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_structural");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    // Adding a derive lands between functions — no stored range overlaps,
    // so the per-hunk fallback must kick in and select every test that ever
    // touched math.rs (test_add and test_multiply). test_greet lives in
    // strings.rs and must NOT be pulled in.
    replace_in_file(
        &dir.join("src/math.rs"),
        "pub struct Counter {",
        "#[derive(Debug, Clone)]\npub struct Counter {",
    );

    let status = cargo_affected(dir, &["affected", "status", "-v"]);
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);

    assert!(
        stdout.contains("test_add"),
        "backstop: test_add should run after struct-derive edit, got:\n{stdout}"
    );
    assert!(
        stdout.contains("test_multiply"),
        "backstop: test_multiply should run after struct-derive edit \
         (no function range overlaps so file-level fallback fires), got:\n{stdout}"
    );
    assert!(
        !stdout.contains("test_greet"),
        "structural edit in math.rs shouldn't pull in strings.rs tests:\n{stdout}"
    );
}
