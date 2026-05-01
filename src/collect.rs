//! Coverage collection pipeline.
//!
//! Delegates build and test execution to `cargo nextest run`. We insert a
//! small runner shim (`cargo-affected runner-shim`) via
//! `CARGO_TARGET_<TRIPLE>_RUNNER` that points `LLVM_PROFILE_FILE` at a
//! per-test subdirectory before `exec`ing the real test binary. After nextest
//! finishes we walk those subdirectories, merge profraws, export coverage,
//! and write per-(test, file) line ranges to SQLite.
//!
//! Approach:
//! 1. Read crate roots from `cargo metadata`, scoped per nextest target
//!    (`binary_id`): each test's sentinel set covers its own crate root,
//!    its package's lib (for non-lib targets), and lib roots of workspace
//!    packages this target transitively depends on. Stored as sentinel-range
//!    rows `(line_start=1, line_end=i64::MAX)` so any hunk in one of those
//!    files overlaps and re-selects the test.
//! 2. `cargo nextest list --message-format json` to enumerate every binary
//!    (id + path) and every testcase. We capture the stable
//!    `(package, target, kind) → binary_id` map alongside the raw
//!    `binary_path → binary_id` map; the stable triple is invariant across
//!    rebuilds and is what disambiguates same-basename binaries (e.g. two
//!    `tests/builds.rs` across workspace members).
//! 3. Immediately before `cargo nextest run`, re-list to capture cargo's
//!    current artifact paths, join against the stable map, and write a fresh
//!    `binary_path → binary_id` JSON for the shim. This minimizes the window
//!    where cargo's hash output could drift out from under the shim.
//! 4. `cargo nextest run` with `-C instrument-coverage` in RUSTFLAGS and the
//!    runner env set. The preceding `list` step built the binaries, so `run`
//!    is a cache hit. nextest handles parallelism and progress.
//! 5. Capture HEAD sha (anchor for future diffs) before extraction so any
//!    git error surfaces before we spend time on coverage parsing.
//! 6. Post-run: for each subdir of profraw_base, read the `meta` sidecar,
//!    merge with `llvm-profdata`, export with `llvm-cov`, parse hit ranges.
//!    Parallelized across workers.
//! 7. Store mappings + collect_sha in the DB keyed by (binary_id, test_name).

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{bail, Context, Result};

use crate::coverage::{self, HitRange};
use crate::db::{affected_dir, Db, TestId, FINGERPRINT_KEEP};
use crate::fingerprint;
use crate::project::{find_project_root, git_head_sha, git_working_tree_dirty};
use crate::selection;

/// Entry point for `cargo affected collect`. Returns nextest's exit code.
///
/// `diff = true` runs an incremental collect: only tests affected by changes
/// since one of the stored `collect_sha`s (or new tests added to the project)
/// are rerun under instrumentation, and their rows are re-anchored at the
/// new HEAD. Other tests' rows stay put. Errors out if there's no prior
/// collect for the current environment, or if any stored sha is no longer
/// reachable from HEAD.
pub fn collect(
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
    let canonical_root = project_root
        .canonicalize()
        .context("failed to canonicalize project root")?;

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
    let runner_env_name = format!(
        "CARGO_TARGET_{}_RUNNER",
        target_triple.to_uppercase().replace('-', "_")
    );
    let runner_env_value = format!("{} runner-shim", self_path.display());

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

    // Profraw files live under target/affected/ alongside the DB. PID suffix
    // so concurrent `collect` invocations don't wipe each other's files.
    let profraw_dir = affected_dir(project_root).join(format!("profraw-{}", std::process::id()));
    if profraw_dir.exists() {
        std::fs::remove_dir_all(&profraw_dir).context("failed to clean profraw dir")?;
    }
    std::fs::create_dir_all(&profraw_dir).context("failed to create profraw dir")?;

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
    let crate_root_ranges_by_binary_id: BTreeMap<String, BTreeSet<HitRange>> =
        crate_root_sentinels
            .into_iter()
            .map(|(binary_id, paths)| {
                let ranges = paths
                    .into_iter()
                    .map(|p| HitRange {
                        file: p,
                        line_start: 1,
                        line_end: u32::MAX,
                    })
                    .collect();
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
    // RUSTFLAGS; the subsequent run is a cache hit. Fingerprint is taken now
    // so Cargo.lock is in its final state — status/run will compare against
    // that same state.
    eprintln!("listing tests with cargo nextest list...");
    let listing = nextest_list(project_root, Some(&rustflags), Some(&profraw_dir))?;
    eprintln!(
        "found {} tests across {} binaries",
        listing.tests.len(),
        listing.binaries.len()
    );
    let env_fingerprint = fingerprint::compute(&project)?;

    let stable_by_target: HashMap<(String, String, String), String> = listing
        .binaries
        .iter()
        .map(|b| (b.key(), b.binary_id.clone()))
        .collect();
    let binary_map_path = profraw_dir.join("binary_map.json");

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
        match plan_diff_collect(project_root, &db, &env_fingerprint, &listing)? {
            DiffOutcome::Plan(plan) => Some(plan),
            DiffOutcome::NothingToRecollect { listed } => {
                let pruned = db.prune_missing_tests(&env_fingerprint, &listed)?;
                if pruned > 0 {
                    let s = if pruned == 1 { "" } else { "s" };
                    eprintln!("pruned {pruned} test{s} no longer present in nextest list");
                }
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

    // Right before nextest run: re-list to capture cargo's current artifact
    // paths and join against the stable map, so the shim's path lookup
    // reflects whatever the run step is about to invoke. Cheap (cache hit
    // when nothing changed) and structurally robust against rebuilds that
    // would otherwise reshuffle hashes between list and run.
    if verbose {
        eprintln!("refreshing binary paths before nextest run...");
    }
    let pre_run = nextest_list(project_root, Some(&rustflags), Some(&profraw_dir))?;
    let runtime_map = build_runtime_binary_map(&pre_run.binaries, &stable_by_target)?;
    write_binary_map(&binary_map_path, &runtime_map)?;

    // Build (or cache-hit) and run, with the runner shim wired up so each
    // test writes to its own per-test profraw directory.
    eprintln!("running tests with cargo nextest run...");
    let mut cmd = Command::new("cargo");
    cmd.arg("nextest")
        .arg("run")
        // `--no-tests=warn` so a filter that matches nothing real (every
        // selected test absent from the listing — common in `--diff` after
        // renames/deletions) doesn't make nextest exit non-zero. We
        // discriminate the legitimate "all phantoms" case from a build
        // failure in `handle_no_profraw_dirs` using the diff plan's
        // live-vs-phantom split, not nextest's exit code.
        .arg("--no-tests=warn")
        .env("RUSTFLAGS", &rustflags)
        .env(&runner_env_name, &runner_env_value)
        .env("CARGO_AFFECTED_PROFRAW_BASE", &profraw_dir)
        .env("CARGO_AFFECTED_BINARY_MAP", &binary_map_path)
        // Catches build-script profraw before the runner shim kicks in for tests.
        .env("LLVM_PROFILE_FILE", profraw_dir.join("build-%p-%m.profraw"))
        .current_dir(project_root);
    if let Some(plan) = &diff_plan {
        for expr in plan.filter_exprs() {
            cmd.arg("-E").arg(expr);
        }
    }
    for a in nextest_args {
        cmd.arg(a);
    }
    let status = cmd
        .status()
        .context("failed to run cargo nextest run")?;
    let nextest_exit = status.code().unwrap_or(1);

    let test_dirs = list_test_dirs(&profraw_dir)?;
    let total = test_dirs.len();
    if total == 0 {
        let exit = handle_no_profraw_dirs(
            &mut db,
            &env_fingerprint,
            diff_plan.as_ref(),
            nextest_exit,
            &profraw_dir,
        )?;
        remove_profraw_dir(&profraw_dir)?;
        return Ok(exit);
    }

    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    eprintln!("extracting coverage for {total} tests with {num_workers} workers...");

    let progress: Mutex<usize> = Mutex::new(0);
    let work: Mutex<VecDeque<(usize, PathBuf)>> =
        Mutex::new(test_dirs.into_iter().enumerate().collect());
    let mappings: Mutex<Vec<(TestId, BTreeSet<HitRange>)>> = Mutex::new(Vec::new());
    let extract_errors: Mutex<Vec<String>> = Mutex::new(Vec::new());

    std::thread::scope(|s| {
        for _ in 0..num_workers {
            s.spawn(|| loop {
                let Some((_idx, dir)) = work.lock().unwrap().pop_front() else {
                    break;
                };
                let t0 = Instant::now();
                let outcome = extract_one(&dir, &llvm_profdata, &llvm_cov, &canonical_root);
                let elapsed = t0.elapsed().as_secs_f64();
                let mut guard = progress.lock().unwrap();
                *guard += 1;
                let n = *guard;
                match outcome {
                    Ok(ExtractOutcome::Collected { test_id, mut ranges }) => {
                        let Some(pkg_ranges) =
                            crate_root_ranges_by_binary_id.get(&test_id.binary_id)
                        else {
                            eprintln!(
                                "[{n}/{total}] {}::{}: ERROR (binary_id is not a known \
                                 workspace target)",
                                test_id.binary_id, test_id.test_name
                            );
                            drop(guard);
                            extract_errors.lock().unwrap().push(format!(
                                "binary_id {:?} is not a known workspace target",
                                test_id.binary_id
                            ));
                            continue;
                        };
                        ranges.extend(pkg_ranges.iter().cloned());
                        eprintln!(
                            "[{n}/{total}] {}::{}: {} ranges ({elapsed:.1}s)",
                            test_id.binary_id,
                            test_id.test_name,
                            ranges.len()
                        );
                        drop(guard);
                        mappings.lock().unwrap().push((test_id, ranges));
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

    let extract_errors = extract_errors.into_inner().unwrap();
    if !extract_errors.is_empty() {
        bail!("coverage extraction failed:\n  {}", extract_errors.join("\n  "));
    }

    let mappings = mappings.into_inner().unwrap();

    let total_elapsed = total_start.elapsed();
    let region_count: usize = mappings.iter().map(|(_, r)| r.len()).sum();

    if let Some(plan) = diff_plan {
        eprintln!(
            "updating coverage for {} tests ({region_count} ranges)...",
            mappings.len()
        );
        db.update_coverage_for_tests(&env_fingerprint, &collect_sha, &mappings)?;
        let pruned = db.prune_missing_tests(&env_fingerprint, &plan.listed)?;
        if pruned > 0 {
            let s = if pruned == 1 { "" } else { "s" };
            eprintln!("pruned {pruned} test{s} no longer present in nextest list");
        }
    } else {
        eprintln!(
            "storing coverage for {} tests ({region_count} ranges)...",
            mappings.len()
        );
        db.store_coverage(&env_fingerprint, &collect_sha, &mappings)?;
    }

    let evicted = db.gc(&env_fingerprint, FINGERPRINT_KEEP)?;
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
    remove_profraw_dir(&profraw_dir)?;
    Ok(nextest_exit)
}

/// Drop the per-collect profraw directory. The raw profile bundles inside
/// (~10 GB on a workspace this size) feed only the in-process `llvm-profdata`
/// → `llvm-cov` extraction; once the resulting hit-ranges land in
/// `coverage.db` they have no further use, and leaving them on disk wastes
/// space locally and overflows the GitHub Actions repo cache cap in CI.
/// Only called from the success paths — failed collects keep their bundles
/// for debugging.
fn remove_profraw_dir(profraw_dir: &Path) -> Result<()> {
    if !profraw_dir.exists() {
        return Ok(());
    }
    std::fs::remove_dir_all(profraw_dir)
        .with_context(|| format!("failed to remove profraw dir {}", profraw_dir.display()))
}

/// Plan for the rerun side of `collect --diff`: which tests to invoke
/// nextest with, and the full listing so a post-run prune can drop tests
/// that disappeared since the last collect.
struct DiffPlan {
    /// Tests selected for rerun — affected + new. Includes "phantoms":
    /// tests in the DB whose stored ranges overlap diff hunks but that no
    /// longer appear in the current nextest listing (renamed/deleted
    /// between collects). nextest filters those out at runtime; the
    /// `total == 0` recovery path uses the live/phantom split to tell
    /// "filter matched nothing real" apart from "runner shim failed".
    selected: BTreeSet<TestId>,
    /// Every test currently in `nextest list`. Drives prune (rows for
    /// renamed/deleted tests) and the live-vs-phantom check above.
    listed: BTreeSet<TestId>,
}

impl DiffPlan {
    /// Nextest `-E` filterset expressions matching every selected test,
    /// including phantoms — nextest will silently match nothing for those.
    /// Multiple chunks when the test set is large enough to risk `E2BIG`.
    fn filter_exprs(&self) -> Vec<String> {
        let v: Vec<TestId> = self.selected.iter().cloned().collect();
        nextest_filter_exprs(&v)
    }

    /// Selected tests that nextest can actually run (those present in the
    /// current listing). Used to distinguish the all-phantoms case from a
    /// runner-shim failure when extraction yields no profraw dirs.
    fn live_selected_count(&self) -> usize {
        self.selected.iter().filter(|t| self.listed.contains(t)).count()
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
    NothingToRecollect {
        listed: BTreeSet<TestId>,
    },
}

/// Run the diff-mode preflight + selection. Read-only against `db` — any
/// row replacement or prune happens at the `collect` call site so the DB
/// write surface stays in one place. Bails out on fingerprint mismatch
/// (no stored coverage) or every-sha-diverged (nothing reachable to query).
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
    if !reach.diverged.is_empty() {
        eprintln!(
            "{}",
            selection::diverged_shas_notice(
                &reach.diverged,
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

    let sel = selection::select_with_reach(project_root, db, env_fingerprint, listing, &reach)?;
    let selected = sel.selected();
    if selected.is_empty() {
        return Ok(DiffOutcome::NothingToRecollect { listed: sel.listed });
    }

    eprintln!("\n{}\n", selection::format_summary(&sel, "to recollect", false));
    Ok(DiffOutcome::Plan(DiffPlan {
        selected,
        listed: sel.listed,
    }))
}

/// Recovery for the case where nextest run produced no per-test profraw
/// directories. Discriminates three buckets so the user gets an actionable
/// message instead of a generic "no profraws" line:
///
/// - **Build or test failure** (nextest exited non-zero) — bail and let
///   nextest's own output explain. We pass `--no-tests=warn` to nextest so
///   "filter matched nothing" doesn't fall in here.
/// - **All-phantom selection** (`--diff` mode, every selected test absent
///   from the current listing) — expected when tests were renamed/deleted
///   between collects. Prune the stale rows and exit 0.
/// - **Runner shim didn't fire** (live tests should have run but no
///   per-test dirs appeared) — bail with a diagnostic pointing at the
///   shim. This is the case where nextest claims success but our
///   instrumentation never engaged.
fn handle_no_profraw_dirs(
    db: &mut Db,
    env_fingerprint: &str,
    diff_plan: Option<&DiffPlan>,
    nextest_exit: i32,
    profraw_dir: &Path,
) -> Result<i32> {
    if nextest_exit != 0 {
        bail!(
            "nextest exited with code {nextest_exit} and produced no per-test \
             profraw directories under {} — build or test failure (see nextest \
             output above)",
            profraw_dir.display(),
        );
    }

    if let Some(plan) = diff_plan {
        let live = plan.live_selected_count();
        if live > 0 {
            // nextest exited 0 with live tests in the filter, but no
            // per-test dirs appeared — those should each have one. The
            // runner shim must have failed to fire.
            bail!(
                "nextest exited 0 but {live} of {} selected tests should have \
                 been instrumented — no per-test profraw directories appeared \
                 under {}; the runner shim may have failed to fire",
                plan.selected.len(),
                profraw_dir.display(),
            );
        }
        eprintln!(
            "no tests rerun: every selected test is absent from the current \
             nextest listing (renamed or deleted between collects)"
        );
        let pruned = db.prune_missing_tests(env_fingerprint, &plan.listed)?;
        if pruned > 0 {
            let s = if pruned == 1 { "" } else { "s" };
            eprintln!("pruned {pruned} test{s} no longer present in nextest list");
        }
        return Ok(0);
    }

    // Full collect with no profraws and no diff plan: either the project
    // has no tests at all (nextest's `--no-tests=warn` lets us distinguish
    // this from a hard failure) or the shim never fired. We can't tell
    // apart from here without re-listing, so default to the more likely
    // explanation in this codepath — empty suite — and surface a hint.
    eprintln!(
        "no per-test profraw directories under {} — \
         project may have no tests, or the runner shim may have failed to fire",
        profraw_dir.display(),
    );
    Ok(0)
}

/// Outcome of coverage extraction for a single per-test directory.
enum ExtractOutcome {
    Collected {
        test_id: TestId,
        ranges: BTreeSet<HitRange>,
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
    canonical_root: &Path,
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

    // POSIX ERE — no negative lookahead, so we enumerate prefixes to drop.
    // The filter shrinks `files[]` (1234 → 113 on a worktrunk-scale test) but
    // doesn't shrink `functions[]`, which is the bulk of the JSON. We still
    // re-filter in coverage.rs via `strip_prefix(project_root)` — this regex
    // is the cheap pre-filter, project-root strip is the authoritative gate.
    let export_output = Command::new(llvm_cov)
        .arg("export")
        .arg("--format=text")
        .arg(format!("--instr-profile={}", profdata_path.display()))
        .arg("--ignore-filename-regex=/rustc/|/\\.cargo/|/target/")
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
    match coverage::extract_hit_ranges(&json, canonical_root) {
        Ok(ranges) => Ok(ExtractOutcome::Collected { test_id, ranges }),
        Err(e) => Ok(ExtractOutcome::Skipped {
            test_id,
            reason: format!("parse error: {e}"),
        }),
    }
}

/// Build nextest `-E` filterset expressions matching exactly the given tests,
/// grouped by `binary_id`. Each returned chunk is a complete expression of the
/// form `(binary_id(=X) & (test(=a) | test(=b)))`; nextest ORs multiple `-E`
/// flags together.
///
/// `binary_id()` (not `binary()`) is the right predicate: the latter matches
/// the short binary name (e.g. `builds`) and so doesn't disambiguate
/// same-named binaries across workspace crates.
///
/// The split exists because Linux's `MAX_ARG_STRLEN` caps a single argv string
/// at ~128 KB. A workspace with ~1900 selected tests overflows that as one
/// expression and `execve` fails with `E2BIG`. We emit one chunk per binary,
/// further splitting a binary's tests across chunks when its predicate alone
/// would exceed `FILTER_EXPR_MAX_BYTES`.
pub(crate) fn nextest_filter_exprs(tests: &[TestId]) -> Vec<String> {
    /// Cap per `-E` argument; conservative versus the 128 KB OS limit so the
    /// command line as a whole stays comfortably under it.
    const FILTER_EXPR_MAX_BYTES: usize = 60_000;
    let mut by_binary: std::collections::BTreeMap<&str, Vec<&str>> =
        std::collections::BTreeMap::new();
    for t in tests {
        by_binary
            .entry(t.binary_id.as_str())
            .or_default()
            .push(t.test_name.as_str());
    }
    let mut exprs: Vec<String> = Vec::new();
    for (binary_id, names) in by_binary {
        let wrapper = format!("(binary_id(={binary_id}) & ())");
        let inner_budget = FILTER_EXPR_MAX_BYTES.saturating_sub(wrapper.len());
        let mut inner = String::new();
        let flush = |inner: &mut String, exprs: &mut Vec<String>| {
            if !inner.is_empty() {
                exprs.push(format!("(binary_id(={binary_id}) & ({inner}))"));
                inner.clear();
            }
        };
        for n in &names {
            let item_len = "test(=)".len() + n.len();
            let sep_len = if inner.is_empty() { 0 } else { " | ".len() };
            if !inner.is_empty() && inner.len() + sep_len + item_len > inner_budget {
                flush(&mut inner, &mut exprs);
            }
            if !inner.is_empty() {
                inner.push_str(" | ");
            }
            inner.push_str("test(=");
            inner.push_str(n);
            inner.push(')');
        }
        flush(&mut inner, &mut exprs);
    }
    exprs
}

/// Result of `cargo nextest list`: every testcase as a (binary_id, test_name)
/// pair, plus per-binary metadata.
pub(crate) struct Listing {
    pub(crate) tests: Vec<TestId>,
    pub(crate) binaries: Vec<BinaryEntry>,
}

/// One binary in nextest's listing.
///
/// `(package, target, kind)` is the cargo-stable identifier — the same triple
/// produced by `cargo metadata` and emitted in `nextest list`'s JSON. It's
/// invariant across rebuilds, unlike `binary_path` (whose hash suffix can
/// change) and unlike the basename (which collides across workspace members
/// that happen to use the same target name).
///
/// `marker` is the project-root-relative source path the binary will embed
/// in panic strings/symbol names. The shim uses it to disambiguate binaries
/// whose basenames collide (e.g. two `tests/builds.rs` across workspace
/// members) when cargo's hash drifts the path lookup miss.
#[derive(Debug, Clone)]
pub(crate) struct BinaryEntry {
    pub(crate) binary_id: String,
    pub(crate) binary_path: String,
    pub(crate) package: String,
    pub(crate) target: String,
    pub(crate) kind: String,
    pub(crate) marker: Option<String>,
}

impl BinaryEntry {
    /// The stable join key — invariant across rebuilds.
    pub(crate) fn key(&self) -> (String, String, String) {
        (self.package.clone(), self.target.clone(), self.kind.clone())
    }
}

/// On-disk entry the shim consumes.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BinaryMapEntry {
    pub binary_id: String,
    /// Project-root-relative source path that the binary embeds (panic
    /// messages, symbol names) — survives `-C debuginfo=0`. None for lib/bin
    /// where the basename match is enough.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
}

/// Compute a marker the binary embeds: the project-root-relative path of its
/// source file. Used by the shim to disambiguate same-basename binaries when
/// cargo's hash-suffix drifts the path lookup miss.
///
/// Only emitted for test/bench/example targets where the convention is
/// stable. Lib/bin targets get None — they typically have unique basenames.
fn marker_for(suite: &serde_json::Value, project_root: &Path) -> Option<String> {
    let kind = suite.get("kind").and_then(|v| v.as_str())?;
    let cwd = suite.get("cwd").and_then(|v| v.as_str())?;
    let binary_name = suite.get("binary-name").and_then(|v| v.as_str())?;
    let dir = match kind {
        "test" => "tests",
        "bench" => "benches",
        "example" => "examples",
        _ => return None,
    };
    let rel = Path::new(cwd).strip_prefix(project_root).ok()?;
    let rel_str = rel.to_str()?;
    Some(if rel_str.is_empty() {
        format!("{dir}/{binary_name}.rs")
    } else {
        format!("{rel_str}/{dir}/{binary_name}.rs")
    })
}

/// Enumerate all tests via `cargo nextest list --message-format json`.
///
/// `rustflags_override` sets RUSTFLAGS in the child env (collect passes
/// `-C instrument-coverage`; run/status leave it `None` to inherit the user's
/// environment). `profraw_dir`, when set, redirects any build-script profraw
/// so stray files don't land in the project root — only collect needs this.
pub(crate) fn nextest_list(
    project_root: &Path,
    rustflags_override: Option<&str>,
    profraw_dir: Option<&Path>,
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
    if let Some(dir) = profraw_dir {
        cmd.env("LLVM_PROFILE_FILE", dir.join("build-%p-%m.profraw"));
    }
    let output = cmd
        .spawn()
        .context("failed to spawn cargo nextest list")?
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
                .context("nextest list entry missing binary-path")?
                .to_string();
            let package = suite
                .get("package-name")
                .and_then(|v| v.as_str())
                .context("nextest list entry missing package-name")?
                .to_string();
            // nextest emits the cargo target name as `binary-name` (lib/bin/test
            // target name, not the file basename — the basename includes cargo's
            // hash suffix).
            let target = suite
                .get("binary-name")
                .and_then(|v| v.as_str())
                .context("nextest list entry missing binary-name")?
                .to_string();
            let kind = suite
                .get("kind")
                .and_then(|v| v.as_str())
                .context("nextest list entry missing kind")?
                .to_string();
            let marker = marker_for(suite, project_root);
            binaries.push(BinaryEntry {
                binary_id: binary_id.clone(),
                binary_path,
                package,
                target,
                kind,
                marker,
            });
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
        binaries,
    })
}

/// Build the `binary_path → binary_id` map the runner shim consumes.
///
/// Joins the freshly-listed binaries (which carry cargo's CURRENT artifact
/// paths) against the stable `(package, target, kind) → binary_id` map
/// captured earlier. The triple is cargo-stable across rebuilds, so any
/// hash-suffix drift in cargo's output flows through transparently — fresh
/// path on the left, stable id on the right. A drifted binary_id would mean
/// nextest itself disagrees about the binary's identity between two list
/// invocations, which we treat as a hard error.
pub(crate) fn build_runtime_binary_map(
    fresh: &[BinaryEntry],
    stable_by_target: &HashMap<(String, String, String), String>,
) -> Result<HashMap<String, BinaryMapEntry>> {
    let mut out = HashMap::with_capacity(fresh.len());
    for entry in fresh {
        let key = entry.key();
        let stable_id = stable_by_target.get(&key).with_context(|| {
            format!(
                "binary {:?} (package={}, target={}, kind={}) appeared in pre-run \
                 listing but not in the initial listing",
                entry.binary_path, entry.package, entry.target, entry.kind,
            )
        })?;
        if stable_id != &entry.binary_id {
            bail!(
                "nextest reported inconsistent binary_ids for (package={}, \
                 target={}, kind={}): {:?} vs {:?}",
                entry.package,
                entry.target,
                entry.kind,
                stable_id,
                entry.binary_id,
            );
        }
        out.insert(
            entry.binary_path.clone(),
            BinaryMapEntry {
                binary_id: entry.binary_id.clone(),
                marker: entry.marker.clone(),
            },
        );
    }
    Ok(out)
}

/// Write the `binary_path → BinaryMapEntry` map as JSON for the runner shim.
fn write_binary_map(path: &Path, map: &HashMap<String, BinaryMapEntry>) -> Result<()> {
    let json = serde_json::to_string(map).context("serializing binary map")?;
    std::fs::write(path, json)
        .with_context(|| format!("writing binary map to {}", path.display()))?;
    Ok(())
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
            "cargo-affected requires cargo-nextest. \
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
    fn filter_exprs_empty() {
        assert!(nextest_filter_exprs(&[]).is_empty());
    }

    #[test]
    fn filter_exprs_one_per_binary() {
        let tests = vec![
            TestId::new("mock-stub::builds", "builds"),
            TestId::new("wt-perf::builds", "builds"),
            TestId::new("worktrunk", "utils::tests::test_x"),
            TestId::new("worktrunk", "utils::tests::test_y"),
        ];
        let exprs = nextest_filter_exprs(&tests);
        assert_eq!(
            exprs,
            vec![
                "(binary_id(=mock-stub::builds) & (test(=builds)))".to_string(),
                "(binary_id(=worktrunk) & (test(=utils::tests::test_x) | test(=utils::tests::test_y)))".to_string(),
                "(binary_id(=wt-perf::builds) & (test(=builds)))".to_string(),
            ]
        );
    }

    /// Regression: ~1900 tests in one binary used to produce a single 80+ KB
    /// `-E` argument and `execve` failed with `Argument list too long (E2BIG)`.
    /// Each chunk must stay under ~60 KB and round-trip the full set.
    #[test]
    fn filter_exprs_chunks_oversized_binary() {
        let names: Vec<String> = (0..2000).map(|i| format!("really_long_test_name_for_chunking_check_{i}")).collect();
        let tests: Vec<TestId> = names
            .iter()
            .map(|n| TestId::new("worktrunk", n))
            .collect();
        let exprs = nextest_filter_exprs(&tests);
        assert!(exprs.len() > 1, "expected chunking, got {} chunk(s)", exprs.len());
        for expr in &exprs {
            assert!(
                expr.len() < 65_000,
                "chunk too large: {} bytes",
                expr.len()
            );
            assert!(expr.starts_with("(binary_id(=worktrunk) & ("));
            assert!(expr.ends_with("))"));
        }
        // Every test name appears exactly once across all chunks.
        let combined = exprs.join(" | ");
        for n in &names {
            assert!(
                combined.contains(&format!("test(={n})")),
                "missing {n}"
            );
        }
    }

    fn entry(
        binary_id: &str,
        binary_path: &str,
        package: &str,
        target: &str,
        kind: &str,
    ) -> BinaryEntry {
        BinaryEntry {
            binary_id: binary_id.into(),
            binary_path: binary_path.into(),
            package: package.into(),
            target: target.into(),
            kind: kind.into(),
            marker: None,
        }
    }

    #[test]
    fn build_runtime_binary_map_joins_by_stable_triple() -> Result<()> {
        // Two crates ship `tests/builds.rs`. The basename collides; the
        // (package, target, kind) triple does not. After a rebuild, paths
        // shift but the triples stay put — the join must still resolve to
        // the original binary_ids.
        let stable: HashMap<(String, String, String), String> = [
            (
                ("mock-stub".into(), "builds".into(), "test".into()),
                "mock-stub::builds".into(),
            ),
            (
                ("wt-perf".into(), "builds".into(), "test".into()),
                "wt-perf::builds".into(),
            ),
        ]
        .into_iter()
        .collect();

        let fresh = vec![
            entry(
                "mock-stub::builds",
                "/t/target/debug/deps/builds-NEW1",
                "mock-stub",
                "builds",
                "test",
            ),
            entry(
                "wt-perf::builds",
                "/t/target/debug/deps/builds-NEW2",
                "wt-perf",
                "builds",
                "test",
            ),
        ];

        let map = build_runtime_binary_map(&fresh, &stable)?;
        assert_eq!(
            map.get("/t/target/debug/deps/builds-NEW1").map(|e| e.binary_id.as_str()),
            Some("mock-stub::builds")
        );
        assert_eq!(
            map.get("/t/target/debug/deps/builds-NEW2").map(|e| e.binary_id.as_str()),
            Some("wt-perf::builds")
        );
        Ok(())
    }

    #[test]
    fn build_runtime_binary_map_errors_on_inconsistent_id() {
        // A new binary appears in the pre-run listing whose triple is known
        // but whose binary_id disagrees with the initial listing — that's a
        // nextest-side inconsistency we'd rather surface than paper over.
        let stable: HashMap<(String, String, String), String> =
            [(("foo".into(), "builds".into(), "test".into()), "foo::builds".into())]
                .into_iter()
                .collect();
        let fresh = vec![entry(
            "foo::different",
            "/t/target/debug/deps/builds-X",
            "foo",
            "builds",
            "test",
        )];
        let err = build_runtime_binary_map(&fresh, &stable).unwrap_err();
        assert!(
            err.to_string().contains("inconsistent binary_ids"),
            "expected mismatch error, got: {err}"
        );
    }

    #[test]
    fn build_runtime_binary_map_errors_on_unknown_triple() {
        // A binary in the pre-run listing whose triple wasn't in the initial
        // listing — likely a workspace-membership change between calls. Hard
        // error rather than silently dropping it from the shim's map.
        let stable: HashMap<(String, String, String), String> = HashMap::new();
        let fresh = vec![entry(
            "foo::builds",
            "/t/target/debug/deps/builds-X",
            "foo",
            "builds",
            "test",
        )];
        let err = build_runtime_binary_map(&fresh, &stable).unwrap_err();
        assert!(
            err.to_string().contains("not in the initial listing"),
            "expected missing-triple error, got: {err}"
        );
    }
}
