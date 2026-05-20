//! New-test detection: a test added since the last `collect` has no coverage
//! data, so it can't be range-matched. The selection layer is supposed to
//! list it via nextest and tag it `(new)` in the verbose output.
//!
//! Two failure modes are guarded here. A perpetually-`#[ignore]`d test is
//! always listed but never collected, so it must not read as "new" forever.
//! And the listing must build with the same cargo features as the run, or a
//! feature-gated new test is invisible to detection.

use std::path::Path;

use crate::{
    cargo_affected, combined_output, init_git_with_initial_commit, replace_in_file,
    write_two_module_project,
};

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

/// An always-`#[ignore]`d test is never run by `cargo nextest run`, so it
/// never gains a coverage row — yet it is always present in `cargo nextest
/// list`. New-test detection ("listed minus DB") must not flag it `new` on
/// every run: that re-selects a test nextest only skips, and a selection of
/// nothing but ignored tests makes `nextest run` exit non-zero.
#[test]
fn ignored_test_not_perpetually_new() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_ignored_test_project(dir, "sample_ignored_new");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    // Clean tree: the only test missing from the DB is the ignored one. It
    // must NOT surface as a new test.
    let status = cargo_affected(dir, &["affected", "status", "-v"]);
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let combined = combined_output(&status);
    assert!(
        !combined.contains("(new)"),
        "an ignored test must not be flagged (new), got:\n{combined}"
    );
    assert!(
        !combined.contains("test_ignored"),
        "the ignored test must not be selected, got:\n{combined}"
    );

    // Removing #[ignore] makes the test runnable — now it genuinely has no
    // coverage and SHOULD be picked up as new.
    replace_in_file(&dir.join("src/lib.rs"), "    #[ignore]\n", "");
    let status = cargo_affected(dir, &["affected", "status", "-v"]);
    assert!(
        status.status.success(),
        "status after un-ignore failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let combined = combined_output(&status);
    assert!(
        combined.contains("test_ignored (new)"),
        "a test that stops being ignored must be picked up as new, got:\n{combined}"
    );
}

/// `cargo affected run` listed tests with no extra features while running
/// them with the user's `--features`, so a feature-gated test added since
/// the last collect was invisible to new-test detection and silently
/// skipped. The listing must build with the run's cargo features.
#[test]
fn new_test_detection_uses_run_features() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_feature_gated_test_project(dir, "sample_feature_listing");
    init_git_with_initial_commit(dir);

    // Collect with no features — only the base test is built and recorded.
    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    // Run with --features extra on a clean tree. test_extra exists only in
    // the featured build and is absent from the DB, so it is a new test —
    // but only if the listing is built with --features too.
    let run = cargo_affected(dir, &["affected", "run", "-v", "--", "--features", "extra"]);
    assert!(
        run.status.success(),
        "run failed: stderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&run.stderr),
        String::from_utf8_lossy(&run.stdout)
    );
    let combined = combined_output(&run);
    assert!(
        combined.contains("test_extra (new)"),
        "the feature-gated test must be detected as new, got:\n{combined}"
    );
    assert!(
        combined.contains("PASS") && combined.contains("test_extra"),
        "nextest must actually run the feature-gated test, got:\n{combined}"
    );
    assert!(
        !combined.contains("test_base"),
        "test_base is in the DB and unaffected — it must not run, got:\n{combined}"
    );
}

/// Single-crate project with one normal test and one always-`#[ignore]`d
/// test. The ignored test is never collected (nextest skips it), so it is
/// the canonical "in the listing, absent from the DB" case.
fn write_ignored_test_project(dir: &Path, crate_name: &str) {
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{crate_name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.join(".gitignore"), "/target\n/Cargo.lock\n").unwrap();
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        r#"pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add() {
        assert_eq!(add(1, 2), 3);
    }

    #[test]
    #[ignore]
    fn test_ignored() {
        assert_eq!(add(0, 0), 0);
    }
}
"#,
    )
    .unwrap();
}

/// Single-crate project with a base test and a `#[cfg(feature = "extra")]`
/// test. Without `--features extra` the gated test is not compiled and does
/// not appear in `cargo nextest list`.
fn write_feature_gated_test_project(dir: &Path, crate_name: &str) {
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{crate_name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [features]\nextra = []\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.join(".gitignore"), "/target\n/Cargo.lock\n").unwrap();
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        r#"pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base() {
        assert_eq!(add(1, 1), 2);
    }

    #[cfg(feature = "extra")]
    #[test]
    fn test_extra() {
        assert_eq!(add(2, 2), 4);
    }
}
"#,
    )
    .unwrap();
}
