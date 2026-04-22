//! Status reporting: show stored coverage data and what would run for current changes.

use std::collections::BTreeSet;

use anyhow::Result;

use crate::collect::{nextest_list, require_nextest};
use crate::db::{db_path, warn_untracked_rs_files, Db, TestId};
use crate::fingerprint;
use crate::project::{find_project_root, git_changed_files};

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

    // List tests so we can flag any added since the last collect. This builds,
    // so status is no longer free — but it's the only way to detect new tests
    // without requiring the user to re-collect first.
    require_nextest(project_root)?;
    eprintln!("checking for new tests...");
    let listing = nextest_list(project_root, None, None)?;
    let known_tests = db.all_tests(&env_fingerprint)?;
    let new_tests: BTreeSet<TestId> = listing
        .tests
        .iter()
        .filter(|t| !known_tests.contains(*t))
        .cloned()
        .collect();

    if !changed_files.is_empty() {
        println!("\nchanged files ({}):", changed_files.len());
        for f in &changed_files {
            println!("  {f}");
        }
    }

    let affected = if changed_files.is_empty() {
        BTreeSet::new()
    } else {
        let file_refs: Vec<&str> = changed_files.iter().map(|s| s.as_str()).collect();
        db.tests_covering(&env_fingerprint, &file_refs)?
    };

    let selected: BTreeSet<TestId> = affected.union(&new_tests).cloned().collect();
    if selected.is_empty() {
        if changed_files.is_empty() {
            if let Some(base) = diff_base {
                println!("\nno changes vs {base} and no new tests");
            } else {
                println!("\nno uncommitted changes and no new tests — nothing would run");
            }
        } else {
            println!("\nno tests cover these files and no new tests");
        }
        return Ok(());
    }

    let skipped = known_count.saturating_sub(affected.len());
    if verbose {
        println!(
            "\n{} tests would run ({} affected + {} new, {skipped} skipped of {known_count} known):",
            selected.len(),
            affected.len(),
            new_tests.len(),
        );
        for t in &selected {
            let tag = if new_tests.contains(t) { " (new)" } else { "" };
            println!("  {}::{}{tag}", t.binary_id, t.test_name);
        }
    } else {
        println!(
            "\n{} tests would run ({} affected + {} new, {skipped} skipped of {known_count} known) \
             — pass -v to list",
            selected.len(),
            affected.len(),
            new_tests.len(),
        );
    }

    Ok(())
}
