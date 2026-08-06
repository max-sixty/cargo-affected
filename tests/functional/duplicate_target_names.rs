//! Workspaces where two integration-test targets share a name.
//!
//! Two crates each ship `tests/builds.rs` — same cargo target name, so the
//! produced binaries are `target/debug/deps/builds-<hash1>` and
//! `target/debug/deps/builds-<hash2>`. The basename is ambiguous; only the
//! `(package, target, kind)` triple cargo emits in its JSON identifies them
//! uniquely.
//!
//! `-C debuginfo=0` is set throughout. Many CI configs strip debug info, and
//! it removes the source-path strings the older runner-shim's "byte-marker"
//! fallback used to fingerprint each binary. This scenario asserts the
//! triple-based lookup handles the duplicate-basename + stripped-debuginfo
//! combination — the failure mode worktrunk hit in CI before the fix.

use std::path::Path;
use std::process::{Command, Output};

use crate::{combined_output, git, init_git_with_initial_commit, replace_in_file};

/// Two-member virtual workspace, both with `tests/builds.rs` containing a
/// single `#[test] fn builds() {}`. Identical bodies on purpose — there's
/// nothing the shim could content-fingerprint even if debuginfo were on.
fn write_duplicate_target_workspace(dir: &Path, ws_name: &str) {
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"[workspace]
resolver = "2"
members = ["mock-stub", "wt-perf"]
"#,
    )
    .unwrap();
    std::fs::write(dir.join(".gitignore"), "/target\n/Cargo.lock\n").unwrap();

    for sub in ["mock-stub", "wt-perf"] {
        let pkg = dir.join(sub);
        std::fs::create_dir_all(pkg.join("src")).unwrap();
        std::fs::create_dir_all(pkg.join("tests")).unwrap();
        // Distinct package names so cargo's per-package shared cache doesn't
        // merge them. Underscored to match cargo's package-name rules.
        std::fs::write(
            pkg.join("Cargo.toml"),
            format!(
                r#"[package]
name = "{ws_name}_{}"
version = "0.1.0"
edition = "2021"
"#,
                sub.replace('-', "_"),
            ),
        )
        .unwrap();
        std::fs::write(pkg.join("src/lib.rs"), "pub fn touch() {}\n").unwrap();
        std::fs::write(pkg.join("tests/builds.rs"), "#[test]\nfn builds() {}\n").unwrap();
    }
}

/// Run cargo-affected with `RUSTFLAGS='-C debuginfo=0'` so the embedded
/// source paths the old marker fallback relied on aren't there to cheat with.
fn cargo_affected_stripped(dir: &Path, args: &[&str]) -> Output {
    let bin = env!("CARGO_BIN_EXE_cargo-affected");
    Command::new(bin)
        .args(args)
        .current_dir(dir)
        .env("RUSTFLAGS", "-C debuginfo=0")
        .output()
        .unwrap_or_else(|e| panic!("failed to run cargo-affected: {e}"))
}

#[test]
fn duplicate_basename_with_stripped_debuginfo_resolves_correctly() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_duplicate_target_workspace(dir, "dup_strip");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected_stripped(dir, &["affected", "collect"]);
    let collect_stderr = String::from_utf8_lossy(&collect.stderr);
    assert!(
        collect.status.success(),
        "collect failed: stderr=\n{collect_stderr}\nstdout=\n{}",
        String::from_utf8_lossy(&collect.stdout)
    );
    // A `binary_id` the shim can't resolve is a *soft* failure: that test
    // lands as `Skipped` and collect still exits 0 on whatever the other
    // binary produced, so `status.success()` above doesn't cover it.
    // `collect` prints this line for any skip.
    assert!(
        !collect_stderr.contains("produced no coverage"),
        "duplicate-basename binaries must not cost a test its coverage; stderr:\n{collect_stderr}"
    );

    // Both crates' `builds` test must land under DISTINCT binary_ids — the
    // whole point of the fix. If the shim merged them, only one binary_id
    // would appear (and one set of regions would silently overwrite the
    // other).
    let db = dir.join("target/affected/coverage.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let ids: Vec<String> = conn
        .prepare("SELECT DISTINCT binary_id FROM test_regions WHERE test_name = 'builds'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        ids.len(),
        2,
        "expected two distinct binary_ids for `builds` test, got {ids:?}"
    );
    // Package names are the directory names with `-` → `_` (cargo
    // requirement), prefixed with the workspace tag; binary_id format is
    // `<package>::<target>`.
    assert!(
        ids.iter().any(|id| id == "dup_strip_mock_stub::builds"),
        "expected mock-stub binary_id in {ids:?}"
    );
    assert!(
        ids.iter().any(|id| id == "dup_strip_wt_perf::builds"),
        "expected wt-perf binary_id in {ids:?}"
    );

    // Force a rebuild of one crate's `builds` binary by editing its lib —
    // cargo re-hashes that binary, leaving the other unchanged. Under the
    // old shim this was the worst-case path: ambiguous basename, debuginfo
    // stripped, partial rebuild. Under the new shim the pre-run listing
    // refreshes paths, so the lookup remains exact.
    replace_in_file(
        &dir.join("mock-stub/src/lib.rs"),
        "pub fn touch() {}",
        "pub fn touch() {} // edited",
    );

    let run = cargo_affected_stripped(dir, &["affected", "run", "-v"]);
    let run_stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        run.status.success(),
        "run failed: stderr=\n{run_stderr}\nstdout=\n{}",
        String::from_utf8_lossy(&run.stdout)
    );
    let combined = combined_output(&run);
    // The mock-stub `builds` test must run because its package's lib changed
    // (per-target sentinels include the package's own lib root). The
    // wt-perf `builds` test must NOT — its package is independent.
    assert!(
        combined.contains("dup_strip_mock_stub::builds"),
        "expected mock-stub builds test selected, got:\n{combined}"
    );
    assert!(
        !combined.contains("dup_strip_wt_perf::builds"),
        "wt-perf builds test must not be selected (its package didn't change), got:\n{combined}"
    );

    // A second collect after the edit exercises the full pipeline again —
    // pre-run listing must still produce a working binary_map. Commit the
    // edit first so collect's clean-tree gate is satisfied; the gate exists
    // because stored ranges would otherwise be filed under HEAD but reflect
    // the dirty working tree.
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "edit mock-stub touch"]);
    let recollect = cargo_affected_stripped(dir, &["affected", "collect"]);
    let recollect_stderr = String::from_utf8_lossy(&recollect.stderr);
    assert!(
        recollect.status.success(),
        "second collect after edit failed: stderr=\n{recollect_stderr}\nstdout=\n{}",
        String::from_utf8_lossy(&recollect.stdout)
    );
    assert!(
        !recollect_stderr.contains("produced no coverage"),
        "post-edit collect must not drop a test's coverage; stderr:\n{recollect_stderr}"
    );

    git(dir, &["checkout", "--", "mock-stub/src/lib.rs"]);
}
