//! Status reporting: show stored coverage data and what would run for current changes.

use anyhow::Result;

use crate::db::Db;
use crate::project::{find_project_root, git_changed_files};

/// Entry point for `cargo difftest status`.
pub fn status(diff_base: Option<&str>) -> Result<()> {
    let project = find_project_root()?;
    let project_root = &project.workspace_root;

    let db_path = project_root.join(".difftest.db");
    if !db_path.exists() {
        println!("no coverage data found — run `cargo difftest collect` first");
        return Ok(());
    }

    let db = Db::open(project_root)?;

    let test_count = db.test_count()?;
    let mapping_count = db.mapping_count()?;
    let last_collected = db.last_collected()?.unwrap_or_else(|| "never".to_string());

    println!("coverage database: .difftest.db");
    println!("last collected: {last_collected}");
    println!("tests tracked: {test_count}");
    println!("test-file mappings: {mapping_count}");

    let changed_files = git_changed_files(project_root, diff_base)?;

    // Warn about changed .rs files not in the DB.
    for f in &changed_files {
        if f.ends_with(".rs") && !db.file_tracked(f)? {
            println!(
                "warning: {f} has no coverage data \
                 — run `cargo difftest collect` to include it"
            );
        }
    }

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
