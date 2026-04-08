//! Coverage collection pipeline.
//!
//! Builds with coverage instrumentation, discovers tests from the compiled
//! binaries, then runs each test individually to collect per-test coverage
//! data. The resulting test-to-file mappings are stored in SQLite.
//!
//! Approach:
//! 1. Find the project/workspace root.
//! 2. Build with `-C instrument-coverage` via `cargo test --no-run`.
//! 3. List tests by running each test binary with `--list --format=terse`.
//! 4. For each test: run it alone with a unique profraw path, then
//!    `llvm-profdata merge` + `llvm-cov export` to get coverage JSON.
//! 5. Parse JSON, extract file list, store in DB.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use camino::Utf8PathBuf;

use crate::coverage;
use crate::db::Db;
use crate::project::{find_project_root, git_changed_files};

/// Entry point for `cargo difftest collect`.
pub fn collect(diff_base: Option<&str>) -> Result<()> {
    let total_start = Instant::now();
    let project = find_project_root()?;
    let project_root = &project.workspace_root;
    eprintln!("project root: {}", project_root.display());

    let llvm_profdata = find_llvm_tool("llvm-profdata")?;
    let llvm_cov = find_llvm_tool("llvm-cov")?;
    eprintln!("llvm-profdata: {}", llvm_profdata.display());
    eprintln!("llvm-cov: {}", llvm_cov.display());

    let profraw_dir = tempfile::tempdir().context("failed to create temp dir for profraw files")?;

    // Step 1: Build with coverage instrumentation and capture binary paths.
    eprintln!("building with coverage instrumentation...");
    let mut rustflags = std::env::var("RUSTFLAGS").unwrap_or_default();
    if !rustflags.is_empty() {
        rustflags.push(' ');
    }
    rustflags.push_str("-C instrument-coverage");
    let build_output = Command::new("cargo")
        .arg("test")
        .arg("--no-run")
        .arg("--message-format=json")
        .env("RUSTFLAGS", &rustflags)
        .env(
            "LLVM_PROFILE_FILE",
            profraw_dir.path().join("%p-%m.profraw").to_str().unwrap(),
        )
        .current_dir(project_root)
        .output()
        .context("failed to run cargo test --no-run")?;
    if !build_output.status.success() {
        bail!(
            "coverage build failed: {}",
            String::from_utf8_lossy(&build_output.stderr)
        );
    }

    // Step 1b: Clean up stray profraw files left in project root by build scripts.
    clean_profraw_files(project_root)?;

    // Step 2: Extract test binary paths and crate root src_paths from the build output.
    let build_info =
        parse_test_binaries(&String::from_utf8_lossy(&build_output.stdout), project_root)?;
    let test_binaries: Vec<PathBuf> = build_info.iter().map(|b| b.executable.clone()).collect();
    eprintln!("found {} test binaries", test_binaries.len());

    // Collect crate root files (lib.rs/main.rs) as implicit dependencies.
    let crate_roots: BTreeSet<Utf8PathBuf> = build_info
        .iter()
        .filter_map(|b| b.src_path.as_ref())
        .cloned()
        .collect();
    if !crate_roots.is_empty() {
        eprintln!(
            "crate roots (implicit deps): {}",
            crate_roots
                .iter()
                .map(|p| p.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Discover tests and which binary owns each one.
    let all_test_entries = discover_tests(&test_binaries, project_root)?;
    eprintln!("found {} tests", all_test_entries.len());

    if all_test_entries.is_empty() {
        eprintln!("no tests found, nothing to collect");
        return Ok(());
    }

    // If --diff-base is provided, only collect affected tests + new tests.
    let test_entries = if let Some(base) = diff_base {
        select_tests_for_incremental(base, project_root, &all_test_entries)?
    } else {
        all_test_entries
    };

    if test_entries.is_empty() {
        eprintln!("no tests to re-collect");
        return Ok(());
    }

    // Step 4: Run each test individually and collect coverage.
    let mut mappings: Vec<(String, BTreeSet<Utf8PathBuf>)> = Vec::new();

    for (i, (test_name, binary)) in test_entries.iter().enumerate() {
        eprint!("[{}/{}] {test_name}... ", i + 1, test_entries.len());
        let test_start = Instant::now();

        let profraw_path = profraw_dir.path().join(format!("test-{i}.profraw"));
        let profdata_path = profraw_dir.path().join(format!("test-{i}.profdata"));

        // Clean any leftover profraw from previous iteration.
        clean_profraw_files(profraw_dir.path())?;

        // Run the test on its owning binary.
        let status = Command::new(binary)
            .arg("--exact")
            .arg(test_name)
            .arg("--nocapture")
            .env("LLVM_PROFILE_FILE", profraw_path.to_str().unwrap())
            .current_dir(project_root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let test_failed = match status {
            Ok(s) if s.success() => false,
            Ok(s) => {
                eprint!("FAIL (exit {}) ", s.code().unwrap_or(-1));
                true
            }
            Err(e) => {
                eprintln!("SKIP (failed to execute: {e})");
                continue;
            }
        };

        // Collect profraw even for failing tests -- they still generated coverage data.
        let profraw_files = list_profraw_files(profraw_dir.path())?;
        if profraw_files.is_empty() {
            eprintln!("{}(no profraw generated)", if test_failed { "" } else { "SKIP " });
            continue;
        }

        // Merge profraw files into a single profdata.
        let mut merge_cmd = Command::new(&llvm_profdata);
        merge_cmd.arg("merge").arg("--sparse");
        for f in &profraw_files {
            merge_cmd.arg(f);
        }
        merge_cmd.arg("-o").arg(&profdata_path);

        let merge_output = merge_cmd
            .output()
            .context("failed to run llvm-profdata merge")?;
        if !merge_output.status.success() {
            eprintln!(
                "SKIP (llvm-profdata merge failed: {})",
                String::from_utf8_lossy(&merge_output.stderr).trim()
            );
            continue;
        }

        // Export coverage JSON using only the test's own binary as the object file.
        let export_output = Command::new(&llvm_cov)
            .arg("export")
            .arg("--format=text")
            .arg(format!("--instr-profile={}", profdata_path.display()))
            .arg(binary)
            .output()
            .context("failed to run llvm-cov export")?;
        if !export_output.status.success() {
            eprintln!(
                "SKIP (llvm-cov export failed: {})",
                String::from_utf8_lossy(&export_output.stderr).trim()
            );
            continue;
        }

        let json = String::from_utf8_lossy(&export_output.stdout);
        match coverage::extract_covered_files(&json, project_root) {
            Ok(mut files) => {
                // Add crate roots as implicit dependencies for all tests.
                files.extend(crate_roots.iter().cloned());
                let elapsed = test_start.elapsed();
                eprintln!("{} files ({:.1}s)", files.len(), elapsed.as_secs_f64());
                mappings.push((test_name.clone(), files));
            }
            Err(e) => {
                eprintln!("SKIP (parse error: {e})");
            }
        }
    }

    // Step 5: Store in DB.
    let total_elapsed = total_start.elapsed();
    let mapping_count: usize = mappings.iter().map(|(_, f)| f.len()).sum();

    if diff_base.is_some() {
        eprintln!("updating {} test mappings in database...", mappings.len());
        let mut db = Db::open(project_root)?;
        db.update_coverage(&mappings)?;
    } else {
        eprintln!("storing {} test mappings in database...", mappings.len());
        let mut db = Db::open(project_root)?;
        db.store_coverage(&mappings)?;
    }

    eprintln!(
        "done. {} tests, {} mappings stored in .difftest.db ({:.1}s total)",
        mappings.len(),
        mapping_count,
        total_elapsed.as_secs_f64(),
    );
    Ok(())
}

/// Select which tests need re-collection for incremental mode.
///
/// Queries the existing DB for tests covering the changed files, then also
/// discovers new tests (in binaries but not in the DB). Returns the subset
/// of test entries to run.
fn select_tests_for_incremental(
    diff_base: &str,
    project_root: &Path,
    all_test_entries: &[(String, PathBuf)],
) -> Result<Vec<(String, PathBuf)>> {
    let changed_files = git_changed_files(project_root, Some(diff_base))?;
    if changed_files.is_empty() {
        eprintln!("no changed files vs {diff_base}");
        return Ok(Vec::new());
    }

    eprintln!(
        "{} changed files vs {diff_base}:",
        changed_files.len()
    );
    for f in &changed_files {
        eprintln!("  {f}");
    }

    let db_path = project_root.join(".difftest.db");
    if !db_path.exists() {
        eprintln!("no existing DB — collecting all tests");
        return Ok(all_test_entries.to_vec());
    }

    let db = Db::open(project_root)?;
    let file_refs: Vec<&str> = changed_files.iter().map(|s| s.as_str()).collect();
    let affected_tests = db.tests_covering(&file_refs)?;
    let known_tests = db.all_test_names()?;

    // New tests: in binaries but not in the DB.
    let new_tests: BTreeSet<&str> = all_test_entries
        .iter()
        .map(|(name, _)| name.as_str())
        .filter(|name| !known_tests.contains(*name))
        .collect();

    let tests_to_run: BTreeSet<&str> = affected_tests
        .iter()
        .map(|s| s.as_str())
        .chain(new_tests.iter().copied())
        .collect();

    if !new_tests.is_empty() {
        eprintln!("{} new tests discovered", new_tests.len());
    }
    eprintln!(
        "{} tests to re-collect ({} affected + {} new)",
        tests_to_run.len(),
        affected_tests.len(),
        new_tests.len()
    );

    let selected: Vec<(String, PathBuf)> = all_test_entries
        .iter()
        .filter(|(name, _)| tests_to_run.contains(name.as_str()))
        .cloned()
        .collect();

    Ok(selected)
}

/// Discover tests and which binary owns each one.
///
/// Returns `(test_name, binary_path)` pairs. Each test is listed once, associated
/// with the first binary that contains it.
fn discover_tests(
    test_binaries: &[PathBuf],
    project_root: &Path,
) -> Result<Vec<(String, PathBuf)>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut entries = Vec::new();

    for binary in test_binaries {
        let list_output = Command::new(binary)
            .arg("--list")
            .arg("--format=terse")
            .current_dir(project_root)
            .output();

        match list_output {
            Ok(o) if o.status.success() => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                for line in stdout.lines() {
                    if let Some(name) = line.strip_suffix(": test") {
                        if seen.insert(name.to_string()) {
                            entries.push((name.to_string(), binary.clone()));
                        }
                    }
                }
            }
            Ok(o) => {
                eprintln!(
                    "warning: failed to list tests from {}: {}",
                    binary.display(),
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
            Err(e) => {
                eprintln!(
                    "warning: failed to execute {}: {e}",
                    binary.display()
                );
            }
        }
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

/// Info about a test binary extracted from cargo's JSON output.
struct TestBinaryInfo {
    executable: PathBuf,
    /// Crate root source path (lib.rs/main.rs), relative to project root.
    src_path: Option<Utf8PathBuf>,
}

/// Parse test binary paths from `cargo test --no-run --message-format=json` output.
///
/// Each JSON line with `"reason":"compiler-artifact"` and a `"test"` profile
/// contains the executable path in `"executable"` and the crate root in
/// `"target"."src_path"`.
fn parse_test_binaries(json_output: &str, project_root: &Path) -> Result<Vec<TestBinaryInfo>> {
    let root = project_root
        .canonicalize()
        .context("failed to canonicalize project root")?;
    let mut binaries = Vec::new();
    for line in json_output.lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if msg.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
            continue;
        }
        // Only include test targets (profile.test == true).
        let is_test = msg
            .get("profile")
            .and_then(|p| p.get("test"))
            .and_then(|t| t.as_bool())
            .unwrap_or(false);
        if !is_test {
            continue;
        }
        if let Some(exe) = msg.get("executable").and_then(|e| e.as_str()) {
            // Extract the crate root path (target.src_path) and make it relative.
            let src_path = msg
                .get("target")
                .and_then(|t| t.get("src_path"))
                .and_then(|s| s.as_str())
                .and_then(|abs| {
                    Path::new(abs)
                        .strip_prefix(&root)
                        .ok()
                        .and_then(|rel| Utf8PathBuf::try_from(rel.to_path_buf()).ok())
                });
            binaries.push(TestBinaryInfo {
                executable: PathBuf::from(exe),
                src_path,
            });
        }
    }
    Ok(binaries)
}

/// Remove all .profraw files in the given directory.
fn clean_profraw_files(dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "profraw") {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// List all .profraw files in the given directory.
fn list_profraw_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "profraw") {
            files.push(path);
        }
    }
    Ok(files)
}

/// Find an LLVM tool by name.
///
/// Search order:
/// 1. Via `rustc --print sysroot` (the sysroot's llvm-tools).
/// 2. Via `xcrun --find` on macOS.
/// 3. On PATH.
fn find_llvm_tool(name: &str) -> Result<PathBuf> {
    // Try rustc sysroot first.
    if let Ok(output) = Command::new("rustc").arg("--print").arg("sysroot").output() {
        if output.status.success() {
            let sysroot = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let tool_path = PathBuf::from(&sysroot)
                .join("lib")
                .join("rustlib")
                .join(current_target())
                .join("bin")
                .join(name);
            if tool_path.exists() {
                return Ok(tool_path);
            }
        }
    }

    // macOS: try xcrun.
    #[cfg(target_os = "macos")]
    if let Ok(output) = Command::new("xcrun").arg("--find").arg(name).output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
    }

    // Fall back to PATH.
    if let Ok(output) = Command::new("which").arg(name).output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
    }

    bail!(
        "could not find {name}. Install `llvm-tools` via `rustup component add llvm-tools` \
         or ensure {name} is on PATH"
    )
}

/// Get the current rustc target triple.
fn current_target() -> String {
    Command::new("rustc")
        .arg("-vV")
        .output()
        .ok()
        .and_then(|o| {
            let stdout = String::from_utf8_lossy(&o.stdout).to_string();
            stdout
                .lines()
                .find(|l| l.starts_with("host:"))
                .map(|l| l.trim_start_matches("host:").trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}
