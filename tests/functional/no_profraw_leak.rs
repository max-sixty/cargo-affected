//! `cargo affected collect` doesn't leak `.profraw` files into the source tree.
//!
//! Build scripts and proc-macros built with `-C instrument-coverage` will drop
//! `default_<hash>_<pid>.profraw` files in cwd if they don't pick up our
//! `LLVM_PROFILE_FILE`. The contract: every cargo subprocess we spawn carries
//! `LLVM_PROFILE_FILE` in its env, cargo passes that through to rustc and
//! every build script/proc-macro it touches, so all instrumented writes land
//! under `target/`. This test fails loudly if any leak escapes — the success
//! criterion is no `.profraw` anywhere outside `target/` after a clean collect.

use std::path::{Path, PathBuf};

use crate::{cargo_affected, init_git_with_initial_commit, write_two_module_project};

fn collect_profraws_outside_target(root: &Path) -> Vec<PathBuf> {
    let mut leaks = Vec::new();
    let target = root.join("target");
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if dir == target {
            continue;
        }
        if let Some(name) = dir.file_name().and_then(|n| n.to_str()) {
            if name == ".git" {
                continue;
            }
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            let path = entry.path();
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() && path.extension().is_some_and(|e| e == "profraw") {
                leaks.push(path);
            }
        }
    }
    leaks
}

#[test]
fn collect_leaves_no_profraw_outside_target() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_two_module_project(dir, "sample_no_profraw_leak");
    init_git_with_initial_commit(dir);

    let collect = cargo_affected(dir, &["affected", "collect"]);
    assert!(
        collect.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&collect.stderr)
    );

    let leaks = collect_profraws_outside_target(dir);
    assert!(
        leaks.is_empty(),
        "expected no .profraw files outside target/ after collect, found:\n  {}",
        leaks
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n  "),
    );
}
