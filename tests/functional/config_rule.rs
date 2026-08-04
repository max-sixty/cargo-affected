//! `[*.metadata.affected]` input rules: force-select tests whose non-Rust
//! inputs changed.
//!
//! A test that reads a data file at runtime (the insta-snapshot / doc-sync
//! shape) has coverage rows only for the Rust lines it executed — never for the
//! data file. So a change confined to that file overlaps no coverage and the
//! test is *skipped*, even though it would fail. This is the documented
//! non-Rust-input false-negative. A `[[package.metadata.affected.rule]]` mapping
//! the file's glob to the test closes the gap. We assert both halves: the miss
//! without a rule, the rescue with it. (Metadata is excluded from the
//! fingerprint, so adding the rule after `collect` keeps the same cache.)

use std::path::Path;

use crate::{cargo_affected, combined_output, git, init_git_with_initial_commit, replace_in_file};

/// Crate whose only test reads `golden.txt` at runtime and compares it to a
/// `const` — a hermetic stand-in for an insta snapshot or doc-sync test.
fn write_golden_project(dir: &Path) {
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
name = "config-rule-sample"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    std::fs::write(dir.join(".gitignore"), "/target\n/Cargo.lock\n").unwrap();

    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub const GREETING: &str = \"hello\";\n",
    )
    .unwrap();

    // The data file the test reads at runtime. llvm-cov never sees it, so no
    // coverage row links it to `golden_matches`.
    std::fs::write(dir.join("golden.txt"), "hello\n").unwrap();

    let tests = dir.join("tests");
    std::fs::create_dir_all(&tests).unwrap();
    std::fs::write(
        tests.join("golden.rs"),
        r#"#[test]
fn golden_matches() {
    let expected = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/golden.txt"),
    )
    .unwrap();
    assert_eq!(config_rule_sample::GREETING, expected.trim());
}
"#,
    )
    .unwrap();
}

/// Same shape as [`write_golden_project`] but with a **second, unrelated**
/// test alongside `golden_matches`. One test is not enough to see whether a
/// rule's filterset is honoured: with a single-test crate, "the tests the
/// filterset names" and "every test in the workspace" are the same set, so an
/// implementation that ignored the filterset entirely would still look right.
fn write_two_test_golden_project(dir: &Path) {
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
name = "config-rule-narrow-sample"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    std::fs::write(dir.join(".gitignore"), "/target\n/Cargo.lock\n").unwrap();

    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub const GREETING: &str = \"hello\";\npub fn unrelated() -> i32 {\n    7\n}\n",
    )
    .unwrap();

    std::fs::write(dir.join("golden.txt"), "hello\n").unwrap();

    let tests = dir.join("tests");
    std::fs::create_dir_all(&tests).unwrap();
    std::fs::write(
        tests.join("golden.rs"),
        r#"#[test]
fn golden_matches() {
    let expected = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/golden.txt"),
    )
    .unwrap();
    assert_eq!(config_rule_narrow_sample::GREETING, expected.trim());
}
"#,
    )
    .unwrap();
    // Reads nothing on disk, so no rule and no coverage row ever links it to
    // `golden.txt`.
    std::fs::write(
        tests.join("unrelated.rs"),
        r#"#[test]
fn unrelated_test() {
    assert_eq!(config_rule_narrow_sample::unrelated(), 7);
}
"#,
    )
    .unwrap();
}

/// Append a `[[package.metadata.affected.rule]]` to the sample crate's
/// Cargo.toml. `globs` is the TOML array body (e.g. `"golden.txt"`).
fn add_affected_rule(dir: &Path, globs: &str, filterset: &str) {
    let cargo_toml = dir.join("Cargo.toml");
    let mut content = std::fs::read_to_string(&cargo_toml).unwrap();
    content.push_str(&format!(
        "\n[[package.metadata.affected.rule]]\nglobs = [{globs}]\nfilterset = \"{filterset}\"\n"
    ));
    std::fs::write(&cargo_toml, content).unwrap();
}

#[test]
fn config_rule_selects_test_for_non_rust_input_change() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_golden_project(dir);
    init_git_with_initial_commit(dir);

    // Seed coverage: `golden_matches` runs, covering `GREETING` and the test
    // body — but nothing links `golden.txt` to it.
    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        combined_output(&collect)
    );

    // The non-Rust input changes. No Rust hunk → coverage selects nothing.
    replace_in_file(&dir.join("golden.txt"), "hello", "hi");

    // --- Without a rule: the change is a coverage blind spot (the miss). ---
    let miss = combined_output(&cargo_affected(dir, &["affected", "status", "-v"]));
    assert!(
        miss.contains("selection=0/1"),
        "expected the golden test skipped (0 of 1 selected): {miss}"
    );
    assert!(
        !miss.contains("golden_matches"),
        "golden_matches should NOT be selected without a rule: {miss}"
    );

    // --- Add the rule (metadata isn't fingerprinted, so the cache survives). ---
    add_affected_rule(dir, "\"golden.txt\"", "test(=golden_matches)");
    let fixed = combined_output(&cargo_affected(dir, &["affected", "status", "-v"]));
    assert!(
        fixed.contains("selection=1/1"),
        "expected the golden test selected (1 of 1): {fixed}"
    );
    assert!(
        fixed.contains("1 config"),
        "expected the rescue attributed to the config category: {fixed}"
    );
    assert!(
        fixed.contains("golden_matches (config)"),
        "expected golden_matches tagged (config): {fixed}"
    );
    assert!(
        fixed.contains("0 skipped of 1 reachable-known"),
        "the rescued test should no longer be skipped: {fixed}"
    );
}

/// A *committed* added input (a new file since `collect_sha`) must rescue its
/// rule's tests. This exercises the `git_added_files_since` path specifically:
/// `git diff -U0` omits a new file (no OLD side) and working-tree queries don't
/// see a committed file, so without that source the addition would be invisible
/// and the gap would silently reopen.
#[test]
fn config_rule_rescues_committed_added_input() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_golden_project(dir);
    add_affected_rule(dir, "\"data/*.snap\"", "test(=golden_matches)");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        combined_output(&collect)
    );

    // Add a brand-new input and commit it: it's an addition since collect_sha,
    // with no modified sibling. Only `git_added_files_since` surfaces it.
    std::fs::create_dir_all(dir.join("data")).unwrap();
    std::fs::write(dir.join("data/new.snap"), "x\n").unwrap();
    git(dir, &["add", "data/new.snap"]);
    git(dir, &["commit", "-q", "-m", "add snapshot"]);

    let out = combined_output(&cargo_affected(dir, &["affected", "status", "-v"]));
    assert!(
        out.contains("1 config"),
        "a committed added input should rescue the test via config: {out}"
    );
    assert!(
        out.contains("golden_matches (config)"),
        "golden_matches should be config-selected for the added input: {out}"
    );
}

/// A bogus filterset in `[*.metadata.affected]` that nextest's parser rejects
/// must error end-to-end, not get swallowed into a silent skip. The "fail
/// loudly" principle (CLAUDE.md) is load-bearing here: a typo'd filterset that
/// silently selected zero tests would re-open the very gap the rule exists to
/// close.
#[test]
fn config_rule_bogus_filterset_fails_loudly() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_golden_project(dir);
    // `&&` is a binary operator with no operands — nextest's filterset parser
    // rejects it.
    add_affected_rule(dir, "\"golden.txt\"", "&&");
    init_git_with_initial_commit(dir);

    // Seed coverage. `collect` doesn't resolve filtersets (no diff yet) so the
    // bogus rule doesn't block the cache.
    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        combined_output(&collect)
    );

    // Touch the rule's input. Resolution now happens — and must fail.
    replace_in_file(&dir.join("golden.txt"), "hello", "hi");

    let out = cargo_affected(dir, &["affected", "status", "-v"]);
    let combined = combined_output(&out);
    assert!(
        !out.status.success(),
        "expected failure for bogus filterset, got success: {combined}"
    );
    assert!(
        combined.contains("filterset"),
        "error must name the offending filterset: {combined}"
    );
}

/// A rule's `filterset` must narrow the selection to the tests it names.
///
/// `cargo nextest list -E <expr>` lists *every* testcase and tags each with
/// `filter-match`, rather than emitting only the matches. Reading the listing
/// without consulting that tag made every rule force-select the whole
/// workspace the moment one of its globs matched — safe, but it silently
/// discards the entire speedup on any diff that touches a configured input,
/// and it made the "filterset selected no tests" warning unreachable in any
/// workspace with at least one test.
#[test]
fn config_rule_filterset_narrows_to_named_tests() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_test_golden_project(dir);
    add_affected_rule(dir, "\"golden.txt\"", "test(=golden_matches)");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        combined_output(&collect)
    );

    // Only the non-Rust input changes: coverage selects nothing, so every
    // selected test comes from the rule.
    replace_in_file(&dir.join("golden.txt"), "hello", "hi");

    let out = combined_output(&cargo_affected(dir, &["affected", "status", "-v"]));
    assert!(
        out.contains("golden_matches (config)"),
        "the filterset names golden_matches, so it must be config-selected: {out}"
    );
    assert!(
        !out.contains("unrelated_test"),
        "unrelated_test is outside the rule's filterset and must NOT be \
         selected — the filterset is being ignored: {out}"
    );
    assert!(
        out.contains("1 config"),
        "exactly one test matches the filterset: {out}"
    );
    assert!(
        out.contains("selection=1/2"),
        "expected 1 of 2 tests selected: {out}"
    );
}

/// A rule whose filterset is *valid* but resolves to zero tests must surface a
/// warning. The fast path already returns `Ok(())` for an empty rule set; the
/// risk is a typo'd test name (a valid filterset that simply matches nothing)
/// silently selecting nothing for the changed input. The warning is the
/// signal that the rule is no longer doing its job.
///
/// Unreachable until the filterset was actually honoured: `nextest list -E`
/// enumerates every testcase, so the resolved set was never empty in a
/// workspace with at least one test.
///
/// The crate carries an `#[ignore]`d test on purpose. nextest reports
/// `ignored` as the mismatch *reason* in preference to `expression`, so an
/// ignored test is never tagged as filterset-rejected — resolving the rule by
/// "listing minus filterset-rejected" alone leaves it in, the set is non-empty,
/// and this warning goes quiet again in any workspace with a single ignored
/// test.
#[test]
fn config_rule_warns_when_filterset_matches_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_golden_project(dir);
    std::fs::write(
        dir.join("tests").join("ignored.rs"),
        "#[test]\n#[ignore]\nfn ignored_test() {}\n",
    )
    .unwrap();
    // Syntactically valid filterset; nextest accepts it and returns zero tests.
    add_affected_rule(dir, "\"golden.txt\"", "test(=no_such_test_anywhere)");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        combined_output(&collect)
    );

    replace_in_file(&dir.join("golden.txt"), "hello", "hi");

    let out = cargo_affected(dir, &["affected", "status", "-v"]);
    let combined = combined_output(&out);
    // The command itself succeeds — an empty match isn't a hard error — but
    // the warning must be visible so a typo can't silently reopen the gap.
    assert!(
        out.status.success(),
        "an empty match is not a hard error: {combined}"
    );
    assert!(
        combined.contains("selected no tests"),
        "expected the no-tests warning to fire: {combined}"
    );
    assert!(
        combined.contains("golden.txt"),
        "warning should name the matched path: {combined}"
    );
    assert!(
        combined.contains("selection=0/1"),
        "an empty filterset must contribute nothing to the selection: {combined}"
    );
}

/// A rule that matches no changed path must be inert: a Rust-only edit takes
/// the exact pre-rule path, with no config category and no extra selection.
#[test]
fn config_rule_inert_when_no_glob_matches() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_golden_project(dir);
    // A rule keyed on a file the edit below never touches.
    add_affected_rule(dir, "\"golden.txt\"", "test(=golden_matches)");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        combined_output(&collect)
    );

    // Edit a Rust file (not golden.txt). The rule's glob doesn't match, so the
    // config category stays empty and selection is driven purely by coverage.
    replace_in_file(&dir.join("src/lib.rs"), "hello", "hello world");
    let out = combined_output(&cargo_affected(dir, &["affected", "status", "-v"]));
    assert!(
        out.contains("0 config"),
        "a non-matching rule must add nothing: {out}"
    );
    assert!(
        out.contains("golden_matches"),
        "the GREETING edit should still select the test via coverage: {out}"
    );
}
