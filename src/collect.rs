//! Coverage collection pipeline.
//!
//! Builds with coverage instrumentation, discovers tests from the compiled
//! binaries, then runs each test individually (in parallel) to collect
//! per-test coverage data. The resulting test-to-file mappings are stored
//! in SQLite.
//!
//! Approach:
//! 1. Find the project/workspace root.
//! 2. Build with `-C instrument-coverage` via `cargo test --no-run`.
//! 3. List tests by running each test binary with `--list --format=terse`.
//! 4. For each test (in parallel across num_cpus workers): run it alone with
//!    its own profraw directory, then `llvm-profdata merge` + `llvm-cov export`
//!    to get coverage JSON.
//! 5. Parse JSON, extract file list, store in DB.
//!
//! Why we orchestrate the workers ourselves instead of piggybacking on a single
//! `cargo nextest run` pass: nextest's `libtest-json-plus` (v0.1 as of 0.9.132)
//! does not emit per-test PIDs, so there's no way to correlate `%p.profraw`
//! files back to test names in one batch run. Running tests ourselves lets us
//! set `LLVM_PROFILE_FILE` per test and know the mapping trivially.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use camino::Utf8PathBuf;

use crate::coverage;
use crate::db::{db_path, difftest_dir, Db};
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

    // Our profraw files live under target/difftest/ alongside the DB — a
    // conventional build-artifact location, gitignored by cargo, and surviving
    // the run so users can inspect artifacts post-mortem. PID-suffixed so
    // concurrent `collect` invocations don't wipe each other's in-flight files.
    let profraw_dir = difftest_dir(project_root).join(format!("profraw-{}", std::process::id()));
    if profraw_dir.exists() {
        std::fs::remove_dir_all(&profraw_dir).context("failed to clean profraw dir")?;
    }
    std::fs::create_dir_all(&profraw_dir).context("failed to create profraw dir")?;

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
        .env("LLVM_PROFILE_FILE", profraw_dir.join("%p-%m.profraw"))
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
    let parsed = parse_build_artifacts(&String::from_utf8_lossy(&build_output.stdout), project_root)?;
    let build_info = parsed.test_binaries;
    let bin_exes = parsed.bin_exes;
    eprintln!("found {} test binaries", build_info.len());
    if !bin_exes.is_empty() {
        eprintln!(
            "bin targets (CARGO_BIN_EXE_*): {}",
            bin_exes.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    }

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
    let all_test_entries = discover_tests(&build_info, project_root)?;
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

    // Step 4: Run tests in parallel and collect coverage.
    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let total = test_entries.len();
    eprintln!("collecting coverage for {total} tests with {num_workers} workers...");

    // `progress` guards both the done-counter and stderr writes so the [n/total]
    // numbering never prints out of order across workers.
    let progress: Mutex<usize> = Mutex::new(0);
    let work: Mutex<VecDeque<(usize, (String, PathBuf))>> =
        Mutex::new(test_entries.into_iter().enumerate().collect());
    let mappings: Mutex<Vec<(String, BTreeSet<Utf8PathBuf>)>> = Mutex::new(Vec::new());

    let ctx = CollectContext {
        profraw_base: &profraw_dir,
        llvm_profdata: &llvm_profdata,
        llvm_cov: &llvm_cov,
        project_root,
        bin_exes: &bin_exes,
    };

    std::thread::scope(|s| {
        for _ in 0..num_workers {
            s.spawn(|| loop {
                let Some((idx, (test_name, binary))) = work.lock().unwrap().pop_front() else {
                    break;
                };
                let test_start = Instant::now();
                let outcome = collect_one_test(&ctx, idx, &test_name, &binary);
                let elapsed = test_start.elapsed().as_secs_f64();
                // Hold the progress lock across increment + eprintln so the
                // rendered sequence matches the counter.
                let mut guard = progress.lock().unwrap();
                *guard += 1;
                let n = *guard;
                match outcome {
                    Ok(CollectOutcome::Collected { mut files, code }) => {
                        files.extend(crate_roots.iter().cloned());
                        let status = match code {
                            Some(0) => String::new(),
                            Some(c) => format!("FAIL (exit {c}) "),
                            None => "FAIL (signal) ".to_string(),
                        };
                        eprintln!(
                            "[{n}/{total}] {test_name}: {status}{} files ({elapsed:.1}s)",
                            files.len()
                        );
                        drop(guard);
                        mappings.lock().unwrap().push((test_name, files));
                    }
                    Ok(CollectOutcome::Skipped(reason)) => {
                        eprintln!("[{n}/{total}] {test_name}: SKIP ({reason})");
                    }
                    Err(e) => {
                        eprintln!("[{n}/{total}] {test_name}: ERROR ({e:#})");
                    }
                }
            });
        }
    });

    let mappings = mappings.into_inner().unwrap();

    // Step 4b: Sweep any stray profraw files that instrumented subprocesses
    // (those that didn't inherit our LLVM_PROFILE_FILE) dropped in project root.
    clean_profraw_files(project_root)?;

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
        "done. {} tests, {} mappings stored in target/difftest/coverage.db ({:.1}s total)",
        mappings.len(),
        mapping_count,
        total_elapsed.as_secs_f64(),
    );
    Ok(())
}

/// Outcome of attempting coverage collection for a single test.
enum CollectOutcome {
    /// Coverage extracted. The test may have passed or failed — failing tests
    /// still produce coverage data worth recording. `code` is the process exit
    /// code (`None` means killed by signal); `Some(0)` implies pass.
    Collected {
        files: BTreeSet<Utf8PathBuf>,
        code: Option<i32>,
    },
    /// Coverage collection could not complete (no profraw, tool failure, parse error).
    Skipped(String),
}

/// Shared, read-only inputs reused for every test collection.
struct CollectContext<'a> {
    profraw_base: &'a Path,
    llvm_profdata: &'a Path,
    llvm_cov: &'a Path,
    project_root: &'a Path,
    bin_exes: &'a BTreeMap<String, PathBuf>,
}

/// Run one test and extract its coverage.
///
/// Uses a per-test subdirectory `<profraw_base>/test-<idx>/` to isolate profraw
/// files from concurrently-running tests. The `%p-%m.profraw` pattern lets
/// subprocesses of the test emit distinct files.
fn collect_one_test(
    ctx: &CollectContext<'_>,
    idx: usize,
    test_name: &str,
    binary: &Path,
) -> Result<CollectOutcome> {
    let test_dir = ctx.profraw_base.join(format!("test-{idx}"));
    std::fs::create_dir_all(&test_dir).context("creating per-test profraw dir")?;

    let profraw_pattern = test_dir.join("%p-%m.profraw");
    let profdata_path = test_dir.join("coverage.profdata");

    let mut cmd = Command::new(binary);
    cmd.arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .env("LLVM_PROFILE_FILE", &profraw_pattern)
        .current_dir(ctx.project_root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Integration tests commonly invoke workspace binaries as subprocesses via
    // `env!("CARGO_BIN_EXE_<name>")` or `env::var("CARGO_BIN_EXE_<name>")`.
    // cargo sets these when it runs tests; we're invoking the test binary
    // directly, so we set them ourselves.
    for (name, path) in ctx.bin_exes {
        cmd.env(format!("CARGO_BIN_EXE_{name}"), path);
    }
    let status = cmd.status().context("failed to run test binary")?;

    let code = status.code();

    let profraw_files = list_profraw_files(&test_dir)?;
    if profraw_files.is_empty() {
        return Ok(CollectOutcome::Skipped("no profraw generated".into()));
    }

    let mut merge_cmd = Command::new(ctx.llvm_profdata);
    merge_cmd.arg("merge").arg("--sparse");
    for f in &profraw_files {
        merge_cmd.arg(f);
    }
    merge_cmd.arg("-o").arg(&profdata_path);
    let merge_output = merge_cmd
        .output()
        .context("failed to run llvm-profdata merge")?;
    if !merge_output.status.success() {
        return Ok(CollectOutcome::Skipped(format!(
            "llvm-profdata merge failed: {}",
            String::from_utf8_lossy(&merge_output.stderr).trim()
        )));
    }

    let export_output = Command::new(ctx.llvm_cov)
        .arg("export")
        .arg("--format=text")
        .arg(format!("--instr-profile={}", profdata_path.display()))
        .arg(binary)
        .output()
        .context("failed to run llvm-cov export")?;
    if !export_output.status.success() {
        return Ok(CollectOutcome::Skipped(format!(
            "llvm-cov export failed: {}",
            String::from_utf8_lossy(&export_output.stderr).trim()
        )));
    }

    let json = String::from_utf8_lossy(&export_output.stdout);
    match coverage::extract_covered_files(&json, ctx.project_root) {
        Ok(files) => Ok(CollectOutcome::Collected { files, code }),
        Err(e) => Ok(CollectOutcome::Skipped(format!("parse error: {e}"))),
    }
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

    if !db_path(project_root).exists() {
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
    test_binaries: &[TestBinaryInfo],
    project_root: &Path,
) -> Result<Vec<(String, PathBuf)>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut entries = Vec::new();

    for info in test_binaries {
        let binary = &info.executable;
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

/// Parsed output from `cargo test --no-run --message-format=json`.
struct BuildArtifacts {
    /// Test executables — anything with `profile.test == true` and a non-null
    /// `executable`. These are what we run to collect coverage per test.
    test_binaries: Vec<TestBinaryInfo>,
    /// Workspace bin targets (kind=["bin"], profile.test=false) keyed by name.
    /// Passed through as `CARGO_BIN_EXE_<name>` env vars so integration tests
    /// that shell out to built binaries can find them.
    bin_exes: BTreeMap<String, PathBuf>,
}

/// Parse compiler-artifact messages from `cargo test --no-run --message-format=json`.
///
/// Extracts both test binaries (to run for coverage) and workspace bin
/// executables (for `CARGO_BIN_EXE_<name>` env vars).
fn parse_build_artifacts(json_output: &str, project_root: &Path) -> Result<BuildArtifacts> {
    let root = project_root
        .canonicalize()
        .context("failed to canonicalize project root")?;
    let mut test_binaries = Vec::new();
    let mut bin_exes = BTreeMap::new();
    for line in json_output.lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if msg.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
            continue;
        }
        let Some(exe) = msg.get("executable").and_then(|e| e.as_str()) else {
            continue;
        };
        let is_test = msg
            .get("profile")
            .and_then(|p| p.get("test"))
            .and_then(|t| t.as_bool())
            .unwrap_or(false);
        let target = msg.get("target");
        let kinds: Vec<&str> = target
            .and_then(|t| t.get("kind"))
            .and_then(|k| k.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        let name = target.and_then(|t| t.get("name")).and_then(|n| n.as_str());

        if is_test {
            // Extract the crate root path (target.src_path) and make it relative.
            let src_path = target
                .and_then(|t| t.get("src_path"))
                .and_then(|s| s.as_str())
                .and_then(|abs| {
                    Path::new(abs)
                        .strip_prefix(&root)
                        .ok()
                        .and_then(|rel| Utf8PathBuf::try_from(rel.to_path_buf()).ok())
                });
            test_binaries.push(TestBinaryInfo {
                executable: PathBuf::from(exe),
                src_path,
            });
        } else if kinds.contains(&"bin") {
            if let Some(name) = name {
                bin_exes.insert(name.to_string(), PathBuf::from(exe));
            }
        }
    }
    Ok(BuildArtifacts {
        test_binaries,
        bin_exes,
    })
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
