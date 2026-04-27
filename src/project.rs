//! Shared project utilities: root detection and git queries.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use camino::Utf8PathBuf;

/// Inclusive line range `[start, end]` of a changed hunk in some file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LineRange {
    pub start: i64,
    pub end: i64,
}

/// How a stored sha relates to the current `HEAD`.
///
/// `Equal` — same commit, no drift.
/// `Ancestor` — committed `commits_ahead` commits since collect; stored line
/// numbers still align (collect_sha is in current history) but the diff
/// against working tree includes those committed changes, so selection is
/// noisier than necessary.
/// `Diverged` — collect_sha is not reachable from HEAD (rebased, branch
/// switched, garbage-collected, beyond shallow boundary). Stored line numbers
/// no longer share a coordinate system with the working tree; the cache is
/// unsafe to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShaRelation {
    Equal,
    Ancestor { commits_ahead: u32 },
    Diverged,
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
    /// Crate roots of all test-producing workspace targets, grouped by package
    /// name. Paths are relative to the workspace root. Used by `collect` to
    /// attach a sentinel `(1, i64::MAX)` range to every test for *its own
    /// package*'s crate roots — a structural edit (`mod foo;`, `use ...;`) in
    /// any of those files must pull in every test in that package, but must
    /// not leak to other packages whose tests can't observe the change.
    ///
    /// The map key is the package name from `metadata.packages[].name`, which
    /// matches the prefix of nextest's `binary_id` (everything before the
    /// first `::`). See `nextest_metadata::RustBinaryId::from_parts`.
    ///
    /// Reads from the cached `metadata` JSON — no cargo spawn.
    pub fn test_src_paths_by_package(&self) -> Result<BTreeMap<String, BTreeSet<Utf8PathBuf>>> {
        let root = self
            .workspace_root
            .canonicalize()
            .context("failed to canonicalize project root")?;

        let mut by_package: BTreeMap<String, BTreeSet<Utf8PathBuf>> = BTreeMap::new();
        let Some(packages) = self.metadata.get("packages").and_then(|v| v.as_array()) else {
            return Ok(by_package);
        };
        for pkg in packages {
            let Some(name) = pkg.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
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
                        by_package.entry(name.to_string()).or_default().insert(u);
                    }
                }
            }
        }
        Ok(by_package)
    }
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
/// Uses `git merge-base --is-ancestor` (exit 0 = ancestor, 1 = not ancestor;
/// any non-zero exit folds into `Diverged` because the user-visible cure is
/// the same: recollect — whether the sha was rebased away, garbage-collected,
/// or simply beyond a shallow clone boundary).
pub fn relation_to_head(project_root: &Path, sha: &str) -> Result<ShaRelation> {
    let head = git_head_sha(project_root)?;
    if head == sha {
        return Ok(ShaRelation::Equal);
    }
    let status = Command::new("git")
        .args(["merge-base", "--is-ancestor", sha, "HEAD"])
        .current_dir(project_root)
        .output()
        .with_context(|| format!("failed to run git merge-base --is-ancestor {sha} HEAD"))?;
    if !status.status.success() {
        return Ok(ShaRelation::Diverged);
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
    Ok(ShaRelation::Ancestor { commits_ahead: count })
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
        let awkward = "a b — \"weird\".txt";
        std::fs::write(dir.path().join(awkward), b"x")?;

        let files = git_changed_files(dir.path())?;
        assert!(
            files.iter().any(|f| f == awkward),
            "expected verbatim {awkward:?} in {files:?}"
        );
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
            ShaRelation::Ancestor { commits_ahead: 3 }
        );
        Ok(())
    }

    #[test]
    fn relation_to_head_diverged_after_reset() -> Result<()> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path())?;
        let init_sha = git_head_sha(dir.path())?;

        // Create a commit, capture its sha, then reset HEAD back. The captured
        // sha is now a sibling of HEAD's history — present in the repo but
        // not an ancestor.
        std::fs::write(dir.path().join("a.txt"), b"x")?;
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "B"]);
        let b_sha = git_head_sha(dir.path())?;
        git(dir.path(), &["reset", "--hard", "-q", &init_sha]);

        assert_eq!(
            relation_to_head(dir.path(), &b_sha)?,
            ShaRelation::Diverged
        );
        Ok(())
    }

    #[test]
    fn relation_to_head_diverged_when_sha_missing() -> Result<()> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path())?;
        // Sha that doesn't exist in the repo at all — folds into Diverged.
        assert_eq!(
            relation_to_head(dir.path(), "deadbeef00000000000000000000000000000000")?,
            ShaRelation::Diverged
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
}
