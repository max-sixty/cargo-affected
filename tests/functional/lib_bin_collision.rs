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

/// The lib and the bin must each hold rows under their own `binary_id`. A
/// merged or dropped target loses one of the two; `when` names which collect
/// the check follows so a failure points at the right one.
fn assert_both_binary_ids(dir: &Path, when: &str) {
    let conn = rusqlite::Connection::open(dir.join("target/affected/coverage.db")).unwrap();
    let mut stmt = conn
        .prepare("SELECT DISTINCT binary_id FROM test_regions")
        .unwrap();
    let ids: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(
        ids.iter().any(|id| id == "wt_perf_collide"),
        "expected lib binary_id {when}, got {ids:?}",
    );
    assert!(
        ids.iter().any(|id| id == "wt_perf_collide::bin/wt-perf"),
        "expected bin binary_id {when}, got {ids:?}",
    );
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
    // Every test must produce coverage. A `binary_id` the shim can't resolve
    // is a *soft* failure — that test lands as `Skipped` and collect still
    // exits 0 on the other binary's rows — so the exit status above proves
    // nothing on its own. `collect` prints this line for any skip. On this
    // first collect the binary_id assertions below would also catch a drop;
    // this line just fails earlier, with a clearer reason.
    assert!(
        !stderr.contains("produced no coverage"),
        "lib+bin same-basename must not cost a target its coverage: stderr=\n{stderr}",
    );

    // Both targets must land under their own binary_ids — nextest's
    // `<package>` for the lib and `<package>::bin/<name>` for the bin.
    assert_both_binary_ids(dir, "after the first collect");

    // A second collect repeats the whole pipeline on a warm `target/` —
    // confirms the two targets stay separately attributed run-to-run, not
    // just on a cold build. Neither half of that attribution goes through a
    // path probe any more: which target a test belongs to comes from
    // `NEXTEST_BINARY_ID`, and each target's function map is filed under its
    // own `wt_perf-<hash>` file name, whose hash differs even though the
    // stem collides.
    let recollect = cargo_affected_stripped(dir, &["affected", "collect"]);
    let combined = combined_output(&recollect);
    assert!(
        recollect.status.success(),
        "second collect failed: {combined}"
    );
    assert!(
        !combined.contains("produced no coverage"),
        "second collect must not regress: {combined}",
    );
    // Re-check the database, not just stderr. The assertion above greps a
    // `collect` message, so a reword of it would silently retire the guard —
    // exactly the drift this scenario has already suffered once. The row
    // check pins the property itself.
    assert_both_binary_ids(dir, "after the second collect");
}
