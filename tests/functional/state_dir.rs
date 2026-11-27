use crate::{
    cargo_affected, combined_output, init_git_with_initial_commit, write_two_module_project,
};

#[test]
fn independent_scopes_keep_builds_and_baselines_separate() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_two_module_project(root, "sample_independent_state");
    init_git_with_initial_commit(root);

    let collect = |scope: &str, filter: &str| {
        let output = cargo_affected(
            root,
            &["affected", "collect", "--state-dir", scope, "--", filter],
        );
        assert!(output.status.success(), "{}", combined_output(&output));
    };
    collect("target/scope-a", "test_add");
    collect("target/scope-b", "test_multiply");

    for scope in ["target/scope-a", "target/scope-b"] {
        assert!(root.join(scope).join("build").is_dir());
        assert!(root.join(scope).join("coverage.db").is_file());
    }
    assert!(!root.join("target/affected/coverage.db").exists());

    // Different scopes must not overwrite the previous collection when the
    // build flags and workspace fingerprint are otherwise identical.
    let status = |scope: &str, filter: &str| {
        let output = cargo_affected(
            root,
            &["affected", "status", "--state-dir", scope, "--", filter],
        );
        assert!(output.status.success(), "{}", combined_output(&output));
        let text = combined_output(&output);
        assert!(text.contains("cache=hit-exact"), "{text}");
        assert!(text.contains("nothing would run"), "{text}");
    };
    status("target/scope-a", "test_add");
    status("target/scope-b", "test_multiply");

    let clean = cargo_affected(
        root,
        &["affected", "--state-dir", "target/scope-a", "clean"],
    );
    assert!(clean.status.success(), "{}", combined_output(&clean));
    let result = cargo_affected(
        root,
        &["affected", "status", "--state-dir", "target/scope-a"],
    );
    assert!(combined_output(&result).contains("miss-no-coverage"));
    status("target/scope-b", "test_multiply");
}
