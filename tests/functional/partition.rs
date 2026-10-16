//! `--partition` is applied once, by `cargo nextest run`, to the selection —
//! never again by the listing that produced it.
//!
//! nextest's listing honours `--partition` too, tagging the tests outside the
//! shard's bucket `filter-match: { status: "mismatch", reason: "partition" }`.
//! Forwarded to both halves, the flag splits the test set twice over
//! different positions, and `count:`/`slice:` shards that should together
//! cover the whole selection instead cover part of it — exiting 0 while
//! affected tests never run. This scenario pins the union.

use std::collections::BTreeSet;
use std::path::Path;

use crate::{cargo_affected, combined_output, init_git_with_initial_commit, replace_in_file};

/// Four tests, all covering one function, so a single edit makes the whole
/// suite affected and the selection is the same set every shard starts from.
const TEST_NAMES: [&str; 4] = ["alpha_case", "beta_case", "gamma_case", "delta_case"];

#[test]
fn partition_shards_together_run_the_whole_selection() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_four_test_project(dir, "sample_partition");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    // Every test calls `shared`, so this edit makes all four affected while
    // leaving them passing.
    replace_in_file(&dir.join("src/work.rs"), "41 + 1", "41 + 1 /* edited */");

    // Baseline: the unpartitioned run is what the shards have to add up to.
    let full = run_shard(dir, None);
    assert_eq!(
        full,
        TEST_NAMES.iter().map(|n| n.to_string()).collect(),
        "an edit to `shared` should make all four tests affected"
    );

    let first = run_shard(dir, Some("count:1/2"));
    let second = run_shard(dir, Some("count:2/2"));

    // The regression: pre-fix each shard ran the tests in its bucket *of its
    // bucket*, so the union was half the selection and both commands exited 0.
    let union: BTreeSet<String> = first.union(&second).cloned().collect();
    assert_eq!(
        union, full,
        "count:1/2 and count:2/2 together must run the whole unpartitioned \
         selection; shard 1 ran {first:?}, shard 2 ran {second:?}"
    );

    // …and the flag still reaches the run, so the shards are a real split
    // rather than two full runs.
    assert!(
        first.is_disjoint(&second),
        "shards must not overlap; shard 1 ran {first:?}, shard 2 ran {second:?}"
    );
    for (label, shard) in [("count:1/2", &first), ("count:2/2", &second)] {
        assert!(
            !shard.is_empty() && *shard != full,
            "{label} should run a proper, non-empty subset of the selection, got {shard:?}"
        );
    }
}

/// Run `cargo affected run`, optionally partitioned, and report which tests
/// nextest actually executed.
///
/// The selection is asserted here rather than in the caller: it is the direct
/// reading of the bug — pre-fix a partitioned run reported `2 tests to run`
/// where the unpartitioned one reported 4, because the listing had already
/// dropped the other shard's tests as `filter-match: mismatch`.
///
/// Test names are read back out of nextest's own output. Without `-v`,
/// cargo-affected prints counts but no names (`selection::format_summary`),
/// so a name in the output can only have come from nextest running it.
fn run_shard(dir: &Path, partition: Option<&str>) -> BTreeSet<String> {
    let mut args = vec!["affected", "run"];
    if let Some(spec) = partition {
        args.extend_from_slice(&["--", "--partition", spec]);
    }
    let out = cargo_affected(dir, &args);
    let combined = combined_output(&out);
    assert!(out.status.success(), "run {args:?} failed:\n{combined}");
    assert!(
        combined.contains("4 tests to run"),
        "the selection must be the full affected set regardless of \
         --partition ({partition:?}), got:\n{combined}"
    );
    TEST_NAMES
        .iter()
        .filter(|name| combined.contains(**name))
        .map(|name| name.to_string())
        .collect()
}

/// A single-crate project whose four tests all call one function in a
/// non-crate-root module, so the edit selects them through real coverage
/// ranges rather than the crate-root sentinel.
fn write_four_test_project(dir: &Path, crate_name: &str) {
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2021"
"#
        ),
    )
    .unwrap();
    std::fs::write(dir.join(".gitignore"), "/target\n/Cargo.lock\n").unwrap();

    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("lib.rs"), "pub mod work;\n").unwrap();

    let tests: String = TEST_NAMES
        .iter()
        .map(|name| {
            format!(
                "
    #[test]
    fn {name}() {{
        assert_eq!(shared(), 42);
    }}
"
            )
        })
        .collect();
    std::fs::write(
        src.join("work.rs"),
        format!(
            r#"pub fn shared() -> i32 {{
    41 + 1
}}

#[cfg(test)]
mod tests {{
    use super::*;
{tests}}}
"#
        ),
    )
    .unwrap();
}
