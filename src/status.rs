//! Status reporting: show stored coverage data and what would run for current changes.

use anyhow::Result;

use crate::db::{db_path, warn_untracked_rs_files, Db};
use crate::fingerprint;
use crate::project::{find_project_root, git_changed_files};

/// Entry point for `cargo difftest status`.
pub fn status(diff_base: Option<&str>, verbose: bool) -> Result<()> {
    let project = find_project_root()?;
    let project_root = &project.workspace_root;

    let path = db_path(project_root);
    if !path.exists() {
        println!("no coverage data found — run `cargo difftest collect` first");
        return Ok(());
    }

    let env_fingerprint = fingerprint::compute(&project)?;
    let db = Db::open(project_root)?;

    let test_count = db.test_count(&env_fingerprint)?;
    let mapping_count = db.mapping_count(&env_fingerprint)?;
    let last_collected = db.last_collected()?.unwrap_or_else(|| "never".to_string());

    let rel_path = path.strip_prefix(project_root).unwrap_or(&path);

    if test_count == 0 {
        let reason = if db.has_any_coverage()? {
            "no coverage data for the current environment \
             (Cargo.lock, Cargo.toml, rustc version, or build flags changed since last collect)"
        } else {
            "no coverage data yet"
        };
        println!(
            "coverage database: {}\n\
             last collected: {last_collected}\n\
             {reason} — run `cargo difftest collect`",
            rel_path.display(),
        );
        return Ok(());
    }

    println!(
        "coverage database: {}\n\
         last collected: {last_collected}\n\
         tests tracked: {test_count}\n\
         test-file mappings: {mapping_count}",
        rel_path.display(),
    );

    let changed_files = git_changed_files(project_root, diff_base)?;

    warn_untracked_rs_files(&db, &env_fingerprint, &changed_files)?;

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
    let tests = db.tests_covering(&env_fingerprint, &file_refs)?;

    if tests.is_empty() {
        println!("\nno tracked tests cover these files");
    } else {
        let skipped = test_count.saturating_sub(tests.len());
        if verbose {
            println!(
                "\n{} tests would run ({skipped} skipped of {test_count} total):",
                tests.len()
            );
            for t in &tests {
                println!("  {t}");
            }
        } else {
            println!(
                "\n{} tests would run ({skipped} skipped of {test_count} total) — pass -v to list",
                tests.len()
            );
        }
    }

    Ok(())
}
