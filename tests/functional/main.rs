//! Functional tests for cargo-affected.
//!
//! Each scenario file sets up a temp project, runs cargo-affected, and asserts
//! on outputs. The full suite runs in ~10s on a cold build and ~5s warm, so
//! it runs as part of the default `cargo test` lane. Scenarios require
//! `llvm-tools` and `cargo-nextest`; see the note above `cargo_affected` in
//! this file for the failure mode when those are missing.
//!
//! ## Layout
//!
//! Modeled on cargo-insta's `tests/functional/`. Cargo auto-discovers
//! `tests/functional/main.rs` as a single integration-test binary; the scenario
//! files are picked up via `mod` declarations below. Shared helpers live here.
//!
//! ## Package names
//!
//! Each scenario uses a *distinct* package name in its sample crate. cargo's
//! incremental cache is keyed on `(workspace_root, package_name)` and gets
//! confused when same-named packages live in different temporary roots —
//! cargo-insta's `functional/main.rs` calls out the same trap. Even though
//! we're not yet sharing a `target/` across scenarios, keeping names unique
//! avoids the foot-gun preemptively.

mod cache_miss;
mod clean;
mod config_rule;
mod db_has_function_ranges;
mod diff_collect;
mod dirty;
mod drift;
mod duplicate_target_names;
mod fingerprint;
mod lib_bin_collision;
mod narrowing;
mod new_test;
mod no_profraw_leak;
mod remapped_paths;
mod run;
mod structural;
mod workspace;

use std::path::Path;
use std::process::{Command, Output};

// llvm-tools is a hard requirement — every scenario invokes `cargo affected
// collect`, which calls `llvm-profdata` and `llvm-cov` directly. We let the
// binary's own missing-tool error path surface: `collect` bails with
// `Install \`llvm-tools\` via \`rustup component add llvm-tools\``, which
// propagates as a failed scenario assertion. Running `cargo test` on a host
// without the component fails loudly with that exact message.

/// Run the cargo-affected binary built by this crate, with `args` in `dir`.
///
/// `args` should start with `"affected"` because the binary's clap setup
/// expects the redundant `affected` subcommand even when invoked directly
/// (it's normally invoked as `cargo affected …`, where cargo passes the verb
/// as argv[1]).
pub(crate) fn cargo_affected(dir: &Path, args: &[&str]) -> Output {
    cargo_affected_with_env(dir, args, &[])
}

/// [`cargo_affected`] with extra environment variables — for scenarios that
/// need to influence the build cargo-affected runs, e.g. via `RUSTFLAGS`.
pub(crate) fn cargo_affected_with_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let bin = env!("CARGO_BIN_EXE_cargo-affected");
    let mut cmd = Command::new(bin);
    cmd.args(args).current_dir(dir);
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.output()
        .unwrap_or_else(|e| panic!("failed to run cargo-affected: {e}"))
}

/// Run a git command in `dir`, panicking on failure.
pub(crate) fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run git {}: {e}", args.join(" ")));
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Concatenate a process output's stderr and stdout (in that order) into a
/// single `String` for substring assertions. Stderr first matches every
/// existing call site — selection summaries and notices land on stderr while
/// nextest's PASS/FAIL lines land on stdout, and tests grep both.
pub(crate) fn combined_output(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    )
}

/// Capture `git rev-parse HEAD` in `dir` as a 40-char sha.
pub(crate) fn git_head(dir: &Path) -> String {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .expect("failed to run git rev-parse");
    assert!(output.status.success(), "git rev-parse HEAD failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Replace exactly `from` with `to` in a file. Panics if `from` is absent so
/// a sample-project rename can't silently no-op.
pub(crate) fn replace_in_file(path: &Path, from: &str, to: &str) {
    let content = std::fs::read_to_string(path).unwrap();
    assert!(
        content.contains(from),
        "expected to find {from:?} in {} so the edit lands on the right line",
        path.display()
    );
    std::fs::write(path, content.replace(from, to)).unwrap();
}

/// Initialize a fresh git repo in `dir`, set local user identity (so commits
/// don't depend on the host's global config), stage everything, and commit.
///
/// Disables `core.autocrlf` so line endings round-trip verbatim — Windows git
/// defaults to `true`, which would silently rewrite `\n` to `\r\n` on
/// checkout and quietly mismatch the byte-exact content tests then patch in
/// via `replace_in_file`. Disables `commit.gpgsign` for the same reason: a
/// host that signs by default fails every commit here, because the signing key
/// belongs to the developer, not to the `test@example.com` identity we just
/// set.
pub(crate) fn init_git_with_initial_commit(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "core.autocrlf", "false"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "initial"]);
}

/// Write a single-crate sample project with two independently-tested modules.
///
/// `math.rs` has two functions (`add`, `multiply`) tested separately so
/// scenarios can verify function-level narrowing — editing `add`'s body must
/// not select `test_multiply`. `strings.rs` is the third-party check: tests in
/// it should never be selected by `math.rs` edits.
///
/// `crate_name` should be unique per scenario (see header note on package
/// names).
pub(crate) fn write_two_module_project(dir: &Path, crate_name: &str) {
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2021"
"#
        ),
    )
    .unwrap();

    // /target — cargo-affected writes profraw dirs under target/; without it
    // those dirs leak into `git status` as "changed files".
    // /Cargo.lock — `cargo nextest list` (run by collect) generates Cargo.lock,
    // which would otherwise appear as a brand-new file in every post-collect
    // diff and produce a phantom "1 changed files" report. Ignoring it matches
    // the typical lib-crate convention; fingerprint::compute still hashes the
    // file's contents.
    std::fs::write(dir.join(".gitignore"), "/target\n/Cargo.lock\n").unwrap();

    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();

    std::fs::write(src.join("lib.rs"), "pub mod math;\npub mod strings;\n").unwrap();

    // Lines kept stable (no top comment) so range assertions can reason about
    // line numbers if needed. The struct between `add` and `multiply` is the
    // structural-edit zone exercised by the structural scenario.
    std::fs::write(
        src.join("math.rs"),
        r#"pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub struct Counter {
    pub n: i32,
}

pub fn multiply(a: i32, b: i32) -> i32 {
    a * b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add() {
        assert_eq!(add(2, 3), 5);
    }

    #[test]
    fn test_multiply() {
        assert_eq!(multiply(3, 4), 12);
    }
}
"#,
    )
    .unwrap();

    std::fs::write(
        src.join("strings.rs"),
        r#"pub fn greet(name: &str) -> String {
    format!("hello, {name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_greet() {
        assert_eq!(greet("world"), "hello, world");
    }
}
"#,
    )
    .unwrap();
}
