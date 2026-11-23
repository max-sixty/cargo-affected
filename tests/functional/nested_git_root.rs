//! A Cargo workspace can be nested below its Git repository. Git diff emits
//! repository-relative paths, so coverage ranges must use the same basis.

use std::{fs, path::Path};

use crate::{cargo_affected, combined_output, git, init_git_with_initial_commit, replace_in_file};

fn write_nested_member(repo: &Path) {
    let member = repo.join("member");
    fs::create_dir_all(member.join("src")).unwrap();
    fs::create_dir_all(member.join("fixtures")).unwrap();
    fs::create_dir_all(member.join(".cargo")).unwrap();
    fs::write(
        repo.join(".gitignore"),
        "/member/target/\n/member/Cargo.lock\n",
    )
    .unwrap();
    // Required for compatibility with Cargo builds that aggressively remove
    // intermediate test binaries as soon as compilation completes.
    fs::write(
        member.join(".cargo/config.toml"),
        "[build]\nephemeral-intermediates = false\n",
    )
    .unwrap();
    fs::write(
        member.join("Cargo.toml"),
        r#"[package]
name = "sample_nested_git"
version = "0.1.0"
edition = "2021"

[[package.metadata.affected.rule]]
globs = ["fixtures/**"]
filterset = "test(/other/)"
"#,
    )
    .unwrap();
    fs::write(
        member.join("src/lib.rs"),
        r#"pub mod logic;
pub mod other;

#[cfg(test)]
mod tests {
    #[test]
    fn forty() {
        assert_eq!(crate::logic::number(), 40);
    }

    #[test]
    fn other() {
        assert_eq!(crate::other::number(), 10);
    }
}
"#,
    )
    .unwrap();
    fs::write(
        member.join("src/logic.rs"),
        "pub fn number() -> u32 {\n    40\n}\n",
    )
    .unwrap();
    fs::write(
        member.join("src/other.rs"),
        "pub fn number() -> u32 {\n    10\n}\n",
    )
    .unwrap();
    fs::write(member.join("fixtures/test.snap"), "baseline\n").unwrap();
}

#[test]
fn nested_git_root_selects_covered_functions_and_workspace_rules() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    write_nested_member(repo);
    init_git_with_initial_commit(repo);
    let member = repo.join("member");

    let collect = cargo_affected(&member, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "baseline collect failed: {}",
        combined_output(&collect),
    );

    // Both the coverage DB and JSON report must name paths relative to Git's
    // root. The old implementation recorded src/logic.rs in the database
    // but Git reported member/src/logic.rs; hit-exact selected zero tests.
    let db = rusqlite::Connection::open(member.join("target/affected/coverage.db")).unwrap();
    let recorded: i64 = db
        .query_row(
            "SELECT count(*) FROM test_regions WHERE source_file = 'member/src/logic.rs'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(recorded > 0, "Git-root path must occur in stored coverage");

    replace_in_file(&member.join("src/logic.rs"), "    40\n", "    41\n");
    let report_path = member.join("target/affected/nested-report.json");
    let report_arg = report_path.to_string_lossy();
    let status = cargo_affected(
        &member,
        &[
            "affected",
            "status",
            "--report-json",
            report_arg.as_ref(),
            "--report-detail",
            "full",
        ],
    );
    assert!(
        status.status.success(),
        "status: {}",
        combined_output(&status)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(&report_path).unwrap()).unwrap();
    assert_eq!(report["cache"]["status"], "hit-exact");
    assert_eq!(report["selection"]["summary"]["selected"], 1);
    assert_eq!(
        report["selection"]["changed_files"][0]["path"],
        "member/src/logic.rs",
    );
    assert_eq!(
        report["selection"]["changed_files"][0]["tracked_by_coverage"],
        true,
    );
    assert!(report["selection"]["selected_tests"][0]["test_name"]
        .as_str()
        .unwrap()
        .contains("forty"),);

    // Verify it really fails when executed, rather than merely assuming
    // the coverage-selected test can detect this particular mutation.
    let full = std::process::Command::new("cargo")
        .args(["nextest", "run"])
        .current_dir(&member)
        .output()
        .unwrap();
    assert!(
        !full.status.success(),
        "broken full suite unexpectedly passed"
    );
    assert!(combined_output(&full).contains("forty"));

    git(repo, &["checkout", "--", "member/src/logic.rs"]);
    fs::write(member.join("fixtures/test.snap"), "modified\n").unwrap();
    let rule = cargo_affected(&member, &["affected", "status"]);
    assert!(
        rule.status.success(),
        "config rule: {}",
        combined_output(&rule)
    );
    assert!(
        combined_output(&rule).contains("selection=1/2"),
        "workspace-relative input rule failed: {}",
        combined_output(&rule),
    );
}
