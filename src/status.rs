//! Status reporting: show stored coverage data and what would run for current changes.

use anyhow::{bail, Context, Result};

use crate::collect::require_nextest;
use crate::db::{db_path, warn_untracked_rs_files, Db};
use crate::fingerprint;
use crate::project::{
    find_project_root, git_changed_files, git_changed_line_ranges, relation_to_head, ShaRelation,
};
use crate::selection;

/// Entry point for `cargo affected status`.
pub fn status(verbose: bool) -> Result<()> {
    let project = find_project_root()?;
    let project_root = &project.workspace_root;

    let path = db_path(project_root);
    if !path.exists() {
        println!("no coverage data found — run `cargo affected collect` first");
        return Ok(());
    }

    let env_fingerprint = fingerprint::compute(&project)?;
    let db = Db::open(project_root)?;

    let known_count = db.test_count(&env_fingerprint)?;
    let region_count = db.region_count(&env_fingerprint)?;
    let last_collected = db.last_collected()?.unwrap_or_else(|| "never".to_string());
    let collect_sha = db.collect_sha(&env_fingerprint)?;

    let rel_path = path.strip_prefix(project_root).unwrap_or(&path);

    if known_count == 0 {
        let reason = if db.has_any_coverage()? {
            "no coverage data for the current environment \
             (Cargo.lock, Cargo.toml, rustc version, or build flags changed since last collect)"
        } else {
            "no coverage data yet"
        };
        println!(
            "coverage database: {}\n\
             last collected: {last_collected}\n\
             {reason} — run `cargo affected collect`",
            rel_path.display(),
        );
        return Ok(());
    }

    println!(
        "coverage database: {}\n\
         last collected: {last_collected}\n\
         tests tracked: {known_count}\n\
         regions stored: {region_count}",
        rel_path.display(),
    );
    if let Some(sha) = collect_sha.as_deref() {
        println!("collect sha: {sha}");
    }

    let collect_sha = collect_sha.context(
        "coverage data exists but is missing collect_sha — run `cargo affected collect`",
    )?;

    match relation_to_head(project_root, &collect_sha)? {
        ShaRelation::Equal => {}
        ShaRelation::Ancestor { commits_ahead } => {
            println!(
                "\nnote: {commits_ahead} commit(s) since collect — \
                 diff vs collect_sha is noisier than necessary; \
                 run `cargo affected collect` to refresh"
            );
        }
        ShaRelation::Diverged => {
            bail!(
                "collect_sha {collect_sha} is not reachable from HEAD \
                 (rebased or branch switched) — \
                 run `cargo affected collect` to re-anchor"
            );
        }
    }

    let changed_files = git_changed_files(project_root)?;
    warn_untracked_rs_files(&db, &env_fingerprint, &changed_files)?;

    if !changed_files.is_empty() {
        println!("\nchanged files ({}):", changed_files.len());
        for f in &changed_files {
            println!("  {f}");
        }
    }

    let changed_ranges = git_changed_line_ranges(project_root, &collect_sha)?;

    require_nextest(project_root)?;
    let sel = selection::compute(project_root, &db, &env_fingerprint, &changed_ranges)?;
    let selected = sel.selected();
    if selected.is_empty() {
        if changed_files.is_empty() {
            println!("\nno uncommitted changes and no new tests — nothing would run");
        } else {
            println!("\nno tests cover the changed lines and no new tests");
        }
        return Ok(());
    }

    println!("\n{}", selection::format_summary(&sel, "would run", verbose));

    Ok(())
}
