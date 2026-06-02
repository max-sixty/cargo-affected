//! Coverage collection pipeline.
//!
//! Delegates build and test execution to `cargo nextest run`. We insert a
//! small runner shim (`cargo-affected runner-shim`) via
//! `--config target.<triple>.runner=[…]` that points `LLVM_PROFILE_FILE` at
//! a per-test subdirectory before `exec`ing the real test binary. After
//! nextest finishes we walk those subdirectories, merge profraws, export
//! coverage, and write per-(test, file) line ranges to SQLite.
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
//! 5. **In parallel** with nextest: a watcher thread polls the per-test
//!    profraw dirs and dispatches each finished test's bundle to a worker
//!    pool that merges with `llvm-profdata`, exports with `llvm-cov`,
//!    parses hit ranges, and **deletes the per-test dir as soon as the
//!    bundle has been consumed**. Peak disk usage becomes
//!    O(num_workers × per-test bundle size) instead of O(whole-suite size).
//!    See [`WatchState`] for the size-stability heuristic that detects
//!    "this test's process has exited" without modifying the shim.
//! 6. Store mappings + collect_sha in the DB keyed by (binary_id, test_name).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::coverage::{self, HitRange};
use crate::db::{affected_dir, Db, TestId, FINGERPRINT_KEEP};
use crate::fingerprint;
use crate::project::{
    canonicalize_no_verbatim, find_project_root, git_head_sha, git_working_tree_dirty,
};
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
                let pruned = db.prune_missing_tests(env_fingerprint, &listed)?;
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

    // Build (or cache-hit) and run, with the runner shim wired up so each
    // test writes to its own per-test profraw directory.
    //
    // `--target-dir` routes build artifacts into target/affected/build/. The
    // build-script LLVM_PROFILE_FILE pattern lives at the target-dir root
    // so consumers can recover the target-dir via dirname(LLVM_PROFILE_FILE)
    // — same convention cargo-llvm-cov uses, which lets nextest setup-scripts
    // that build helper binaries match the runner's target-dir without
    // having to know cargo-affected's specific layout.
    eprintln!("running tests with cargo nextest run...");
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
        // failure in `handle_no_profraw_dirs` using the diff plan's
        // live-vs-phantom split, not nextest's exit code.
        .arg("--no-tests=warn")
        .env("RUSTFLAGS", &rustflags)
        .env("CARGO_AFFECTED_PROFRAW_BASE", &profraw_dir)
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
    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    // Approximate denominator for the per-test progress line. Ignored tests
    // don't fire the runner shim, so they never show up as profraw dirs;
    // subtract them so `[n/total]` doesn't undercount by them. With --diff,
    // restrict to the live (non-phantom) subset of the selection.
    let intended_total = match &diff_plan {
        Some(plan) => plan.live_selected_count(),
        None => listing.tests.len().saturating_sub(listing.ignored.len()),
    };
    eprintln!(
        "extracting coverage with {num_workers} workers (streaming as tests finish)..."
    );

    let work = WorkQueue::default();
    let progress: Mutex<usize> = Mutex::new(0);
    let mappings: Mutex<Vec<(TestId, BTreeSet<HitRange>)>> = Mutex::new(Vec::new());
    let extract_errors: Mutex<Vec<String>> = Mutex::new(Vec::new());

    let mut child = cmd.spawn().context("failed to spawn cargo nextest run")?;
    let nextest_running = AtomicBool::new(true);

    let nextest_exit: i32 = std::thread::scope(|s| -> Result<i32> {
        // Watcher: polls the profraw dir and dispatches per-test bundles to
        // workers as soon as they look complete. Lives on its own thread so
        // extraction overlaps nextest's run instead of waiting until it's
        // over.
        s.spawn(|| {
            let mut state = WatchState::default();
            loop {
                let nextest_done = !nextest_running.load(Ordering::Acquire);
                match state.poll(&profraw_dir, nextest_done) {
                    Ok(ready) => {
                        for dir in ready {
                            work.push(dir);
                        }
                    }
                    Err(e) => {
                        extract_errors
                            .lock()
                            .unwrap()
                            .push(format!("profraw watcher: {e:#}"));
                    }
                }
                if nextest_done {
                    // Final pass already happened above (the `nextest_done`
                    // arg to `poll` disables the size-stability hold-back).
                    // Nothing more will appear on disk.
                    break;
                }
                std::thread::sleep(WATCH_POLL_INTERVAL);
            }
            work.mark_done();
        });

        // Worker pool: drains `work`, extracts, and deletes each per-test
        // dir as soon as its bundle has been consumed. That delete is the
        // whole point of the streaming pipeline — peak retention drops
        // from O(whole suite) to O(num_workers × per-test).
        for _ in 0..num_workers {
            s.spawn(|| {
                while let Some(dir) = work.pop_blocking() {
                    let t0 = Instant::now();
                    let outcome =
                        extract_one(&dir, &llvm_profdata, &llvm_cov, &canonical_root);
                    let elapsed = t0.elapsed().as_secs_f64();
                    let mut guard = progress.lock().unwrap();
                    *guard += 1;
                    let n = *guard;
                    drop(guard);
                    match outcome {
                        Ok(ExtractOutcome::Collected {
                            test_id,
                            mut ranges,
                        }) => {
                            let Some(pkg_ranges) =
                                crate_root_ranges_by_binary_id.get(&test_id.binary_id)
                            else {
                                eprintln!(
                                    "[{n}/{intended_total}] {}::{}: ERROR \
                                     (binary_id is not a known workspace target)",
                                    test_id.binary_id, test_id.test_name
                                );
                                extract_errors.lock().unwrap().push(format!(
                                    "binary_id {:?} is not a known workspace target",
                                    test_id.binary_id
                                ));
                                drop_per_test_dir(&dir);
                                continue;
                            };
                            ranges.extend(pkg_ranges.iter().cloned());
                            eprintln!(
                                "[{n}/{intended_total}] {}::{}: {} ranges ({elapsed:.1}s)",
                                test_id.binary_id,
                                test_id.test_name,
                                ranges.len()
                            );
                            mappings.lock().unwrap().push((test_id, ranges));
                        }
                        Ok(ExtractOutcome::Skipped { test_id, reason }) => {
                            eprintln!(
                                "[{n}/{intended_total}] {}::{}: SKIP ({reason})",
                                test_id.binary_id, test_id.test_name
                            );
                        }
                        Err(e) => {
                            eprintln!(
                                "[{n}/{intended_total}] {}: ERROR ({e:#})",
                                dir.display()
                            );
                        }
                    }
                    // Free the bundle whatever the outcome — the streaming
                    // pipeline exists specifically to keep peak retention
                    // bounded, and the per-test dir is no longer needed
                    // once we've logged either way.
                    drop_per_test_dir(&dir);
                }
            });
        }

        // Main scope thread: wait for nextest, then signal the watcher to
        // drain. The scope's implicit join waits for the watcher and
        // workers to finish processing whatever's still in flight.
        let status = child.wait().context("failed to wait for nextest")?;
        nextest_running.store(false, Ordering::Release);
        Ok(status.code().unwrap_or(1))
    })?;

    if let Some(config) = &filter_config {
        // Best-effort cleanup; a stale file in gitignored target/ is harmless.
        let _ = std::fs::remove_file(config);
    }

    let extract_errors = extract_errors.into_inner().unwrap();
    if !extract_errors.is_empty() {
        bail!(
            "coverage extraction failed:\n  {}",
            extract_errors.join("\n  ")
        );
    }

    let processed = *progress.lock().unwrap();
    if processed == 0 {
        let exit = handle_no_profraw_dirs(
            &mut db,
            env_fingerprint,
            diff_plan.as_ref(),
            nextest_exit,
            &profraw_dir,
        )?;
        remove_profraw_dir(&profraw_dir)?;
        return Ok(exit);
    }

    let mappings = mappings.into_inner().unwrap();

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
        let pruned = db.prune_missing_tests(env_fingerprint, &plan.listed)?;
        if pruned > 0 {
            let s = if pruned == 1 { "" } else { "s" };
            eprintln!("pruned {pruned} test{s} no longer present in nextest list");
        }
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
    remove_profraw_dir(&profraw_dir)?;
    Ok(nextest_exit)
}

/// Drop the per-collect profraw directory. By this point the streaming
/// extraction in `collect` has already removed each per-test bundle as the
/// worker pool finished with it (see [`drop_per_test_dir`]); what remains
/// here is the bookkeeping shell — empty per-binary parent dirs, the
/// occasional leftover from an error path. Only called from the success
/// paths — failed collects keep whatever's still on disk for debugging.
fn remove_profraw_dir(profraw_dir: &Path) -> Result<()> {
    if !profraw_dir.exists() {
        return Ok(());
    }
    std::fs::remove_dir_all(profraw_dir)
        .with_context(|| format!("failed to remove profraw dir {}", profraw_dir.display()))
}

/// Best-effort delete of one per-test bundle once its coverage has been
/// extracted. Errors are intentionally swallowed: a leftover here at worst
/// costs a bit of disk that `remove_profraw_dir` will sweep at end-of-run
/// anyway, and bubbling the error up would prevent the worker from
/// continuing.
fn drop_per_test_dir(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// How often the watcher revisits the profraw tree. Short enough that even
/// fast tests get dispatched within ~200 ms of the LLVM runtime closing
/// their profraw, long enough to be invisible against test runtime.
const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Tracks which per-test directories the watcher has already handed off,
/// and the prior total profraw size for dirs that may still be writing.
///
/// The stability heuristic: a per-test bundle is "complete" when at least
/// one `.profraw` file is present AND the sum of profraw sizes is non-zero
/// and unchanged across two consecutive polls. LLVM's profile runtime
/// writes each `.profraw` in a single `fopen`/`write`/`fclose` at process
/// exit, so size stability is a reliable proxy for "the test process has
/// exited and the runtime flushed its buffer." Polling at
/// [`WATCH_POLL_INTERVAL`] caps the per-test detection latency at ~2 ×
/// that interval.
///
/// After nextest has exited (the `final_pass` flag on [`Self::poll`]) every
/// remaining dir is dispatched unconditionally — at that point all test
/// processes are reaped and no more writes can land.
#[derive(Default)]
struct WatchState {
    /// Per-test dirs already pushed onto the work queue. Sticky so we never
    /// dispatch the same bundle twice.
    dispatched: BTreeSet<PathBuf>,
    /// Last total profraw size observed for dirs that have at least one
    /// `.profraw` but haven't yet seen two consecutive equal-size polls.
    pending_size: BTreeMap<PathBuf, u64>,
}

impl WatchState {
    fn poll(&mut self, profraw_dir: &Path, final_pass: bool) -> Result<Vec<PathBuf>> {
        if !profraw_dir.exists() {
            return Ok(Vec::new());
        }
        let dirs = list_test_dirs(profraw_dir)?;
        let mut ready = Vec::new();
        for dir in dirs {
            if self.dispatched.contains(&dir) {
                continue;
            }
            let profraws = list_profraw_files(&dir)?;
            let total: u64 = profraws
                .iter()
                .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
                .sum();

            if final_pass {
                // nextest is gone; whatever's on disk is final. Dispatch
                // even if there's no profraw — `extract_one` will record
                // it as a Skipped("no profraw generated") so the user
                // sees it in the log instead of it silently vanishing.
                self.dispatched.insert(dir.clone());
                self.pending_size.remove(&dir);
                ready.push(dir);
                continue;
            }

            if profraws.is_empty() || total == 0 {
                // Test still running, or runtime hasn't flushed yet.
                continue;
            }
            match self.pending_size.get(&dir).copied() {
                Some(prev) if prev == total => {
                    self.dispatched.insert(dir.clone());
                    self.pending_size.remove(&dir);
                    ready.push(dir);
                }
                _ => {
                    self.pending_size.insert(dir, total);
                }
            }
        }
        Ok(ready)
    }
}

/// Blocking SPMC work queue used to hand per-test profraw dirs from the
/// watcher to the extraction workers. The watcher pushes and eventually
/// calls [`Self::mark_done`]; workers loop on [`Self::pop_blocking`] and
/// see `None` once the queue is drained AND `done` is set.
#[derive(Default)]
struct WorkQueue {
    inner: Mutex<WorkQueueInner>,
    cond: Condvar,
}

#[derive(Default)]
struct WorkQueueInner {
    queue: VecDeque<PathBuf>,
    done: bool,
}

impl WorkQueue {
    fn push(&self, dir: PathBuf) {
        let mut inner = self.inner.lock().unwrap();
        inner.queue.push_back(dir);
        self.cond.notify_one();
    }

    fn pop_blocking(&self) -> Option<PathBuf> {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if let Some(dir) = inner.queue.pop_front() {
                return Some(dir);
            }
            if inner.done {
                return None;
            }
            inner = self.cond.wait(inner).unwrap();
        }
    }

    fn mark_done(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.done = true;
        self.cond.notify_all();
    }
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
    /// Nextest filterset expression matching every selected test, including
    /// phantoms — nextest will silently match nothing for those.
    fn filter_expr(&self) -> String {
        let v: Vec<TestId> = self.selected.iter().cloned().collect();
        nextest_filter_expr(&v)
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
            return Err(e)
                .with_context(|| format!("failed to read {}", project_config.display()))
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

/// One binary in nextest's listing. Carried for the suite-level count
/// surfaced in collect's progress output; the runner shim sources binary_id
/// directly from `NEXTEST_BINARY_ID` at test time.
#[derive(Debug, Clone)]
pub(crate) struct BinaryEntry {
    #[allow(dead_code)]
    pub(crate) binary_id: String,
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
pub(crate) fn nextest_list(
    project_root: &Path,
    rustflags_override: Option<&str>,
    build_dir: Option<&Path>,
    build_args: &[String],
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
    let mut ignored = BTreeSet::new();
    let mut binaries = Vec::new();
    if let Some(suites) = json.get("rust-suites").and_then(|v| v.as_object()) {
        for suite in suites.values() {
            let binary_id = suite
                .get("binary-id")
                .and_then(|v| v.as_str())
                .context("nextest list entry missing binary-id")?
                .to_string();
            binaries.push(BinaryEntry {
                binary_id: binary_id.clone(),
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

/// List subdirectories of `profraw_dir` that look like per-test output
/// (contain a `meta` sidecar).
///
/// The shim writes per-test dirs at `profraw_dir/<binary_id>/<test_name>/`,
/// so we walk two levels. Splitting binary_id and test_name into separate
/// path components avoids the collision case where sanitization collapses
/// `::` into `_` and two genuinely-distinct (binary_id, test_name) pairs
/// produce the same single-level name.
fn list_test_dirs(profraw_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for binary_entry in std::fs::read_dir(profraw_dir)? {
        let binary_entry = binary_entry?;
        let binary_path = binary_entry.path();
        if !binary_path.is_dir() {
            continue;
        }
        for test_entry in std::fs::read_dir(&binary_path)? {
            let test_entry = test_entry?;
            let test_path = test_entry.path();
            if test_path.is_dir() && test_path.join("meta").exists() {
                dirs.push(test_path);
            }
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
    let line = std::str::from_utf8(&stdout).unwrap_or_default().lines().next().unwrap_or("");
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
/// Conservative: any unparseable version is treated as too old.
fn nextest_version_at_least(actual: &str, required: &str) -> bool {
    fn parts(v: &str) -> Option<Vec<u64>> {
        v.split(['-', '+']).next()?
            .split('.')
            .map(|p| p.parse().ok())
            .collect()
    }
    match (parts(actual), parts(required)) {
        (Some(a), Some(r)) => a >= r,
        _ => false,
    }
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
            doc["profile"]["default"]["default-filter"].as_str().unwrap(),
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
            doc["profile"]["default"]["scripts"].as_array_of_tables().unwrap().len(),
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
            doc["profile"]["default"]["default-filter"].as_str().unwrap(),
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
            doc["profile"]["default"]["default-filter"].as_str().unwrap(),
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
        // Unparseable: conservative — treat as too old.
        assert!(!nextest_version_at_least("garbage", "0.9.116"));
        assert!(!nextest_version_at_least("", "0.9.116"));
    }

    /// Lay out a per-test profraw dir under the watcher's tree root the same
    /// way the shim does: `<root>/<binary_id>/<test_name>/`, with a `meta`
    /// sidecar and the named `.profraw` file pre-sized to `profraw_size`.
    fn make_test_dir(
        root: &Path,
        binary_id: &str,
        test_name: &str,
        profraw_size: usize,
    ) -> PathBuf {
        let dir = root.join(binary_id).join(test_name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta"), format!("{test_name}\nbin\n{binary_id}\n")).unwrap();
        if profraw_size > 0 {
            std::fs::write(dir.join("1-1.profraw"), vec![0u8; profraw_size]).unwrap();
        }
        dir
    }

    /// Single dispatch: a dir that has shown the same non-zero profraw size
    /// across two consecutive polls is ready. One poll alone isn't enough —
    /// the LLVM runtime could still be mid-write the first time we see the
    /// file.
    #[test]
    fn watch_state_dispatches_after_two_stable_polls() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dir = make_test_dir(root, "crate-a", "test_a", 64);

        let mut state = WatchState::default();

        // First poll: profraw is there but we haven't seen the size before;
        // hold back to confirm it isn't still growing.
        let first = state.poll(root, false).unwrap();
        assert!(first.is_empty(), "first poll should hold back: {first:?}");

        // Second poll with the same size: dispatch.
        let second = state.poll(root, false).unwrap();
        assert_eq!(second, vec![dir.clone()]);

        // Third poll: nothing new, and we don't redispatch the same dir.
        let third = state.poll(root, false).unwrap();
        assert!(third.is_empty(), "should not redispatch: {third:?}");
    }

    /// A dir with no `.profraw` yet (test still running, or runtime hasn't
    /// flushed) is skipped while nextest is alive. The size-stability rule
    /// also rejects a zero-byte profraw — LLVM writes the buffer in one
    /// shot, so zero bytes means we caught it between `fopen` and the first
    /// `write`.
    #[test]
    fn watch_state_holds_back_until_profraw_has_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let no_profraw = make_test_dir(root, "crate-a", "test_running", 0);
        let zero_byte = make_test_dir(root, "crate-a", "test_zero", 0);
        std::fs::write(zero_byte.join("1-1.profraw"), b"").unwrap();

        let mut state = WatchState::default();
        for _ in 0..3 {
            assert!(state.poll(root, false).unwrap().is_empty());
        }
        assert!(!state.dispatched.contains(&no_profraw));
        assert!(!state.dispatched.contains(&zero_byte));
    }

    /// The final pass — invoked once nextest has exited — dispatches every
    /// remaining dir unconditionally, including ones with no `.profraw` and
    /// ones we'd been holding back. By then no more writes can land, so
    /// "still growing" is no longer a possibility, and we want even the
    /// failure shapes (`Skipped("no profraw generated")`) to surface in the
    /// per-test log.
    #[test]
    fn watch_state_final_pass_drains_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let with_profraw = make_test_dir(root, "crate-a", "test_a", 64);
        let no_profraw = make_test_dir(root, "crate-a", "test_b", 0);

        let mut state = WatchState::default();
        // One pre-final poll registers a pending size for the live dir but
        // doesn't dispatch yet.
        assert!(state.poll(root, false).unwrap().is_empty());

        let final_ready = state.poll(root, true).unwrap();
        let ready_set: BTreeSet<_> = final_ready.into_iter().collect();
        assert_eq!(
            ready_set,
            BTreeSet::from([with_profraw, no_profraw]),
        );
    }

    /// Once a dir has been dispatched (e.g. by the streaming pass), the
    /// final pass must NOT redispatch it — otherwise workers would
    /// double-process the bundle.
    #[test]
    fn watch_state_does_not_redispatch_on_final_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dir = make_test_dir(root, "crate-a", "test_a", 64);

        let mut state = WatchState::default();
        assert!(state.poll(root, false).unwrap().is_empty());
        assert_eq!(state.poll(root, false).unwrap(), vec![dir.clone()]);
        // Streaming worker would have removed the dir at this point.
        let _ = std::fs::remove_dir_all(&dir);

        let final_ready = state.poll(root, true).unwrap();
        assert!(final_ready.is_empty(), "redispatched: {final_ready:?}");
    }

    /// The work queue is single-producer / multi-consumer with a `done`
    /// sentinel. Workers block while the queue is empty, wake on push, and
    /// see `None` once the watcher marks done AND the queue is drained.
    #[test]
    fn work_queue_blocks_then_drains_on_done() {
        use std::sync::Arc;
        let q = Arc::new(WorkQueue::default());

        let q_pop = Arc::clone(&q);
        let handle = std::thread::spawn(move || {
            let mut got = Vec::new();
            while let Some(p) = q_pop.pop_blocking() {
                got.push(p);
            }
            got
        });

        q.push(PathBuf::from("a"));
        q.push(PathBuf::from("b"));
        // The consumer may have woken between pushes — either way both items
        // are in the worker's `got` once it's seen `done` and the queue is
        // empty.
        q.mark_done();
        let drained = handle.join().unwrap();
        assert_eq!(drained, vec![PathBuf::from("a"), PathBuf::from("b")]);
    }
}
