//! `cargo affected collect --diff`: incremental collect that reruns only
//! tests affected by changes since one of the stored `collect_sha`s and
//! re-anchors their rows at the new HEAD, leaving unaffected tests' rows
//! at their original sha.
//!
//! This file exercises the headline guarantee — multiple shas can coexist
//! in `test_regions` for the same fingerprint after `--diff` — along with
//! the row-maintenance and selection behaviors built on it: re-anchoring
//! only affected tests, accumulating distinct shas across rounds, pruning
//! deleted/renamed tests while keeping merely-ignored ones, recovering from
//! an all-phantom selection, and short-circuiting a clean tree. A few `run`
//! cases live here too, since they exercise the multi-sha state `--diff`
//! creates: a sibling sha is reachable rather than fatal (#9), and a single
//! diverged sha only strands its own tests instead of widening the run.
//! The one hard error covered is the "fail loudly" no-prior-collect path.

use crate::{
    cargo_affected, combined_output, git, git_head, init_git_with_initial_commit, replace_in_file,
    write_two_module_project,
};

/// Distinct collect_shas that anchor `test_name`'s rows. Sorted for stable
/// assertion errors.
fn shas_for_test(db_path: &std::path::Path, test_name: &str) -> Vec<String> {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let mut stmt = conn
        .prepare("SELECT DISTINCT collect_sha FROM test_regions WHERE test_name = ?1")
        .unwrap();
    let mut shas: Vec<String> = stmt
        .query_map([test_name], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    shas.sort();
    shas
}

#[test]
fn diff_collect_re_anchors_only_affected_tests() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_diff_collect_reanchor");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "initial collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );
    let initial_sha = git_head(dir);

    // Edit `add`'s body and commit so HEAD moves. The edit overlaps test_add's
    // stored range only — test_multiply and test_greet shouldn't be touched.
    replace_in_file(&dir.join("src/math.rs"), "a + b", "a + b /* edited */");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "edit add"]);
    let edited_sha = git_head(dir);
    assert_ne!(initial_sha, edited_sha);

    let diff = cargo_affected(dir, &["affected", "collect", "--diff"]);
    assert!(
        diff.status.success(),
        "collect --diff failed: stderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&diff.stderr),
        String::from_utf8_lossy(&diff.stdout),
    );

    // The re-collect summary should pick exactly one test (test_add).
    let combined = combined_output(&diff);
    assert!(
        combined.contains("1 tests to recollect"),
        "expected '1 tests to recollect' in diff output, got:\n{combined}"
    );

    // DB invariant: rerun test now anchored at edited_sha; the others remain
    // at initial_sha. Two distinct shas coexist in test_regions — the whole
    // point of `--diff`. Test names in `test_regions` are the full nextest
    // paths (`<module>::tests::<fn>`), not the leaf identifiers.
    let db_path = dir.join("target/affected/coverage.db");
    assert_eq!(
        shas_for_test(&db_path, "math::tests::test_add"),
        vec![edited_sha.clone()],
        "test_add should be re-anchored at the new HEAD",
    );
    assert_eq!(
        shas_for_test(&db_path, "math::tests::test_multiply"),
        vec![initial_sha.clone()],
        "test_multiply unchanged → rows stay at initial_sha",
    );
    assert_eq!(
        shas_for_test(&db_path, "strings::tests::test_greet"),
        vec![initial_sha.clone()],
        "test_greet (strings.rs untouched) should stay at initial_sha",
    );
}

/// Three modules, one test each: lets us drive the multi-sha test below
/// without the structural-edit backstop pulling extra tests in. The shared
/// `write_two_module_project` puts both `add` and `multiply` in `math.rs`,
/// which causes round-2 hunks against the round-1 sha to fire the backstop
/// on the test that's still anchored at sha0. One-test-per-file isolates
/// each test's row set per file, so each round's diff selects exactly its
/// intended rerun.
fn write_three_one_test_modules(dir: &std::path::Path, crate_name: &str) {
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\n\
             name = \"{crate_name}\"\n\
             version = \"0.1.0\"\n\
             edition = \"2021\"\n",
        ),
    )
    .unwrap();
    std::fs::write(dir.join(".gitignore"), "/target\n/Cargo.lock\n").unwrap();

    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub mod a;\npub mod b;\npub mod c;\n",
    )
    .unwrap();
    for (file, name) in [("a.rs", "a"), ("b.rs", "b"), ("c.rs", "c")] {
        std::fs::write(
            src.join(file),
            format!(
                "pub fn f{name}(x: i32) -> i32 {{\n    \
                 x + 1\n\
                 }}\n\
                 \n\
                 #[cfg(test)]\n\
                 mod tests {{\n    \
                 use super::*;\n    \
                 #[test]\n    \
                 fn test_f{name}() {{\n        \
                 assert_eq!(f{name}(1), 2);\n    \
                 }}\n\
                 }}\n",
            ),
        )
        .unwrap();
    }
}

/// Two `--diff` rounds editing *different files* (each holding exactly one
/// test) end up with three distinct shas in `test_regions`: the original
/// full-collect sha for tests untouched throughout, round 1's sha for the
/// first round's rerun set, and round 2's sha for the second's. This is the
/// headline "multi-sha state" the schema change exists for.
#[test]
fn diff_collect_accumulates_distinct_shas_across_rounds() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_three_one_test_modules(dir, "sample_diff_three_rounds");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "initial collect failed: {}",
        String::from_utf8_lossy(&collect.stderr),
    );
    let sha0 = git_head(dir);

    // Round 1: edit a.rs `fa` body. Only test_fa should rerun.
    replace_in_file(&dir.join("src/a.rs"), "x + 1", "x + 1 /* round1 */");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "round1: edit fa"]);
    let sha1 = git_head(dir);
    let diff1 = cargo_affected(dir, &["affected", "collect", "--diff"]);
    assert!(
        diff1.status.success(),
        "round1 --diff failed: {}",
        String::from_utf8_lossy(&diff1.stderr),
    );

    // Round 2: edit b.rs `fb` body. Only test_fb should rerun. test_fa
    // remains at sha1, test_fc remains at sha0 — three distinct shas.
    replace_in_file(&dir.join("src/b.rs"), "x + 1", "x + 1 /* round2 */");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "round2: edit fb"]);
    let sha2 = git_head(dir);
    let diff2 = cargo_affected(dir, &["affected", "collect", "--diff"]);
    assert!(
        diff2.status.success(),
        "round2 --diff failed: stderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&diff2.stderr),
        String::from_utf8_lossy(&diff2.stdout),
    );
    let combined2 = combined_output(&diff2);
    assert!(
        combined2.contains("1 tests to recollect"),
        "round2 should rerun exactly test_fb, got:\n{combined2}"
    );

    let db_path = dir.join("target/affected/coverage.db");
    assert_eq!(
        shas_for_test(&db_path, "a::tests::test_fa"),
        vec![sha1.clone()],
        "test_fa: round1 rerun → anchored at sha1",
    );
    assert_eq!(
        shas_for_test(&db_path, "b::tests::test_fb"),
        vec![sha2.clone()],
        "test_fb: round2 rerun → anchored at sha2",
    );
    assert_eq!(
        shas_for_test(&db_path, "c::tests::test_fc"),
        vec![sha0.clone()],
        "test_fc: never touched → still anchored at sha0",
    );

    // Three distinct shas coexist — the schema's raison d'être for `--diff`.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let mut stmt = conn
        .prepare("SELECT DISTINCT collect_sha FROM test_regions")
        .unwrap();
    let mut all_shas: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    all_shas.sort();
    let mut expected = vec![sha0, sha1, sha2];
    expected.sort();
    assert_eq!(all_shas, expected, "three distinct collect_shas should coexist");
}

#[test]
fn diff_collect_errors_with_no_prior_collect() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_diff_collect_no_prior");
    init_git_with_initial_commit(dir);

    let diff = cargo_affected(dir, &["affected", "collect", "--diff"]);
    assert!(
        !diff.status.success(),
        "collect --diff with no prior coverage should fail; got success:\nstderr=\n{}",
        String::from_utf8_lossy(&diff.stderr),
    );

    let stderr = String::from_utf8_lossy(&diff.stderr);
    assert!(
        stderr.contains("--diff requires a prior") || stderr.contains("no stored coverage"),
        "expected helpful error about missing prior collect, got:\n{stderr}"
    );
}

/// When one sha out of several diverges, `run` proceeds with the rows still
/// anchored at reachable shas — only tests stranded at the diverged sha
/// rerun (as "new"). The diverged rows stay in the DB until the user runs
/// `cargo affected clean`.
///
/// One-test-per-file isolates each test's row set per file so the
/// structural-edit backstop doesn't pull unrelated tests in.
#[test]
fn run_uses_reachable_shas_when_one_sha_diverges() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_three_one_test_modules(dir, "sample_diff_partial_diverge");
    init_git_with_initial_commit(dir);
    let sha0 = git_head(dir);

    // Full collect at sha0 — every test anchored at sha0.
    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr),
    );

    // Edit a.rs, commit, --diff → test_fa moves to sha1; test_fb/test_fc
    // remain at sha0.
    replace_in_file(&dir.join("src/a.rs"), "x + 1", "x + 1 /* edited */");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "edit fa"]);
    let sha1 = git_head(dir);
    let diff = cargo_affected(dir, &["affected", "collect", "--diff"]);
    assert!(
        diff.status.success(),
        "--diff failed: {}",
        String::from_utf8_lossy(&diff.stderr),
    );

    // Reset HEAD back to sha0. sha1 is now a sibling (still in the repo via
    // its loose object, just not on HEAD's lineage); sha0 is HEAD itself.
    git(dir, &["reset", "--hard", "-q", &sha0]);

    // Working tree is clean relative to sha0. Both shas are reachable: sha0
    // has zero diff against itself, and sha1's diff against HEAD shows the
    // `/* edited */` line — which is exactly the line test_fa was last
    // anchored at. So selection picks test_fa as `affected`, not `new`, and
    // the other tests stay skipped.
    let run = cargo_affected(dir, &["affected", "run", "-v"]);
    assert!(
        run.status.success(),
        "run across reachable shas should succeed: stderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&run.stderr),
        String::from_utf8_lossy(&run.stdout),
    );
    let combined = combined_output(&run);

    // Selection chose exactly one test (test_fa — anchored at sha1, which is
    // a sibling but reachable, and the diff against it picks up the edit).
    assert!(
        combined.contains("1 tests to run"),
        "expected '1 tests to run' (test_fa affected via sha1), got:\n{combined}"
    );
    assert!(
        combined.contains("test_fa"),
        "expected test_fa in selection, got:\n{combined}"
    );
    // Sibling shas are usable, not missing — no full-suite widening, and no
    // missing-sha notice.
    assert!(
        !combined.contains("running all tests"),
        "should not have widened to running all tests, got:\n{combined}"
    );
    assert!(
        !combined.contains("not in the repo"),
        "sha1 is a sibling, not missing; should not emit the missing-sha notice, got:\n{combined}"
    );

    // Sha1's rows are still in the DB. `cargo affected clean` is the cure
    // for stale rows; `run` doesn't auto-prune.
    let db_path = dir.join("target/affected/coverage.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let sibling_row_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM test_regions WHERE collect_sha = ?1",
            [&sha1],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        sibling_row_count > 0,
        "rows anchored at sibling sha {sha1} should remain in the DB; got {sibling_row_count}",
    );
}

/// Middle-case: one sha diverged, one reachable — and there's a real edit
/// against a file covered by the reachable sha. Verifies the union of
/// `affected` (over the reachable sha) and `new_tests` (the stranded one)
/// is what runs, not just one or the other.
#[test]
fn run_unions_affected_and_stranded_when_partially_diverged() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_three_one_test_modules(dir, "sample_diff_partial_diverge_with_edit");
    init_git_with_initial_commit(dir);
    let sha0 = git_head(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr),
    );

    // Diff round: edit a.rs, commit, --diff → test_fa moves to sha1.
    // test_fb/test_fc stay at sha0.
    replace_in_file(&dir.join("src/a.rs"), "x + 1", "x + 1 /* edited */");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "edit fa"]);
    let _sha1 = git_head(dir);
    let diff = cargo_affected(dir, &["affected", "collect", "--diff"]);
    assert!(
        diff.status.success(),
        "--diff failed: {}",
        String::from_utf8_lossy(&diff.stderr),
    );

    // Reset HEAD to sha0 (orphans sha1) AND modify b.rs so there's a real
    // diff against sha0. Selection should pick test_fb (overlap at sha0)
    // AND test_fa (stranded → "new").
    git(dir, &["reset", "--hard", "-q", &sha0]);
    replace_in_file(&dir.join("src/b.rs"), "x + 1", "x + 1 /* run-time edit */");

    let run = cargo_affected(dir, &["affected", "run", "-v"]);
    assert!(
        run.status.success(),
        "run failed: stderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&run.stderr),
        String::from_utf8_lossy(&run.stdout),
    );
    let combined = combined_output(&run);

    assert!(
        combined.contains("2 tests to run"),
        "expected '2 tests to run' (test_fb affected + test_fa as new), got:\n{combined}"
    );
    for t in ["test_fa", "test_fb"] {
        assert!(
            combined.contains(t),
            "expected {t} in selection, got:\n{combined}"
        );
    }
    assert!(
        !combined.contains("test_fc"),
        "test_fc (c.rs untouched, anchored at reachable sha) should NOT run, got:\n{combined}"
    );
    assert!(
        !combined.contains("running all tests"),
        "should not have widened to running all tests, got:\n{combined}"
    );
}

/// `collect --diff` with a sibling `collect_sha` succeeds: the diff
/// resolves both trees, selection feeds nextest, the rerun re-anchors
/// every affected test at the new HEAD. Old behavior bailed because
/// "not an ancestor" was treated as fatal — but the diff is still
/// meaningful, so there's no reason to refuse.
#[test]
fn diff_collect_succeeds_when_sha_is_sibling() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_diff_collect_sibling");
    init_git_with_initial_commit(dir);
    let init_sha = git_head(dir);

    // Add a commit, collect at it, then reset back. The recorded collect_sha
    // is a sibling (present in the repo, not an ancestor of HEAD).
    std::fs::write(dir.join("src/extra.rs"), "pub fn extra() -> i32 { 1 }\n").unwrap();
    let lib_path = dir.join("src/lib.rs");
    let lib = std::fs::read_to_string(&lib_path).unwrap();
    std::fs::write(&lib_path, format!("{lib}pub mod extra;\n")).unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "add extra module"]);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr),
    );

    git(dir, &["reset", "--hard", "-q", &init_sha]);

    let diff = cargo_affected(dir, &["affected", "collect", "--diff"]);
    assert!(
        diff.status.success(),
        "collect --diff with a sibling collect_sha should succeed:\nstderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&diff.stderr),
        String::from_utf8_lossy(&diff.stdout),
    );
    let stderr = String::from_utf8_lossy(&diff.stderr);
    assert!(
        !stderr.contains("not in the repo"),
        "sibling sha must not be reported as missing, got:\n{stderr}"
    );
}

/// Removing the `#[test]` attribute drops `test_multiply` from the listing
/// while its rows linger in the DB. `--diff` must prune those stale rows so
/// the cache doesn't carry phantom tests forward indefinitely. Renames take
/// the same code path: old name absent from listing → row dropped.
#[test]
fn diff_collect_prunes_deleted_tests() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_diff_prune_deleted");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "initial collect failed: {}",
        String::from_utf8_lossy(&collect.stderr),
    );

    // Strip just the `#[test]` attribute so the function still compiles
    // (avoids a syntactic delete that would re-flow nearby line numbers
    // in noisy ways) but stops appearing in `cargo nextest list`.
    replace_in_file(
        &dir.join("src/math.rs"),
        "    #[test]\n    fn test_multiply()",
        "    fn test_multiply()",
    );
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "demote test_multiply"]);

    let diff = cargo_affected(dir, &["affected", "collect", "--diff"]);
    assert!(
        diff.status.success(),
        "collect --diff failed: stderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&diff.stderr),
        String::from_utf8_lossy(&diff.stdout),
    );

    let combined = combined_output(&diff);
    assert!(
        combined.contains("pruned 1 test"),
        "expected 'pruned 1 test' in --diff output, got:\n{combined}"
    );

    // DB invariant: test_multiply's rows are gone; the survivors stay.
    let db_path = dir.join("target/affected/coverage.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let surviving: Vec<String> = conn
        .prepare("SELECT DISTINCT test_name FROM test_regions")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(
        !surviving.iter().any(|t| t == "math::tests::test_multiply"),
        "test_multiply rows should be pruned, got: {surviving:?}",
    );
    assert!(
        surviving.iter().any(|t| t == "math::tests::test_add"),
        "test_add rows should remain, got: {surviving:?}",
    );
    assert!(
        surviving.iter().any(|t| t == "strings::tests::test_greet"),
        "test_greet rows should remain, got: {surviving:?}",
    );
}

/// An `#[ignore]`d test is skipped by `cargo nextest run` but still appears
/// in `cargo nextest list`. `--diff`'s prune compares the listing against
/// the DB to drop renamed/deleted tests — it must NOT drop a merely-ignored
/// test, whose stored coverage is still valid. (New-test detection excludes
/// ignored tests, but they stay in `listing.tests` precisely so the prune
/// keeps their rows.)
#[test]
fn diff_collect_keeps_ignored_test_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_diff_ignored_rows");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "initial collect failed: {}",
        String::from_utf8_lossy(&collect.stderr),
    );

    // Edit `add`'s body so test_add is rerun (a non-ignored test must run,
    // or `--diff` would have no profraw to extract), and mark test_multiply
    // `#[ignore]` — it stays listed, so the prune must keep its rows.
    replace_in_file(&dir.join("src/math.rs"), "a + b", "a + b + 0");
    replace_in_file(
        &dir.join("src/math.rs"),
        "    #[test]\n    fn test_multiply()",
        "    #[test]\n    #[ignore]\n    fn test_multiply()",
    );
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "ignore test_multiply"]);

    let diff = cargo_affected(dir, &["affected", "collect", "--diff"]);
    assert!(
        diff.status.success(),
        "collect --diff failed: stderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&diff.stderr),
        String::from_utf8_lossy(&diff.stdout),
    );
    let combined = combined_output(&diff);
    assert!(
        !combined.contains("pruned"),
        "an ignored test must not be pruned, got:\n{combined}"
    );

    // DB invariant: test_multiply's rows survive the prune.
    let db_path = dir.join("target/affected/coverage.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let surviving: Vec<String> = conn
        .prepare("SELECT DISTINCT test_name FROM test_regions")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(
        surviving.iter().any(|t| t == "math::tests::test_multiply"),
        "ignored test_multiply's rows should remain, got: {surviving:?}",
    );
}

/// All-phantom selection: every selected test exists in the DB but has been
/// removed from the nextest listing. The structural-edit backstop pulls
/// both math.rs tests into `affected`; both are now absent from the
/// listing, so the nextest filter matches nothing and the runner shim
/// never fires. `handle_no_results` should recognize this as the
/// expected "filter matched nothing real" case (rather than a runner
/// shim failure) and prune the stale rows.
#[test]
fn diff_collect_all_phantom_selection_prunes_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_diff_all_phantoms");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "initial collect failed: {}",
        String::from_utf8_lossy(&collect.stderr),
    );

    // Strip BOTH `#[test]` attributes in math.rs. Both functions still
    // compile (the dead_code lint is just a warning) but neither is a test
    // anymore — listing drops to just test_greet. The diff hunks land on
    // attribute lines that don't overlap any stored body range, so the
    // backstop fires and selects test_add + test_multiply. Both are
    // phantoms now.
    replace_in_file(
        &dir.join("src/math.rs"),
        "    #[test]\n    fn test_add()",
        "    fn test_add()",
    );
    replace_in_file(
        &dir.join("src/math.rs"),
        "    #[test]\n    fn test_multiply()",
        "    fn test_multiply()",
    );
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "demote both math tests"]);

    let diff = cargo_affected(dir, &["affected", "collect", "--diff"]);
    assert!(
        diff.status.success(),
        "collect --diff with all-phantom selection should exit 0; \
         stderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&diff.stderr),
        String::from_utf8_lossy(&diff.stdout),
    );

    let combined = combined_output(&diff);
    // The recovery message names the cause specifically — it must NOT
    // surface as a "runner shim may have failed" diagnostic.
    assert!(
        combined.contains("every selected test is absent"),
        "expected all-phantom recovery message, got:\n{combined}"
    );
    assert!(
        !combined.contains("runner shim may have failed"),
        "all-phantom selection should not surface as a shim failure:\n{combined}"
    );
    assert!(
        combined.contains("pruned 2 tests"),
        "expected 'pruned 2 tests' in --diff output, got:\n{combined}"
    );

    // DB invariant: both math tests pruned, only test_greet remains.
    let db_path = dir.join("target/affected/coverage.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let surviving: Vec<String> = conn
        .prepare("SELECT DISTINCT test_name FROM test_regions")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        surviving,
        vec!["strings::tests::test_greet".to_string()],
        "only test_greet should remain, got: {surviving:?}",
    );
}

/// `--diff` on a clean working tree (HEAD == prior collect_sha, no
/// uncommitted edits) should short-circuit with exit 0 and announce that
/// nothing needs to be recollected — no nextest invocation, no DB writes.
#[test]
fn diff_collect_clean_tree_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_diff_clean_tree");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "initial collect failed: {}",
        String::from_utf8_lossy(&collect.stderr),
    );

    let db_path = dir.join("target/affected/coverage.db");
    let row_count = || -> i64 {
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM test_regions", [], |r| r.get(0))
            .unwrap()
    };
    let before = row_count();

    let diff = cargo_affected(dir, &["affected", "collect", "--diff"]);
    assert_eq!(
        diff.status.code(),
        Some(0),
        "collect --diff on clean tree should exit 0; stderr=\n{}\nstdout=\n{}",
        String::from_utf8_lossy(&diff.stderr),
        String::from_utf8_lossy(&diff.stdout),
    );

    let combined = combined_output(&diff);
    assert!(
        combined.contains("nothing to recollect"),
        "expected 'nothing to recollect' in --diff output, got:\n{combined}"
    );
    // Sanity: we must not have invoked nextest run, since there's nothing to
    // recollect. The "running tests with cargo nextest run..." line is only
    // printed for the rerun path.
    assert!(
        !combined.contains("running tests with cargo nextest run"),
        "clean-tree --diff should skip nextest; output suggests it ran:\n{combined}"
    );
    // DB invariant: no rows added or removed. The output assertions above
    // are about user messaging; this is the structural check that the
    // no-op path really was a no-op.
    assert_eq!(
        row_count(),
        before,
        "clean-tree --diff should leave test_regions row count unchanged"
    );
}
