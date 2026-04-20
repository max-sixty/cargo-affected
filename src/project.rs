//! Shared project utilities: root detection and git queries.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Project root information.
pub struct ProjectRoot {
    /// Workspace root directory. Git operations and the DB live here.
    /// For single-crate projects this equals the crate root.
    pub workspace_root: PathBuf,
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
    Ok(ProjectRoot {
        workspace_root: PathBuf::from(workspace_root),
    })
}

/// Get the list of changed files from git.
///
/// When `diff_base` is `None`, returns staged + unstaged + untracked changes
/// (working tree mode). When `diff_base` is `Some(ref)`, returns files changed
/// between the merge-base of `ref` and HEAD (three-dot diff).
///
/// Returns paths relative to the project root.
pub fn git_changed_files(project_root: &Path, diff_base: Option<&str>) -> Result<Vec<String>> {
    let mut files = Vec::new();

    if let Some(base) = diff_base {
        let range = format!("{base}...HEAD");
        let args = vec!["diff", "--name-only", range.as_str()];
        let output = Command::new("git")
            .args(&args)
            .current_dir(project_root)
            .output()
            .with_context(|| format!("failed to run git {}", args.join(" ")))?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if !line.is_empty() {
                    files.push(line.to_string());
                }
            }
        }
    } else {
        for args in [
            vec!["diff", "--name-only"],
            vec!["diff", "--name-only", "--cached"],
            vec!["ls-files", "--others", "--exclude-standard"],
        ] {
            let output = Command::new("git")
                .args(&args)
                .current_dir(project_root)
                .output()
                .with_context(|| format!("failed to run git {}", args.join(" ")))?;

            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    if !line.is_empty() && !files.contains(&line.to_string()) {
                        files.push(line.to_string());
                    }
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
