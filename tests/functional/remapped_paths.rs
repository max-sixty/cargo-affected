//! A build whose recorded source paths don't sit under the project root must
//! fail the collect, not produce a coverage-free one.
//!
//! `--remap-path-prefix` rewrites the paths the compiler embeds in the
//! coverage map, so `Path::strip_prefix(canonical_root)` discards every
//! function and the binary's function map comes out empty. That used to be
//! survivable in the worst way: extraction found no ranges for any test,
//! `collect` folded in each test's crate-root sentinels anyway, and the run
//! exited 0 with a database that only ever selects tests for crate-root edits.
//! Every other edit would select nothing — silent under-selection, which the
//! tool has no way to notice afterwards.
//!
//! This is the reachable, user-facing instance of that failure: one RUSTFLAGS
//! entry. It stands in for the whole class (a symbol-name form the join
//! doesn't recognise, a map exported from a different build), all of which
//! land at the same two guards — `build_function_map` producing nothing, and
//! `hit_ranges` matching nothing.

use crate::{
    cargo_affected_with_env, combined_output, init_git_with_initial_commit,
    write_two_module_project,
};

#[test]
fn remapped_source_paths_fail_the_collect() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_remapped_paths");
    init_git_with_initial_commit(dir);

    // rustc matches the prefix against the path it records, and which form
    // that is depends on the platform: macOS records a temp dir reached via
    // `/var` under `/private/var`, so the raw path misses, while Windows
    // records the plain path cargo was handed, so the canonical form — which
    // `std::fs::canonicalize` returns as a `\\?\`-prefixed verbatim path —
    // misses instead. Offer both; rustc applies whichever matches. (Neither
    // may contain whitespace, since cargo splits RUSTFLAGS on it. Temp
    // directories don't.)
    let canonical = dir.canonicalize().unwrap();
    let canonical = canonical.to_string_lossy();
    let canonical = canonical.strip_prefix(r"\\?\").unwrap_or(&canonical);
    let remap = format!(
        "--remap-path-prefix={}=/elsewhere --remap-path-prefix={canonical}=/elsewhere",
        dir.display(),
    );
    let collect = cargo_affected_with_env(
        dir,
        &["affected", "collect"],
        &[("RUSTFLAGS", remap.as_str())],
    );

    let output = combined_output(&collect);
    assert!(
        !collect.status.success(),
        "collect must fail rather than store a sentinel-only database:\n{output}",
    );
    assert!(
        output.contains("no instrumented functions under"),
        "error should name the empty coverage map:\n{output}",
    );

    // And it failed before storing anything: the point of the guard is that a
    // sentinel-only database never reaches disk.
    let db = dir.join("target/affected/coverage.db");
    if db.exists() {
        let conn = rusqlite::Connection::open(&db).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM test_regions", [], |r| r.get(0))
            .unwrap_or(0);
        assert_eq!(rows, 0, "nothing should have been stored:\n{output}");
    }
}
