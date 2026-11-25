//! Process-safe aggregation of the same LLVM profiles used by the per-test
//! collector. Each batch becomes an indexed profile; equal-sized batches
//! compact like a binary counter. A short filesystem lock protects queue and
//! level ownership, never an LLVM merge. Concurrent shims can keep producing
//! profiles while a different shim compacts its claimed batch.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

const BATCH_SIZE: usize = 32;
const PENDING_PREFIX: &str = "pending-";
const CLAIM_PREFIX: &str = "batch-";
const LEVEL_PREFIX: &str = "level-";

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

fn entries(dir: &Path, prefix: &str) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(prefix) {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

fn claim_file(path: &Path, claim: &Path) -> Result<PathBuf> {
    let name = path.file_name().context("profile has no file name")?;
    let destination = claim.join(name);
    fs::rename(path, &destination)?;
    Ok(destination)
}

/// Claim a complete text batch under the queue lock. A persistent claim
/// directory is also a crash marker: finish refuses a report if any claim
/// did not complete, even if the shim was terminated mid-merge.
fn enqueue(dir: &Path, profile: &Path) -> Result<Option<(PathBuf, Vec<PathBuf>)>> {
    let _lock = lock(dir)?;
    let temp = tempfile::Builder::new()
        .prefix(PENDING_PREFIX)
        .suffix(".profile")
        .tempfile_in(dir)?
        .into_temp_path();
    fs::rename(profile, &temp)?;
    temp.keep().context("retaining aggregate input")?;

    let mut pending = entries(dir, PENDING_PREFIX)?;
    if pending.len() < BATCH_SIZE {
        return Ok(None);
    }
    pending.truncate(BATCH_SIZE);
    let claim = tempfile::Builder::new()
        .prefix(CLAIM_PREFIX)
        .tempdir_in(dir)?
        .keep();
    let inputs = pending
        .iter()
        .map(|path| claim_file(path, &claim))
        .collect::<Result<Vec<_>>>()?;
    Ok(Some((claim, inputs)))
}

/// Merge a claimed batch outside the shared lock. Each occupied level
/// represents 2^level batches. Atomically take ownership of any adjacent
/// occupied levels; after merging, retry publication if another compactor
/// filled the destination. Nothing can be counted twice.
pub(crate) fn add(dir: &Path, tool: &Path, profile: &Path) -> Result<()> {
    let Some((claim, mut inputs)) = enqueue(dir, profile)? else {
        return Ok(());
    };
    let mut level = 0usize;
    loop {
        let output = claim.join(format!("merged-{level}.profdata"));
        // Claim lower/equal weight profiles before merging. Other shims are
        // free to enqueue and merge their own batches concurrently.
        let next_level = {
            let _lock = lock(dir)?;
            let mut next = level;
            loop {
                let slot = dir.join(format!("{LEVEL_PREFIX}{next}.profdata"));
                if !slot.exists() {
                    break next;
                }
                inputs.push(claim_file(&slot, &claim)?);
                next += 1;
            }
        };
        merge(tool, &inputs, &output)?;
        for input in inputs {
            fs::remove_file(input)?;
        }
        level = next_level;
        {
            let _lock = lock(dir)?;
            let slot = dir.join(format!("{LEVEL_PREFIX}{level}.profdata"));
            if !slot.exists() {
                fs::rename(&output, &slot)?;
                // Publication is atomic; the claim marker persists until
                // cleanup, so a killed compactor cannot silently lose data.
                fs::remove_dir(&claim)?;
                return Ok(());
            }
        }
        // Another compactor published this level during our merge. The
        // just-merged output is now the input for a higher-weight merge.
        // Use a fresh filename even if the slot was claimed in the meantime.
        let carry = claim.join("carry.profdata");
        fs::rename(output, &carry)?;
        inputs = vec![carry];
    }
}

/// Publish atomically after all runner shims exit. Incomplete transactions
/// cannot be mistaken for a valid aggregate.
pub(crate) fn finish(dir: &Path, tool: &Path, output: &Path) -> Result<()> {
    let _lock = lock(dir)?;
    if !entries(dir, CLAIM_PREFIX)?.is_empty() {
        bail!("an interrupted aggregate profile transaction cannot be reused");
    }
    let mut inputs = entries(dir, PENDING_PREFIX)?;
    inputs.extend(entries(dir, LEVEL_PREFIX)?);
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
