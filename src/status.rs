//! Status reporting: show stored coverage data and what would run for current changes.
//!
//! Mirrors `run`'s contract: reports "would run all tests" with an
//! explanation when the cache offers nothing usable (no data, fingerprint
//! mismatch, or every stored `collect_sha` unreachable). Partial divergence
//! proceeds with the reachable subset, same as `run`.

use anyhow::Result;

use crate::collect::{nextest_list, require_nextest};
use crate::db::{db_path, warn_untracked_rs_files, Db};
use crate::fingerprint;
use crate::project::{find_project_root, git_changed_files};
use crate::selection;

/// Entry point for `cargo affected status`.
///
/// Reports "would run all tests" (with an explanation) when the cache
/// offers nothing usable — no coverage, fingerprint mismatch, or every
/// stored `collect_sha` unreachable. Partial divergence proceeds with the
/// reachable subset and surfaces stranded tests as "new", mirroring `run`
/// so the dry-run accurately predicts what `run` would do.
pub fn status(verbose: bool) -> Result<()> {
    let project = find_project_root()?;
    let project_root = &project.workspace_root;

    let path = db_path(project_root);
    if !path.exists() {
        println!(
            "no coverage data found — would run all tests; \
             run `cargo affected collect` to enable selection"
        );
        return Ok(());
    }

    let env_fingerprint = fingerprint::compute(&project)?;
    let db = Db::open(project_root)?;

    let known_count = db.test_count(&env_fingerprint)?;
    let region_count = db.region_count(&env_fingerprint)?;
    let last_collected = db.last_collected()?.unwrap_or_else(|| "never".to_string());
    let collect_shas = db.collect_shas(&env_fingerprint)?;

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
             {reason} — would run all tests; run `cargo affected collect` to enable selection",
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
    let mut sha_list: Vec<&str> = collect_shas.iter().map(String::as_str).collect();
    sha_list.sort();
    println!("collect shas: {}", sha_list.join(", "));

    let reach = selection::check_shas_reachable(project_root, &collect_shas)?;
    if !reach.diverged.is_empty() {
        let stale_rows = db.region_count_at_shas(&env_fingerprint, &reach.diverged)?;
        println!(
            "\n{}\nstale rows: {stale_rows} (anchored at diverged sha{})",
            selection::diverged_shas_notice(&reach.diverged, "would rerun as 'new'"),
            if reach.diverged.len() == 1 { "" } else { "s" },
        );
    }
    if reach.reachable.is_empty() {
        println!(
            "\nnote: no reachable collect_sha for the current environment — \
             would run all tests; run `cargo affected collect` to re-anchor"
        );
        return Ok(());
    }
    if reach.max_commits_ahead > 0 {
        println!(
            "\nnote: {} commit(s) since collect — \
             diff vs collect_sha is noisier than necessary; \
             run `cargo affected collect` to refresh",
            reach.max_commits_ahead,
        );
    }

    let changed_files = git_changed_files(project_root)?;
    warn_untracked_rs_files(&db, &env_fingerprint, &changed_files)?;

    if !changed_files.is_empty() {
        println!("\nchanged files ({}):", changed_files.len());
        for f in &changed_files {
            println!("  {f}");
        }
    }

    require_nextest(project_root)?;
    eprintln!("checking for new tests...");
    let listing = nextest_list(project_root, None, None)?;
    let sel = selection::select_with_reach(
        project_root,
        &db,
        &env_fingerprint,
        &listing,
        &reach,
    )?;
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
