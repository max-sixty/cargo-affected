//! Coverage collection pipeline.
//!
//! Delegates build and test execution to `cargo nextest run`. We insert a
//! small runner shim (`cargo-affected runner-shim`) via
//! `--config target.<triple>.runner=[…]` that points `LLVM_PROFILE_FILE` at a
//! per-test subdirectory, runs the real test binary, and turns its profile
//! into line ranges on the spot. Each binary's function map — the expensive,
//! test-invariant half of that translation — is exported once, before the run.
//! After nextest finishes we read the per-test results back and write
//! per-(test, file) line ranges to SQLite.
//!
//! We use the `--config` array form rather than the
//! `CARGO_TARGET_<TRIPLE>_RUNNER` env var because cargo only
//! whitespace-splits the env-var form — a path like
//! `C:\Users\Joe Smith\…\cargo-affected.exe` (Windows) or
//! `/Users/joe/Library/Application Support/…` (macOS) would be
//! mis-tokenised. The TOML array preserves the path as one argv slot.
//!
//! Approach:
//! 1. Read crate roots from `cargo metadata`, scoped per nextest target
//!    (`binary_id`): each test's sentinel set covers its own crate root,
//!    its package's lib (for non-lib targets), and lib roots of workspace
//!    packages this target transitively depends on. Stored as sentinel-range
//!    rows via [`HitRange::sentinel`] (line 1 through
//!    `CRATE_ROOT_SENTINEL_END`) so any hunk in one of those files
//!    overlaps and re-selects the test.
//! 2. `cargo nextest list --message-format json` to enumerate every binary
//!    and every testcase.
//! 3. `cargo nextest run` with `-C instrument-coverage` in RUSTFLAGS and the
//!    runner wired in via `--config`. The preceding `list` step built the
//!    binaries, so `run` is a cache hit. nextest handles parallelism and
//!    progress. Each test invocation gets its `binary_id` straight from
//!    `NEXTEST_BINARY_ID` — the runner shim doesn't need to map paths.
//! 4. Capture HEAD sha (anchor for future diffs) before extraction so any
//!    git error surfaces before we spend time on coverage parsing.
//! 5. Export each participating binary's coverage map **once**, with
//!    `llvm-cov export` against an empty profile, and leave it under
//!    `CARGO_AFFECTED_FUNCTION_MAPS_DIR`. This is the expensive half of extraction
//!    and it doesn't vary per test — see [`write_function_maps`].
//! 6. **Inside the runner shim**: each test's shim (`cargo-affected
//!    runner-shim`) spawns the test binary, waits for it, then merges its
//!    profraw to text with `llvm-profdata`, joins the functions it names
//!    against the binary's map, writes a small per-test result file under
//!    `CARGO_AFFECTED_RESULTS_DIR`, and **deletes the per-test profraw dir
//!    before exiting**. Extraction runs in the process that ran the test, so
//!    peak disk usage is bounded by nextest's own concurrency
//!    (O(test-threads × per-test bundle) instead of O(whole-suite)) with no
//!    external watcher or completion heuristic — the completion signal is the
//!    test process exiting. See [`crate::shim`].
//! 7. After nextest exits, read the per-test result files and store mappings
//!    + collect_sha in the DB keyed by (binary_id, test_name).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{bail, Context, Result};

use crate::coverage::{self, HitRange};
use crate::db::{affected_dir, Db, TestId, FINGERPRINT_KEEP};
use crate::fingerprint;
use crate::project::{
    canonicalize_no_verbatim, find_project_root, git_head_sha, git_working_tree_dirty,
};
use crate::selection;
use crate::shim::{self, TestOutcome, TestResult};

/// Entry point for `cargo affected collect`. Returns nextest's exit code.
///
/// `diff = true` runs an incremental collect: only tests affected by changes
/// since one of the stored `collect_sha`s (or new tests added to the project)
/// are rerun under instrumentation, and their rows are re-anchored at the
/// new HEAD. Other tests' rows stay put. Errors out if there's no prior
/// collect for the current environment, or if any stored sha is no longer
/// reachable from HEAD.
pub(crate) fn collect(
    diff: bool,
    verbose: bool,
    allow_dirty: bool,
    nextest_args: &[String],
) -> Result<i32> {
    let total_start = Instant::now();
    let project = find_project_root()?;
    let project_root = &project.workspace_root;
    if verbose {
        eprintln!("project root: {}", project_root.display());
    }
    let canonical_root = canonicalize_no_verbatim(project_root)?;

    // Refuse to collect on a dirty tree by default: ranges would be filed
    // under HEAD but reflect working-tree line numbers, knocking the DB out
    // of phase with every later `git diff <collect_sha>` query.
    if git_working_tree_dirty(project_root)? {
        if allow_dirty {
            eprintln!(
                "warning: collecting on a dirty working tree (--allow-dirty); \
                 stored ranges may not align with future `affected run` queries"
            );
        } else {
            bail!(
                "working tree has uncommitted changes; commit or stash them \
                 before `cargo affected collect`, or pass --allow-dirty for a \
                 throwaway run (selection will be unreliable)"
            );
        }
    }

    require_nextest(project_root)?;
    let self_path = std::env::current_exe().context("failed to resolve current executable")?;
    let target_triple = current_target();
    let runner_config = format_runner_config(&target_triple, &self_path);

    let llvm_profdata = find_llvm_tool("llvm-profdata")?;
    let llvm_cov = find_llvm_tool("llvm-cov")?;
    if verbose {
        eprintln!(
            "llvm-profdata: {}\nllvm-cov: {}",
            llvm_profdata.display(),
            llvm_cov.display()
        );
    }

    // Anchor for future `run`/`status` diffs. Captured up front so a missing
    // HEAD (e.g., empty repo) errors before we spend time on builds.
    let collect_sha = git_head_sha(project_root)?;
    eprintln!("collect sha: {collect_sha}");

    // Build artifacts live under target/affected/build/ rather than the
    // project's default target/. Without isolation, cargo's main build phase
    // compiles every workspace package — including helper binaries pulled in
    // by `default-members` — into target/debug/ with the
    // `-C instrument-coverage` we set below. Those instrumented binaries
    // then linger after `collect` exits, and any later non-coverage
    // `cargo test` that spawns them writes `default_*.profraw` files to its
    // CWD. Routing the build into target/affected/build/ keeps the
    // instrumented copies out of target/debug/, where downstream tooling
    // (cargo-dist, IDEs, plain `cargo run`) expects clean artifacts.
    //
    // The matching `cargo affected run` flow leaves --target-dir unset so
    // it reuses target/debug/ — the user's normal cache — since `run`
    // doesn't enable instrumentation.
    let build_dir = affected_dir(project_root).join("build");
    std::fs::create_dir_all(&build_dir).context("failed to create build dir")?;
    // Sweep stale build-script profraws from prior collects; they accumulate
    // every time a build.rs reruns under instrumentation and aren't useful
    // between collects (build scripts don't show up in the test coverage we
    // care about — they ran during compile, not under the runner shim).
    for entry in std::fs::read_dir(&build_dir).context("scanning build dir")? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|e| e == "profraw") {
            let _ = std::fs::remove_file(entry.path());
        }
    }

    // Profraw bundles, the per-binary function maps and the per-test result
    // files the shim writes all live under target/affected/ alongside the DB,
    // each PID-suffixed so concurrent `collect` invocations don't wipe each
    // other's staging.
    let pid = std::process::id();
    let profraw_dir = affected_dir(project_root).join(format!("{PROFRAW_DIR_PREFIX}{pid}"));
    let results_dir = affected_dir(project_root).join(format!("{RESULTS_DIR_PREFIX}{pid}"));
    let function_maps_dir =
        affected_dir(project_root).join(format!("{FUNCTION_MAPS_DIR_PREFIX}{pid}"));
    for dir in [&profraw_dir, &results_dir, &function_maps_dir] {
        if dir.exists() {
            std::fs::remove_dir_all(dir)
                .with_context(|| format!("failed to clean {}", dir.display()))?;
        }
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }

    // Per-target sentinel set keyed by nextest's `binary_id`. Each test's
    // sentinel ranges cover its own crate root, its package's lib (if it's
    // not the lib itself), and the libs of any workspace packages it
    // transitively depends on. See `crate_root_sentinels_by_binary_id` for
    // the reasoning.
    let crate_root_sentinels = project.crate_root_sentinels_by_binary_id()?;
    if verbose {
        for (binary_id, paths) in &crate_root_sentinels {
            eprintln!(
                "crate-root sentinels for {binary_id}: {}",
                paths
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    let crate_root_ranges_by_binary_id: BTreeMap<String, BTreeSet<HitRange>> = crate_root_sentinels
        .into_iter()
        .map(|(binary_id, paths)| {
            let ranges = paths.into_iter().map(HitRange::sentinel).collect();
            (binary_id, ranges)
        })
        .collect();

    let mut rustflags = std::env::var("RUSTFLAGS").unwrap_or_default();
    if !rustflags.is_empty() {
        rustflags.push(' ');
    }
    rustflags.push_str("-C instrument-coverage");

    // List first. Gives us the stable (package, target, kind) → binary_id map
    // we'll use to disambiguate same-basename binaries (e.g. two crates with
    // their own `tests/builds.rs`). The list step builds with the same
    // RUSTFLAGS and build flags as the run below, so the subsequent run is a
    // cache hit. Fingerprint is taken now so Cargo.lock is in its final
    // state — status/run will compare against that same state.
    eprintln!("listing tests with cargo nextest list...");
    let listing = nextest_list(
        project_root,
        Some(&rustflags),
        Some(&build_dir),
        &cargo_build_args(nextest_args),
        None,
    )?;
    eprintln!(
        "found {} tests across {} binaries",
        listing.tests.len(),
        listing.binaries.len()
    );
    let fingerprint = fingerprint::compute(&project)?;
    let env_fingerprint = &fingerprint.hex;

    // Open the DB once and thread it through. Eager open lets a busy/locked
    // database error out before we spend time on extraction.
    let mut db = Db::open(project_root)?;

    // Diff mode: validate prior collect, run selection, build the
    // nextest filter expression for the rerun set. Done after the listing
    // step so we use the same fingerprint for read and write — the list
    // step can update Cargo.lock, which would otherwise leave us reading
    // under one fingerprint and writing under another.
    //
    // The planner is read-only against the DB; any prune or row replacement
    // happens later in this function so the DB write surface stays in one
    // place.
    let diff_plan = if diff {
        match plan_diff_collect(project_root, &db, env_fingerprint, &listing)? {
            DiffOutcome::Plan(plan) => Some(plan),
            DiffOutcome::NothingToRecollect { listed } => {
                prune_and_report(&mut db, env_fingerprint, &listed)?;
                eprintln!(
                    "done. nothing to recollect — no affected tests and no new tests \
                     ({:.1}s total)",
                    total_start.elapsed().as_secs_f64(),
                );
                return Ok(0);
            }
        }
    } else {
        None
    };

    // Resolve the binary half of extraction before any test runs. Only the
    // binaries this run will actually touch need a map — a `--diff` collect
    // rerunning three tests shouldn't pay to export a whole workspace.
    let mapped = binaries_for_run(&listing, diff_plan.as_ref());
    let s = if mapped.len() == 1 { "y" } else { "ies" };
    eprintln!("exporting coverage maps for {} binary{s}...", mapped.len());
    write_function_maps(
        &function_maps_dir,
        &mapped,
        &llvm_profdata,
        &llvm_cov,
        &canonical_root,
    )?;

    // Build (or cache-hit) and run, with the runner shim wired up so each
    // test writes to its own per-test profraw directory.
    //
    // `--target-dir` routes build artifacts into target/affected/build/. The
    // build-script LLVM_PROFILE_FILE pattern lives at the target-dir root
    // so consumers can recover the target-dir via dirname(LLVM_PROFILE_FILE)
    // — same convention cargo-llvm-cov uses, which lets nextest setup-scripts
    // that build helper binaries match the runner's target-dir without
    // having to know cargo-affected's specific layout.
    let mut cmd = Command::new("cargo");
    cmd.arg("nextest")
        .arg("run")
        .arg("--config")
        .arg(&runner_config)
        .arg("--target-dir")
        .arg(&build_dir)
        // `--no-tests=warn` so a filter that matches nothing real (every
        // selected test absent from the listing — common in `--diff` after
        // renames/deletions) doesn't make nextest exit non-zero. We
        // discriminate the legitimate "all phantoms" case from a build
        // failure in `handle_no_results` using the diff plan's live-vs-phantom
        // split, not nextest's exit code.
        .arg("--no-tests=warn")
        .env("RUSTFLAGS", &rustflags)
        // The runner shim extracts each test's coverage in-process and writes
        // a result file; it needs the staging locations, llvm-profdata, and
        // the function maps exported above. Names are shared constants so the
        // setter here can't drift from the reader.
        .env(shim::ENV_PROFRAW_BASE, &profraw_dir)
        .env(shim::ENV_RESULTS_DIR, &results_dir)
        .env(shim::ENV_LLVM_PROFDATA, &llvm_profdata)
        .env(shim::ENV_FUNCTION_MAPS_DIR, &function_maps_dir)
        // Catches build-script profraw before the runner shim kicks in for tests.
        .env("LLVM_PROFILE_FILE", build_dir.join("build-%p-%m.profraw"))
        .current_dir(project_root);
    let filter_config = match &diff_plan {
        Some(plan) => {
            let config = write_nextest_config(project_root, &plan.filter_expr())?;
            cmd.arg("--config-file").arg(&config);
            Some(config)
        }
        None => None,
    };
    for a in nextest_args {
        cmd.arg(a);
    }
    eprintln!("running tests (each extracts its own coverage as it finishes)...");
    let status = cmd.status().context("failed to run cargo nextest run")?;
    let nextest_exit = status.code().unwrap_or(1);

    if let Some(config) = &filter_config {
        // Best-effort cleanup; a stale file in gitignored target/ is harmless.
        let _ = std::fs::remove_file(config);
    }

    // The shim already merged/exported/parsed each test's profraw and deleted
    // the bundle; here we just read back the small result files it left.
    let results = read_results(&results_dir)?;
    if results.is_empty() {
        let exit = handle_no_results(
            &mut db,
            env_fingerprint,
            diff_plan.as_ref(),
            nextest_exit,
            &results_dir,
        )?;
        remove_staging_dirs(&[&profraw_dir, &results_dir, &function_maps_dir])?;
        return Ok(exit);
    }

    // Fold each test's crate-root sentinels into its ranges. An unknown
    // binary_id means `cargo metadata` and the nextest listing diverged — a
    // real invariant break — so surface it rather than dropping coverage.
    let mut mappings: Vec<(TestId, BTreeSet<HitRange>)> = Vec::new();
    let mut unknown_binaries: BTreeSet<String> = BTreeSet::new();
    let mut skipped = 0usize;
    for result in results {
        match result.outcome {
            TestOutcome::Collected { mut ranges } => {
                let test_id = TestId::new(result.binary_id, result.test_name);
                let Some(pkg_ranges) = crate_root_ranges_by_binary_id.get(&test_id.binary_id)
                else {
                    unknown_binaries.insert(test_id.binary_id);
                    continue;
                };
                ranges.extend(pkg_ranges.iter().cloned());
                mappings.push((test_id, ranges));
            }
            TestOutcome::Skipped { reason } => {
                skipped += 1;
                eprintln!(
                    "  skipped {}::{}: {reason}",
                    result.binary_id, result.test_name
                );
            }
        }
    }
    if !unknown_binaries.is_empty() {
        bail!(
            "coverage results reference binary_ids absent from the workspace \
             (metadata/listing divergence): {}",
            unknown_binaries.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    if skipped > 0 {
        let s = if skipped == 1 { "" } else { "s" };
        eprintln!("{skipped} test{s} produced no coverage");
    }
    // Every test that ran produced no usable coverage. A real test always
    // covers at least its own crate-root sentinels, so an empty mapping here
    // means extraction failed across the board — instrumentation never engaged
    // (no profraw), or the llvm tools failed on every binary — rather than a
    // legitimately empty suite, which `handle_no_results` already handled
    // above. Bail instead of storing nothing, which for a full collect would
    // `DELETE` and wipe the prior coverage.
    if mappings.is_empty() {
        bail!(
            "nextest ran but coverage extraction yielded no ranges for any of \
             the {skipped} completed test{} (see the skip reasons above) — \
             refusing to overwrite stored coverage",
            if skipped == 1 { "" } else { "s" },
        );
    }

    let total_elapsed = total_start.elapsed();
    let region_count: usize = mappings.iter().map(|(_, r)| r.len()).sum();

    if let Some(plan) = diff_plan {
        eprintln!(
            "updating coverage for {} tests ({region_count} ranges)...",
            mappings.len()
        );
        db.update_coverage_for_tests(
            env_fingerprint,
            &fingerprint.components,
            &collect_sha,
            &mappings,
        )?;
        prune_and_report(&mut db, env_fingerprint, &plan.listed)?;
    } else {
        eprintln!(
            "storing coverage for {} tests ({region_count} ranges)...",
            mappings.len()
        );
        db.store_coverage(
            env_fingerprint,
            &fingerprint.components,
            &collect_sha,
            &mappings,
        )?;
    }

    let evicted = db.gc(env_fingerprint, FINGERPRINT_KEEP)?;
    if evicted > 0 {
        let kept = db.fingerprint_count()?;
        let s = if evicted == 1 { "" } else { "s" };
        eprintln!("evicted {evicted} stale fingerprint{s} (kept {kept} of {FINGERPRINT_KEEP})");
    }

    eprintln!(
        "done. {} tests, {} ranges stored in target/affected/coverage.db ({:.1}s total)",
        mappings.len(),
        region_count,
        total_elapsed.as_secs_f64(),
    );
    remove_staging_dirs(&[&profraw_dir, &results_dir, &function_maps_dir])?;
    Ok(nextest_exit)
}

/// Per-collect staging-dir name prefixes under `target/affected/`, each
/// suffixed with the collect's PID. Shared between the create site, the
/// success-path teardown, and the `clean` sweep so the three can't drift.
const PROFRAW_DIR_PREFIX: &str = "profraw-";
const RESULTS_DIR_PREFIX: &str = "results-";
const FUNCTION_MAPS_DIR_PREFIX: &str = "function-maps-";

/// All of them, for the sweeps that don't care which is which. Adding a
/// staging dir means adding it here, or `clean` silently stops reclaiming it.
const STAGING_DIR_PREFIXES: &[&str] = &[
    PROFRAW_DIR_PREFIX,
    RESULTS_DIR_PREFIX,
    FUNCTION_MAPS_DIR_PREFIX,
];

/// The binaries whose tests this run will execute, and so the ones needing a
/// coverage map.
///
/// Both branches key off testcases rather than binaries, which also excludes
/// the testless ones nextest lists anyway — a lib target holding only
/// constants compiles to a test binary with no instrumented code at all, and
/// `llvm-cov export` rejects a binary with no coverage map.
///
/// A full collect runs every listed test. A `--diff` collect runs only the
/// selected ones, whose selection can include "phantoms" (tests still in the
/// DB but gone from the listing) with no binary left to export.
///
/// A user filter in the post-`--` passthrough can narrow the run further, and
/// isn't accounted for here — nextest owns filterset evaluation. The cost of
/// over-answering is one export per unused binary, bounded by the binary count
/// rather than the test count.
fn binaries_for_run<'a>(
    listing: &'a Listing,
    diff_plan: Option<&DiffPlan>,
) -> Vec<&'a BinaryEntry> {
    let wanted: BTreeSet<&str> = match diff_plan {
        Some(plan) => plan
            .selected
            .iter()
            .filter(|test| plan.listed.contains(test))
            .map(|test| test.binary_id.as_str())
            .collect(),
        None => listing.tests.iter().map(|t| t.binary_id.as_str()).collect(),
    };
    listing
        .binaries
        .iter()
        .filter(|b| wanted.contains(b.binary_id.as_str()))
        .collect()
}

/// Export each binary's function map into `function_maps_dir`, where the
/// runner shim reads it back (see [`shim::map_path`] for the naming).
///
/// The export is run against a deliberately empty profile: `llvm-cov export`
/// needs one, but every count it produces is discarded here. What survives is
/// the part of its output that the profile can't change — which functions the
/// binary contains and which source lines each occupies. That is the expensive
/// part (34 MB of JSON for a 63,000-function test binary, most of it
/// dependencies) and the part that is identical for every test in the binary,
/// which is the whole reason it's hoisted out of the per-test path.
///
/// Each map is stamped with the binary it came from, so the shim can tell a
/// map apart from one built for an earlier build of the same file — cargo's
/// hash suffix can't, being metadata-derived. See [`coverage::BinaryStamp`].
///
/// Failures bail rather than degrade, including a map that comes back empty:
/// a binary running tests always has instrumented project code, so no
/// functions under the project root means the paths didn't line up
/// (`--remap-path-prefix`, a root that isn't a prefix of the sources) and
/// every test in that binary would silently collect nothing but crate-root
/// sentinels. Better to say so before running the suite than after.
fn write_function_maps(
    function_maps_dir: &Path,
    binaries: &[&BinaryEntry],
    llvm_profdata: &Path,
    llvm_cov: &Path,
    canonical_root: &Path,
) -> Result<()> {
    // An empty text profile merges into a valid profile containing no records,
    // which is exactly what "report every function as unexecuted" needs.
    let proftext = function_maps_dir.join("empty.proftext");
    let profdata = function_maps_dir.join("empty.profdata");
    std::fs::write(&proftext, "").context("failed to write the empty profile")?;
    let merge = Command::new(llvm_profdata)
        .arg("merge")
        .arg(&proftext)
        .arg("-o")
        .arg(&profdata)
        .output()
        .context("failed to run llvm-profdata merge")?;
    if !merge.status.success() {
        bail!(
            "llvm-profdata merge failed to build an empty profile: {}",
            String::from_utf8_lossy(&merge.stderr).trim(),
        );
    }

    for binary in binaries {
        // POSIX ERE — no negative lookahead, so the regex enumerates prefixes
        // to drop. It shrinks `files[]` but leaves `functions[]` (the bulk of
        // the JSON) intact; `coverage::build_function_map` filters
        // authoritatively via `strip_prefix(canonical_root)`.
        let export = Command::new(llvm_cov)
            .arg("export")
            .arg("--format=text")
            .arg(format!("--instr-profile={}", profdata.display()))
            .arg("--ignore-filename-regex=/rustc/|/\\.cargo/|/target/")
            .arg(&binary.binary_path)
            .output()
            .with_context(|| format!("failed to run llvm-cov export for {}", binary.binary_id))?;
        if !export.status.success() {
            bail!(
                "llvm-cov export failed for {}: {}",
                binary.binary_id,
                String::from_utf8_lossy(&export.stderr).trim(),
            );
        }
        let json = String::from_utf8_lossy(&export.stdout);
        let functions = coverage::build_function_map(&json, canonical_root)
            .with_context(|| format!("failed to read the coverage map of {}", binary.binary_id))?;
        if functions.is_empty() {
            bail!(
                "{} has no instrumented functions under {} — every test in it \
                 would collect nothing. Check that the project root is a \
                 prefix of the paths the compiler recorded (a \
                 --remap-path-prefix in RUSTFLAGS is the usual cause).",
                binary.binary_id,
                canonical_root.display(),
            );
        }
        let map = coverage::BinaryFunctionMap {
            binary: coverage::BinaryStamp::of(&binary.binary_path)?,
            functions,
        };
        let path = shim::map_path(function_maps_dir, &binary.binary_path).with_context(|| {
            format!(
                "{} has no file name to key its map by",
                binary.binary_path.display(),
            )
        })?;
        std::fs::write(&path, serde_json::to_vec(&map)?)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    // Leave the directory holding nothing but maps, so anything that later
    // walks it doesn't have to know about the scaffolding.
    for scratch in [&proftext, &profdata] {
        std::fs::remove_file(scratch)
            .with_context(|| format!("failed to remove {}", scratch.display()))?;
    }
    Ok(())
}

/// Drop this collect's staging directories. The shim has already removed each
/// per-test profraw bundle as it finished; what remains here is the
/// bookkeeping shell — empty per-binary parent dirs, the small result files we
/// just consumed, and the function maps. Only called from the success paths —
/// failed collects keep whatever's still on disk for debugging.
fn remove_staging_dirs(dirs: &[&Path]) -> Result<()> {
    for dir in dirs {
        if dir.exists() {
            std::fs::remove_dir_all(dir)
                .with_context(|| format!("failed to remove {}", dir.display()))?;
        }
    }
    Ok(())
}

/// Remove every leftover staging dir under `target/affected/`. A successful
/// `collect` removes its own, but a crashed or cancelled run — or a shim
/// SIGKILL'd mid-extraction before its own cleanup — can orphan bundles
/// (potentially the multi-GB profraw set this design exists to bound).
/// `clean` reclaims them. Returns the count removed.
///
/// Only invoked from `clean` (an explicit, destructive user command), never at
/// collect startup: the PID suffix lets concurrent collects coexist, and a
/// blind sweep would delete a sibling collect's live staging.
pub(crate) fn clean_staging_dirs(project_root: &Path) -> Result<usize> {
    let dir = affected_dir(project_root);
    if !dir.exists() {
        return Ok(0);
    }
    let mut removed = 0;
    for entry in std::fs::read_dir(&dir).context("scanning target/affected")? {
        let path = entry?.path();
        let is_staging = path.is_dir()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| STAGING_DIR_PREFIXES.iter().any(|p| n.starts_with(p)));
        if is_staging {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Read every per-test [`TestResult`] the shim wrote under `results_dir`. The
/// layout mirrors the profraw tree — `<binary_id>/<test_name>.json` — so two
/// levels of walking find them all. Sorted by `(binary_id, test_name)` so the
/// skip log and stored rows come out in a stable order.
///
/// An unreadable or unparsable result file is skipped with a warning, not a
/// hard error: the shim writes atomically (`.tmp` + rename), so a torn file
/// shouldn't occur, but if one does (e.g. the shim was SIGKILL'd at just the
/// wrong moment, or the disk filled), losing one test's coverage — it
/// re-selects next `--diff` — beats discarding every other test's.
fn read_results(results_dir: &Path) -> Result<Vec<TestResult>> {
    let mut results = Vec::new();
    if !results_dir.exists() {
        return Ok(results);
    }
    for binary_entry in std::fs::read_dir(results_dir).context("scanning results dir")? {
        let binary_path = binary_entry?.path();
        if !binary_path.is_dir() {
            continue;
        }
        for file_entry in std::fs::read_dir(&binary_path)? {
            let path = file_entry?.path();
            if path.extension().is_some_and(|e| e == "json") {
                match std::fs::read_to_string(&path)
                    .map_err(anyhow::Error::from)
                    .and_then(|raw| Ok(serde_json::from_str::<TestResult>(&raw)?))
                {
                    Ok(result) => results.push(result),
                    Err(e) => eprintln!(
                        "warning: ignoring unreadable coverage result {}: {e:#}",
                        path.display()
                    ),
                }
            }
        }
    }
    results.sort_by(|a, b| (&a.binary_id, &a.test_name).cmp(&(&b.binary_id, &b.test_name)));
    Ok(results)
}

/// Plan for the rerun side of `collect --diff`: which tests to invoke
/// nextest with, and the full listing so a post-run prune can drop tests
/// that disappeared since the last collect.
struct DiffPlan {
    /// Tests selected for rerun — affected + new. Includes "phantoms":
    /// tests in the DB whose stored ranges overlap diff hunks but that no
    /// longer appear in the current nextest listing (renamed/deleted
    /// between collects). nextest filters those out at runtime; the
    /// no-results recovery path uses the live/phantom split to tell
    /// "filter matched nothing real" apart from "runner shim failed".
    selected: BTreeSet<TestId>,
    /// Every test currently in `nextest list`. Drives prune (rows for
    /// renamed/deleted tests) and the live-vs-phantom check above.
    listed: BTreeSet<TestId>,
}

impl DiffPlan {
    /// Nextest filterset expression matching every selected test, including
    /// phantoms — nextest will silently match nothing for those.
    fn filter_expr(&self) -> String {
        let v: Vec<TestId> = self.selected.iter().cloned().collect();
        nextest_filter_expr(&v)
    }

    /// Selected tests that nextest can actually run (those present in the
    /// current listing). Used to distinguish the all-phantoms case from a
    /// runner-shim failure when extraction yields no results.
    fn live_selected_count(&self) -> usize {
        self.selected
            .iter()
            .filter(|t| self.listed.contains(t))
            .count()
    }
}

/// Result of the `collect --diff` preflight. The variant tells `collect`
/// whether to invoke nextest or short-circuit, and either way the listing
/// (carried as `listed`) drives the post-step prune.
enum DiffOutcome {
    /// Selection picked at least one test — invoke nextest with `Plan.filter_expr`,
    /// then write rows and prune.
    Plan(DiffPlan),
    /// Nothing to recollect — no affected tests, no new tests. Caller still
    /// runs prune so renamed/deleted tests' rows go away.
    NothingToRecollect { listed: BTreeSet<TestId> },
}

/// Run the diff-mode preflight + selection. Read-only against `db` — any
/// row replacement or prune happens at the `collect` call site so the DB
/// write surface stays in one place. Bails out on fingerprint mismatch
/// (no stored coverage) or every-sha-missing (nothing reachable to query).
fn plan_diff_collect(
    project_root: &Path,
    db: &Db,
    env_fingerprint: &str,
    listing: &Listing,
) -> Result<DiffOutcome> {
    let prior_shas = db.collect_shas(env_fingerprint)?;
    if prior_shas.is_empty() {
        bail!(
            "--diff requires a prior `cargo affected collect` for the \
             current environment (Cargo.lock / rustc / build flags); \
             no stored coverage matches"
        );
    }
    let reach = selection::check_shas_reachable(project_root, &prior_shas)?;
    if !reach.missing.is_empty() {
        eprintln!(
            "{}",
            selection::missing_shas_notice(
                &reach.missing,
                "will be rerun and re-anchored at the new HEAD",
            ),
        );
    }
    if reach.reachable.is_empty() {
        bail!(
            "no reachable collect_sha for the current environment (every \
             stored sha is rebased away or otherwise unreachable from HEAD); \
             run `cargo affected collect` to re-anchor"
        );
    }
    if reach.max_commits_ahead > 0 {
        eprintln!(
            "note: {} commit(s) since prior collect — \
             re-anchoring affected tests at the new HEAD",
            reach.max_commits_ahead,
        );
    }

    let sel = selection::select_with_reach(
        project_root,
        db,
        env_fingerprint,
        listing,
        &reach,
        selection::DiagnosticDetail::Summary,
    )?;
    let selected = sel.selected();
    if selected.is_empty() {
        return Ok(DiffOutcome::NothingToRecollect { listed: sel.listed });
    }

    eprintln!(
        "\n{}\n",
        selection::format_summary(&sel, "to recollect", false)
    );
    Ok(DiffOutcome::Plan(DiffPlan {
        selected,
        listed: sel.listed,
    }))
}

/// Recovery for the case where nextest run produced no per-test coverage
/// results. Discriminates three buckets so the user gets an actionable
/// message instead of a generic "no results" line:
///
/// - **Build or test failure** (nextest exited non-zero) — bail and let
///   nextest's own output explain. We pass `--no-tests=warn` to nextest so
///   "filter matched nothing" doesn't fall in here.
/// - **All-phantom selection** (`--diff` mode, every selected test absent
///   from the current listing) — expected when tests were renamed/deleted
///   between collects. Prune the stale rows and exit 0.
/// - **Runner shim didn't fire** (live tests should have run but no results
///   appeared) — bail with a diagnostic pointing at the shim. This is the
///   case where nextest claims success but our instrumentation never engaged.
fn handle_no_results(
    db: &mut Db,
    env_fingerprint: &str,
    diff_plan: Option<&DiffPlan>,
    nextest_exit: i32,
    results_dir: &Path,
) -> Result<i32> {
    if nextest_exit != 0 {
        bail!(
            "nextest exited with code {nextest_exit} and produced no coverage \
             results under {} — build or test failure (see nextest output \
             above)",
            results_dir.display(),
        );
    }

    if let Some(plan) = diff_plan {
        let live = plan.live_selected_count();
        if live > 0 {
            // nextest exited 0 with live tests in the filter, but none wrote a
            // result — each should have. The runner shim must have failed to
            // fire.
            bail!(
                "nextest exited 0 but {live} of {} selected tests should have \
                 been instrumented — no coverage results appeared under {}; \
                 the runner shim may have failed to fire",
                plan.selected.len(),
                results_dir.display(),
            );
        }
        eprintln!(
            "no tests rerun: every selected test is absent from the current \
             nextest listing (renamed or deleted between collects)"
        );
        prune_and_report(db, env_fingerprint, &plan.listed)?;
        return Ok(0);
    }

    // Full collect with no results and no diff plan: either the project has no
    // tests at all (nextest's `--no-tests=warn` lets us distinguish this from
    // a hard failure) or the shim never fired. We can't tell apart from here
    // without re-listing, so default to the more likely explanation in this
    // codepath — empty suite — and surface a hint.
    eprintln!(
        "no coverage results under {} — project may have no tests, or the \
         runner shim may have failed to fire",
        results_dir.display(),
    );
    Ok(0)
}

/// Build a single nextest filterset expression matching exactly the given
/// tests, grouped by `binary_id`. The result has the form
/// `(binary_id(=X) & (test(=a) | test(=b))) | (binary_id(=Y) & (test(=c)))`.
/// Empty input yields `none()` — a valid filterset that matches nothing.
///
/// `binary_id()` (not `binary()`) is the right predicate: the latter matches
/// the short binary name (e.g. `builds`) and so doesn't disambiguate
/// same-named binaries across workspace crates.
///
/// The expression can be arbitrarily long — it reaches nextest as a
/// `default-filter` in a config file (see [`write_nextest_config`]), never as
/// an inline command-line argument, so no OS argv-length limit applies.
pub(crate) fn nextest_filter_expr(tests: &[TestId]) -> String {
    if tests.is_empty() {
        return "none()".to_string();
    }
    let mut by_binary: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
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

/// Write the nextest config file that pins the run to `filter_expr`, and
/// return its absolute path for passing to nextest via `--config-file`.
///
/// The affected-test selection reaches nextest as a `default-filter` inside a
/// config file rather than as inline `-E` arguments. A large affected set
/// built an `-E` argument list megabytes long, which overflowed Windows'
/// ~32 KB `CreateProcess` command-line limit (`os error 206`). A config file
/// has no such limit: the command line stays a fixed `--config-file <path>`
/// no matter how many tests are selected.
///
/// `--config-file` replaces nextest's repo-config slot
/// (`<workspace>/.config/nextest.toml`), so the project's own config — if any
/// — is merged in: every key it sets is preserved and only
/// `[profile.default].default-filter` is touched, keeping the project's
/// profiles, setup-scripts, timeouts and JUnit settings intact. When the
/// project already sets `default-filter`, the selection is intersected with
/// it so the effective set matches the old inline-`-E` behavior (`-E` was
/// likewise intersected with the default filter).
pub(crate) fn write_nextest_config(project_root: &Path, filter_expr: &str) -> Result<PathBuf> {
    let project_config = project_root.join(".config").join("nextest.toml");
    let existing = match std::fs::read_to_string(&project_config) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", project_config.display()))
        }
    };
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .with_context(|| format!("failed to parse {}", project_config.display()))?;

    let filter = match doc
        .get("profile")
        .and_then(|p| p.get("default"))
        .and_then(|d| d.get("default-filter"))
        .and_then(|v| v.as_str())
    {
        Some(existing) => format!("({filter_expr}) & ({existing})"),
        None => filter_expr.to_string(),
    };
    doc["profile"]["default"]["default-filter"] = toml_edit::value(filter);

    let dir = affected_dir(project_root);
    std::fs::create_dir_all(&dir).context("failed to create target/affected dir")?;
    // PID-suffixed so two concurrent `cargo affected` runs can't overwrite
    // each other's selection between writing the file and nextest reading it
    // — the same reasoning as the `profraw-<pid>` staging dir. The caller
    // removes it once nextest exits.
    let path = dir.join(format!("nextest-config-{}.toml", std::process::id()));
    std::fs::write(&path, doc.to_string())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

/// Boolean cargo build flags accepted by both `cargo nextest list` and
/// `cargo nextest run` — no value token follows.
const BUILD_FLAGS_BARE: &[&str] = &[
    "--workspace",
    "--all",
    "--lib",
    "--bins",
    "--examples",
    "--tests",
    "--benches",
    "--all-targets",
    "--all-features",
    "--no-default-features",
    "--release",
    "-r",
    "--frozen",
    "--locked",
    "--offline",
    "--ignore-rust-version",
    "--future-incompat-report",
    "--unit-graph",
];

/// Long cargo build flags that consume a value — `--flag value` or the
/// joined `--flag=value`.
///
/// `--target-dir` is deliberately absent: it changes only where artifacts
/// land, not which tests exist, and `collect` already passes its own
/// `--target-dir` to `nextest_list` — forwarding a second one would make
/// `cargo nextest list` reject the duplicate.
const BUILD_FLAGS_VALUED: &[&str] = &[
    "--package",
    "--exclude",
    "--bin",
    "--example",
    "--test",
    "--bench",
    "--features",
    "--cargo-profile",
    "--target",
    "--manifest-path",
    "--build-jobs",
    "--config",
];

/// Short cargo build flags that consume a value — `-p mycrate` or the
/// joined `-pmycrate`.
const BUILD_FLAGS_SHORT_VALUED: &[&str] = &["-p", "-F", "-Z"];

/// Extract the cargo *build* flags from the post-`--` passthrough so the
/// `cargo nextest list` used for new-test detection builds the same test set
/// as the eventual `cargo nextest run`.
///
/// `list` and `run` share cargo's build options (`--features`, `-p`,
/// `--release`, …) but `run` adds runner/reporter options (`--retries`,
/// `--no-fail-fast`, `--no-tests`, …) that `list` rejects outright.
/// Forwarding the whole passthrough to `list` would break on any of those;
/// forwarding nothing lists a feature-less build while `run` builds with the
/// user's features, so "listed minus DB = new" compares two different test
/// sets. Hence an allowlist of the build flags — anything else (run-only
/// flags, test-name filters, positionals) is dropped: it either doesn't
/// affect which test binaries get built or `list` wouldn't accept it.
pub(crate) fn cargo_build_args(nextest_args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut iter = nextest_args.iter();
    while let Some(arg) = iter.next() {
        let name = arg.split('=').next().unwrap_or(arg);
        if BUILD_FLAGS_BARE.contains(&name) {
            out.push(arg.clone());
        } else if BUILD_FLAGS_VALUED.contains(&name) {
            out.push(arg.clone());
            // `--flag value` carries the value in the next token;
            // `--flag=value` carries it inline.
            if !arg.contains('=') {
                if let Some(value) = iter.next() {
                    out.push(value.clone());
                }
            }
        } else if BUILD_FLAGS_SHORT_VALUED.contains(&arg.as_str()) {
            out.push(arg.clone());
            if let Some(value) = iter.next() {
                out.push(value.clone());
            }
        } else if BUILD_FLAGS_SHORT_VALUED.iter().any(|s| arg.starts_with(*s)) {
            // Joined short form: `-pmycrate`, `-Ffeature`.
            out.push(arg.clone());
        }
    }
    out
}

/// Result of `cargo nextest list`: every testcase as a (binary_id, test_name)
/// pair, the subset that is ignored, plus per-binary metadata.
pub(crate) struct Listing {
    /// Every testcase nextest enumerated, ignored or not. The complete set —
    /// `collect --diff` prunes DB rows against it, so a merely-ignored test
    /// must stay in here or its rows would be dropped.
    pub(crate) tests: Vec<TestId>,
    /// Subset of `tests` that nextest reports as `#[ignore]`d on this
    /// platform (covers conditional `#[cfg_attr(.., ignore)]` too). These
    /// are skipped by `cargo nextest run`, so they never gain coverage;
    /// new-test detection must exclude them or they read as "new" forever.
    pub(crate) ignored: BTreeSet<TestId>,
    pub(crate) binaries: Vec<BinaryEntry>,
}

/// One binary in nextest's listing. `binary_path` is where its coverage map
/// comes from and what names the map file; the runner shim sources binary_id
/// directly from `NEXTEST_BINARY_ID` at test time.
#[derive(Debug, Clone)]
pub(crate) struct BinaryEntry {
    pub(crate) binary_id: String,
    pub(crate) binary_path: PathBuf,
}

/// Enumerate all tests via `cargo nextest list --message-format json`.
///
/// `rustflags_override` sets RUSTFLAGS in the child env (collect passes
/// `-C instrument-coverage`; run/status leave it `None` to inherit the user's
/// environment). `build_dir`, when set, routes build artifacts into that
/// directory (via `--target-dir`) and points LLVM_PROFILE_FILE at the same
/// directory so build-script profraws land alongside cargo's debug/ tree
/// rather than in the project root. Only collect passes this — run/status
/// reuse the user's default target/.
///
/// `build_args` are the cargo build flags (`--features`, `-p`, …) extracted
/// from the post-`--` passthrough by [`cargo_build_args`]. They must match
/// the build config of the subsequent `cargo nextest run`, or the listing
/// enumerates a different test set than the run builds and new-test
/// detection ("listed minus DB") becomes unsound.
///
/// `filter_expr`, when set, passes `-E <expr>` so the listing is restricted to
/// tests matching a nextest filterset — used to resolve `[workspace.metadata.affected]`
/// rules to concrete tests. Leave `None` for a full listing.
pub(crate) fn nextest_list(
    project_root: &Path,
    rustflags_override: Option<&str>,
    build_dir: Option<&Path>,
    build_args: &[String],
    filter_expr: Option<&str>,
) -> Result<Listing> {
    let mut cmd = Command::new("cargo");
    cmd.arg("nextest")
        .arg("list")
        .arg("--message-format")
        .arg("json")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .current_dir(project_root);
    if let Some(rf) = rustflags_override {
        cmd.env("RUSTFLAGS", rf);
    }
    if let Some(dir) = build_dir {
        cmd.arg("--target-dir").arg(dir);
        cmd.env("LLVM_PROFILE_FILE", dir.join("build-%p-%m.profraw"));
    }
    for a in build_args {
        cmd.arg(a);
    }
    if let Some(expr) = filter_expr {
        cmd.arg("-E").arg(expr);
    }
    let output = cmd
        .spawn()
        .context("failed to spawn cargo nextest list")?
        .wait_with_output()
        .context("failed to wait for cargo nextest list")?;
    if !output.status.success() {
        bail!(
            "cargo nextest list failed (exit {:?})",
            output.status.code()
        );
    }

    let stdout = std::str::from_utf8(&output.stdout)
        .context("cargo nextest list stdout was not valid UTF-8")?;
    let json: serde_json::Value =
        serde_json::from_str(stdout).context("failed to parse nextest list JSON")?;

    let mut tests = BTreeSet::new();
    let mut ignored = BTreeSet::new();
    let mut binaries = Vec::new();
    if let Some(suites) = json.get("rust-suites").and_then(|v| v.as_object()) {
        for suite in suites.values() {
            let binary_id = suite
                .get("binary-id")
                .and_then(|v| v.as_str())
                .context("nextest list entry missing binary-id")?
                .to_string();
            let binary_path = suite
                .get("binary-path")
                .and_then(|v| v.as_str())
                .context("nextest list entry missing binary-path")?;
            binaries.push(BinaryEntry {
                binary_id: binary_id.clone(),
                binary_path: PathBuf::from(binary_path),
            });
            let Some(cases) = suite.get("testcases").and_then(|v| v.as_object()) else {
                continue;
            };
            for (name, case) in cases {
                let test_id = TestId::new(binary_id.clone(), name.clone());
                let is_ignored = case
                    .get("ignored")
                    .and_then(|v| v.as_bool())
                    .context("nextest list testcase missing `ignored` flag")?;
                if is_ignored {
                    ignored.insert(test_id.clone());
                }
                tests.insert(test_id);
            }
        }
    }
    Ok(Listing {
        tests: tests.into_iter().collect(),
        ignored,
        binaries,
    })
}

/// Ensure `cargo nextest` is available and recent enough that it sets
/// `NEXTEST_BINARY_ID` per test invocation (the runner shim relies on it
/// to attribute coverage). Fails with an install hint otherwise.
pub(crate) fn require_nextest(project_root: &Path) -> Result<()> {
    let output = Command::new("cargo")
        .arg("nextest")
        .arg("--version")
        .current_dir(project_root)
        .stderr(std::process::Stdio::null())
        .output();
    let stdout = match output {
        Ok(o) if o.status.success() => o.stdout,
        _ => bail!(
            "cargo-affected requires cargo-nextest >= {MIN_NEXTEST_VERSION}. \
             Install it with `cargo install cargo-nextest --locked`."
        ),
    };
    // `cargo nextest --version` prints `cargo-nextest 0.9.132 (...)`. Pull
    // out the second whitespace-separated field on the first line.
    let line = std::str::from_utf8(&stdout)
        .unwrap_or_default()
        .lines()
        .next()
        .unwrap_or("");
    let version = line.split_whitespace().nth(1).unwrap_or("");
    if !nextest_version_at_least(version, MIN_NEXTEST_VERSION) {
        bail!(
            "cargo-affected requires cargo-nextest >= {MIN_NEXTEST_VERSION} \
             (NEXTEST_BINARY_ID env support); found {:?}. \
             Upgrade with `cargo install cargo-nextest --locked`.",
            version,
        );
    }
    Ok(())
}

/// First nextest release that sets `NEXTEST_BINARY_ID` (and `NEXTEST_TEST_NAME`).
const MIN_NEXTEST_VERSION: &str = "0.9.116";

/// Compare `actual` >= `required` using semver-ish dotted-number ordering.
/// Trailing pre-release/build metadata after `-` or `+` is ignored.
/// Conservative: any unparsable version is treated as too old.
fn nextest_version_at_least(actual: &str, required: &str) -> bool {
    fn parts(v: &str) -> Option<Vec<u64>> {
        v.split(['-', '+'])
            .next()?
            .split('.')
            .map(|p| p.parse().ok())
            .collect()
    }
    match (parts(actual), parts(required)) {
        (Some(a), Some(r)) => a >= r,
        _ => false,
    }
}

/// Drop rows for tests that have vanished from the nextest listing, and say
/// how many went.
///
/// Three paths reach this same point from different directions — a `--diff`
/// with nothing to recollect, a `--diff` after its run, and the no-results
/// handler — and they are all reporting one fact, so they report it one way.
fn prune_and_report(db: &mut Db, env_fingerprint: &str, listed: &BTreeSet<TestId>) -> Result<()> {
    let pruned = db.prune_missing_tests(env_fingerprint, listed)?;
    if pruned > 0 {
        let s = if pruned == 1 { "" } else { "s" };
        eprintln!("pruned {pruned} test{s} no longer present in nextest list");
    }
    Ok(())
}

/// Find an LLVM tool by name.
fn find_llvm_tool(name: &str) -> Result<PathBuf> {
    // `EXE_SUFFIX` is `.exe` on Windows and empty elsewhere — `llvm-tools`
    // ships as `llvm-cov.exe` / `llvm-profdata.exe` on `*-windows-msvc`, so
    // probing the bare name finds nothing without it.
    let exe_name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    if let Ok(output) = Command::new("rustc").arg("--print").arg("sysroot").output() {
        if output.status.success() {
            let sysroot = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let tool_path = PathBuf::from(&sysroot)
                .join("lib")
                .join("rustlib")
                .join(current_target())
                .join("bin")
                .join(&exe_name);
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

    // PATH lookup. `which` on unix, `where` on Windows — both print the
    // resolved path on stdout. `where` may print multiple matches one per
    // line; take the first.
    let which_cmd = if cfg!(windows) { "where" } else { "which" };
    if let Ok(output) = Command::new(which_cmd).arg(&exe_name).output() {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let path = stdout.lines().next().unwrap_or("").trim().to_string();
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

/// Build the value for `--config target.<triple>.runner=…` as a TOML
/// array literal. The array form is required because cargo only
/// whitespace-splits the legacy env-var form (`CARGO_TARGET_<TRIPLE>_RUNNER`),
/// which silently breaks any binary path containing a space — common on
/// Windows (`C:\Users\Joe Smith\…`) and macOS
/// (`~/Library/Application Support/…`). Inside the array each element is a
/// TOML basic string, so `\` and `"` need escaping; nothing else realistic
/// in a filesystem path does.
fn format_runner_config(target_triple: &str, self_path: &Path) -> String {
    let escaped = self_path
        .to_string_lossy()
        .replace('\\', r"\\")
        .replace('"', "\\\"");
    format!(r#"target.{target_triple}.runner=["{escaped}", "runner-shim"]"#)
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

    fn listing(binaries: &[(&str, &str)], tests: &[(&str, &str)]) -> Listing {
        Listing {
            tests: tests.iter().map(|(b, t)| TestId::new(*b, *t)).collect(),
            ignored: BTreeSet::new(),
            binaries: binaries
                .iter()
                .map(|(id, path)| BinaryEntry {
                    binary_id: id.to_string(),
                    binary_path: PathBuf::from(path),
                })
                .collect(),
        }
    }

    /// A full collect maps every binary that has a test to run — and only
    /// those. A lib target holding no tests compiles to a binary with no
    /// coverage map at all, which `llvm-cov export` rejects outright.
    #[test]
    fn binaries_for_run_skips_testless_binaries() {
        let listing = listing(
            &[
                ("sample", "deps/sample-1"),
                ("sample::golden", "deps/golden-2"),
            ],
            &[("sample::golden", "golden_matches")],
        );
        let mapped: Vec<&str> = binaries_for_run(&listing, None)
            .iter()
            .map(|b| b.binary_id.as_str())
            .collect();
        assert_eq!(mapped, ["sample::golden"]);
    }

    /// A `--diff` collect maps only the binaries its selection will reach —
    /// the point of doing this per run rather than per workspace. Phantoms
    /// (selected but no longer listed) contribute no binary.
    #[test]
    fn binaries_for_run_narrows_to_the_diff_selection() {
        let listing = listing(
            &[
                ("crate-a", "deps/crate_a-1"),
                ("crate-b", "deps/crate_b-2"),
                ("crate-c", "deps/crate_c-3"),
            ],
            &[
                ("crate-a", "test_one"),
                ("crate-b", "test_two"),
                ("crate-c", "test_three"),
            ],
        );
        let plan = DiffPlan {
            selected: [
                TestId::new("crate-a", "test_one"),
                // Phantom: in the DB, gone from the listing.
                TestId::new("crate-c", "test_renamed_away"),
            ]
            .into_iter()
            .collect(),
            listed: listing.tests.iter().cloned().collect(),
        };
        let mapped: Vec<&str> = binaries_for_run(&listing, Some(&plan))
            .iter()
            .map(|b| b.binary_id.as_str())
            .collect();
        assert_eq!(mapped, ["crate-a"]);
    }

    #[test]
    fn filter_expr_empty_matches_nothing() {
        assert_eq!(nextest_filter_expr(&[]), "none()");
    }

    #[test]
    fn filter_expr_groups_by_binary() {
        let tests = vec![
            TestId::new("mock-stub::builds", "builds"),
            TestId::new("wt-perf::builds", "builds"),
            TestId::new("worktrunk", "utils::tests::test_x"),
            TestId::new("worktrunk", "utils::tests::test_y"),
        ];
        assert_eq!(
            nextest_filter_expr(&tests),
            "(binary_id(=mock-stub::builds) & (test(=builds))) | \
             (binary_id(=worktrunk) & (test(=utils::tests::test_x) | test(=utils::tests::test_y))) | \
             (binary_id(=wt-perf::builds) & (test(=builds)))",
        );
    }

    #[test]
    fn cargo_build_args_keeps_build_flags_drops_run_only() {
        let args: Vec<String> = [
            "--features",
            "shell-integration-tests",
            "--no-fail-fast",
            "--retries",
            "2",
            "--release",
            "--no-tests=warn",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // `--features <value>` and `--release` survive; the run-only flags —
        // and `--retries`'s separate value token — are dropped.
        assert_eq!(
            cargo_build_args(&args),
            vec!["--features", "shell-integration-tests", "--release"],
        );
    }

    #[test]
    fn cargo_build_args_handles_joined_and_short_forms() {
        let args: Vec<String> = [
            "--features=a,b",
            "-p",
            "mycrate",
            "-r",
            "--max-fail=3",
            "some_test_filter",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // `--flag=value`, `-p <value>`, and the `-r` short flag are build
        // args; `--max-fail=3` is run-only and the bare positional filter is
        // neither — both dropped.
        assert_eq!(
            cargo_build_args(&args),
            vec!["--features=a,b", "-p", "mycrate", "-r"],
        );
    }

    #[test]
    fn cargo_build_args_empty() {
        assert!(cargo_build_args(&[]).is_empty());
    }

    /// Regression for the Windows command-line overflow: a large affected set
    /// produced an `-E` argument list that blew past Windows' ~32 KB
    /// `CreateProcess` limit (`os error 206`). The selection now travels in a
    /// config file, so the filterset can be arbitrarily large while the
    /// command line stays a fixed `--config-file <path>`.
    #[test]
    fn large_selection_travels_in_config_file_not_argv() {
        let names: Vec<String> = (0..3000)
            .map(|i| format!("really_long_test_name_for_overflow_check_{i}"))
            .collect();
        let tests: Vec<TestId> = names.iter().map(|n| TestId::new("worktrunk", n)).collect();
        let expr = nextest_filter_expr(&tests);
        // The filterset itself dwarfs Windows' 32 KB command-line limit...
        assert!(
            expr.len() > 32 * 1024,
            "expected a large filterset, got {} bytes",
            expr.len()
        );
        // ...but it reaches nextest through a config file. The only thing
        // that lands on the command line is `--config-file <path>`, whose
        // length is bounded by the path, not the test count.
        let dir = tempfile::tempdir().unwrap();
        let config = write_nextest_config(dir.path(), &expr).unwrap();
        assert!(config.starts_with(dir.path()));
        let written = std::fs::read_to_string(&config).unwrap();
        for n in &names {
            assert!(written.contains(&format!("test(={n})")), "missing {n}");
        }
    }

    /// `write_nextest_config` overrides only `default-filter`; every other
    /// setting in the project's own `.config/nextest.toml` is carried through
    /// so its profiles, setup-scripts and timeouts still apply. The fixture
    /// mirrors the shape of a real consumer's config — top-level
    /// `experimental`, setup scripts, and an array-of-tables script binding —
    /// since `--config-file` replacing the repo config slot would otherwise
    /// silently drop all of it.
    #[test]
    fn write_nextest_config_preserves_project_settings() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join(".config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("nextest.toml"),
            "experimental = [\"setup-scripts\"]\n\
             \n\
             [profile.default]\n\
             slow-timeout = \"60s\"\n\
             \n\
             [profile.ci]\n\
             retries = 2\n\
             \n\
             [scripts.setup.build-bins]\n\
             command = [\"bash\", \"-c\", \"true\"]\n\
             \n\
             [[profile.default.scripts]]\n\
             filter = \"binary(integration)\"\n\
             setup = \"build-bins\"\n",
        )
        .unwrap();
        let config = write_nextest_config(dir.path(), "binary_id(=x) & test(=y)").unwrap();
        let doc: toml_edit::DocumentMut =
            std::fs::read_to_string(&config).unwrap().parse().unwrap();
        // Our selection landed under [profile.default].
        assert_eq!(
            doc["profile"]["default"]["default-filter"]
                .as_str()
                .unwrap(),
            "binary_id(=x) & test(=y)",
        );
        // The project's own settings survived untouched.
        assert_eq!(doc["experimental"].as_array().unwrap().len(), 1);
        assert_eq!(
            doc["profile"]["default"]["slow-timeout"].as_str().unwrap(),
            "60s",
        );
        assert_eq!(doc["profile"]["ci"]["retries"].as_integer().unwrap(), 2);
        assert!(doc["scripts"]["setup"]["build-bins"]["command"].is_array());
        assert_eq!(
            doc["profile"]["default"]["scripts"]
                .as_array_of_tables()
                .unwrap()
                .len(),
            1,
        );
    }

    /// A project that already sets `default-filter` keeps it: the selection
    /// is intersected with it, matching the old inline-`-E` behavior (`-E`
    /// was intersected with the default filter).
    #[test]
    fn write_nextest_config_intersects_existing_default_filter() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join(".config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("nextest.toml"),
            "[profile.default]\ndefault-filter = \"not test(slow)\"\n",
        )
        .unwrap();
        let config = write_nextest_config(dir.path(), "test(=y)").unwrap();
        let doc: toml_edit::DocumentMut =
            std::fs::read_to_string(&config).unwrap().parse().unwrap();
        assert_eq!(
            doc["profile"]["default"]["default-filter"]
                .as_str()
                .unwrap(),
            "(test(=y)) & (not test(slow))",
        );
    }

    /// With no project config, a fresh one is generated carrying just the
    /// selection. The filename carries the process id so concurrent
    /// invocations write to distinct files.
    #[test]
    fn write_nextest_config_without_project_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = write_nextest_config(dir.path(), "test(=solo)").unwrap();
        assert!(config
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .contains(&std::process::id().to_string()));
        let doc: toml_edit::DocumentMut =
            std::fs::read_to_string(&config).unwrap().parse().unwrap();
        assert_eq!(
            doc["profile"]["default"]["default-filter"]
                .as_str()
                .unwrap(),
            "test(=solo)",
        );
    }

    #[test]
    fn runner_config_uses_toml_array_form() {
        let path = PathBuf::from("/usr/local/bin/cargo-affected");
        assert_eq!(
            format_runner_config("aarch64-apple-darwin", &path),
            r#"target.aarch64-apple-darwin.runner=["/usr/local/bin/cargo-affected", "runner-shim"]"#,
        );
    }

    /// Spaces in the binary path are why we use the array form — cargo's
    /// `CARGO_TARGET_<TRIPLE>_RUNNER` env var only whitespace-splits, so a
    /// path containing a space would be mis-tokenised.
    #[test]
    fn runner_config_preserves_spaces_in_path() {
        let path = PathBuf::from("/Users/Joe Smith/.cargo/bin/cargo-affected");
        assert_eq!(
            format_runner_config("aarch64-apple-darwin", &path),
            r#"target.aarch64-apple-darwin.runner=["/Users/Joe Smith/.cargo/bin/cargo-affected", "runner-shim"]"#,
        );
    }

    /// Windows paths bring backslashes and (in pathological cases)
    /// double-quotes; both need TOML basic-string escaping inside the array.
    #[test]
    fn runner_config_escapes_backslashes_and_quotes() {
        let path = PathBuf::from(r#"C:\Users\Joe "Q" Smith\cargo-affected.exe"#);
        assert_eq!(
            format_runner_config("x86_64-pc-windows-msvc", &path),
            r#"target.x86_64-pc-windows-msvc.runner=["C:\\Users\\Joe \"Q\" Smith\\cargo-affected.exe", "runner-shim"]"#,
        );
    }

    #[test]
    fn nextest_version_compares_dotted_numbers() {
        assert!(nextest_version_at_least("0.9.116", "0.9.116"));
        assert!(nextest_version_at_least("0.9.132", "0.9.116"));
        assert!(nextest_version_at_least("0.10.0", "0.9.116"));
        assert!(nextest_version_at_least("1.0.0", "0.9.116"));
        assert!(!nextest_version_at_least("0.9.115", "0.9.116"));
        assert!(!nextest_version_at_least("0.9.99", "0.9.116"));
        assert!(!nextest_version_at_least("0.8.999", "0.9.116"));
        // Pre-release / build metadata after `-`/`+` is ignored.
        assert!(nextest_version_at_least("0.9.132-dev", "0.9.116"));
        assert!(nextest_version_at_least("0.9.116+sha.abc", "0.9.116"));
        // Unparsable: conservative — treat as too old.
        assert!(!nextest_version_at_least("garbage", "0.9.116"));
        assert!(!nextest_version_at_least("", "0.9.116"));
    }

    /// A torn/garbage result file is skipped, not fatal: one bad file must not
    /// discard the coverage of every other test in the run. `.tmp` siblings
    /// (an interrupted atomic write) are ignored entirely.
    #[test]
    fn read_results_skips_unreadable_files() {
        let tmp = tempfile::tempdir().unwrap();
        let results = tmp.path();
        let dir = results.join("crate-a");
        std::fs::create_dir_all(&dir).unwrap();

        let good = TestResult {
            binary_id: "crate-a".into(),
            test_name: "t_ok".into(),
            outcome: TestOutcome::Skipped { reason: "x".into() },
        };
        std::fs::write(dir.join("t_ok.json"), serde_json::to_vec(&good).unwrap()).unwrap();
        std::fs::write(dir.join("t_torn.json"), b"{ not valid json").unwrap();
        std::fs::write(dir.join("t_partial.json.tmp"), b"{").unwrap();

        let got = read_results(results).unwrap();
        assert_eq!(got.len(), 1, "only the parseable result survives");
        assert_eq!(got[0].test_name, "t_ok");
    }

    /// `clean` removes every leftover staging dir but leaves everything else
    /// under target/affected/ (the DB, the build dir) alone.
    #[test]
    fn clean_staging_dirs_removes_only_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let affected = root.join("target").join("affected");
        std::fs::create_dir_all(affected.join("profraw-123")).unwrap();
        std::fs::create_dir_all(affected.join("results-456")).unwrap();
        std::fs::create_dir_all(affected.join("function-maps-789")).unwrap();
        std::fs::create_dir_all(affected.join("build")).unwrap();
        std::fs::write(affected.join("coverage.db"), b"db").unwrap();

        let removed = clean_staging_dirs(root).unwrap();
        assert_eq!(removed, 3);
        assert!(!affected.join("profraw-123").exists());
        assert!(!affected.join("results-456").exists());
        assert!(!affected.join("function-maps-789").exists());
        assert!(affected.join("build").exists(), "build dir preserved");
        assert!(affected.join("coverage.db").exists(), "DB preserved");
    }
}
