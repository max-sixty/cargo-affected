//! Status reporting: show stored coverage data and what would run for current changes.

use anyhow::Result;

use crate::collect::require_nextest;
use crate::db::{db_path, warn_untracked_rs_files, Db};
use crate::fingerprint;
use crate::project::{find_project_root, git_changed_files};
use crate::selection;

/// Entry point for `cargo affected status`.
pub fn status(diff_base: Option<&str>, verbose: bool) -> Result<()> {
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
    let mapping_count = db.mapping_count(&env_fingerprint)?;
    let last_collected = db.last_collected()?.unwrap_or_else(|| "never".to_string());

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
         test-file mappings: {mapping_count}",
        rel_path.display(),
    );

    let changed_files = git_changed_files(project_root, diff_base)?;
    warn_untracked_rs_files(&db, &env_fingerprint, &changed_files)?;

    if !changed_files.is_empty() {
        println!("\nchanged files ({}):", changed_files.len());
        for f in &changed_files {
            println!("  {f}");
        }
    }

    require_nextest(project_root)?;
    let sel = selection::compute(project_root, &db, &env_fingerprint, &changed_files)?;
    let selected = sel.selected();
    if selected.is_empty() {
        match (changed_files.is_empty(), diff_base) {
            (true, None) => println!("\nno uncommitted changes and no new tests — nothing would run"),
            (true, Some(base)) => println!("\nno changes vs {base} and no new tests"),
            (false, _) => println!("\nno tests cover these files and no new tests"),
        }
        return Ok(());
    }

    println!("\n{}", selection::format_summary(&sel, "would run", verbose));

    Ok(())
}
