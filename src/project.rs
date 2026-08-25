//! Shared project utilities: root detection and git queries.
//!
//! Two jobs sit together here because they answer the same question from
//! different directions — *what is this project, right now*. `cargo metadata`
//! says which crates it contains and where its root is; git says what has
//! changed and which snapshots are still addressable.
//!
//! ## Line numbers are in the collect_sha's coordinate system
//!
//! The invariant everything downstream rests on. [`git_changed_line_ranges`]
//! returns **OLD-side** line numbers — positions in the `collect_sha`
//! snapshot, not in the working tree — because that is the coordinate system
//! `test_regions` rows were recorded in. Overlap queries compare the two
//! directly, so a switch to NEW-side numbers wouldn't fail loudly; it would
//! quietly select the wrong tests, more wrongly the further HEAD drifts from
//! the anchor.
//!
//! [`relation_to_head`] is what lets that hold for shas that aren't ancestors
//! of HEAD. Only a sha genuinely absent from the repo is `Missing`; a sibling
//! or post-`reset` orphan stays `Reachable`, because `git diff <sha> HEAD`
//! resolves either way and the ranges still live in `<sha>`'s coordinates.
//! `Reachable { commits_ahead: 0 }` is such a sibling — it resolves, but its
//! tree differs from HEAD's, which is why callers treat it as divergence
//! rather than an exact hit.
//!
//! ## Git failure is never "nothing changed"
//!
//! Every query here propagates a non-zero git exit rather than degrading to an
//! empty result. An empty change set is indistinguishable from a clean tree,
//! so a swallowed error would select zero tests and report success — silent
//! under-selection, the one failure this tool cannot detect downstream.
//!
//! ## Paths must match what the compiler recorded
//!
//! [`canonicalize_no_verbatim`] exists because the two platforms fail in
//! opposite directions: macOS needs canonicalization (temp dirs reached via
//! `/var` are recorded under `/private/var`), while Windows needs its absence
//! (`Path::canonicalize` adds a `\\?\` prefix and expands 8.3 names, neither
//! of which cargo or llvm-cov produce). Getting this wrong makes
//! `strip_prefix` against the project root discard every function — see
//! `tests/functional/remapped_paths.rs` for what that costs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use camino::Utf8PathBuf;

use crate::coverage::to_db_relative;

/// Inclusive line range `[start, end]` of a changed hunk in some file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct LineRange {
    pub(crate) start: i64,
    pub(crate) end: i64,
}

/// How a stored sha relates to the current `HEAD`.
///
/// `Equal` — same commit, no drift.
/// `Reachable` — sha exists in the repo. `commits_ahead` is the number of
/// commits in `HEAD` that aren't in the sha (`git rev-list --count
/// {sha}..HEAD`); zero when sha is the immediate parent of HEAD or shares its
/// tip. The OLD-side line numbers in `git diff <sha> HEAD` still belong to
/// the sha's coordinate system, which matches stored coverage ranges, so
/// selection works whether the sha is a strict ancestor or a sibling on a
/// different branch (CI's typical PR shape: cached collect ran on the latest
/// main commit, which is *ahead of* the PR's merge-base rather than behind
/// HEAD). Sibling diffs over-select for any commits-on-main-but-not-on-PR;
/// strict ancestors don't have that noise.
/// `Missing` — sha is not in the repo (rebased and pruned, garbage-collected,
/// or beyond a shallow clone boundary). The diff has no anchor and the cache
/// is unusable; tests anchored at this sha rerun as new.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShaRelation {
    Equal,
    Reachable { commits_ahead: u32 },
    Missing,
}

/// Project root information.
pub(crate) struct ProjectRoot {
    /// Workspace root directory. Git operations and the DB live here.
    /// For single-crate projects this equals the crate root.
    pub(crate) workspace_root: PathBuf,
    /// All Cargo.toml files belonging to the workspace — the root manifest
    /// plus every member's manifest. Sorted, deduplicated. Used for
    /// environment fingerprinting.
    pub(crate) manifest_paths: Vec<PathBuf>,
    /// Raw `cargo metadata --no-deps` JSON. Parsed once at root detection so
    /// less-common lookups (test src paths) don't have to re-spawn cargo.
    pub(crate) metadata: serde_json::Value,
}

/// `Path::canonicalize` adapted for cross-platform path-prefix arithmetic.
///
/// On unix this is `Path::canonicalize` — resolves symlinks so the result
/// matches what rustc/llvm-cov embed in coverage maps (macOS tempdirs
/// hide behind `/var → /private/var` and stripping the symlinked form
/// against llvm-cov's resolved paths would silently fail).
///
/// On Windows it returns the path unchanged. `Path::canonicalize` there
/// adds a `\\?\` verbatim prefix AND expands 8.3 short names
/// (`RUNNER~1` → `runneradmin`), neither of which match cargo metadata's
/// or llvm-cov's path forms — `strip_prefix` against the canonicalized
/// root drops every match. Cargo's own tooling doesn't canonicalize, so
/// the cargo-given path matches itself fine without help.
pub(crate) fn canonicalize_no_verbatim(path: &Path) -> Result<PathBuf> {
    #[cfg(windows)]
    {
        Ok(path.to_path_buf())
    }
    #[cfg(not(windows))]
    {
        path.canonicalize()
            .with_context(|| format!("failed to canonicalize {}", path.display()))
    }
}

/// Find the project root via `cargo metadata`.
///
/// Uses `cargo metadata --no-deps --format-version=1` to reliably determine
/// the workspace root, which handles both single-crate and workspace projects.
pub(crate) fn find_project_root() -> Result<ProjectRoot> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version=1"])
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
    let workspace_root = meta["workspace_root"]
        .as_str()
        .context("cargo metadata missing workspace_root")?;
    let workspace_root = PathBuf::from(workspace_root);

    let mut manifest_paths: Vec<PathBuf> = meta["packages"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.get("manifest_path").and_then(|m| m.as_str()))
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default();
    // Virtual workspaces have a root Cargo.toml that isn't listed in `packages[]`,
    // yet its `[workspace]` section controls which crates are in the workspace
    // and its `[workspace.dependencies]`/`[workspace.package]` sections propagate
    // to members — changes must invalidate the fingerprint. Push only if absent
    // to keep single-crate projects (where it IS a package) from double-hashing.
    let root_manifest = workspace_root.join("Cargo.toml");
    if root_manifest.exists() && !manifest_paths.contains(&root_manifest) {
        manifest_paths.push(root_manifest);
    }
    manifest_paths.sort();

    Ok(ProjectRoot {
        workspace_root,
        manifest_paths,
        metadata: meta,
    })
}

impl ProjectRoot {
    /// Build the per-target sentinel map: `binary_id → {crate_root paths}`.
    ///
    /// Each test's `binary_id` (nextest's stable target identifier) maps to
    /// the set of source files whose hunks must overlap a sentinel
    /// `(1, i64::MAX)` row to re-select that test. The set captures cargo's
    /// compile-time dependencies the function-level coverage can't observe:
    ///
    /// 1. **The target's own crate root** — `mod foo;` / `use ...;` in a
    ///    file with no executable regions still affects every test in that
    ///    target.
    /// 2. **The package's lib crate root**, if this target isn't the lib
    ///    itself — bins/integration tests/examples/benches all link against
    ///    their package's lib by default. Over-including is safe (extra test
    ///    run, not a miss) for the rare bin-without-lib case.
    /// 3. **Lib crate roots of transitively-depended workspace packages**
    ///    (path/workspace deps, normal + dev) — a structural edit in
    ///    `strings/src/lib.rs` must pull in `math`'s tests when `math`
    ///    depends on `strings`. Registry deps are excluded; their files
    ///    aren't in the working tree and can't appear in a diff anyway.
    ///
    /// `binary_id`s are constructed to match nextest's `RustBinaryId::from_parts`
    /// (part of nextest's stable API), so a direct lookup against
    /// `TestId.binary_id` from `cargo nextest list` is exact.
    ///
    /// Reads from the cached `metadata` JSON — no cargo spawn.
    pub(crate) fn crate_root_sentinels_by_binary_id(
        &self,
    ) -> Result<BTreeMap<String, BTreeSet<Utf8PathBuf>>> {
        // No canonicalize: `workspace_root` and `target.src_path` both come
        // from the same `cargo metadata` invocation and share whatever
        // normalisation cargo applied. Calling `Path::canonicalize` here
        // would diverge on Windows (verbatim `\\?\` prefix, 8.3 → long-name
        // expansion of `RUNNER~1` → `runneradmin`) and the strip_prefix
        // below would silently drop every target.
        let root = &self.workspace_root;

        let Some(packages) = self.metadata.get("packages").and_then(|v| v.as_array()) else {
            return Ok(BTreeMap::new());
        };

        let workspace_names: BTreeSet<&str> = packages
            .iter()
            .filter_map(|p| p.get("name").and_then(|v| v.as_str()))
            .collect();

        // Per-package: lib src_path (if any), test-producing targets, and
        // workspace-package deps (normal + dev — both compile into tests).
        let mut lib_src: BTreeMap<&str, Utf8PathBuf> = BTreeMap::new();
        let mut targets_by_pkg: BTreeMap<&str, Vec<TestTarget>> = BTreeMap::new();
        let mut deps_by_pkg: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();

        for pkg in packages {
            let Some(name) = pkg.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            if let Some(target_arr) = pkg.get("targets").and_then(|v| v.as_array()) {
                for target in target_arr {
                    if let Some(parsed) = parse_target(target, root) {
                        // Recorded regardless of `runs_tests`, because the two
                        // roles are independent: a `[lib] test = false` lib
                        // builds no test harness of its own, but every other
                        // target in the package — and every workspace package
                        // that depends on it — still compiles against it. Its
                        // crate root has to stay available to the "links to
                        // own lib" and transitive-dep rules below, or a
                        // structural edit there selects nothing.
                        if matches!(parsed.kind, TargetKind::Lib) {
                            lib_src.insert(name, parsed.src_path.clone());
                        }
                        if parsed.runs_tests {
                            targets_by_pkg.entry(name).or_default().push(parsed);
                        }
                    }
                }
            }
            if let Some(deps) = pkg.get("dependencies").and_then(|v| v.as_array()) {
                let mut workspace_deps = BTreeSet::new();
                for dep in deps {
                    let Some(dep_name) = dep.get("name").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    // `kind`: null = normal, "dev" = dev-dep, "build" = build-dep.
                    // build-deps only affect build.rs, not test compilation.
                    let kind = dep.get("kind").and_then(|v| v.as_str());
                    if kind == Some("build") {
                        continue;
                    }
                    if !workspace_names.contains(dep_name) {
                        continue;
                    }
                    workspace_deps.insert(dep_name);
                }
                deps_by_pkg.insert(name, workspace_deps);
            }
        }

        // Transitive closure of workspace-package deps. Only used to union in
        // the depended-on packages' lib src_paths, so the closure is shallow
        // by structure (workspaces rarely have deep internal chains).
        let transitive_deps: BTreeMap<&str, BTreeSet<&str>> = workspace_names
            .iter()
            .map(|p| (*p, transitive_closure(p, &deps_by_pkg)))
            .collect();

        let mut out: BTreeMap<String, BTreeSet<Utf8PathBuf>> = BTreeMap::new();
        for (pkg_name, targets) in &targets_by_pkg {
            for target in targets {
                let binary_id = build_binary_id(pkg_name, target.kind, &target.name);
                let mut sentinels = BTreeSet::new();
                sentinels.insert(target.src_path.clone());
                if !matches!(target.kind, TargetKind::Lib) {
                    if let Some(lib) = lib_src.get(pkg_name) {
                        sentinels.insert(lib.clone());
                    }
                }
                if let Some(deps) = transitive_deps.get(pkg_name) {
                    for dep in deps {
                        if let Some(lib) = lib_src.get(dep) {
                            sentinels.insert(lib.clone());
                        }
                    }
                }
                out.insert(binary_id, sentinels);
            }
        }
        Ok(out)
    }
}

/// A target's role for sentinel scoping. Mirrors the cargo-target kinds
/// nextest considers test-runnable; everything else (custom-build, examples,
/// benches) is dropped at parse time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Lib,
    /// Includes proc-macro libs — they can have unit tests, and nextest gives
    /// them the same `<package>` binary_id shape as a regular lib.
    ProcMacro,
    Bin,
    /// `tests/*.rs` integration test target.
    Test,
}

#[derive(Debug)]
struct TestTarget {
    name: String,
    kind: TargetKind,
    /// Path relative to the workspace root.
    src_path: Utf8PathBuf,
    /// Cargo's `test` flag for this target — whether cargo builds a test
    /// harness for it, so nextest gets a binary with tests in it. False for
    /// `[lib] test = false` and its `[[bin]]`/`[[test]]` equivalents. Such a
    /// target contributes no `binary_id` of its own but is still a
    /// compile-time input to the ones that do.
    runs_tests: bool,
}

/// Parse one `cargo metadata` target into a [`TestTarget`], or `None` if it
/// isn't a kind that can matter for sentinels — `custom-build`, `example` and
/// `bench` are dropped here and nowhere else.
///
/// Cargo's `test` flag is carried through rather than filtered on: a target
/// with `test = false` still compiles into its package's other targets, so
/// dropping it here would lose its crate root as a sentinel.
fn parse_target(target: &serde_json::Value, root: &Path) -> Option<TestTarget> {
    let kinds: Vec<&str> = target
        .get("kind")
        .and_then(|v| v.as_array())
        .map(|ks| ks.iter().filter_map(|k| k.as_str()).collect())
        .unwrap_or_default();
    let kind = if kinds.contains(&"lib") {
        TargetKind::Lib
    } else if kinds.contains(&"proc-macro") {
        TargetKind::ProcMacro
    } else if kinds.contains(&"bin") {
        TargetKind::Bin
    } else if kinds.contains(&"test") {
        TargetKind::Test
    } else {
        return None;
    };
    let name = target.get("name").and_then(|v| v.as_str())?.to_string();
    let abs = target.get("src_path").and_then(|v| v.as_str())?;
    let rel = Path::new(abs).strip_prefix(root).ok()?;
    let src_path = to_db_relative(rel)?;
    let runs_tests = target
        .get("test")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    Some(TestTarget {
        name,
        kind,
        src_path,
        runs_tests,
    })
}

/// Construct nextest's stable `binary_id` for a workspace target. Mirrors
/// `nextest_metadata::RustBinaryId::from_parts`:
/// - lib/proc-macro → `<package>`
/// - integration test (kind=test) → `<package>::<target>`
/// - other (bin/bench/example) → `<package>::<kind>/<target>`
fn build_binary_id(package: &str, kind: TargetKind, target_name: &str) -> String {
    match kind {
        TargetKind::Lib | TargetKind::ProcMacro => package.to_string(),
        TargetKind::Test => format!("{package}::{target_name}"),
        TargetKind::Bin => format!("{package}::bin/{target_name}"),
    }
}

/// Compute the transitive closure of workspace-package deps starting from
/// `start`. The closure excludes `start` itself even when a cycle leads
/// back to it.
fn transitive_closure<'a>(
    start: &'a str,
    deps_by_pkg: &BTreeMap<&'a str, BTreeSet<&'a str>>,
) -> BTreeSet<&'a str> {
    let mut out: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<&str> = deps_by_pkg
        .get(start)
        .map(|s| s.iter().copied().collect())
        .unwrap_or_default();
    while let Some(p) = stack.pop() {
        if p == start || !out.insert(p) {
            continue;
        }
        if let Some(next) = deps_by_pkg.get(p) {
            for n in next {
                if *n != start && !out.contains(n) {
                    stack.push(n);
                }
            }
        }
    }
    out
}

/// List files changed in the working tree relative to HEAD: staged + unstaged
/// + untracked.
///
/// Returns paths relative to the project root. A non-zero git exit (corrupt
/// repo, missing object, permissions) is a hard error: silently returning "no
/// changed files" would look like a clean tree and select zero tests.
pub(crate) fn git_changed_files(project_root: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    for args in [
        vec!["diff", "--no-color", "--no-ext-diff", "--name-only", "-z"],
        vec![
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--name-only",
            "--cached",
            "-z",
        ],
        vec!["ls-files", "-z", "--others", "--exclude-standard"],
    ] {
        for path in run_git(project_root, &args)? {
            if !files.contains(&path) {
                files.push(path);
            }
        }
    }

    // Filter out stray profraw files (from instrumented subprocesses that
    // didn't inherit our LLVM_PROFILE_FILE). The DB lives under target/ so
    // git already ignores it.
    files.retain(|f| !f.ends_with(".profraw"));

    files.sort();
    files.dedup();
    Ok(files)
}

/// Whether the working tree has uncommitted changes — staged, unstaged, or
/// untracked — relative to `HEAD`. Ignored files don't count.
///
/// `collect` refuses to run on a dirty tree by default: stored line numbers
/// reflect the working-tree files cargo actually compiled, but the captured
/// `collect_sha` points at `HEAD`. Later, `run`/`status` ask git for hunks via
/// `git diff -U0 <collect_sha>`, whose OLD-side line numbers are in HEAD's
/// coordinate system — out of phase with what's in the DB. The
/// structural-edit backstop hides some of the damage but only when a hunk
/// overlaps no stored range; point edits within a function silently
/// mis-select.
pub(crate) fn git_working_tree_dirty(project_root: &Path) -> Result<bool> {
    // `--porcelain=v1 -z` emits one NUL-terminated entry per changed path
    // (renames split into two entries; we only care about emptiness).
    // Untracked files are reported by default; ignored files are not. That's
    // the right calibration: an untracked .rs in the workspace can be
    // compiled into tests, so its line numbers end up in the DB just like a
    // modified file's.
    let lines = run_git(project_root, &["status", "--porcelain=v1", "-z"])?;
    Ok(!lines.is_empty())
}

/// Capture the current git HEAD sha. Hard error if HEAD is unreachable —
/// detached/initial-commit repos can't anchor function-level coverage and
/// silently using "" would later fail with a confusing diff error.
pub(crate) fn git_head_sha(project_root: &Path) -> Result<String> {
    let lines = run_git(project_root, &["rev-parse", "HEAD"])?;
    let sha = lines
        .into_iter()
        .next()
        .context("git rev-parse HEAD returned no output")?
        .trim()
        .to_string();
    if sha.is_empty() {
        bail!("git rev-parse HEAD returned an empty sha");
    }
    Ok(sha)
}

/// Compare `sha` against current `HEAD` for drift reporting.
///
/// `Missing` only when the sha is not in the repo at all — rebased and
/// pruned, garbage-collected, or beyond a shallow clone boundary. A sha that
/// exists but isn't an ancestor of HEAD (sibling branches, post-`git reset`
/// orphans, the PR-vs-main-tip shape) is still `Reachable`: `git diff <sha>
/// HEAD` resolves the trees fine, and stored coverage ranges live in `sha`'s
/// coordinate system, which matches the diff's OLD side either way.
pub(crate) fn relation_to_head(project_root: &Path, sha: &str) -> Result<ShaRelation> {
    let head = git_head_sha(project_root)?;
    if head == sha {
        return Ok(ShaRelation::Equal);
    }
    let exists = Command::new("git")
        .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
        .current_dir(project_root)
        .output()
        .with_context(|| format!("failed to run git cat-file -e {sha}"))?;
    if !exists.status.success() {
        return Ok(ShaRelation::Missing);
    }
    let lines = run_git(
        project_root,
        &["rev-list", "--count", &format!("{sha}..HEAD")],
    )?;
    let count = lines
        .into_iter()
        .next()
        .context("git rev-list --count returned no output")?
        .trim()
        .parse::<u32>()
        .context("git rev-list --count returned non-numeric output")?;
    Ok(ShaRelation::Reachable {
        commits_ahead: count,
    })
}

/// Per-file changed line ranges between `collect_sha` and the working tree.
///
/// Runs `git diff -U0 --no-color --no-ext-diff --no-renames <collect_sha>`
/// (plus prefix and quotePath settings — see the invocation) and parses
/// `@@ -A,B +C,D @@` headers. Returns OLD-side line ranges (i.e. line
/// numbers in the `collect_sha` snapshot, which is what `test_regions`
/// stores). Pure insertions (`@@ -A,0 +C,D @@`) collapse to the single line
/// `[A, A]` — the line in old before which content was inserted; this
/// over-selects only at file edges.
///
/// Untracked files (no OLD-side at all) don't appear here; callers receive
/// the file list from `git_changed_files` and warn separately.
///
/// Errors are loud — git failure (bad sha, corrupt repo, etc.) is propagated
/// rather than silently emitting an empty map.
pub(crate) fn git_changed_line_ranges(
    project_root: &Path,
    collect_sha: &str,
) -> Result<BTreeMap<String, Vec<LineRange>>> {
    // `--src-prefix=a/ --dst-prefix=b/` forces the standard prefixes — without
    // them, `git diff <commit>` against the working tree uses `c/` and `w/`
    // and our parser would skip every `--- ` line. `core.quotePath=false`
    // stops git from octal-escaping non-ASCII path bytes into a C-quoted
    // string our parser can't read. Paths git still quotes (embedded quotes,
    // backslashes, control characters) stay unparsed and their ranges are
    // dropped — accepted, since such names essentially never reach rustc,
    // and config-rule matching still sees them verbatim via
    // `git_changed_files`' `-z`.
    let output = Command::new("git")
        .args([
            "-c",
            "core.quotePath=false",
            "diff",
            "-U0",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            collect_sha,
        ])
        .current_dir(project_root)
        .output()
        .context("failed to run git diff -U0")?;
    if !output.status.success() {
        let code = output
            .status
            .code()
            .map_or_else(|| "signal".to_string(), |c| c.to_string());
        bail!(
            "git diff -U0 {} failed (exit {}): {}",
            collect_sha,
            code,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    parse_unified_diff(&output.stdout)
}

/// Files added between `collect_sha` and the working tree (status `A`).
/// `git_changed_line_ranges` skips these (no OLD-side hunks), and
/// `git_changed_files` only sees uncommitted changes — so for committed
/// additions, the diagnostic report needs this dedicated query to know
/// the file existed in the diff at all. Returns paths relative to the
/// project root.
pub(crate) fn git_added_files_since(project_root: &Path, collect_sha: &str) -> Result<Vec<String>> {
    let mut out = run_git(
        project_root,
        &[
            "diff",
            "--name-only",
            "--no-color",
            "--no-renames",
            "--diff-filter=A",
            "-z",
            collect_sha,
        ],
    )?;
    out.sort();
    out.dedup();
    Ok(out)
}

/// Operates on bytes because content lines can carry arbitrary non-UTF-8
/// data: git diffs a file as text whenever its leading bytes look text-like,
/// so a fixture with binary content later in the file lands verbatim in the
/// diff. Only the `--- ` and `@@ ` header lines are decoded; content lines
/// are never interpreted. Diff framing is LF even on Windows, so splitting
/// on `\n` is exact.
fn parse_unified_diff(diff: &[u8]) -> Result<BTreeMap<String, Vec<LineRange>>> {
    let mut map: BTreeMap<String, Vec<LineRange>> = BTreeMap::new();
    let mut current_file: Option<String> = None;
    // Content lines still owed by the current hunk, per side. While either
    // is outstanding the incoming line is hunk content and must not be
    // header-matched: a deleted line whose content starts with `-- ` (an SQL
    // comment, say) renders as `--- ...` and would otherwise be mistaken for
    // a file header, silently dropping the file's remaining hunks.
    let (mut rem_old, mut rem_new) = (0i64, 0i64);
    for line in diff.split(|&b| b == b'\n') {
        if rem_old > 0 || rem_new > 0 {
            match line.first() {
                Some(b'-') => rem_old -= 1,
                Some(b'+') => rem_new -= 1,
                // `\ No newline at end of file` — annotation, not content.
                Some(b'\\') => {}
                // `-U0` emits no context lines, so anything else means the
                // budgets are out of sync with the stream; guessing would
                // corrupt them silently.
                _ => bail!(
                    "unexpected line in git diff hunk content: {}",
                    String::from_utf8_lossy(line)
                ),
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"--- ") {
            // `--- a/path/to/file` or `--- /dev/null` for new files. We don't
            // emit ranges for /dev/null (the new file's lines all live on the
            // NEW side, with no OLD-side coordinates). A path that isn't
            // valid UTF-8 can't have coverage rows — cargo and llvm-cov both
            // model paths as UTF-8 (camino) — so skipping it loses no
            // selection.
            current_file = std::str::from_utf8(rest).ok().and_then(parse_diff_path);
        } else if line.starts_with(b"@@ ") {
            // The function-name context after the closing `@@` is file
            // content and can be non-UTF-8; lossy decode keeps the ASCII
            // numeric part intact.
            let text = String::from_utf8_lossy(line);
            let Some(hunk) = parse_hunk_header(&text) else {
                // Content lines are consumed by the budgets above, so an
                // unparsable `@@ ` line here is a corrupt header — and
                // without its counts the following content would be
                // header-matched.
                bail!("malformed hunk header in git diff output: {text}");
            };
            // Set the budgets even when the hunk's ranges are skipped, so
            // its content lines are still consumed above.
            (rem_old, rem_new) = (hunk.old_count, hunk.new_count);
            let Some(file) = current_file.clone() else {
                continue;
            };
            // /dev/null sentinel — skip.
            if file == "/dev/null" {
                continue;
            }
            map.entry(file).or_default().push(hunk.old_range);
        }
    }

    // Coalesce overlapping/adjacent ranges so downstream queries don't
    // double-count. Adjacent (end+1 == next.start) hunks are rare from
    // -U0 but harmless to merge.
    for ranges in map.values_mut() {
        ranges.sort_by_key(|r| (r.start, r.end));
        let mut merged: Vec<LineRange> = Vec::with_capacity(ranges.len());
        for r in ranges.drain(..) {
            match merged.last_mut() {
                Some(prev) if r.start <= prev.end + 1 => {
                    prev.end = prev.end.max(r.end);
                }
                _ => merged.push(r),
            }
        }
        *ranges = merged;
    }

    Ok(map)
}

/// Strip the `a/`/`b/` prefix git adds and trim the trailing tab+timestamp.
fn parse_diff_path(rest: &str) -> Option<String> {
    // Format: `a/path/to/file` (or `/dev/null`). Sometimes followed by tab+timestamp.
    let path = rest.split('\t').next().unwrap_or(rest);
    if path == "/dev/null" {
        return Some("/dev/null".to_string());
    }
    path.strip_prefix("a/").map(String::from)
}

/// A parsed `@@` hunk header: the OLD-side range plus both sides' line
/// counts (the number of `-`/`+` content lines that follow the header).
struct Hunk {
    old_range: LineRange,
    old_count: i64,
    new_count: i64,
}

/// Parse `@@ -OLD_START[,OLD_COUNT] +NEW_START[,NEW_COUNT] @@ ...`. The
/// OLD-side range is inclusive; for pure insertions (`OLD_COUNT == 0`) it
/// collapses to `[OLD_START, OLD_START]` — the line before which content was
/// inserted, so functions containing that line are still picked up (slight
/// over-select at file edges, acceptable).
fn parse_hunk_header(line: &str) -> Option<Hunk> {
    // Stripping leading "@@ " and finding the next " @@" boundary keeps
    // surrounding context (function name on inline-context lines) out of
    // the parse.
    let inner = line.strip_prefix("@@ ")?;
    let end_idx = inner.find(" @@")?;
    let body = &inner[..end_idx];
    // body looks like: "-OLD +NEW" — split on space.
    let mut parts = body.split_whitespace();
    let old = parts.next()?;
    let new = parts.next()?;
    let parse_side = |side: &str, sign: char| -> Option<(i64, i64)> {
        let side = side.strip_prefix(sign)?;
        match side.split_once(',') {
            Some((s, c)) => Some((s.parse().ok()?, c.parse().ok()?)),
            None => Some((side.parse().ok()?, 1)),
        }
    };
    let (old_start, old_count) = parse_side(old, '-')?;
    let (_, new_count) = parse_side(new, '+')?;
    let old_range = if old_count == 0 {
        LineRange {
            start: old_start,
            end: old_start,
        }
    } else {
        LineRange {
            start: old_start,
            end: old_start + old_count - 1,
        }
    };
    Some(Hunk {
        old_range,
        old_count,
        new_count,
    })
}

/// Run `git <args>` in `project_root` and return NUL-separated stdout entries.
/// Callers must pass `-z` so paths come through verbatim — without it, git
/// quotes paths containing special characters and a path with a literal
/// newline would be split into two phantom entries. Errors include the full
/// command, exit code, and stderr; silent skips here would mask repo
/// corruption, bad refs, or missing objects as a clean tree.
fn run_git(project_root: &Path, args: &[&str]) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(project_root)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    if !output.status.success() {
        let code = output
            .status
            .code()
            .map_or_else(|| "signal".to_string(), |c| c.to_string());
        bail!(
            "git {} failed (exit {}): {}",
            args.join(" "),
            code,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(output
        .stdout
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Initialize a fresh git repo in `dir` with a single committed file.
    /// Sets local user.name/user.email so the commit succeeds even when the
    /// host has no global git identity configured.
    fn init_repo(dir: &Path) -> Result<()> {
        let run = |args: &[&str]| -> Result<()> {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .with_context(|| format!("git {}", args.join(" ")))?;
            if !out.status.success() {
                bail!(
                    "git {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            Ok(())
        };
        run(&["init", "-q", "-b", "main"])?;
        run(&["config", "user.email", "test@example.com"])?;
        run(&["config", "user.name", "Test"])?;
        std::fs::write(dir.join("README.md"), b"hello\n")?;
        run(&["add", "README.md"])?;
        run(&["commit", "-q", "-m", "init"])?;
        Ok(())
    }

    #[test]
    fn working_tree_happy_path() -> Result<()> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path())?;
        std::fs::write(dir.path().join("new.txt"), b"x")?;

        let files = git_changed_files(dir.path())?;
        assert!(
            files.iter().any(|f| f == "new.txt"),
            "expected new.txt in {files:?}"
        );
        Ok(())
    }

    #[test]
    fn awkward_filename_round_trips() -> Result<()> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path())?;
        // Space + em-dash trigger git's `core.quotePath` machinery; the `-z`
        // flag bypasses it so bytes come through verbatim. We deliberately
        // avoid `"` (and the rest of `* ? < > |`) since Windows forbids
        // those in filenames.
        let awkward = "a b — weird-name.txt";
        std::fs::write(dir.path().join(awkward), b"x")?;

        let files = git_changed_files(dir.path())?;
        assert!(
            files.iter().any(|f| f == awkward),
            "expected verbatim {awkward:?} in {files:?}"
        );
        Ok(())
    }

    #[test]
    fn working_tree_dirty_distinguishes_states() -> Result<()> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path())?;

        // Fresh repo with one committed file: clean.
        assert!(!git_working_tree_dirty(dir.path())?);

        // Untracked file → dirty.
        std::fs::write(dir.path().join("new.txt"), b"x")?;
        assert!(git_working_tree_dirty(dir.path())?);
        std::fs::remove_file(dir.path().join("new.txt"))?;
        assert!(!git_working_tree_dirty(dir.path())?);

        // Modified tracked file → dirty.
        std::fs::write(dir.path().join("README.md"), b"changed\n")?;
        assert!(git_working_tree_dirty(dir.path())?);

        Ok(())
    }

    #[test]
    fn line_ranges_modify_in_place() -> Result<()> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path())?;

        // Commit a file with 10 lines, then modify line 5 in the working tree.
        let lines: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.path().join("a.txt"), &lines)?;
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "add a"]);

        let modified: String = (1..=10)
            .map(|i| {
                if i == 5 {
                    "modified\n".into()
                } else {
                    format!("line {i}\n")
                }
            })
            .collect();
        std::fs::write(dir.path().join("a.txt"), &modified)?;

        let head = git_head_sha(dir.path())?;
        let map = git_changed_line_ranges(dir.path(), &head)?;
        let ranges = map.get("a.txt").expect("a.txt should appear");
        assert_eq!(ranges, &vec![LineRange { start: 5, end: 5 }]);
        Ok(())
    }

    #[test]
    fn line_ranges_pure_insertion_is_single_line() -> Result<()> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path())?;

        let lines: String = (1..=5).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.path().join("a.txt"), &lines)?;
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "add a"]);

        // Insert two lines after line 3 (pure insertion: old_count=0).
        let modified = "line 1\nline 2\nline 3\nINSERTED A\nINSERTED B\nline 4\nline 5\n";
        std::fs::write(dir.path().join("a.txt"), modified)?;

        let head = git_head_sha(dir.path())?;
        let map = git_changed_line_ranges(dir.path(), &head)?;
        let ranges = map.get("a.txt").expect("a.txt should appear");
        // OLD-side hunk header: `@@ -3,0 +4,2 @@` → collapse to [3, 3].
        assert_eq!(ranges, &vec![LineRange { start: 3, end: 3 }]);
        Ok(())
    }

    #[test]
    fn line_ranges_tolerate_binary_content_in_diff() -> Result<()> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path())?;

        // Non-UTF-8 bytes but no NUL: git's binary heuristic (NUL in the
        // first 8000 bytes) doesn't fire, so the raw bytes land in the diff
        // as text. Deleting such a committed fixture used to abort the whole
        // run with "git diff stdout was not valid UTF-8".
        let payload: Vec<u8> = (0..40u32)
            .flat_map(|i| {
                let mut line = format!("line {i} ").into_bytes();
                line.extend_from_slice(&[0xFF, 0xFE, 0xFA, b'\n']);
                line
            })
            .collect();
        // Extension-less name so a stray `*.bin binary` gitattributes rule on
        // the host can't flip git's text detection.
        std::fs::write(dir.path().join("binfixture"), &payload)?;
        git(dir.path(), &["add", "binfixture"]);
        git(dir.path(), &["commit", "-q", "-m", "add fixture"]);

        let head = git_head_sha(dir.path())?;
        std::fs::remove_file(dir.path().join("binfixture"))?;

        let map = git_changed_line_ranges(dir.path(), &head)?;
        let ranges = map.get("binfixture").expect("binfixture should appear");
        assert_eq!(ranges, &vec![LineRange { start: 1, end: 40 }]);
        Ok(())
    }

    #[test]
    fn line_ranges_content_masquerading_as_file_header() -> Result<()> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path())?;

        // Line 2 starts with `-- `; deleted, it renders as `--- SQL ...` in
        // the diff. Without hunk-budget tracking the parser mistook it for a
        // file header, resetting `current_file` and silently dropping the
        // file's remaining hunks (the line-9 edit here).
        let lines: String = (1..=10)
            .map(|i| {
                if i == 2 {
                    "-- SQL comment style\n".to_string()
                } else {
                    format!("line {i}\n")
                }
            })
            .collect();
        std::fs::write(dir.path().join("a.sql"), &lines)?;
        git(dir.path(), &["add", "a.sql"]);
        git(dir.path(), &["commit", "-q", "-m", "add a.sql"]);

        let modified: String = (1..=10)
            .map(|i| {
                if i == 2 || i == 9 {
                    format!("changed {i}\n")
                } else {
                    format!("line {i}\n")
                }
            })
            .collect();
        std::fs::write(dir.path().join("a.sql"), &modified)?;

        let head = git_head_sha(dir.path())?;
        let map = git_changed_line_ranges(dir.path(), &head)?;
        let ranges = map.get("a.sql").expect("a.sql should appear");
        assert_eq!(
            ranges,
            &vec![
                LineRange { start: 2, end: 2 },
                LineRange { start: 9, end: 9 }
            ]
        );
        Ok(())
    }

    #[test]
    fn line_ranges_non_ascii_path() -> Result<()> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path())?;

        // Under git's default `core.quotePath=true` a non-ASCII path is
        // octal-escaped into a C-quoted string the parser can't read; the
        // diff invocation disables that. Em-dash rather than an accented
        // letter so macOS's NFD normalization can't change the bytes.
        let awkward = "a b — weird-name.txt";
        let lines: String = (1..=5).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.path().join(awkward), &lines)?;
        git(dir.path(), &["add", awkward]);
        git(dir.path(), &["commit", "-q", "-m", "add awkward"]);

        let modified = lines.replace("line 3", "changed 3");
        std::fs::write(dir.path().join(awkward), &modified)?;

        let head = git_head_sha(dir.path())?;
        let map = git_changed_line_ranges(dir.path(), &head)?;
        let ranges = map.get(awkward).expect("awkward path should appear");
        assert_eq!(ranges, &vec![LineRange { start: 3, end: 3 }]);
        Ok(())
    }

    #[test]
    fn parse_unified_diff_tolerates_non_utf8_bytes() -> Result<()> {
        let mut diff = Vec::new();
        diff.extend_from_slice(b"--- a/data.bin\n");
        diff.extend_from_slice(b"+++ b/data.bin\n");
        // Non-UTF-8 function-name context after the closing `@@`.
        diff.extend_from_slice(b"@@ -3,2 +3,2 @@ \xff\xfe\n");
        diff.extend_from_slice(b"-old \xf0\x28\x8c\x28 payload\n");
        diff.extend_from_slice(b"-old second line\n");
        diff.extend_from_slice(b"+new \xff payload\n");
        diff.extend_from_slice(b"+new second line\n");
        // A path that isn't valid UTF-8 (possible on Linux with
        // `core.quotePath=false`): skipped, with its hunk content consumed.
        diff.extend_from_slice(b"--- a/b\xffad\n");
        diff.extend_from_slice(b"+++ b/b\xffad\n");
        diff.extend_from_slice(b"@@ -1,1 +1,1 @@\n");
        diff.extend_from_slice(b"-x\n");
        diff.extend_from_slice(b"+y\n");

        let map = parse_unified_diff(&diff)?;
        assert_eq!(
            map.get("data.bin"),
            Some(&vec![LineRange { start: 3, end: 4 }])
        );
        assert_eq!(map.len(), 1, "undecodable path should be skipped");
        Ok(())
    }

    #[test]
    fn parse_unified_diff_rejects_corrupt_input() {
        // A malformed hunk header has no counts to budget with, so the
        // parser can't safely skip past its content.
        let err = parse_unified_diff(b"--- a/x\n+++ b/x\n@@ garbage @@\n-x\n+y\n").unwrap_err();
        assert!(err.to_string().contains("malformed hunk header"), "{err}");

        // A hunk content line without a -/+/\ prefix means the budgets are
        // out of sync with the stream.
        let err = parse_unified_diff(b"--- a/x\n+++ b/x\n@@ -1,2 +1,0 @@\n-x\nzz\n").unwrap_err();
        assert!(err.to_string().contains("unexpected line"), "{err}");
    }

    #[test]
    fn parse_hunk_header_variants() {
        let h = parse_hunk_header("@@ -10,3 +20,1 @@").unwrap();
        assert_eq!(h.old_range, LineRange { start: 10, end: 12 });
        assert_eq!((h.old_count, h.new_count), (3, 1));

        // No comma → count of 1.
        let h = parse_hunk_header("@@ -7 +7 @@ fn foo()").unwrap();
        assert_eq!(h.old_range, LineRange { start: 7, end: 7 });
        assert_eq!((h.old_count, h.new_count), (1, 1));

        // Pure insertion → single-line range, zero old-side budget.
        let h = parse_hunk_header("@@ -5,0 +6,2 @@").unwrap();
        assert_eq!(h.old_range, LineRange { start: 5, end: 5 });
        assert_eq!((h.old_count, h.new_count), (0, 2));
    }

    /// Run `git <args>` in `dir`, asserting success. Used by tests that
    /// need to drive a repo through several states.
    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|e| panic!("git {} failed to spawn: {e}", args.join(" ")));
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    #[test]
    fn relation_to_head_equal_when_unchanged() -> Result<()> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path())?;
        let head = git_head_sha(dir.path())?;
        assert_eq!(relation_to_head(dir.path(), &head)?, ShaRelation::Equal);
        Ok(())
    }

    #[test]
    fn relation_to_head_ancestor_counts_commits_ahead() -> Result<()> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path())?;
        let collect_sha = git_head_sha(dir.path())?;

        for i in 1..=3 {
            let name = format!("f{i}.txt");
            std::fs::write(dir.path().join(&name), b"x")?;
            git(dir.path(), &["add", &name]);
            git(dir.path(), &["commit", "-q", "-m", &format!("c{i}")]);
        }

        assert_eq!(
            relation_to_head(dir.path(), &collect_sha)?,
            ShaRelation::Reachable { commits_ahead: 3 }
        );
        Ok(())
    }

    #[test]
    fn relation_to_head_reachable_after_reset() -> Result<()> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path())?;
        let init_sha = git_head_sha(dir.path())?;

        // Create a commit, capture its sha, then reset HEAD back. The captured
        // sha is now a sibling of HEAD's history — present in the repo but
        // not an ancestor. `git diff <sha> HEAD` still resolves both trees,
        // so the cache is usable; classify as Reachable.
        std::fs::write(dir.path().join("a.txt"), b"x")?;
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "B"]);
        let b_sha = git_head_sha(dir.path())?;
        git(dir.path(), &["reset", "--hard", "-q", &init_sha]);

        let rel = relation_to_head(dir.path(), &b_sha)?;
        assert!(
            matches!(rel, ShaRelation::Reachable { .. }),
            "expected Reachable, got {rel:?}"
        );
        Ok(())
    }

    #[test]
    fn relation_to_head_missing_when_sha_absent() -> Result<()> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path())?;
        // Sha that doesn't exist in the repo at all — only this case is
        // Missing; the diff has no anchor to resolve.
        assert_eq!(
            relation_to_head(dir.path(), "deadbeef00000000000000000000000000000000")?,
            ShaRelation::Missing
        );
        Ok(())
    }

    #[test]
    fn bad_sha_errors_loudly() -> Result<()> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path())?;
        let err = git_changed_line_ranges(dir.path(), "deadbeef0000000000000000000000000000")
            .expect_err("bad sha must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("git diff"),
            "error should name the failing command: {msg}"
        );
        Ok(())
    }

    #[test]
    fn binary_id_matches_nextest_format() {
        // Lib and proc-macro: bare package name.
        assert_eq!(
            build_binary_id("foo-lib", TargetKind::Lib, "foo_lib"),
            "foo-lib"
        );
        assert_eq!(
            build_binary_id("foo-derive", TargetKind::ProcMacro, "derive"),
            "foo-derive"
        );
        // Integration test: package::target.
        assert_eq!(
            build_binary_id("foo-lib", TargetKind::Test, "foo_test"),
            "foo-lib::foo_test"
        );
        // Bin: package::bin/target.
        assert_eq!(
            build_binary_id("foo-lib", TargetKind::Bin, "foo_bin"),
            "foo-lib::bin/foo_bin"
        );
    }

    #[test]
    fn transitive_closure_walks_dep_graph() {
        // a -> b -> c, a -> d, e isolated
        let mut deps: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        deps.insert("a", ["b", "d"].into_iter().collect());
        deps.insert("b", ["c"].into_iter().collect());
        deps.insert("c", BTreeSet::new());
        deps.insert("d", BTreeSet::new());
        deps.insert("e", BTreeSet::new());

        assert_eq!(
            transitive_closure("a", &deps),
            ["b", "c", "d"].into_iter().collect()
        );
        assert_eq!(transitive_closure("b", &deps), ["c"].into_iter().collect());
        assert_eq!(transitive_closure("c", &deps), BTreeSet::new());
        assert_eq!(transitive_closure("e", &deps), BTreeSet::new());
        // Unknown package: no deps known, empty closure.
        assert_eq!(transitive_closure("zzz", &deps), BTreeSet::new());
    }

    #[test]
    fn transitive_closure_handles_cycles() {
        // a -> b -> a (a hypothetical cycle the metadata layer might surface).
        let mut deps: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        deps.insert("a", ["b"].into_iter().collect());
        deps.insert("b", ["a"].into_iter().collect());

        // Walk must terminate; closure excludes the start node.
        assert_eq!(transitive_closure("a", &deps), ["b"].into_iter().collect());
    }

    /// Synthesize a `cargo metadata --no-deps` JSON shape inside a tempdir
    /// and check that `crate_root_sentinels_by_binary_id` returns the
    /// per-target sentinel sets we expect: own crate root, package's lib for
    /// non-lib targets, and lib roots of transitively-depended workspace
    /// packages. Covers both the per-target (within-package) and
    /// cross-package (transitive dep) layers.
    #[test]
    fn sentinels_per_target_with_path_dep_chain() -> Result<()> {
        let dir = tempfile::tempdir()?;
        // Use the same canonicalize the production path uses: bare
        // canonicalize on Windows returns a `\\?\` verbatim path, which the
        // production `crate_root_sentinels_by_binary_id` strips before
        // calling strip_prefix — so the synthesized metadata src_paths must
        // already be in the stripped form, otherwise strip_prefix sees one
        // verbatim path and one plain path and silently rejects every target.
        let root = canonicalize_no_verbatim(dir.path())?;
        let math_lib = root.join("math/src/lib.rs");
        let math_int = root.join("math/tests/integration.rs");
        let strings_lib = root.join("strings/src/lib.rs");
        let utils_lib = root.join("utils/src/lib.rs");
        for f in [&math_lib, &math_int, &strings_lib, &utils_lib] {
            std::fs::create_dir_all(f.parent().unwrap())?;
            std::fs::write(f, b"")?;
        }

        // math depends on strings; strings depends on utils. So math
        // transitively depends on utils, and editing utils/src/lib.rs must
        // pull in math's tests too.
        let metadata = serde_json::json!({
            "workspace_root": root.to_string_lossy(),
            "packages": [
                {
                    "name": "math",
                    "manifest_path": root.join("math/Cargo.toml").to_string_lossy(),
                    "targets": [
                        {"name": "math", "kind": ["lib"], "test": true,
                         "src_path": math_lib.to_string_lossy()},
                        {"name": "integration", "kind": ["test"], "test": true,
                         "src_path": math_int.to_string_lossy()},
                    ],
                    "dependencies": [
                        {"name": "strings", "kind": null, "source": null},
                        // Build-dep: must NOT propagate.
                        {"name": "utils", "kind": "build", "source": null},
                    ],
                },
                {
                    "name": "strings",
                    "manifest_path": root.join("strings/Cargo.toml").to_string_lossy(),
                    "targets": [
                        {"name": "strings", "kind": ["lib"], "test": true,
                         "src_path": strings_lib.to_string_lossy()},
                    ],
                    "dependencies": [
                        {"name": "utils", "kind": null, "source": null},
                    ],
                },
                {
                    "name": "utils",
                    "manifest_path": root.join("utils/Cargo.toml").to_string_lossy(),
                    "targets": [
                        {"name": "utils", "kind": ["lib"], "test": true,
                         "src_path": utils_lib.to_string_lossy()},
                    ],
                    "dependencies": [],
                },
            ],
        });

        let project = ProjectRoot {
            workspace_root: root.clone(),
            manifest_paths: vec![],
            metadata,
        };
        let map = project.crate_root_sentinels_by_binary_id()?;

        let p = |s: &str| Utf8PathBuf::from(s);

        // math (lib unit tests): own lib + strings/lib (direct dep) +
        // utils/lib (transitive). NOT its own integration target.
        // utils is reached via the strings dep, NOT the math→utils
        // build-dep (which is excluded).
        assert_eq!(
            map.get("math").unwrap(),
            &[
                p("math/src/lib.rs"),
                p("strings/src/lib.rs"),
                p("utils/src/lib.rs")
            ]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        );

        // math::integration: own crate root + math's lib + transitive deps.
        assert_eq!(
            map.get("math::integration").unwrap(),
            &[
                p("math/src/lib.rs"),
                p("math/tests/integration.rs"),
                p("strings/src/lib.rs"),
                p("utils/src/lib.rs"),
            ]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        );

        // strings: own lib + utils (direct dep). Does not include math.
        assert_eq!(
            map.get("strings").unwrap(),
            &[p("strings/src/lib.rs"), p("utils/src/lib.rs")]
                .into_iter()
                .collect::<BTreeSet<_>>(),
        );

        // utils: just its own lib.
        assert_eq!(
            map.get("utils").unwrap(),
            &[p("utils/src/lib.rs")].into_iter().collect::<BTreeSet<_>>(),
        );

        Ok(())
    }

    /// `[lib] test = false` makes cargo report `test: false` for the lib
    /// target. The lib then has no tests of its own — but the package's
    /// integration tests, and every workspace package that depends on it,
    /// still compile against it, so its crate root must remain a sentinel for
    /// those binaries.
    ///
    /// `parse_target` used to bail on `test: false`, which dropped the lib
    /// before `lib_src` ever saw it: `math::integration` lost
    /// `math/src/lib.rs` and `strings/src/lib.rs`, so a structural edit to
    /// either (a new `mod`, a changed `use`) selected no test at all — silent
    /// under-selection, with nothing downstream to catch it.
    #[test]
    fn lib_without_test_harness_still_seeds_sentinels() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = canonicalize_no_verbatim(dir.path())?;
        let math_lib = root.join("math/src/lib.rs");
        let math_int = root.join("math/tests/integration.rs");
        let strings_lib = root.join("strings/src/lib.rs");
        for f in [&math_lib, &math_int, &strings_lib] {
            std::fs::create_dir_all(f.parent().unwrap())?;
            std::fs::write(f, b"")?;
        }

        // Both libs opt out of their own test harness; only the integration
        // target carries tests.
        let metadata = serde_json::json!({
            "workspace_root": root.to_string_lossy(),
            "packages": [
                {
                    "name": "math",
                    "manifest_path": root.join("math/Cargo.toml").to_string_lossy(),
                    "targets": [
                        {"name": "math", "kind": ["lib"], "test": false,
                         "src_path": math_lib.to_string_lossy()},
                        {"name": "integration", "kind": ["test"], "test": true,
                         "src_path": math_int.to_string_lossy()},
                    ],
                    "dependencies": [
                        {"name": "strings", "kind": null, "source": null},
                    ],
                },
                {
                    "name": "strings",
                    "manifest_path": root.join("strings/Cargo.toml").to_string_lossy(),
                    "targets": [
                        {"name": "strings", "kind": ["lib"], "test": false,
                         "src_path": strings_lib.to_string_lossy()},
                    ],
                    "dependencies": [],
                },
            ],
        });

        let project = ProjectRoot {
            workspace_root: root.clone(),
            manifest_paths: vec![],
            metadata,
        };
        let map = project.crate_root_sentinels_by_binary_id()?;

        let p = |s: &str| Utf8PathBuf::from(s);

        // The harness-less libs produce no binary_id of their own...
        assert!(
            !map.contains_key("math"),
            "lib with test = false has no test binary; got {map:?}"
        );
        assert!(!map.contains_key("strings"), "same for the dep's lib");

        // ...but both crate roots still seed the integration target.
        assert_eq!(
            map.get("math::integration").unwrap(),
            &[
                p("math/src/lib.rs"),
                p("math/tests/integration.rs"),
                p("strings/src/lib.rs"),
            ]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        );

        Ok(())
    }
}
