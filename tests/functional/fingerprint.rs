//! Fingerprint invalidation.
//!
//! Coverage rows are scoped by `env_fingerprint` (a hash of Cargo.lock,
//! workspace Cargo.toml files, rustc version, and build flags). Changing any
//! of those inputs flips the fingerprint, so queries scoped to the new
//! fingerprint naturally read as "no data" — a structural cache miss with no
//! invalidation pass.
//!
//! After mutating Cargo.toml in a way that changes the fingerprint, status
//! must report there's no usable coverage and point the user at `collect`.

use crate::{cargo_affected, init_git_with_initial_commit, write_two_module_project};

#[test]
fn cargo_toml_change_invalidates_fingerprint() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_fingerprint");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    // Mutate the package's Cargo.toml. Any tracked field works — bumping the
    // version is the cheapest change that touches the fingerprint without
    // requiring network access for a new dep. fingerprint::compute hashes
    // the manifest contents byte-for-byte, so this is enough.
    let cargo_toml = dir.join("Cargo.toml");
    let original = std::fs::read_to_string(&cargo_toml).unwrap();
    let modified = original.replace("0.1.0", "0.2.0");
    assert_ne!(original, modified, "version bump should change Cargo.toml");
    std::fs::write(&cargo_toml, modified).unwrap();

    let status = cargo_affected(dir, &["affected", "status"]);
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);

    // status.rs prints this exact phrase when has_any_coverage() == true but
    // count == 0 for the current fingerprint.
    assert!(
        stdout.contains("no coverage data for the current environment"),
        "expected fingerprint-mismatch message, got:\n{stdout}"
    );
    assert!(
        stdout.contains("cargo affected collect"),
        "should suggest re-running collect, got:\n{stdout}"
    );
}
