//! New-test detection: a test added since the last `collect` has no coverage
//! data, so it can't be range-matched. The selection layer is supposed to
//! list it via nextest and tag it `(new)` in the verbose output.

use crate::{cargo_affected, init_git_with_initial_commit, write_two_module_project};

#[test]
fn new_integration_test_is_selected_and_tagged() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_new_test");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    // Add a test that didn't exist at collect time. It has no row in the DB,
    // so the only way for it to surface is via the nextest-list new-test path.
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(
        dir.join("tests/integration_new.rs"),
        "#[test]\nfn test_brand_new() {\n    assert_eq!(1 + 1, 2);\n}\n",
    )
    .unwrap();

    let status = cargo_affected(dir, &["affected", "status", "-v"]);
    assert!(
        status.status.success(),
        "status (with new test) failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);

    assert!(
        stdout.contains("test_brand_new"),
        "status should list the new test, got:\n{stdout}"
    );
    assert!(
        stdout.contains("(new)"),
        "status should tag new tests with (new), got:\n{stdout}"
    );
    assert!(
        stdout.contains("1 new"),
        "status should report 1 new test in the summary, got:\n{stdout}"
    );
}
