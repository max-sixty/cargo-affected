//! `[lib]` and `[[bin]]` in the same crate whose target names normalize
//! to the same compiled basename.
//!
//! Cargo emits `wt_perf-<hash>` for both the lib's unit-test binary and
//! the bin's unit-test binary. The basename collides; the runner shim
//! used to bail with `basename fallback ambiguous — marker probe matched
//! 0 of them` because lib/bin candidates couldn't be told apart by path
//! alone (issue #13).
//!
//! Resolved by reading `NEXTEST_BINARY_ID` straight from the env at test
//! invocation. Nextest ≥ 0.9.116 sets it per test; the shim no longer
//! needs to map paths or probe binaries.
//!
//! `-C debuginfo=0` to mirror the worktrunk CI environment that originally
//! tripped the bug.

use std::path::Path;
use std::process::{Command, Output};

use crate::{combined_output, init_git_with_initial_commit};

/// Single-crate project with a `[lib]` and `[[bin]]` whose target names
/// (`wt_perf` and `wt-perf`) both normalize to `wt_perf` after cargo's
/// hyphen-to-underscore. The bin uses the lib AND has its own
/// `#[cfg(test)]` block, so both produce test binaries with the colliding
/// `wt_perf-<hash>` basename.
fn write_lib_bin_collision(dir: &Path) {
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
name = "wt_perf_collide"
version = "0.1.0"
edition = "2021"

[lib]
name = "wt_perf"
path = "src/lib.rs"

[[bin]]
name = "wt-perf"
path = "src/main.rs"
"#,
    )
    .unwrap();
    std::fs::write(dir.join(".gitignore"), "/target\n/Cargo.lock\n").unwrap();

    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();

    // Lib has its own unit tests so cargo emits a kind=lib test binary.
    std::fs::write(
        src.join("lib.rs"),
        r#"pub fn double(x: i32) -> i32 { x * 2 }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lib_test_double() {
        assert_eq!(double(3), 6);
    }
}
"#,
    )
    .unwrap();

    // Bin uses the lib AND has its own #[cfg(test)] block, so cargo emits a
    // kind=bin test binary.
    std::fs::write(
        src.join("main.rs"),
        r#"fn main() {
    println!("{}", wt_perf::double(21));
}

#[cfg(test)]
mod tests {
    #[test]
    fn bin_test_invokes_lib() {
        assert_eq!(wt_perf::double(7), 14);
    }
}
"#,
    )
    .unwrap();
}

/// Run cargo-affected with `RUSTFLAGS='-C debuginfo=0'` so the issue's
/// production-like stripped-binary case is exercised, not just the
/// debug-info-rich default.
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
fn lib_bin_same_basename_resolves_via_nextest_binary_id() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_lib_bin_collision(dir);
    init_git_with_initial_commit(dir);

    let collect = cargo_affected_stripped(dir, &["affected", "collect"]);
    let stderr = String::from_utf8_lossy(&collect.stderr);
    assert!(
        collect.status.success(),
        "collect failed: stderr=\n{stderr}\nstdout=\n{}",
        String::from_utf8_lossy(&collect.stdout)
    );
    assert!(
        !stderr.contains("failed to resolve binary_id"),
        "shim must not bail on lib+bin same-basename: stderr=\n{stderr}",
    );
    assert!(
        !stderr.contains("basename fallback ambiguous"),
        "marker probe must disambiguate lib+bin: stderr=\n{stderr}",
    );

    // Both targets must land under their own binary_ids — nextest's
    // `<package>` for the lib and `<package>::bin/<name>` for the bin.
    let db = dir.join("target/affected/coverage.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let ids: Vec<String> = conn
        .prepare("SELECT DISTINCT binary_id FROM test_regions")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(
        ids.iter().any(|id| id == "wt_perf_collide"),
        "expected lib binary_id in {ids:?}",
    );
    assert!(
        ids.iter().any(|id| id == "wt_perf_collide::bin/wt-perf"),
        "expected bin binary_id in {ids:?}",
    );

    // A second collect re-runs the full pipeline — confirms `binary_id`
    // resolution stays stable run-to-run, not just on a cold target/.
    let recollect = cargo_affected_stripped(dir, &["affected", "collect"]);
    let combined = combined_output(&recollect);
    assert!(
        recollect.status.success(),
        "second collect failed: {combined}"
    );
    assert!(
        !combined.contains("failed to resolve binary_id"),
        "second collect must not regress: {combined}",
    );
}
