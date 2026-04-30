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
//! The shim recovers the test name from `--exact <name>`, looks up the
//! invoking binary's stable `binary_id` via a path→id map serialized by
//! `collect` (env `CARGO_AFFECTED_BINARY_MAP`), points `LLVM_PROFILE_FILE` at a
//! per-test subdirectory under `CARGO_AFFECTED_PROFRAW_BASE`, writes a sidecar
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
        eprintln!("cargo-affected runner-shim: missing test binary argument");
        std::process::exit(2);
    };

    if let Some(name) = find_exact(rest) {
        let base = std::env::var("CARGO_AFFECTED_PROFRAW_BASE").unwrap_or_else(|_| {
            eprintln!("cargo-affected runner-shim: CARGO_AFFECTED_PROFRAW_BASE not set");
            std::process::exit(2);
        });
        let map_path = std::env::var("CARGO_AFFECTED_BINARY_MAP").unwrap_or_else(|_| {
            eprintln!("cargo-affected runner-shim: CARGO_AFFECTED_BINARY_MAP not set");
            std::process::exit(2);
        });
        let binary_path = PathBuf::from(binary);
        let binary_id = resolve_binary_id(Path::new(&map_path), &binary_path).unwrap_or_else(|e| {
            eprintln!(
                "cargo-affected runner-shim: failed to resolve binary_id for {}: {e}",
                binary_path.display()
            );
            std::process::exit(2);
        });

        let subdir = format!("{}__{}", sanitize(&binary_id), sanitize(&name));
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
            format!("{name}\n{binary}\n{binary_id}\n"),
        );
        std::env::set_var("LLVM_PROFILE_FILE", dir.join("%p-%m.profraw"));
    }
    // No --exact: likely --list / --help / discovery. Passthrough without coverage.

    let err = Command::new(binary).args(rest).exec();
    eprintln!(
        "cargo-affected runner-shim: exec {} failed: {err}",
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
/// `collect`.
///
/// Tries in order:
/// 1. Path as-is.
/// 2. Canonicalized path (cargo may pass a relative or symlinked path but
///    the map was built from nextest's canonicalized, absolute listing).
/// 3. Target-name fallback. If `cargo nextest run` rebuilds the test
///    binaries between the `list` and `run` steps (e.g. a `build.rs`
///    that's sensitive to env vars `list` doesn't set but `run` does),
///    the new binaries have different `-<hash>` suffixes than the ones
///    in the map. Strip the hash suffix and match by target name. If
///    multiple map entries share a basename (e.g. several
///    `tests/integration.rs` across workspace crates), narrow further
///    by filesystem state — after a partial rebuild, the un-rebuilt
///    candidates still have their original on-disk paths, while the
///    rebuilt binary's old path was replaced. The unique
///    no-longer-on-disk candidate is the right match. Fail with a
///    clear error only when even that doesn't disambiguate.
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

    // Target-name fallback: match by basename modulo the cargo hash suffix.
    let needle = binary_path.file_name().and_then(|s| s.to_str()).map(target_name);
    if let Some(needle) = needle {
        let candidates: Vec<(&str, &String)> = map
            .iter()
            .filter(|(p, _)| {
                Path::new(p)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(target_name)
                    == Some(needle)
            })
            .map(|(p, id)| (p.as_str(), id))
            .collect();

        let resolved: Option<&String> = match candidates.len() {
            0 => None,
            1 => Some(candidates[0].1),
            _ => {
                // Disambiguate via filesystem state: the rebuilt binary's old
                // map path no longer exists. If exactly one candidate's old
                // path is gone, that's the match.
                let rebuilt: Vec<&String> = candidates
                    .iter()
                    .filter(|(p, _)| !Path::new(p).exists())
                    .map(|(_, id)| *id)
                    .collect();
                if rebuilt.len() == 1 {
                    Some(rebuilt[0])
                } else {
                    None
                }
            }
        };

        if let Some(id) = resolved {
            eprintln!(
                "cargo-affected runner-shim: warning — exact path missed, \
                 resolved {} via target-name fallback (binaries appear to \
                 have rebuilt since collect's `nextest list` step)",
                binary_path.display(),
            );
            return Ok(id.clone());
        }

        if !candidates.is_empty() {
            return Err(anyhow::anyhow!(
                "binary not found in map (path: {}); target-name fallback \
                 ambiguous — {} entries share basename {:?} and filesystem \
                 state can't disambiguate",
                binary_path.display(),
                candidates.len(),
                needle
            ));
        }
    }

    Err(anyhow::anyhow!(
        "binary not found in map (path: {})",
        binary_path.display()
    ))
}

/// Strip cargo's `-<16 hex chars>` hash suffix from a binary basename, leaving
/// the stable target name. Returns the input unchanged if the suffix isn't
/// present (e.g., a binary from outside cargo, or a future cargo with a
/// different hash format).
fn target_name(basename: &str) -> &str {
    let bytes = basename.as_bytes();
    if bytes.len() >= 17 && bytes[bytes.len() - 17] == b'-' {
        let hash = &basename[basename.len() - 16..];
        if hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return &basename[..basename.len() - 17];
        }
    }
    basename
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

    #[test]
    fn target_name_strips_hash() {
        assert_eq!(target_name("worktrunk-e19bc0c52c763540"), "worktrunk");
        assert_eq!(target_name("mock-stub-abcdef0123456789"), "mock-stub");
        assert_eq!(target_name("foo-0123456789abcdef"), "foo");
    }

    #[test]
    fn target_name_passthrough_when_no_hash() {
        assert_eq!(target_name("worktrunk"), "worktrunk");
        assert_eq!(target_name("plain"), "plain");
        // Hash too short.
        assert_eq!(target_name("foo-abc"), "foo-abc");
        // Suffix present but not 16 hex chars (uppercase isn't hex-lowercase).
        assert_eq!(target_name("foo-ABCDEF0123456789"), "foo-ABCDEF0123456789");
        // Suffix present but contains non-hex.
        assert_eq!(target_name("foo-zzzzzzzzzzzzzzzz"), "foo-zzzzzzzzzzzzzzzz");
    }

    #[test]
    fn resolve_binary_id_target_name_fallback() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let map_path = dir.path().join("map.json");
        let mut map = HashMap::new();
        // Map was built when binary had hash `aaaa...`.
        map.insert(
            "/tmp/target/debug/deps/worktrunk-aaaaaaaaaaaaaaaa".to_string(),
            "worktrunk".to_string(),
        );
        std::fs::write(&map_path, serde_json::to_string(&map)?)?;

        // Run-step rebuild gave the same target a different hash.
        let id = resolve_binary_id(
            &map_path,
            Path::new("/tmp/target/debug/deps/worktrunk-bbbbbbbbbbbbbbbb"),
        )?;
        assert_eq!(id, "worktrunk");
        Ok(())
    }

    #[test]
    fn resolve_binary_id_disambiguates_partial_rebuild() -> anyhow::Result<()> {
        // Two crates each have a `tests/builds.rs` → both produce a
        // `builds-<hash>` binary with the same basename. After a partial
        // rebuild only one of them gets a new hash; the un-rebuilt one's
        // original path is still on disk. Filesystem state should
        // disambiguate.
        let dir = tempfile::tempdir()?;
        let unrebuilt = dir.path().join("builds-aaaaaaaaaaaaaaaa");
        let map_path = dir.path().join("map.json");

        std::fs::write(&unrebuilt, b"")?;
        // No file at builds-bbbb…: that binary was rebuilt and its old hash
        // path is gone.
        let mut map = HashMap::new();
        map.insert(
            unrebuilt.to_str().unwrap().to_string(),
            "crate_a::builds".to_string(),
        );
        map.insert(
            dir.path()
                .join("builds-bbbbbbbbbbbbbbbb")
                .to_str()
                .unwrap()
                .to_string(),
            "crate_b::builds".to_string(),
        );
        std::fs::write(&map_path, serde_json::to_string(&map)?)?;

        let id = resolve_binary_id(
            &map_path,
            // The new (rebuilt) hash for crate_b's binary.
            &dir.path().join("builds-cccccccccccccccc"),
        )?;
        assert_eq!(id, "crate_b::builds");
        Ok(())
    }

    #[test]
    fn resolve_binary_id_target_name_fallback_truly_ambiguous() -> anyhow::Result<()> {
        // Both candidates rebuilt — filesystem state can't disambiguate.
        let dir = tempfile::tempdir()?;
        let map_path = dir.path().join("map.json");
        let mut map = HashMap::new();
        map.insert(
            dir.path()
                .join("builds-aaaaaaaaaaaaaaaa")
                .to_str()
                .unwrap()
                .to_string(),
            "crate_a::builds".to_string(),
        );
        map.insert(
            dir.path()
                .join("builds-bbbbbbbbbbbbbbbb")
                .to_str()
                .unwrap()
                .to_string(),
            "crate_b::builds".to_string(),
        );
        std::fs::write(&map_path, serde_json::to_string(&map)?)?;

        let err = resolve_binary_id(
            &map_path,
            &dir.path().join("builds-cccccccccccccccc"),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("ambiguous"),
            "expected ambiguous error, got: {err}"
        );
        Ok(())
    }
}
