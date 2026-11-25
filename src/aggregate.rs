//! Bounded, process-safe aggregation of the same LLVM profiles used by the
//! per-test collector. A batch becomes an indexed profile; equal-sized batches
//! are compacted like a binary counter. Each input is merged O(log n) times,
//! and only one batch of text plus O(log n) indexed profiles is retained.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

const BATCH_SIZE: usize = 32;
const PENDING_PREFIX: &str = "pending-";
const DIRTY: &str = "incomplete";

pub(crate) fn directory(results_dir: &Path) -> PathBuf {
    results_dir.join("aggregate")
}

fn lock(dir: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("lock"))?;
    file.lock()?;
    if dir.join(DIRTY).exists() {
        bail!("an interrupted aggregate profile transaction cannot be reused");
    }
    Ok(file)
}

fn merge(tool: &Path, inputs: &[PathBuf], output: &Path) -> Result<()> {
    if inputs.is_empty() {
        bail!("cannot merge an empty aggregate profile");
    }
    let result = Command::new(tool)
        .args(["merge", "--sparse", "--failure-mode=any", "-o"])
        .arg(output)
        .args(inputs)
        .output()
        .context("running llvm-profdata merge for aggregate coverage")?;
    if !result.status.success() {
        bail!(
            "aggregate llvm-profdata merge failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    Ok(())
}

fn pending(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(PENDING_PREFIX)
        {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

/// Merge one completed test's already-merged text profile. The lock also
/// guards compaction; a crash during mutation poisons this collect rather
/// than risking an apparently successful report with double-counted inputs.
pub(crate) fn add(dir: &Path, tool: &Path, profile: &Path) -> Result<()> {
    let _lock = lock(dir)?;
    let dirty = dir.join(DIRTY);
    File::create(&dirty)?;
    let temp = tempfile::Builder::new()
        .prefix(PENDING_PREFIX)
        .suffix(".profile")
        .tempfile_in(dir)?
        .into_temp_path();
    // Both source and queue live in target/affected. Rename rather than
    // copying potentially large textual profiles while holding the lock.
    fs::rename(profile, &temp)?;
    temp.keep().context("retaining aggregate input")?;
    let batch = pending(dir)?;
    if batch.len() >= BATCH_SIZE {
        let mut inputs = batch;
        for level in 0.. {
            let slot = dir.join(format!("level-{level}.profdata"));
            if slot.exists() {
                inputs.push(slot);
                continue;
            }
            // All occupied lower levels plus this batch have the same total
            // weight as this free level; merge them in one invocation.
            let tmp = tempfile::NamedTempFile::new_in(dir)?.into_temp_path();
            merge(tool, &inputs, &tmp)?;
            tmp.persist(&slot).context("publishing aggregate level")?;
            for input in inputs {
                fs::remove_file(input)?;
            }
            break;
        }
    }
    fs::remove_file(dirty)?;
    Ok(())
}

/// Publish atomically, after the successful full test run. An incomplete
/// transaction is never mistaken for a valid coverage report.
pub(crate) fn finish(dir: &Path, tool: &Path, output: &Path) -> Result<()> {
    let _lock = lock(dir)?;
    let mut inputs = pending(dir)?;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with("level-") {
            inputs.push(entry.path());
        }
    }
    inputs.sort();
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = tempfile::NamedTempFile::new_in(parent)?.into_temp_path();
    merge(tool, &inputs, &tmp)?;
    tmp.persist(output)
        .context("publishing aggregate profile")?;
    Ok(())
}
