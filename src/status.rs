//! Status reporting: show stored coverage data and what would run for current changes.

use anyhow::Result;

use crate::db::{db_path, warn_untracked_rs_files, Db};
use crate::project::{find_project_root, git_changed_files};

/// Entry point for `cargo difftest status`.
pub fn status(diff_base: Option<&str>) -> Result<()> {
    let project = find_project_root()?;
    let project_root = &project.workspace_root;

    let path = db_path(project_root);
    if !path.exists() {
        println!("no coverage data found — run `cargo difftest collect` first");
        return Ok(());
    }

    let db = Db::open(project_root)?;

    let test_count = db.test_count()?;
    let mapping_count = db.mapping_count()?;
    let last_collected = db.last_collected()?.unwrap_or_else(|| "never".to_string());

    let rel_path = path.strip_prefix(project_root).unwrap_or(&path);
    println!(
        "coverage database: {}\n\
         last collected: {last_collected}\n\
         tests tracked: {test_count}\n\
         test-file mappings: {mapping_count}",
        rel_path.display(),
    );

    let changed_files = git_changed_files(project_root, diff_base)?;

    warn_untracked_rs_files(&db, &changed_files)?;

    if changed_files.is_empty() {
        if let Some(base) = diff_base {
            println!("\nno changes vs {base}");
        } else {
            println!("\nno uncommitted changes — nothing would run");
        }
        return Ok(());
    }

    println!("\nchanged files ({}):", changed_files.len());
    for f in &changed_files {
        println!("  {f}");
    }

    let file_refs: Vec<&str> = changed_files.iter().map(|s| s.as_str()).collect();
    let tests = db.tests_covering(&file_refs)?;

    if tests.is_empty() {
        println!("\nno tracked tests cover these files");
    } else {
        let skipped = test_count.saturating_sub(tests.len());
        println!(
            "\ntests that would run ({}, {} skipped):",
            tests.len(),
            skipped
        );
        for t in &tests {
            println!("  {t}");
        }
    }

    Ok(())
}
