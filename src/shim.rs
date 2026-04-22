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
//! The shim recovers the test name from `--exact <name>`, looks up the
//! invoking binary's stable `binary_id` via a path→id map serialized by
//! `collect` (env `DIFFTEST_BINARY_MAP`), points `LLVM_PROFILE_FILE` at a
//! per-test subdirectory under `DIFFTEST_PROFRAW_BASE`, writes a sidecar
//! `meta` file (test name + binary path + binary_id), then `exec`s the test
//! binary. No other setup — nextest and cargo already provide the full test
//! environment.
//!
//! The subdir name includes the binary_id so two tests that share a name
//! across different binaries (e.g. a `builds` test in `mock-stub` and
//! `wt-perf`) get independent storage — without this their profraws and
//! coverage mappings collide and one silently wins.
//!
//! Unix-only (uses `execvp`). Windows isn't supported.

use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
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
        let map_path = std::env::var("DIFFTEST_BINARY_MAP").unwrap_or_else(|_| {
            eprintln!("cargo-difftest runner-shim: DIFFTEST_BINARY_MAP not set");
            std::process::exit(2);
        });
        let binary_path = PathBuf::from(binary);
        let binary_id = resolve_binary_id(Path::new(&map_path), &binary_path).unwrap_or_else(|e| {
            eprintln!(
                "cargo-difftest runner-shim: failed to resolve binary_id for {}: {e}",
                binary_path.display()
            );
            std::process::exit(2);
        });

        let subdir = format!("{}__{}", sanitize(&binary_id), sanitize(&name));
        let dir = Path::new(&base).join(subdir);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!(
                "cargo-difftest runner-shim: failed to create {}: {e}",
                dir.display()
            );
            std::process::exit(2);
        }
        // Sidecar — post-pipeline reads test name + binary path + binary_id
        // from here instead of inverting sanitize() or fanning out across
        // binaries. Format: `<test-name>\n<binary-path>\n<binary-id>\n`.
        let _ = std::fs::write(
            dir.join("meta"),
            format!("{name}\n{binary}\n{binary_id}\n"),
        );
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

/// Look up the binary's nextest `binary_id` in the map written by
/// `collect`. Tries the path as-is and canonicalized, since cargo may pass a
/// relative or symlinked path but the map was built from nextest's
/// (canonicalized, absolute) listing.
fn resolve_binary_id(map_path: &Path, binary_path: &Path) -> anyhow::Result<String> {
    let json = std::fs::read_to_string(map_path).map_err(|e| {
        anyhow::anyhow!("reading binary map at {}: {e}", map_path.display())
    })?;
    let map: HashMap<String, String> = serde_json::from_str(&json)
        .map_err(|e| anyhow::anyhow!("parsing binary map {}: {e}", map_path.display()))?;

    let as_str = binary_path.to_str().unwrap_or("");
    if let Some(id) = map.get(as_str) {
        return Ok(id.clone());
    }
    if let Ok(canon) = std::fs::canonicalize(binary_path) {
        if let Some(id) = canon.to_str().and_then(|s| map.get(s)) {
            return Ok(id.clone());
        }
    }
    Err(anyhow::anyhow!(
        "binary not found in map (path: {})",
        binary_path.display()
    ))
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
        assert_eq!(sanitize("mock-stub::builds"), "mock-stub::builds");
    }

    #[test]
    fn sanitize_replaces_hostile_chars() {
        assert_eq!(sanitize("a/b"), "a_b");
        assert_eq!(sanitize("a\\b"), "a_b");
        assert_eq!(sanitize("a b"), "a_b");
    }

    #[test]
    fn resolve_binary_id_exact_match() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let map_path = dir.path().join("map.json");
        let mut map = HashMap::new();
        map.insert(
            "/tmp/target/debug/deps/foo-abc123".to_string(),
            "mycrate::foo".to_string(),
        );
        std::fs::write(&map_path, serde_json::to_string(&map)?)?;

        let id = resolve_binary_id(&map_path, Path::new("/tmp/target/debug/deps/foo-abc123"))?;
        assert_eq!(id, "mycrate::foo");
        Ok(())
    }

    #[test]
    fn resolve_binary_id_missing_errors() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let map_path = dir.path().join("map.json");
        std::fs::write(&map_path, "{}")?;

        let err = resolve_binary_id(&map_path, Path::new("/nowhere/foo")).unwrap_err();
        assert!(err.to_string().contains("not found"));
        Ok(())
    }
}
