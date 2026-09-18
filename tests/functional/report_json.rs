//! `--report-json` writes the artifact `docs/report-json.md` describes.
//!
//! The schema is a published contract — `docs/report-json.md` is 200-odd lines
//! of field-by-field promises, and `--report-detail` decides which half of it
//! materialises. `report.rs`'s unit tests cover the builders in isolation
//! (sort orders, the full-suite shape), but nothing ran the CLI end to end, so
//! a rename or a rewiring between selection and the report builder could ship
//! green while the documented artifact silently changed shape.
//!
//! These scenarios pin the parts of the doc a consumer would actually break
//! on: the enum spellings (`cache.status`, `mode`, `kind`), the count
//! arithmetic the doc states as identities, and the two detail levels'
//! null-vs-populated split.
//!
//! The report goes under `target/` (gitignored) rather than the project root:
//! an untracked file at the root would show up as a changed file and pad
//! `selection.changed_files` with the artifact itself.

use crate::{
    cargo_affected, init_git_with_initial_commit, replace_in_file, write_two_module_project,
};

/// Read and parse a report the CLI just wrote.
fn read_report(path: &std::path::Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("expected a report at {}: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("report at {} is not valid JSON: {e}", path.display()))
}

/// Selection-mode report with `--report-detail full`: every documented section
/// is populated, the enum spellings match the doc, and the two count
/// identities the doc states hold (`selected` is the union of the four
/// categories; `skipped` is `total_reachable_known - affected - config`).
#[test]
fn report_json_documents_selection_in_full_detail() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_report_json");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    // Function-body edit on `add` — one test selected, by line overlap.
    replace_in_file(&dir.join("src/math.rs"), "a + b", "a + b /* edited */");

    let out = cargo_affected(
        dir,
        &[
            "affected",
            "status",
            "--report-json",
            "target/affected/report.json",
            "--report-detail",
            "full",
        ],
    );
    assert!(
        out.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report = read_report(&dir.join("target/affected/report.json"));

    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["command"], "status");
    // Nothing has been committed since collect and the sha is HEAD, so this is
    // the exact-match branch — the enum's kebab-case spelling is the contract.
    assert_eq!(report["cache"]["status"], "hit-exact");

    // The fingerprint section: current components populated, and the stored
    // fingerprint we just collected under is a zero-diff match.
    assert!(
        report["cache"]["current_fingerprint"].is_string(),
        "current_fingerprint should be populated whenever the fingerprint was computed"
    );
    let stored = report["cache"]["stored_fingerprints"]
        .as_array()
        .expect("stored_fingerprints should be an array");
    assert_eq!(
        stored[0]["diff_count"], 0,
        "the fingerprint collect just stored under must sort first with a zero diff, got: {stored:?}"
    );
    let shas = report["cache"]["collect_shas"]
        .as_array()
        .expect("collect_shas should be populated when the fingerprint matched");
    assert_eq!(shas.len(), 1, "one collect, one anchor sha, got: {shas:?}");
    assert_eq!(shas[0]["relation"], "equal");
    // `.get()`, not `["commits_ahead"].is_null()`: indexing a missing key on a
    // `Value` yields `Null`, so `is_null()` can't tell "skipped" from "emitted
    // as null" — and skipped is exactly what `CollectShaEntry` documents,
    // because consumers use field presence to detect reachable shas.
    assert!(
        shas[0].get("commits_ahead").is_none(),
        "commits_ahead must be omitted entirely (not null) for a non-reachable sha, got: {}",
        shas[0]
    );

    let summary = &report["selection"]["summary"];
    assert_eq!(summary["mode"], "selection");
    assert_eq!(summary["affected"], 1);
    assert_eq!(summary["config"], 0);
    assert_eq!(summary["new"], 0);
    assert_eq!(summary["stranded"], 0);
    // `selected` = union of the four categories; `skipped` =
    // total_reachable_known - affected - config. Both are stated as identities
    // in the doc, so derive them rather than hardcoding a second time.
    let count = |k: &str| {
        summary[k]
            .as_u64()
            .unwrap_or_else(|| panic!("{k} should be a number"))
    };
    assert_eq!(
        count("selected"),
        count("affected") + count("config") + count("new") + count("stranded"),
        "selected must equal the sum of the four disjoint categories, got: {summary}"
    );
    assert_eq!(
        count("skipped"),
        count("total_reachable_known") - count("affected") - count("config"),
        "skipped must equal total_reachable_known - affected - config, got: {summary}"
    );

    // Only `src/math.rs` changed, and it carries coverage rows.
    let changed = report["selection"]["changed_files"]
        .as_array()
        .expect("changed_files should be populated in selection mode");
    assert_eq!(
        changed.len(),
        1,
        "only src/math.rs changed (the report itself lives under gitignored target/), got: {changed:?}"
    );
    let math = &changed[0];
    assert_eq!(math["path"], "src/math.rs");
    assert_eq!(math["tracked_by_coverage"], true);
    assert_eq!(math["tests_pulled_total"], 1);
    assert_eq!(math["tests_pulled_by_reason"]["line_overlap"], 1);
    // The four counters sum to tests_pulled_total — the doc's dedup-by-
    // strongest-reason promise.
    let by_reason = &math["tests_pulled_by_reason"];
    let reason_sum: u64 = [
        "line_overlap",
        "structural_backstop",
        "crate_root_sentinel",
        "config_rule",
    ]
    .iter()
    .map(|k| {
        by_reason[*k]
            .as_u64()
            .unwrap_or_else(|| panic!("{k} should be a number"))
    })
    .sum();
    assert_eq!(
        reason_sum,
        math["tests_pulled_total"].as_u64().unwrap(),
        "the four reason counters must sum to tests_pulled_total, got: {math}"
    );

    // Per-test detail: `full` populates it, and the reason names the hunk that
    // pulled the test in.
    let tests = report["selection"]["selected_tests"]
        .as_array()
        .expect("selected_tests should be populated under --report-detail full");
    assert_eq!(tests.len(), 1, "expected only test_add, got: {tests:?}");
    let entry = &tests[0];
    assert!(
        entry["test_name"].as_str().unwrap().contains("test_add"),
        "expected test_add, got: {entry}"
    );
    assert_eq!(entry["kind"], "affected");
    let reasons = entry["reasons"]
        .as_array()
        .expect("reasons should be an array");
    assert_eq!(reasons.len(), 1, "one edit, one reason, got: {reasons:?}");
    assert_eq!(reasons[0]["kind"], "line_overlap");
    assert_eq!(reasons[0]["file"], "src/math.rs");
    assert!(
        reasons[0]["stored_range"].is_array(),
        "a line_overlap reason names the stored row it matched, got: {}",
        reasons[0]
    );
    assert!(
        reasons[0]["matched_hunk"].is_array(),
        "a line_overlap reason names the diff hunk that matched, got: {}",
        reasons[0]
    );
}

/// The default `--report-detail summary` keeps the per-file aggregates but
/// omits the per-test vectors — the bound that makes the default safe on a
/// large suite. Same scenario as above so the only difference is the flag.
#[test]
fn report_json_summary_detail_omits_per_test_vectors() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_report_json_summary");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );
    replace_in_file(&dir.join("src/math.rs"), "a + b", "a + b /* edited */");

    let out = cargo_affected(
        dir,
        &[
            "affected",
            "status",
            "--report-json",
            "target/affected/report.json",
        ],
    );
    assert!(
        out.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report = read_report(&dir.join("target/affected/report.json"));
    assert_eq!(report["selection"]["summary"]["mode"], "selection");
    assert!(
        report["selection"]["changed_files"].is_array(),
        "summary detail still carries the per-file aggregates"
    );
    assert!(
        report["selection"]["selected_tests"].is_null(),
        "selected_tests requires --report-detail full, got: {}",
        report["selection"]["selected_tests"]
    );
}

/// `run --all` skips selection deliberately, so the report is the partial
/// full-suite shape: `forced-all`, every count null, both arrays null. This is
/// the one path that reaches `Report::build_full_suite` through the CLI, and
/// the only `--all` coverage in the functional suite.
#[test]
fn report_json_full_suite_shape_under_all() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_report_json_all");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    let out = cargo_affected(
        dir,
        &[
            "affected",
            "run",
            "--all",
            "--report-json",
            "target/affected/report.json",
        ],
    );
    assert!(
        out.status.success(),
        "run --all failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report = read_report(&dir.join("target/affected/report.json"));
    assert_eq!(report["command"], "run");
    assert_eq!(report["cache"]["status"], "forced-all");
    assert_eq!(
        report["selection"]["summary"]["mode"],
        "full-suite-no-listing"
    );
    for field in [
        "selected",
        "affected",
        "config",
        "new",
        "stranded",
        "skipped",
        "total_reachable_known",
    ] {
        assert!(
            report["selection"]["summary"][field].is_null(),
            "{field} must be null when no listing happened, got: {}",
            report["selection"]["summary"][field]
        );
    }
    assert!(report["selection"]["changed_files"].is_null());
    assert!(report["selection"]["selected_tests"].is_null());

    // The notice alone proves nothing — it prints before any work happens, and
    // nothing was edited here, so a selection-mode run would have picked zero
    // tests. Name each test instead: seeing all three is what pins that `--all`
    // ran the suite rather than just announcing it.
    let combined = crate::combined_output(&out);
    assert!(
        combined.contains("running all tests (--all)"),
        "expected the --all notice, got:\n{combined}"
    );
    for test in ["test_add", "test_multiply", "test_greet"] {
        assert!(
            combined.contains(test),
            "--all must run the whole suite; {test} is missing from:\n{combined}"
        );
    }
}
