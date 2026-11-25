use crate::{cargo_affected, combined_output, git, init_git_with_initial_commit, replace_in_file};
use std::{fs, path::Path};

fn write_project(dir: &Path, package: &str) {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::create_dir_all(dir.join("tests")).unwrap();
    fs::write(dir.join(".gitignore"), "/target\n/Cargo.lock\n").unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        r#"
[package]
name = "PACKAGE_NAME"
version = "0.1.0"
edition = "2021"
[[bin]]
name = "parent"
path = "src/parent.rs"
test = false
[[bin]]
name = "child"
path = "src/child.rs"
test = false
[[bin]]
name = "other"
path = "src/other.rs"
test = false
"#
        .replace("PACKAGE_NAME", package),
    )
    .unwrap();
    fs::write(
        dir.join("src/parent.rs"),
        r#"
fn main() {
    let path = std::env::args_os().nth(1).unwrap();
    assert!(std::process::Command::new(path).status().unwrap().success());
}
"#,
    )
    .unwrap();
    fs::write(
        dir.join("src/child.rs"),
        "mod arithmetic;\nfn main() { assert_eq!(arithmetic::answer(), 42); }\n",
    )
    .unwrap();
    fs::write(
        dir.join("src/arithmetic.rs"),
        "#[inline(never)]\npub fn answer() -> u32 { 40 + 2 }\n",
    )
    .unwrap();
    fs::write(
        dir.join("src/other.rs"),
        "fn main() { println!(\"other\"); }\n",
    )
    .unwrap();
    fs::write(
        dir.join("tests/commands.rs"),
        r#"
#[test]
fn launches_child() {
    assert!(std::process::Command::new(env!("CARGO_BIN_EXE_parent"))
        .arg(env!("CARGO_BIN_EXE_child")).status().unwrap().success());
}
#[test]
fn launches_other() {
    assert!(std::process::Command::new(env!("CARGO_BIN_EXE_other"))
        .status().unwrap().success());
}
#[cfg(unix)]
#[test]
fn replaces_test_process() {
    use std::os::unix::process::CommandExt;
    panic!("exec failed: {}", std::process::Command::new(env!("CARGO_BIN_EXE_other")).exec());
}
#[test]
fn no_subprocess() { assert_eq!(2 + 2, 4); }
"#,
    )
    .unwrap();
    init_git_with_initial_commit(dir);
}

fn selection(dir: &Path) -> Vec<String> {
    let path = dir.join("target/selection.json");
    let status = cargo_affected(
        dir,
        &[
            "affected",
            "status",
            "--report-json",
            path.to_str().unwrap(),
            "--report-detail",
            "full",
        ],
    );
    assert!(status.status.success(), "{}", combined_output(&status));
    let json: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(json["cache"]["status"], "hit-with-divergence", "{json}");
    json["selection"]["selected_tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["test_name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn executable_and_grandchild_coverage_selects_only_the_calling_test() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_project(dir, "sample_executable_coverage");
    let result = cargo_affected(dir, &["affected", "collect", "--", "--no-fail-fast"]);
    assert!(result.status.success(), "{}", combined_output(&result));
    assert!(combined_output(&result).contains("3 non-test executable(s)"));
    // Change a function that exists only in a separately compiled grandchild,
    // not in the test harness or its directly launched parent.
    replace_in_file(&dir.join("src/arithmetic.rs"), "40 + 2", "41 + 1");
    git(dir, &["add", "src/arithmetic.rs"]);
    git(dir, &["commit", "-m", "change grandchild function"]);
    assert_eq!(selection(dir), ["launches_child"]);
    // Structural edits to the executable root also select its caller.
    replace_in_file(
        &dir.join("src/child.rs"),
        "mod arithmetic;",
        "mod arithmetic; // changed",
    );
    git(dir, &["add", "src/child.rs"]);
    git(dir, &["commit", "-m", "change executable root"]);
    assert_eq!(selection(dir), ["launches_child"]);
}

#[test]
fn direct_and_exec_replacement_profiles_select_the_executable_callers() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_project(dir, "sample_exec_replacement");
    let result = cargo_affected(dir, &["affected", "collect", "--", "--no-fail-fast"]);
    assert!(result.status.success(), "{}", combined_output(&result));
    replace_in_file(&dir.join("src/other.rs"), "other", "other changed");
    git(dir, &["add", "src/other.rs"]);
    git(dir, &["commit", "-m", "change second executable"]);
    let mut selected = selection(dir);
    selected.sort();
    if cfg!(unix) {
        assert_eq!(selected, ["launches_other", "replaces_test_process"]);
    } else {
        assert_eq!(selected, ["launches_other"]);
    }
}
