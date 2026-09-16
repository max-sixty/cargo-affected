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

/// Same golden-file shape, but with two tests the rule's filterset does *not*
/// name — the shape that catches a rule resolving to the whole project.
fn write_multi_test_golden_project(dir: &Path) {
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
name = "config-rule-multi-sample"
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
    assert_eq!(config_rule_multi_sample::GREETING, expected.trim());
}

#[test]
fn unrelated_one() {
    assert_eq!(2 + 2, 4);
}

#[test]
fn unrelated_two() {
    assert_eq!(3 * 3, 9);
}
"#,
    )
    .unwrap();
}

/// A rule resolves to the tests its filterset *matches*, not to every test in
/// the project.
///
/// `cargo nextest list -E <filterset>` reports every testcase and tags the
/// non-matches `filter-match: { status: "mismatch", reason: "expression" }` —
/// it does not omit them. Reading the listing's test set whole made a rule that
/// matched one changed path force-select the entire suite, which is both wrong
/// (unrelated tests run) and invisible (they pass).
#[test]
fn config_rule_selects_only_the_tests_its_filterset_matches() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_multi_test_golden_project(dir);
    add_affected_rule(dir, "\"golden.txt\"", "test(=golden_matches)");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        combined_output(&collect)
    );

    replace_in_file(&dir.join("golden.txt"), "hello", "hi");
    let out = combined_output(&cargo_affected(dir, &["affected", "status", "-v"]));
    assert!(
        out.contains("selection=1/3"),
        "only the filterset's test should be selected (1 of 3): {out}"
    );
    assert!(
        out.contains("1 config"),
        "expected exactly one config hit: {out}"
    );
    assert!(
        out.contains("golden_matches (config)"),
        "golden_matches is the test the filterset names: {out}"
    );
    assert!(
        !out.contains("unrelated_one") && !out.contains("unrelated_two"),
        "tests the filterset doesn't name must not be config-selected: {out}"
    );
}

/// The caller's own `-E` must not leak into the rule-resolution listing.
///
/// `run`/`status` forward the post-`--` filters to `cargo nextest list` so the
/// listing matches what `nextest run` will admit — but nextest *unions*
/// repeated `-E` flags, so passing the user's expression alongside a rule's
/// filterset would make every test the user's expression matches a hit for
/// that rule. Here `-E test(=unrelated_one)` is the user's filter and the
/// rule names `golden_matches`: the rule's test is filtered out by the user,
/// so nothing is config-selected — and `unrelated_one` in particular must not
/// be, since no rule names it.
#[test]
fn config_rule_ignores_the_callers_filterset() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_multi_test_golden_project(dir);
    add_affected_rule(dir, "\"golden.txt\"", "test(=golden_matches)");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        combined_output(&collect)
    );

    replace_in_file(&dir.join("golden.txt"), "hello", "hi");
    let out = combined_output(&cargo_affected(
        dir,
        &[
            "affected",
            "status",
            "-v",
            "--",
            "-E",
            "test(=unrelated_one)",
        ],
    ));
    assert!(
        out.contains("selection=0/3"),
        "the rule's own test is excluded by the caller's filter, so nothing is \
         selected: {out}"
    );
    assert!(
        !out.contains("unrelated_one"),
        "the caller's filterset must not turn its own matches into config hits: {out}"
    );
}

/// A caller's positional filter must not make the rule report itself broken.
///
/// The rule listing keeps the caller's positional substring filters (nextest
/// intersects them with the rule's filterset, so they can only narrow the rule
/// toward what `nextest run` will admit). Resolving the rule against the whole
/// `filter-match: mismatch` set therefore made the rule's own test look like a
/// non-match whenever a positional excluded it, collapsing the rule to nothing
/// and firing the "matched no tests" warning — whose whole purpose is catching
/// a typo'd filterset — against a filterset that is not at fault. The `-E`
/// spelling of the same narrowing never warned, because the caller's filtersets
/// are stripped from the rule listing.
#[test]
fn config_rule_does_not_warn_when_the_caller_filters_its_test_out() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_multi_test_golden_project(dir);
    add_affected_rule(dir, "\"golden.txt\"", "test(=golden_matches)");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        combined_output(&collect)
    );

    replace_in_file(&dir.join("golden.txt"), "hello", "hi");
    let out = combined_output(&cargo_affected(
        dir,
        &["affected", "status", "-v", "--", "unrelated"],
    ));
    assert!(
        !out.contains("matched no tests"),
        "the rule's filterset is valid — only the caller's positional excluded \
         its test, so no filterset warning should fire: {out}"
    );
    assert!(
        out.contains("selection=0/3"),
        "the rule's test is filtered out by the caller, so nothing runs: {out}"
    );
    assert!(
        !out.contains("unrelated_one") && !out.contains("unrelated_two"),
        "the caller's positional must not turn its own matches into config \
         hits: {out}"
    );
}

/// Crate shaped like [`write_multi_test_golden_project`] but carrying an
/// `#[ignore]`d test, which is what makes the filterset warning's keying
/// observable.
fn write_ignored_test_golden_project(dir: &Path) {
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
name = "config-rule-ignored-sample"
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
    assert_eq!(config_rule_ignored_sample::GREETING, expected.trim());
}

#[test]
#[ignore]
fn ignored_one() {}
"#,
    )
    .unwrap();
}

/// A filterset that matches nothing must warn even when some other filter
/// rejected a test first.
///
/// nextest reports exactly one `filter-match` reason per testcase, chosen in a
/// fixed filter order that puts `#[ignore]` and positional substring filters
/// ahead of filtersets. So a test the rule's own `-E` rejects comes back
/// tagged `ignored` (or `string`) whenever one of those rejects it too, and a
/// rule's hit set computed as "everything not tagged `expression`" stays
/// non-empty no matter how broken the filterset is: one `#[ignore]`d test
/// anywhere in the project was enough to swallow the warning entirely, which
/// is the only thing standing between a typo'd filterset and inputs that
/// silently go untested.
#[test]
fn config_rule_warns_for_a_filterset_that_matches_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_ignored_test_golden_project(dir);
    add_affected_rule(dir, "\"golden.txt\"", "test(=no_such_test)");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        combined_output(&collect)
    );

    replace_in_file(&dir.join("golden.txt"), "hello", "hi");
    let out = combined_output(&cargo_affected(dir, &["affected", "status", "-v"]));
    assert!(
        out.contains("matched no tests"),
        "a filterset matching nothing must warn even though `ignored_one` is \
         tagged `ignored` rather than `expression`: {out}"
    );
}
