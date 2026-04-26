//! Shared project utilities: root detection and git queries.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Project root information.
pub struct ProjectRoot {
    /// Workspace root directory. Git operations and the DB live here.
    /// For single-crate projects this equals the crate root.
    pub workspace_root: PathBuf,
    /// All Cargo.toml files belonging to the workspace — the root manifest
    /// plus every member's manifest. Sorted, deduplicated. Used for
    /// environment fingerprinting.
    pub manifest_paths: Vec<PathBuf>,
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
    })
}

/// Get the list of changed files from git.
///
/// When `diff_base` is `None`, returns staged + unstaged + untracked changes
/// (working tree mode). When `diff_base` is `Some(ref)`, returns files changed
/// between the merge-base of `ref` and HEAD (three-dot diff).
///
/// Returns paths relative to the project root. A non-zero git exit (corrupt
/// repo, unknown ref, missing object, permissions) is a hard error: silently
/// returning "no changed files" would look like a clean tree and select zero
/// tests.
pub fn git_changed_files(project_root: &Path, diff_base: Option<&str>) -> Result<Vec<String>> {
    let mut files = Vec::new();

    if let Some(base) = diff_base {
        let range = format!("{base}...HEAD");
        let args = vec![
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--name-only",
            "-z",
            range.as_str(),
        ];
        for path in run_git(project_root, &args)? {
            files.push(path);
        }
    } else {
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
    }

    // Filter out stray profraw files (from instrumented subprocesses that
    // didn't inherit our LLVM_PROFILE_FILE). The DB lives under target/ so
    // git already ignores it.
    files.retain(|f| !f.ends_with(".profraw"));

    files.sort();
    files.dedup();
    Ok(files)
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

        let files = git_changed_files(dir.path(), None)?;
        assert!(
            files.iter().any(|f| f == "new.txt"),
            "expected new.txt in {files:?}"
        );
        Ok(())
    }

    /// Paths with spaces or non-ASCII characters would be C-style quoted by
    /// `git diff --name-only` without `-z` (e.g. `"a b.txt"`); `-z` plus
    /// NUL-splitting returns them verbatim.
    #[test]
    fn awkward_filename_round_trips() -> Result<()> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path())?;
        let awkward = "a b — \"weird\".txt";
        std::fs::write(dir.path().join(awkward), b"x")?;

        let files = git_changed_files(dir.path(), None)?;
        assert!(
            files.iter().any(|f| f == awkward),
            "expected verbatim {awkward:?} in {files:?}"
        );
        Ok(())
    }

    #[test]
    fn bad_diff_base_errors_loudly() -> Result<()> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path())?;

        let err = git_changed_files(dir.path(), Some("nonexistent-ref-xyz"))
            .expect_err("bad ref must error, not silently return empty");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("git diff"),
            "error should name the failing command: {msg}"
        );
        assert!(
            msg.contains("nonexistent-ref-xyz"),
            "error should propagate git stderr (which mentions the bad ref): {msg}"
        );
        Ok(())
    }
}
