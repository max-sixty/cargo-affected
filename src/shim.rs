//! Per-test coverage runner shim.
//!
//! Invoked by cargo/nextest through `CARGO_TARGET_<TRIPLE>_RUNNER`. Cargo sets
//! that var to `<cargo-difftest> runner-shim` and calls it with the test binary
//! and its args:
//!
//! ```text
//! cargo-difftest runner-shim <test-binary> <test-args…>
//! ```
//!
//! The shim recovers the test name from `--exact <name>`, points
//! `LLVM_PROFILE_FILE` at a per-test subdirectory under
//! `DIFFTEST_PROFRAW_BASE`, writes a sidecar `name` file (so the post-run
//! pipeline can map subdir → test name without inverting sanitization), then
//! `exec`s the test binary. No other setup — nextest and cargo already
//! provide the full test environment.
//!
//! Unix-only (uses `execvp`). Windows isn't supported.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

/// Entry point. `args` is everything after `runner-shim` on argv:
/// `[<test-binary>, <test-args…>]`.
///
/// Never returns — either `exec`s the test binary or exits with an error.
pub fn run(args: &[String]) -> ! {
    let Some((binary, rest)) = args.split_first() else {
        eprintln!("cargo-difftest runner-shim: missing test binary argument");
        std::process::exit(2);
    };

    if let Some(name) = find_exact(rest) {
        let base = std::env::var("DIFFTEST_PROFRAW_BASE").unwrap_or_else(|_| {
            eprintln!("cargo-difftest runner-shim: DIFFTEST_PROFRAW_BASE not set");
            std::process::exit(2);
        });
        let dir = Path::new(&base).join(sanitize(&name));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!(
                "cargo-difftest runner-shim: failed to create {}: {e}",
                dir.display()
            );
            std::process::exit(2);
        }
        // Sidecar — post-pipeline reads test name + binary path from here
        // instead of inverting sanitize() and fanning out across binaries.
        // Format: `<test-name>\n<binary-path>\n`.
        let _ = std::fs::write(dir.join("meta"), format!("{name}\n{binary}\n"));
        std::env::set_var("LLVM_PROFILE_FILE", dir.join("%p-%m.profraw"));
    }
    // No --exact: likely --list / --help / discovery. Passthrough without coverage.

    let err = Command::new(binary).args(rest).exec();
    eprintln!(
        "cargo-difftest runner-shim: exec {} failed: {err}",
        binary
    );
    std::process::exit(127);
}

/// Find the value of `--exact <name>` or `--exact=<name>` in args.
fn find_exact(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--exact" {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix("--exact=") {
            return Some(v.to_string());
        }
    }
    None
}

/// Make a test name safe for use as a single filesystem directory component.
///
/// Keeps `::`, alphanumerics, `_`, `-`, `.`. Replaces everything else with `_`.
/// Rust test names are `::`-joined identifiers, so ordinary names pass through
/// unchanged and collisions are unlikely in practice.
pub fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == ':' || c == '_' || c == '-' || c == '.' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_exact_space_form() {
        let args: Vec<String> = ["--nocapture", "--exact", "foo::bar", "--test-threads=1"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(find_exact(&args).as_deref(), Some("foo::bar"));
    }

    #[test]
    fn find_exact_equals_form() {
        let args: Vec<String> = ["--exact=foo::bar", "--nocapture"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(find_exact(&args).as_deref(), Some("foo::bar"));
    }

    #[test]
    fn find_exact_missing() {
        let args: Vec<String> = ["--list", "--format=terse"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(find_exact(&args), None);
    }

    #[test]
    fn sanitize_passthrough() {
        assert_eq!(sanitize("math::tests::test_add"), "math::tests::test_add");
        assert_eq!(sanitize("plain_name"), "plain_name");
    }

    #[test]
    fn sanitize_replaces_hostile_chars() {
        assert_eq!(sanitize("a/b"), "a_b");
        assert_eq!(sanitize("a\\b"), "a_b");
        assert_eq!(sanitize("a b"), "a_b");
    }
}
