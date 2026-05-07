//! Shared project utilities: root detection and git queries.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use camino::Utf8PathBuf;

use crate::coverage::to_db_relative;

/// Inclusive line range `[start, end]` of a changed hunk in some file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LineRange {
    pub start: i64,
    pub end: i64,
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
pub enum ShaRelation {
    Equal,
    Reachable { commits_ahead: u32 },
    Missing,
}

/// Project root information.
pub struct ProjectRoot {
    /// Workspace root directory. Git operations and the DB live here.
    /// For single-crate projects this equals the crate root.
    pub workspace_root: PathBuf,
    /// All Cargo.toml files belonging to the workspace — the root manifest
    /// plus every member's manifest. Sorted, deduplicated. Used for
    /// environment fingerprinting.
    pub manifest_paths: Vec<PathBuf>,
    /// Raw `cargo metadata --no-deps` JSON. Parsed once at root detection so
    /// less-common lookups (test src paths) don't have to re-spawn cargo.
    pub metadata: serde_json::Value,
}

/// `Path::canonicalize` that strips Windows verbatim/UNC prefixes so the
/// result lines up with the path strings cargo metadata, llvm-cov, and git
/// emit (none of which use the `\\?\` form). On Windows, `canonicalize`
/// always returns a `\\?\C:\…` (or `\\?\UNC\server\share\…`) path, which
/// would break every `Path::strip_prefix` we rely on for relative paths.
/// On unix this is a plain canonicalize.
///
/// Mirrors the small subset of the `dunce` crate's logic we need; inlining
/// it avoids the extra dependency for what amounts to a one-time prefix
/// strip.
pub fn canonicalize_no_verbatim(path: &Path) -> Result<PathBuf> {
    let canon = path
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", path.display()))?;
    #[cfg(windows)]
    {
        let s = canon.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return Ok(PathBuf::from(format!(r"\\{rest}")));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return Ok(PathBuf::from(rest.to_string()));
        }
    }
    Ok(canon)
}

/// Find the project root via `cargo metadata`.
///
/// Uses `cargo metadata --no-deps --format-version=1` to reliably determine
/// the workspace root, which handles both single-crate and workspace projects.
pub fn find_project_root() -> Result<ProjectRoot> {
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
    pub fn crate_root_sentinels_by_binary_id(
        &self,
    ) -> Result<BTreeMap<String, BTreeSet<Utf8PathBuf>>> {
        let root = canonicalize_no_verbatim(&self.workspace_root)?;

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
                    if let Some(parsed) = parse_target(target, &root) {
                        // Lib src_path is recorded separately so non-lib
                        // targets can pick it up via the "links to own lib"
                        // rule even if the lib isn't itself a test target.
                        if matches!(parsed.kind, TargetKind::Lib) {
                            lib_src.insert(name, parsed.src_path.clone());
                        }
                        if parsed.is_test_runnable() {
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
}

impl TestTarget {
    /// Whether this target produces a binary nextest would actually run.
    /// Lib/proc-macro/bin/test all qualify; bench/example don't.
    fn is_test_runnable(&self) -> bool {
        matches!(
            self.kind,
            TargetKind::Lib | TargetKind::ProcMacro | TargetKind::Bin | TargetKind::Test
        )
    }
}

fn parse_target(target: &serde_json::Value, root: &Path) -> Option<TestTarget> {
    let is_test = target.get("test").and_then(|v| v.as_bool()).unwrap_or(false);
    if !is_test {
        return None;
    }
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
    Some(TestTarget { name, kind, src_path })
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
pub fn git_changed_files(project_root: &Path) -> Result<Vec<String>> {
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
pub fn git_working_tree_dirty(project_root: &Path) -> Result<bool> {
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
pub fn git_head_sha(project_root: &Path) -> Result<String> {
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
pub fn relation_to_head(project_root: &Path, sha: &str) -> Result<ShaRelation> {
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
    Ok(ShaRelation::Reachable { commits_ahead: count })
}

/// Per-file changed line ranges between `collect_sha` and the working tree.
///
/// Runs `git diff -U0 --no-color --no-ext-diff <collect_sha>` and parses
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
pub fn git_changed_line_ranges(
    project_root: &Path,
    collect_sha: &str,
) -> Result<BTreeMap<String, Vec<LineRange>>> {
    // `--src-prefix=a/ --dst-prefix=b/` forces the standard prefixes — without
    // them, `git diff <commit>` against the working tree uses `c/` and `w/`
    // and our parser would skip every `--- ` line.
    let output = Command::new("git")
        .args([
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

    let stdout = std::str::from_utf8(&output.stdout)
        .context("git diff stdout was not valid UTF-8")?;
    parse_unified_diff(stdout)
}

/// Files added between `collect_sha` and the working tree (status `A`).
/// `git_changed_line_ranges` skips these (no OLD-side hunks), and
/// `git_changed_files` only sees uncommitted changes — so for committed
/// additions, the diagnostic report needs this dedicated query to know
/// the file existed in the diff at all. Returns paths relative to the
/// project root.
pub fn git_added_files_since(
    project_root: &Path,
    collect_sha: &str,
) -> Result<Vec<String>> {
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

fn parse_unified_diff(diff: &str) -> Result<BTreeMap<String, Vec<LineRange>>> {
    let mut map: BTreeMap<String, Vec<LineRange>> = BTreeMap::new();
    let mut current_file: Option<String> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("--- ") {
            // `--- a/path/to/file` or `--- /dev/null` for new files. We don't
            // emit ranges for /dev/null (the new file's lines all live on the
            // NEW side, with no OLD-side coordinates).
            current_file = parse_diff_path(rest);
        } else if line.starts_with("@@ ") {
            let Some(file) = current_file.clone() else { continue };
            // /dev/null sentinel — skip.
            if file == "/dev/null" {
                continue;
            }
            let Some(range) = parse_hunk_header(line) else {
                continue;
            };
            map.entry(file).or_default().push(range);
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

/// Parse `@@ -OLD_START[,OLD_COUNT] +NEW_START[,NEW_COUNT] @@ ...` and return
/// the OLD-side inclusive line range. For pure insertions (`OLD_COUNT == 0`),
/// returns `[OLD_START, OLD_START]` — the line before which content was
/// inserted, so functions containing that line are still picked up.
fn parse_hunk_header(line: &str) -> Option<LineRange> {
    // Stripping leading "@@ " and finding the next " @@" boundary keeps
    // surrounding context (function name on inline-context lines) out of
    // the parse.
    let inner = line.strip_prefix("@@ ")?;
    let end_idx = inner.find(" @@")?;
    let body = &inner[..end_idx];
    // body looks like: "-OLD +NEW" — split on space.
    let mut parts = body.split_whitespace();
    let old = parts.next()?;
    let _new = parts.next()?;
    let old = old.strip_prefix('-')?;
    let (start, count) = match old.split_once(',') {
        Some((s, c)) => (s.parse::<i64>().ok()?, c.parse::<i64>().ok()?),
        None => (old.parse::<i64>().ok()?, 1),
    };
    if count == 0 {
        // Pure insertion: line `start` is the line before the insert. Use it
        // as a single-line range so functions containing line `start` are
        // selected — slight over-select at file edges, acceptable.
        Some(LineRange {
            start,
            end: start,
        })
    } else {
        Some(LineRange {
            start,
            end: start + count - 1,
        })
    }
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
        Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(dir.path())
            .output()?;
        Command::new("git")
            .args(["commit", "-q", "-m", "add a"])
            .current_dir(dir.path())
            .output()?;

        let modified: String = (1..=10)
            .map(|i| if i == 5 { "modified\n".into() } else { format!("line {i}\n") })
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
        Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(dir.path())
            .output()?;
        Command::new("git")
            .args(["commit", "-q", "-m", "add a"])
            .current_dir(dir.path())
            .output()?;

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
    fn parse_hunk_header_variants() {
        assert_eq!(
            parse_hunk_header("@@ -10,3 +20,1 @@"),
            Some(LineRange { start: 10, end: 12 })
        );
        // No comma → count of 1.
        assert_eq!(
            parse_hunk_header("@@ -7 +7 @@ fn foo()"),
            Some(LineRange { start: 7, end: 7 })
        );
        // Pure insertion → single-line.
        assert_eq!(
            parse_hunk_header("@@ -5,0 +6,2 @@"),
            Some(LineRange { start: 5, end: 5 })
        );
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
        let root = dir.path().canonicalize()?;
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
            &[p("math/src/lib.rs"), p("strings/src/lib.rs"), p("utils/src/lib.rs")]
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
}
