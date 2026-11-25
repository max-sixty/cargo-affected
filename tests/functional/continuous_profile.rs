#![cfg(target_os = "linux")]

use std::{fs, path::Path};

use crate::{
    assert_no_staging_dirs, cargo_affected, combined_output, git, init_git_with_initial_commit,
    replace_in_file,
};

fn write_project(root: &Path) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(root.join(".gitignore"), "/target\n/Cargo.lock\n").unwrap();
    fs::write(
        root.join("Cargo.toml"),
        r#"
[package]
name = "sample_continuous_child"
version = "0.1.0"
edition = "2021"
[[bin]]
name = "daemon"
path = "src/main.rs"
test = false
"#,
    )
    .unwrap();
    fs::write(
        root.join("src/logic.rs"),
        r#"
#[inline(never)]
pub fn daemon_work() -> u32 { std::hint::black_box(42) }
"#,
    )
    .unwrap();
    fs::write(
        root.join("src/main.rs"),
        r#"
mod logic;
fn main() {
    // The caller intentionally clears the environment except for the
    // profiling destination. Verify that the isolation still holds.
    if std::env::args().any(|arg| arg == "isolated") {
        assert!(std::env::vars_os().all(|(key, _)| {
            key == "LLVM_PROFILE_FILE" || key == "__LLVM_PROFILE_RT_INIT_ONCE"
        }));
    }
    assert!(std::env::var_os("LLVM_PROFILE_FILE").is_some());
    println!("{}", logic::daemon_work());
    loop { std::thread::park(); }
}
"#,
    )
    .unwrap();
    fs::write(
        root.join("tests/daemon.rs"),
        r#"
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

fn kill_daemon(isolated: bool) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_daemon"));
    if isolated {
        command.env_clear()
            .env("LLVM_PROFILE_FILE", std::env::var_os("LLVM_PROFILE_FILE").unwrap())
            .arg("isolated");
    }
    let mut child = command.stdout(Stdio::piped()).spawn().unwrap();
    let mut line = String::new();
    let result = BufReader::new(child.stdout.take().unwrap()).read_line(&mut line);
    // SIGKILL deliberately bypasses all normal runtime exit handlers.
    let killed = child.kill();
    let status = child.wait().unwrap();
    result.unwrap();
    killed.unwrap();
    assert_eq!(line.trim(), "42");
    assert!(!status.success());
}

#[test]
fn killed_daemon() {
    kill_daemon(true);
    kill_daemon(false);
}

#[test]
fn unrelated() { assert_eq!(1 + 1, 2); }
"#,
    )
    .unwrap();
    init_git_with_initial_commit(root);
}

#[test]
fn killed_environment_isolated_child_selects_its_caller() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_project(root);
    let result = cargo_affected(
        root,
        &[
            "affected",
            "collect",
            "--continuous",
            "--",
            "--no-fail-fast",
        ],
    );
    let log = combined_output(&result);
    assert!(result.status.success(), "{log}");
    assert!(
        log.contains("verified continuous LLVM profile persistence"),
        "{log}"
    );
    assert_no_staging_dirs(root, "continuous collect");
    replace_in_file(
        &root.join("src/logic.rs"),
        "black_box(42)",
        "black_box(40 + 2)",
    );
    git(root, &["add", "src/logic.rs"]);
    git(root, &["commit", "-m", "change killed daemon"]);
    let report = root.join("target/selection.json");
    let result = cargo_affected(
        root,
        &[
            "affected",
            "status",
            "--continuous",
            "--report-json",
            report.to_str().unwrap(),
            "--report-detail",
            "full",
        ],
    );
    assert!(result.status.success(), "{}", combined_output(&result));
    let selection: serde_json::Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
    assert_eq!(
        selection["cache"]["status"], "hit-with-divergence",
        "{selection}"
    );
    let tests = selection["selection"]["selected_tests"].as_array().unwrap();
    assert_eq!(tests.len(), 1, "{selection}");
    assert_eq!(tests[0]["test_name"], "killed_daemon");
    let wrong_mode = cargo_affected(
        root,
        &[
            "affected",
            "status",
            "--report-json",
            report.to_str().unwrap(),
        ],
    );
    assert!(
        wrong_mode.status.success(),
        "{}",
        combined_output(&wrong_mode)
    );
    let report: serde_json::Value = serde_json::from_slice(&fs::read(report).unwrap()).unwrap();
    assert_eq!(report["cache"]["status"], "miss-fingerprint", "{report}");
}
