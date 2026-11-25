use std::path::PathBuf;
use std::process::Command;

use crate::{
    assert_no_staging_dirs, cargo_affected, cargo_affected_with_env, combined_output, git,
    init_git_with_initial_commit, replace_in_file, write_two_module_project,
};

fn llvm_tool(name: &str) -> PathBuf {
    let sysroot = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .unwrap();
    assert!(sysroot.status.success());
    let version = Command::new("rustc").arg("-vV").output().unwrap();
    assert!(version.status.success());
    let version = String::from_utf8(version.stdout).unwrap();
    let host = version
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap();
    PathBuf::from(String::from_utf8(sysroot.stdout).unwrap().trim())
        .join("lib")
        .join("rustlib")
        .join(host)
        .join("bin")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

#[test]
fn aggregate_keeps_exact_counts_across_compaction_batches() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_aggregate_profile");
    let mut source = String::from(
        "#[inline(never)]\npub fn choose(value: bool) -> u32 {\n    if value { 1 } else { 2 }\n}\n",
    );
    // Cross more than two aggregate batches, including an incomplete batch.
    for i in 0..70 {
        let value = i % 2 == 0;
        source.push_str(&format!(
            "#[test]\nfn case_{i}() {{ assert_eq!(choose({value}), {}); }}\n",
            if value { 1 } else { 2 }
        ));
    }
    std::fs::write(dir.join("src/lib.rs"), source).unwrap();
    init_git_with_initial_commit(dir);
    let profile = dir.join("target/affected/aggregate.profdata");
    let result = cargo_affected(
        dir,
        &[
            "affected",
            "collect",
            "--aggregate-profile",
            profile.to_str().unwrap(),
            "--",
            "--no-fail-fast",
        ],
    );
    assert!(result.status.success(), "{}", combined_output(&result));
    assert_no_staging_dirs(dir, "aggregate collect");

    // Read the actual compiler's binary metadata rather than depending on
    // Cargo's current intermediate artifact layout.
    let list = Command::new("cargo")
        .args([
            "nextest",
            "list",
            "--message-format",
            "json",
            "--target-dir",
        ])
        .arg(dir.join("target/affected/build"))
        .env("RUSTFLAGS", "-C instrument-coverage")
        .env(
            "LLVM_PROFILE_FILE",
            dir.join("target/affected/build/discovery-%p-%m.profraw"),
        )
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(list.status.success(), "{}", combined_output(&list));
    let listing: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    let binary = listing["rust-suites"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()["binary-path"]
        .as_str()
        .unwrap();
    let output = Command::new(llvm_tool("llvm-cov"))
        .arg("export")
        .arg(binary)
        .arg("--instr-profile")
        .arg(&profile)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", combined_output(&output));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let function = report["data"][0]["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| {
            f["name"]
                .as_str()
                .is_some_and(|name| name.contains("choose"))
        })
        .expect("choose coverage");
    assert_eq!(function["count"], 70);
    let counts = function["regions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|region| region[4].as_u64())
        .collect::<Vec<_>>();
    assert!(counts.contains(&35), "{function}");
}

#[test]
fn collect_predicts_before_overwriting_baseline_and_runs_every_test() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_collect_shadow");
    init_git_with_initial_commit(dir);
    let path = dir.join("target/selection.json");
    let args = [
        "affected",
        "collect",
        "--report-json",
        path.to_str().unwrap(),
        "--report-detail",
        "full",
        "--",
        "--no-fail-fast",
        "--message-format",
        "libtest-json-plus",
    ];
    let run = || cargo_affected_with_env(dir, &args, &[("NEXTEST_EXPERIMENTAL_LIBTEST_JSON", "1")]);
    let first = run();
    assert!(first.status.success(), "{}", combined_output(&first));
    let report =
        || serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(report()["cache"]["status"], "miss-no-coverage");

    replace_in_file(&dir.join("src/math.rs"), "a + b", "a.wrapping_add(b)");
    git(dir, &["add", "src/math.rs"]);
    git(dir, &["commit", "-m", "change add"]);
    let second = run();
    assert!(second.status.success(), "{}", combined_output(&second));
    let predicted = report();
    assert_eq!(predicted["cache"]["status"], "hit-with-divergence");
    let selected = predicted["selection"]["selected_tests"].as_array().unwrap();
    assert!(selected
        .iter()
        .any(|t| t["test_name"].as_str().unwrap().contains("add")));
    assert!(!selected
        .iter()
        .any(|t| t["test_name"].as_str().unwrap().contains("multiply")));
    // The prediction was advisory: multiply still ran, and the baseline
    // was refreshed using the same instrumented execution.
    let events = String::from_utf8(second.stdout).unwrap();
    assert!(events
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .any(|e| e["event"] == "ok"
            && e["name"]
                .as_str()
                .is_some_and(|name| name.contains("multiply"))));
    assert_no_staging_dirs(dir, "shadow collect");
}
