//! Coverage collection pipeline.
//!
//! Delegates build and test execution to `cargo nextest run`. We insert a
//! small runner shim (`cargo-difftest runner-shim`) via
//! `CARGO_TARGET_<TRIPLE>_RUNNER` that points `LLVM_PROFILE_FILE` at a
//! per-test subdirectory before `exec`ing the real test binary. After nextest
//! finishes we walk those subdirectories, merge profraws, export coverage,
//! and write test-to-file mappings to SQLite.
//!
//! Approach:
//! 1. Read crate roots (lib.rs/main.rs/tests/*.rs) from `cargo metadata` —
//!    every test implicitly depends on these so edits re-select the
//!    corresponding tests.
//! 2. `cargo nextest list --message-format json` to enumerate every binary
//!    (id + path) and every testcase. We write a binary_path → binary_id map
//!    to disk and hand it to the shim via `DIFFTEST_BINARY_MAP` — the shim
//!    needs binary_id to disambiguate same-named tests across binaries. In
//!    incremental mode, the listing also drives the nextest `-E` filter.
//! 3. `cargo nextest run` with `-C instrument-coverage` in RUSTFLAGS and the
//!    runner env set. The preceding `list` step built the binaries, so `run`
//!    is a cache hit on cargo. nextest handles parallelism and progress.
//! 4. Post-run: for each subdir of profraw_base, read the `meta` sidecar,
//!    merge with `llvm-profdata`, export with `llvm-cov`, parse covered files.
//!    Parallelized across workers.
//! 5. Store mappings in the DB keyed by (binary_id, test_name).

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use camino::Utf8PathBuf;

use crate::coverage;
use crate::db::{db_path, difftest_dir, Db, TestId, FINGERPRINT_KEEP};
use crate::fingerprint;
use crate::project::{find_project_root, git_changed_files};

/// Entry point for `cargo difftest collect`. Returns nextest's exit code.
pub fn collect(diff_base: Option<&str>, nextest_args: &[String]) -> Result<i32> {
    let total_start = Instant::now();
    let project = find_project_root()?;
    let project_root = &project.workspace_root;
    eprintln!("project root: {}", project_root.display());

    require_nextest(project_root)?;
    let self_path = std::env::current_exe().context("failed to resolve current executable")?;
    let target_triple = current_target();
    let runner_env_name = format!(
        "CARGO_TARGET_{}_RUNNER",
        target_triple.to_uppercase().replace('-', "_")
    );
    let runner_env_value = format!("{} runner-shim", self_path.display());

    let llvm_profdata = find_llvm_tool("llvm-profdata")?;
    let llvm_cov = find_llvm_tool("llvm-cov")?;
    eprintln!("llvm-profdata: {}", llvm_profdata.display());
    eprintln!("llvm-cov: {}", llvm_cov.display());

    // Profraw files live under target/difftest/ alongside the DB. PID suffix
    // so concurrent `collect` invocations don't wipe each other's files.
    let profraw_dir = difftest_dir(project_root).join(format!("profraw-{}", std::process::id()));
    if profraw_dir.exists() {
        std::fs::remove_dir_all(&profraw_dir).context("failed to clean profraw dir")?;
    }
    std::fs::create_dir_all(&profraw_dir).context("failed to create profraw dir")?;

    // Crate roots (lib.rs/main.rs/tests/*.rs) are added to every test's
    // coverage set so edits to them re-select the corresponding tests.
    let crate_roots = workspace_test_src_paths(project_root)?;
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

    let mut rustflags = std::env::var("RUSTFLAGS").unwrap_or_default();
    if !rustflags.is_empty() {
        rustflags.push(' ');
    }
    rustflags.push_str("-C instrument-coverage");

    // List first (always). Gives us:
    //   (a) binary_path → binary_id map for the shim
    //   (b) every (binary_id, test_name) pair, used in incremental mode to
    //       compute the rerun set
    // The list step builds with the same RUSTFLAGS; the subsequent run is a
    // cache hit. Fingerprint is taken now so Cargo.lock is in its final state
    // — status/run will compare against that same state.
    eprintln!("listing tests with cargo nextest list...");
    let listing = nextest_list(project_root, &rustflags, &profraw_dir)?;
    eprintln!(
        "found {} tests across {} binaries",
        listing.tests.len(),
        listing.binary_map.len()
    );
    let env_fingerprint = fingerprint::compute(&project)?;

    let binary_map_path = profraw_dir.join("binary_map.json");
    write_binary_map(&binary_map_path, &listing.binary_map)?;

    let nextest_filter = match diff_base {
        None => None,
        Some(base) => {
            let selected =
                select_tests_for_incremental(base, project_root, &env_fingerprint, &listing.tests)?;
            if selected.is_empty() {
                eprintln!("no tests to re-collect");
                return Ok(0);
            }
            Some(nextest_filter_expr(&selected))
        }
    };

    // Stray profraw files left in project root by the list-step build.
    clean_profraw_files(project_root)?;

    // Build (or cache-hit) and run, with the runner shim wired up so each
    // test writes to its own per-test profraw directory.
    eprintln!("running tests with cargo nextest run...");
    let mut cmd = Command::new("cargo");
    cmd.arg("nextest")
        .arg("run")
        .env("RUSTFLAGS", &rustflags)
        .env(&runner_env_name, &runner_env_value)
        .env("DIFFTEST_PROFRAW_BASE", &profraw_dir)
        .env("DIFFTEST_BINARY_MAP", &binary_map_path)
        // Catches build-script profraw before the runner shim kicks in for tests.
        .env("LLVM_PROFILE_FILE", profraw_dir.join("build-%p-%m.profraw"))
        .current_dir(project_root);
    if let Some(expr) = &nextest_filter {
        cmd.arg("-E").arg(expr);
    }
    for a in nextest_args {
        cmd.arg(a);
    }
    let status = cmd
        .status()
        .context("failed to run cargo nextest run")?;
    let nextest_exit = status.code().unwrap_or(1);

    // Sweep any stray profraw files instrumented subprocesses dropped in root.
    clean_profraw_files(project_root)?;

    // Step 4: Walk profraw_base subdirs — one per test — and extract coverage.
    let test_dirs = list_test_dirs(&profraw_dir)?;
    let total = test_dirs.len();
    if total == 0 {
        eprintln!("no per-test profraw directories found under {}", profraw_dir.display());
        eprintln!("(nextest exit code: {nextest_exit})");
        return Ok(nextest_exit);
    }

    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    eprintln!("extracting coverage for {total} tests with {num_workers} workers...");

    let progress: Mutex<usize> = Mutex::new(0);
    let work: Mutex<VecDeque<(usize, PathBuf)>> =
        Mutex::new(test_dirs.into_iter().enumerate().collect());
    let mappings: Mutex<Vec<(TestId, BTreeSet<Utf8PathBuf>)>> = Mutex::new(Vec::new());

    std::thread::scope(|s| {
        for _ in 0..num_workers {
            s.spawn(|| loop {
                let Some((_idx, dir)) = work.lock().unwrap().pop_front() else {
                    break;
                };
                let t0 = Instant::now();
                let outcome = extract_one(&dir, &llvm_profdata, &llvm_cov, project_root);
                let elapsed = t0.elapsed().as_secs_f64();
                let mut guard = progress.lock().unwrap();
                *guard += 1;
                let n = *guard;
                match outcome {
                    Ok(ExtractOutcome::Collected { test_id, mut files }) => {
                        files.extend(crate_roots.iter().cloned());
                        eprintln!(
                            "[{n}/{total}] {}::{}: {} files ({elapsed:.1}s)",
                            test_id.binary_id,
                            test_id.test_name,
                            files.len()
                        );
                        drop(guard);
                        mappings.lock().unwrap().push((test_id, files));
                    }
                    Ok(ExtractOutcome::Skipped { test_id, reason }) => {
                        eprintln!(
                            "[{n}/{total}] {}::{}: SKIP ({reason})",
                            test_id.binary_id, test_id.test_name
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "[{n}/{total}] {}: ERROR ({e:#})",
                            dir.display()
                        );
                    }
                }
            });
        }
    });

    let mappings = mappings.into_inner().unwrap();

    // Step 5: Store in DB.
    let total_elapsed = total_start.elapsed();
    let mapping_count: usize = mappings.iter().map(|(_, f)| f.len()).sum();

    let mut db = Db::open(project_root)?;
    if diff_base.is_some() {
        eprintln!("updating {} test mappings in database...", mappings.len());
        db.update_coverage(&env_fingerprint, &mappings)?;
    } else {
        eprintln!("storing {} test mappings in database...", mappings.len());
        db.store_coverage(&env_fingerprint, &mappings)?;
    }

    let evicted = db.gc(&env_fingerprint, FINGERPRINT_KEEP)?;
    if evicted > 0 {
        let kept = db.fingerprint_count()?;
        let s = if evicted == 1 { "" } else { "s" };
        eprintln!("evicted {evicted} stale fingerprint{s} (kept {kept} of {FINGERPRINT_KEEP})");
    }

    eprintln!(
        "done. {} tests, {} mappings stored in target/difftest/coverage.db ({:.1}s total)",
        mappings.len(),
        mapping_count,
        total_elapsed.as_secs_f64(),
    );
    Ok(nextest_exit)
}

/// Outcome of coverage extraction for a single per-test directory.
enum ExtractOutcome {
    Collected {
        test_id: TestId,
        files: BTreeSet<Utf8PathBuf>,
    },
    Skipped {
        test_id: TestId,
        reason: String,
    },
}

/// Merge profraws in `dir` and export coverage.
///
/// Reads the `meta` sidecar the shim wrote (test name + binary path +
/// binary_id) so we know exactly which binary to pass to `llvm-cov export`
/// and how to store the result in the DB.
fn extract_one(
    dir: &Path,
    llvm_profdata: &Path,
    llvm_cov: &Path,
    project_root: &Path,
) -> Result<ExtractOutcome> {
    let meta = std::fs::read_to_string(dir.join("meta"))
        .with_context(|| format!("reading sidecar {}/meta", dir.display()))?;
    let mut lines = meta.lines();
    let test_name = lines
        .next()
        .context("empty meta sidecar")?
        .to_string();
    let binary = lines
        .next()
        .context("meta sidecar missing binary path")?
        .to_string();
    let binary_id = lines
        .next()
        .context("meta sidecar missing binary_id")?
        .to_string();
    let test_id = TestId::new(binary_id, test_name);
    let binary = PathBuf::from(binary);

    let profraw_files = list_profraw_files(dir)?;
    if profraw_files.is_empty() {
        return Ok(ExtractOutcome::Skipped {
            test_id,
            reason: "no profraw generated".into(),
        });
    }

    let profdata_path = dir.join("coverage.profdata");
    let mut merge_cmd = Command::new(llvm_profdata);
    merge_cmd.arg("merge").arg("--sparse");
    for f in &profraw_files {
        merge_cmd.arg(f);
    }
    merge_cmd.arg("-o").arg(&profdata_path);
    let merge_output = merge_cmd
        .output()
        .context("failed to run llvm-profdata merge")?;
    if !merge_output.status.success() {
        return Ok(ExtractOutcome::Skipped {
            test_id,
            reason: format!(
                "llvm-profdata merge failed: {}",
                String::from_utf8_lossy(&merge_output.stderr).trim()
            ),
        });
    }

    let export_output = Command::new(llvm_cov)
        .arg("export")
        .arg("--format=text")
        .arg(format!("--instr-profile={}", profdata_path.display()))
        .arg(&binary)
        .output()
        .context("failed to run llvm-cov export")?;
    if !export_output.status.success() {
        return Ok(ExtractOutcome::Skipped {
            test_id,
            reason: format!(
                "llvm-cov export failed: {}",
                String::from_utf8_lossy(&export_output.stderr).trim()
            ),
        });
    }

    let json = String::from_utf8_lossy(&export_output.stdout);
    match coverage::extract_covered_files(&json, project_root) {
        Ok(files) => Ok(ExtractOutcome::Collected { test_id, files }),
        Err(e) => Ok(ExtractOutcome::Skipped {
            test_id,
            reason: format!("parse error: {e}"),
        }),
    }
}

/// Build a nextest `-E` filter expression matching exactly the given tests,
/// grouped by binary_id so one `binary_id(=X)` predicate covers each crate's
/// tests: `(binary_id(=X) & (test(=a) | test(=b))) | (binary_id(=Y) & test(=c))`.
///
/// `binary_id()` (not `binary()`) is the right predicate: the latter matches
/// the short binary name (e.g. `builds`) and so doesn't disambiguate
/// same-named binaries across workspace crates.
pub(crate) fn nextest_filter_expr(tests: &[TestId]) -> String {
    let mut by_binary: std::collections::BTreeMap<&str, Vec<&str>> =
        std::collections::BTreeMap::new();
    for t in tests {
        by_binary
            .entry(t.binary_id.as_str())
            .or_default()
            .push(t.test_name.as_str());
    }
    by_binary
        .into_iter()
        .map(|(binary_id, names)| {
            let inner = names
                .iter()
                .map(|n| format!("test(={n})"))
                .collect::<Vec<_>>()
                .join(" | ");
            format!("(binary_id(={binary_id}) & ({inner}))")
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Select which tests need re-collection for incremental mode.
fn select_tests_for_incremental(
    diff_base: &str,
    project_root: &Path,
    fingerprint: &str,
    all_tests: &[TestId],
) -> Result<Vec<TestId>> {
    let changed_files = git_changed_files(project_root, Some(diff_base))?;
    if changed_files.is_empty() {
        eprintln!("no changed files vs {diff_base}");
        return Ok(Vec::new());
    }

    eprintln!("{} changed files vs {diff_base}:", changed_files.len());
    for f in &changed_files {
        eprintln!("  {f}");
    }

    if !db_path(project_root).exists() {
        eprintln!("no existing DB — collecting all tests");
        return Ok(all_tests.to_vec());
    }

    let db = Db::open(project_root)?;
    let file_refs: Vec<&str> = changed_files.iter().map(|s| s.as_str()).collect();
    let affected_tests = db.tests_covering(fingerprint, &file_refs)?;
    let known_tests = db.all_tests(fingerprint)?;

    let new_tests: BTreeSet<&TestId> = all_tests
        .iter()
        .filter(|t| !known_tests.contains(*t))
        .collect();

    let tests_to_run: BTreeSet<TestId> = affected_tests
        .iter()
        .cloned()
        .chain(new_tests.iter().map(|t| (*t).clone()))
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

    Ok(tests_to_run.into_iter().collect())
}

/// Result of `cargo nextest list`: every testcase as a (binary_id, test_name)
/// pair, plus a binary_path → binary_id map for the runner shim.
struct Listing {
    tests: Vec<TestId>,
    binary_map: HashMap<String, String>,
}

/// Enumerate all tests via `cargo nextest list --message-format json`.
///
/// Builds test binaries with the given RUSTFLAGS (coverage instrumentation)
/// and parses the JSON on stdout. We don't set the runner env here —
/// nextest just asks binaries for their listings.
fn nextest_list(project_root: &Path, rustflags: &str, profraw_dir: &Path) -> Result<Listing> {
    let child = Command::new("cargo")
        .arg("nextest")
        .arg("list")
        .arg("--message-format")
        .arg("json")
        .env("RUSTFLAGS", rustflags)
        // Contain any stray build-script profraw within our workspace.
        .env("LLVM_PROFILE_FILE", profraw_dir.join("build-%p-%m.profraw"))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .current_dir(project_root)
        .spawn()
        .context("failed to spawn cargo nextest list")?;
    let output = child
        .wait_with_output()
        .context("failed to wait for cargo nextest list")?;
    if !output.status.success() {
        bail!("cargo nextest list failed (exit {:?})", output.status.code());
    }

    let stdout = std::str::from_utf8(&output.stdout)
        .context("cargo nextest list stdout was not valid UTF-8")?;
    let json: serde_json::Value =
        serde_json::from_str(stdout).context("failed to parse nextest list JSON")?;

    let mut tests = BTreeSet::new();
    let mut binary_map = HashMap::new();
    if let Some(suites) = json.get("rust-suites").and_then(|v| v.as_object()) {
        for suite in suites.values() {
            let binary_id = suite
                .get("binary-id")
                .and_then(|v| v.as_str())
                .context("nextest list entry missing binary-id")?
                .to_string();
            if let Some(binary_path) = suite.get("binary-path").and_then(|v| v.as_str()) {
                binary_map.insert(binary_path.to_string(), binary_id.clone());
            }
            let Some(cases) = suite.get("testcases").and_then(|v| v.as_object()) else {
                continue;
            };
            for case in cases.keys() {
                tests.insert(TestId::new(binary_id.clone(), case.clone()));
            }
        }
    }
    Ok(Listing {
        tests: tests.into_iter().collect(),
        binary_map,
    })
}

/// Write the binary_path → binary_id map as JSON for the runner shim.
fn write_binary_map(path: &Path, map: &HashMap<String, String>) -> Result<()> {
    let json = serde_json::to_string(map).context("serializing binary map")?;
    std::fs::write(path, json)
        .with_context(|| format!("writing binary map to {}", path.display()))?;
    Ok(())
}

/// Crate roots of all test-producing workspace targets, relative to the
/// project root. These are added as implicit deps for every test.
fn workspace_test_src_paths(project_root: &Path) -> Result<BTreeSet<Utf8PathBuf>> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version=1"])
        .current_dir(project_root)
        .output()
        .context("failed to run cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let meta: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("failed to parse cargo metadata")?;

    let root = project_root
        .canonicalize()
        .context("failed to canonicalize project root")?;

    let mut src_paths = BTreeSet::new();
    let Some(packages) = meta.get("packages").and_then(|v| v.as_array()) else {
        return Ok(src_paths);
    };
    for pkg in packages {
        let Some(targets) = pkg.get("targets").and_then(|v| v.as_array()) else {
            continue;
        };
        for target in targets {
            let is_test_target = target
                .get("test")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !is_test_target {
                continue;
            }
            let kinds: Vec<&str> = target
                .get("kind")
                .and_then(|v| v.as_array())
                .map(|ks| ks.iter().filter_map(|k| k.as_str()).collect())
                .unwrap_or_default();
            // Nextest builds and runs tests for lib/bin/test targets; skip
            // examples, benches, and custom-build so their src_paths don't
            // pollute the implicit-dep set.
            if !kinds.iter().any(|k| matches!(*k, "lib" | "bin" | "test")) {
                continue;
            }
            let Some(abs) = target.get("src_path").and_then(|v| v.as_str()) else {
                continue;
            };
            if let Ok(rel) = Path::new(abs).strip_prefix(&root) {
                if let Ok(u) = Utf8PathBuf::try_from(rel.to_path_buf()) {
                    src_paths.insert(u);
                }
            }
        }
    }
    Ok(src_paths)
}

/// List subdirectories of `profraw_dir` that look like per-test output
/// (contain a `meta` sidecar).
fn list_test_dirs(profraw_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(profraw_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() && path.join("meta").exists() {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// Remove all .profraw files in the given directory (non-recursive).
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

/// Ensure `cargo nextest` is available. Fails with an install hint otherwise.
pub(crate) fn require_nextest(project_root: &Path) -> Result<()> {
    let ok = Command::new("cargo")
        .arg("nextest")
        .arg("--version")
        .current_dir(project_root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        bail!(
            "cargo-difftest requires cargo-nextest. \
             Install it with `cargo install cargo-nextest --locked`."
        );
    }
    Ok(())
}

/// Find an LLVM tool by name.
fn find_llvm_tool(name: &str) -> Result<PathBuf> {
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

    #[cfg(target_os = "macos")]
    if let Ok(output) = Command::new("xcrun").arg("--find").arg(name).output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_expr_empty() {
        assert_eq!(nextest_filter_expr(&[]), "");
    }

    #[test]
    fn filter_expr_groups_by_binary() {
        // Tests sharing a name across binaries must both appear, each scoped
        // to its own binary — the whole point of the (binary, test) tuple.
        let tests = vec![
            TestId::new("mock-stub::builds", "builds"),
            TestId::new("wt-perf::builds", "builds"),
            TestId::new("worktrunk", "utils::tests::test_x"),
            TestId::new("worktrunk", "utils::tests::test_y"),
        ];
        let expr = nextest_filter_expr(&tests);
        // Grouping is by binary (BTreeMap order, so alphabetic by binary_id).
        assert_eq!(
            expr,
            "(binary_id(=mock-stub::builds) & (test(=builds))) | \
             (binary_id(=worktrunk) & (test(=utils::tests::test_x) | test(=utils::tests::test_y))) | \
             (binary_id(=wt-perf::builds) & (test(=builds)))"
        );
    }
}

