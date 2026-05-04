//! Per-test coverage runner shim.
//!
//! Invoked by cargo/nextest through `CARGO_TARGET_<TRIPLE>_RUNNER`. Cargo sets
//! that var to `<cargo-affected> runner-shim` and calls it with the test binary
//! and its args:
//!
//! ```text
//! cargo-affected runner-shim <test-binary> <test-args…>
//! ```
//!
//! The shim reads `NEXTEST_BINARY_ID` and `NEXTEST_TEST_NAME` from the env
//! (nextest sets both for every per-test invocation since 0.9.116), points
//! `LLVM_PROFILE_FILE` at a per-test subdirectory under
//! `CARGO_AFFECTED_PROFRAW_BASE`, writes a sidecar `meta` file (test name +
//! binary path + binary_id), then `exec`s the test binary. No other setup —
//! nextest and cargo already provide the full test environment.
//!
//! Reading the binary_id straight from the env sidesteps the path-drift
//! problem entirely: cargo's hash suffix can shift between collect's listing
//! and the shim invocation (CI rust-cache restore races, build-script env
//! sensitivity), but nextest knows the stable id of the test it just
//! launched and tells us directly. Same answer for `[lib]`/`[[bin]]` pairs
//! that normalize to the same compiled basename — no marker probe needed.
//!
//! The subdir name includes the binary_id so two tests that share a name
//! across different binaries (e.g. a `builds` test in `mock-stub` and
//! `wt-perf`) get independent storage — without this their profraws and
//! coverage mappings collide and one silently wins.
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
        eprintln!("cargo-affected runner-shim: missing test binary argument");
        std::process::exit(2);
    };

    // Discovery passes (`--list`, `--help`, `--ignored` count) don't run a
    // specific test, so nextest doesn't set NEXTEST_BINARY_ID/NEXTEST_TEST_NAME.
    // Pass through without coverage in that case.
    if let (Ok(binary_id), Ok(test_name)) = (
        std::env::var("NEXTEST_BINARY_ID"),
        std::env::var("NEXTEST_TEST_NAME"),
    ) {
        let base = std::env::var("CARGO_AFFECTED_PROFRAW_BASE").unwrap_or_else(|_| {
            eprintln!("cargo-affected runner-shim: CARGO_AFFECTED_PROFRAW_BASE not set");
            std::process::exit(2);
        });

        let subdir = format!("{}__{}", sanitize(&binary_id), sanitize(&test_name));
        let dir = Path::new(&base).join(subdir);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!(
                "cargo-affected runner-shim: failed to create {}: {e}",
                dir.display()
            );
            std::process::exit(2);
        }
        // Sidecar — post-pipeline reads test name + binary path + binary_id
        // from here instead of inverting sanitize() or fanning out across
        // binaries. Format: `<test-name>\n<binary-path>\n<binary-id>\n`.
        let _ = std::fs::write(
            dir.join("meta"),
            format!("{test_name}\n{binary}\n{binary_id}\n"),
        );
        std::env::set_var("LLVM_PROFILE_FILE", dir.join("%p-%m.profraw"));
    }

    let err = Command::new(binary).args(rest).exec();
    eprintln!(
        "cargo-affected runner-shim: exec {} failed: {err}",
        binary
    );
    std::process::exit(127);
}

/// Make a test name or binary id safe for use as a single filesystem directory
/// component.
///
/// Keeps `::`, alphanumerics, `_`, `-`, `.`. Replaces everything else with `_`.
/// Rust test names and nextest binary ids are `::`-joined identifiers, so
/// ordinary names pass through unchanged and collisions are unlikely in
/// practice.
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
    fn sanitize_passthrough() {
        assert_eq!(sanitize("math::tests::test_add"), "math::tests::test_add");
        assert_eq!(sanitize("plain_name"), "plain_name");
        assert_eq!(sanitize("mock-stub::builds"), "mock-stub::builds");
    }

    #[test]
    fn sanitize_replaces_hostile_chars() {
        assert_eq!(sanitize("a/b"), "a_b");
        assert_eq!(sanitize("a\\b"), "a_b");
        assert_eq!(sanitize("a b"), "a_b");
    }
}
