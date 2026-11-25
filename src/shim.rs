//! Per-test coverage runner shim.
//!
//! Invoked by cargo/nextest as the configured target runner. `collect` wires
//! cargo via `--config target.<triple>.runner=["<cargo-affected>", "runner-shim"]`,
//! so each test invocation arrives as:
//!
//! ```text
//! cargo-affected runner-shim <test-binary> <test-args…>
//! ```
//!
//! The shim reads `NEXTEST_BINARY_ID` and `NEXTEST_TEST_NAME` from the env
//! (nextest sets both for every per-test invocation since 0.9.116), points
//! `LLVM_PROFILE_FILE` at a per-test subdirectory under
//! `CARGO_AFFECTED_PROFRAW_BASE`, then spawns the test binary and waits.
//!
//! Because the shim *waits* for the test (rather than `exec`ing into it), it
//! regains control the moment the test process exits — at which point the
//! LLVM runtime has flushed the profile. So extraction happens right here, in
//! the process that ran the test: merge the profraw into its text form with
//! `llvm-profdata`, write the executed function names to a per-test
//! [`TestResult`] JSON file under
//! `CARGO_AFFECTED_RESULTS_DIR`, and delete the per-test profraw dir before
//! exiting. `collect` reads those result files once nextest finishes.
//!
//! The function map is the half of the answer that doesn't vary per test —
//! where each instrumented function lives in the source — so `collect` derives
//! it once per binary and retains it in memory. The shim reads only a small
//! build stamp under `CARGO_AFFECTED_FUNCTION_MAPS_DIR`; it contributes the
//! executed names, which the collector joins after the run. See [`crate::coverage`] for what the join
//! costs in precision and what skipping it would cost in time.
//!
//! Doing the work here is what bounds peak disk: each profraw is consumed and
//! deleted by its own shim, so at most nextest's concurrency (`test-threads`)
//! worth of bundles exist at once — O(test-threads × per-test) instead of
//! O(whole-suite). No external watcher, no completion heuristic: the
//! completion signal is `wait()` returning.
//!
//! Reading the binary_id straight from the env sidesteps the path-drift
//! problem for *attribution*: cargo's hash suffix can shift between collect's
//! listing and the shim invocation (CI rust-cache restore races, build-script
//! env sensitivity), but nextest knows the stable id of the test it just
//! launched and tells us directly. Same answer for `[lib]`/`[[bin]]` pairs
//! that normalize to the same compiled basename — no marker probe needed.
//!
//! The *map* needs more than the id, because cargo's hash suffix tracks build
//! metadata rather than contents — a rebuilt binary keeps its name. Each map
//! therefore carries a stamp of the binary it was exported from, and the shim
//! checks it. See [`validate_function_map`].
//!
//! Storage layout under `CARGO_AFFECTED_PROFRAW_BASE` (and, mirrored, under
//! `CARGO_AFFECTED_RESULTS_DIR`) is two levels:
//! `<binary_key>/<test_key>/`. Each key is the SHA-256 digest of the full
//! UTF-8 name. Hashing preserves distinctions that replacing punctuation
//! loses (`a::b` versus `a__b`, or different Unicode identifiers), and bounds
//! path-component length even for long test names. Two levels keep the
//! binary and test identities separate.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::coverage::{self, BinaryStamp};

/// Environment-variable contract between `collect` (which sets all of these on
/// the `cargo nextest run` command) and the shim (which reads them). Shared
/// constants so a rename can't silently desync the two sides into a runtime
/// `exit(2)`. The shim requires all of them whenever nextest sets
/// `NEXTEST_BINARY_ID` (i.e. for a real per-test invocation).
pub(crate) const ENV_PROFRAW_BASE: &str = "CARGO_AFFECTED_PROFRAW_BASE";
pub(crate) const ENV_RESULTS_DIR: &str = "CARGO_AFFECTED_RESULTS_DIR";
pub(crate) const ENV_LLVM_PROFDATA: &str = "CARGO_AFFECTED_LLVM_PROFDATA";
pub(crate) const ENV_FUNCTION_MAPS_DIR: &str = "CARGO_AFFECTED_FUNCTION_MAPS_DIR";

/// Per-test coverage result the shim writes and `collect` reads back. Carries
/// the verbatim `(binary_id, test_name)` so `collect` doesn't have to invert
/// [`storage_key`] from the filesystem path.
#[derive(Serialize, Deserialize)]
pub(crate) struct TestResult {
    pub(crate) binary_id: String,
    pub(crate) test_name: String,
    pub(crate) outcome: TestOutcome,
}

/// Outcome of extracting one test's coverage. `Skipped` covers every soft
/// failure (no profraw, no function map, a failed llvm-tool invocation, a
/// parse error): the test simply gains no coverage this round and gets
/// re-selected on the next `--diff`. A *systematic* failure (e.g. a
/// present-but-unrunnable llvm tool) turns every test into `Skipped`;
/// `collect` catches that case — zero tests collected — and bails rather than
/// storing (and, for a full collect, wiping) coverage. See the
/// empty-`mappings` guard in `collect`.
#[derive(Serialize, Deserialize)]
pub(crate) enum TestOutcome {
    Collected {
        functions: Vec<String>,
        profile_count: usize,
    },
    Skipped {
        reason: String,
    },
}

/// Entry point. `args` is everything after `runner-shim` on argv:
/// `[<test-binary>, <test-args…>]`.
///
/// Never returns — runs the test binary, extracts its coverage, and exits with
/// the test's exit code.
pub(crate) fn run(args: &[String], mode: crate::profile::ProfileMode) -> ! {
    let Some((binary, rest)) = args.split_first() else {
        eprintln!("cargo-affected runner-shim: missing test binary argument");
        std::process::exit(2);
    };

    // Discovery passes (`--list`, `--help`, `--ignored` count) don't run a
    // specific test, so nextest doesn't set NEXTEST_BINARY_ID/NEXTEST_TEST_NAME.
    // Run the binary through without coverage in that case.
    let (Ok(binary_id), Ok(test_name)) = (
        std::env::var("NEXTEST_BINARY_ID"),
        std::env::var("NEXTEST_TEST_NAME"),
    ) else {
        std::process::exit(run_test(binary, rest));
    };

    let env = CoverageEnv::from_env();
    let dir = env
        .profraw_base
        .join(storage_key(&binary_id))
        .join(storage_key(&test_name));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "cargo-affected runner-shim: failed to create {}: {e}",
            dir.display()
        );
        std::process::exit(2);
    }
    std::env::set_var("LLVM_PROFILE_FILE", dir.join(mode.profile_name()));

    let code = run_test(binary, rest);

    // The test has exited, so its profile is flushed. Extract now, record the
    // result, and free the bundle — the delete is what keeps peak disk
    // bounded to nextest's concurrency.
    let outcome = extract(&dir, &binary_id, Path::new(binary), &env);
    write_result(&env.results_dir, &binary_id, &test_name, outcome);
    let _ = std::fs::remove_dir_all(&dir);

    std::process::exit(code);
}

/// Spawn the test binary, inheriting our stdio, and return its exit code.
///
/// Unlike a bare `exec`, this keeps the shim alive across the test so it can
/// extract coverage afterwards. nextest runs each test in its own process
/// group and signals that group on cancellation, so the spawned child (in the
/// same group) is signalled directly — the shim doesn't forward signals. A
/// test killed by a signal yields no exit code; we report 1 so nextest still
/// sees a failure.
///
/// Because extraction runs after this returns but before the shim exits, the
/// llvm-tool wall-time falls inside nextest's per-test timeout budget: a fast
/// test with slow extraction can trip nextest's SLOW warning, or be
/// SIGKILL'd if the project configures `terminate-after`. That's the cost of
/// extracting in-process rather than from an outside watcher.
fn run_test(binary: &str, rest: &[String]) -> i32 {
    match Command::new(binary).args(rest).status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("cargo-affected runner-shim: spawn {binary} failed: {e}");
            127
        }
    }
}

/// The llvm-profdata path, the binary stamps to validate, and where to
/// stage output — everything `collect` hands the shim via the environment (see
/// the `ENV_*` constants). Present together or not at all: `collect` sets every
/// one whenever it sets [`ENV_PROFRAW_BASE`].
struct CoverageEnv {
    profraw_base: PathBuf,
    results_dir: PathBuf,
    llvm_profdata: PathBuf,
    function_maps_dir: PathBuf,
}

impl CoverageEnv {
    /// Read the coverage env contract. A missing variable is a setup bug in
    /// `collect`, not a recoverable condition — exit loudly so the failure
    /// surfaces as a failed test rather than silently-missing coverage.
    fn from_env() -> Self {
        let var = |name: &str| -> PathBuf {
            std::env::var_os(name)
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    eprintln!("cargo-affected runner-shim: {name} not set");
                    std::process::exit(2);
                })
        };
        Self {
            profraw_base: var(ENV_PROFRAW_BASE),
            results_dir: var(ENV_RESULTS_DIR),
            llvm_profdata: var(ENV_LLVM_PROFDATA),
            function_maps_dir: var(ENV_FUNCTION_MAPS_DIR),
        }
    }
}

/// Merge the profraws in `dir` and return their executed function names and
/// raw-profile count, or a `Skipped` reason.
///
/// `--sparse` drops all-zero records, so the merged profile is already close
/// to the set of functions the test reached; the counter values decide the
/// rest. `--text` is what makes the merge the *only* llvm-tool invocation
/// here: the binary side of the join was resolved once, by `collect`.
fn extract(dir: &Path, binary_id: &str, binary: &Path, env: &CoverageEnv) -> TestOutcome {
    let profraw_files = match list_profraw_files(dir) {
        Ok(files) => files,
        Err(e) => {
            return TestOutcome::Skipped {
                reason: format!("listing profraw files: {e}"),
            }
        }
    };
    if profraw_files.is_empty() {
        return TestOutcome::Skipped {
            reason: "no profraw generated".into(),
        };
    }

    if let Err(reason) = validate_function_map(&env.function_maps_dir, binary_id, binary) {
        return TestOutcome::Skipped { reason };
    }

    let proftext_path = dir.join("coverage.proftext");
    let mut merge_cmd = Command::new(&env.llvm_profdata);
    merge_cmd.arg("merge").arg("--sparse").arg("--text");
    for f in &profraw_files {
        merge_cmd.arg(f);
    }
    merge_cmd.arg("-o").arg(&proftext_path);
    let merge_output = match merge_cmd.output() {
        Ok(output) => output,
        Err(e) => {
            return TestOutcome::Skipped {
                reason: format!("llvm-profdata merge failed to run: {e}"),
            }
        }
    };
    if !merge_output.status.success() {
        return TestOutcome::Skipped {
            reason: format!(
                "llvm-profdata merge failed: {}",
                String::from_utf8_lossy(&merge_output.stderr).trim()
            ),
        };
    }

    let proftext = match std::fs::read_to_string(&proftext_path) {
        Ok(text) => text,
        Err(e) => {
            return TestOutcome::Skipped {
                reason: format!("reading merged profile: {e}"),
            }
        }
    };
    let executed = match coverage::executed_functions(&proftext) {
        Ok(executed) => executed,
        Err(e) => {
            return TestOutcome::Skipped {
                reason: format!("parse error: {e:#}"),
            }
        }
    };

    TestOutcome::Collected {
        functions: executed.into_iter().map(str::to_owned).collect(),
        profile_count: profraw_files.len(),
    }
}

/// Read the binary stamp `collect` wrote for `binary`, and check it describes
/// the binary we were actually handed.
///
/// The filename can't carry that check on its own: cargo's hash suffix is
/// derived from build metadata, not from the binary's contents, so a rebuild
/// between `collect`'s listing pass and the run produces the *same* name over
/// different line numbers. The stamp inside the file is what distinguishes
/// them ([`coverage::BinaryStamp`]). A mismatch skips the test rather than
/// filing it under stale lines; if it happens to every test, `collect` bails
/// instead of overwriting stored coverage.
fn validate_function_map(
    function_maps_dir: &Path,
    binary_id: &str,
    binary: &Path,
) -> Result<(), String> {
    let path = stamp_path(function_maps_dir, binary_id);
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("reading function map {}: {e}", path.display()))?;
    let expected: BinaryStamp = serde_json::from_str(&raw)
        .map_err(|e| format!("parsing function map {}: {e}", path.display()))?;
    let stamp = BinaryStamp::of(binary).map_err(|e| format!("{e:#}"))?;
    if stamp != expected {
        return Err(format!(
            "{} was rebuilt after its function map was exported; the map \
             describes a different build and its line numbers no longer apply",
            binary.display(),
        ));
    }
    Ok(())
}

/// Identify the stamp by Nextest's stable binary ID, not by the executable
/// basename, which may collide across targets or build directories.
pub(crate) fn stamp_path(function_maps_dir: &Path, binary_id: &str) -> PathBuf {
    function_maps_dir.join(format!("{}.json", storage_key(binary_id)))
}

/// Write the per-test result to
/// `<results_dir>/<storage_key(binary_id)>/<storage_key(test_name)>.json`.
///
/// Written atomically — to a `.tmp` sibling, then renamed — so the reader
/// (`collect::read_results`) only ever sees a complete file. Extraction runs
/// inside nextest's per-test timeout budget, so the shim can be SIGKILL'd
/// mid-write. Either way that test loses this round's coverage and is
/// re-selected on the next `--diff`; what atomicity buys is that the loss
/// doesn't also *look* like corruption. A half-written `.json` is read,
/// fails to parse, and warns `ignoring unreadable coverage result` — a
/// data-integrity message for what is really just a killed process. A killed
/// write instead leaves only a `.tmp`, which the reader skips on extension
/// without a word.
///
/// Best-effort: a failure here costs this test one collect (it's re-selected
/// next `--diff`), so we warn rather than abort the test.
fn write_result(results_dir: &Path, binary_id: &str, test_name: &str, outcome: TestOutcome) {
    let dir = results_dir.join(storage_key(binary_id));
    let path = dir.join(format!("{}.json", storage_key(test_name)));
    let tmp = path.with_extension("json.tmp");
    let result = TestResult {
        binary_id: binary_id.to_string(),
        test_name: test_name.to_string(),
        outcome,
    };
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&tmp, serde_json::to_vec(&result)?)?;
        std::fs::rename(&tmp, &path)
    };
    if let Err(e) = write() {
        eprintln!(
            "cargo-affected runner-shim: failed to write result {}: {e}",
            path.display()
        );
    }
}

/// List all `.profraw` files directly in `dir`.
fn list_profraw_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "profraw") {
            files.push(path);
        }
    }
    Ok(files)
}

/// Derive a deterministic, fixed-length filesystem key from the full name.
///
/// Hex digests are safe on Windows (including case-insensitive filesystems).
/// These paths are collect-local staging data; result JSON retains the
/// original identity, and no persistent coverage schema depends on the key.
fn storage_key(name: &str) -> String {
    format!("{:x}", Sha256::digest(name.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_keys_are_safe_bounded_components() {
        for name in [
            "",
            ".",
            "..",
            "CON",
            "a/b",
            "a\\b",
            "a b",
            "é",
            &"a".repeat(1000),
        ] {
            let key = storage_key(name);
            assert_eq!(key.len(), 64);
            assert!(key.bytes().all(|b| b.is_ascii_hexdigit()));
        }
        assert_ne!(storage_key("A"), storage_key("a"));
    }

    /// A dir with no `.profraw` (test produced no profile — `#[ignore]`d at
    /// runtime, exited before the runtime flushed, etc.) yields a `Skipped`
    /// with a stable reason, never an error. This path needs no llvm tools
    /// and no function map.
    #[test]
    fn extract_without_profraw_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let env = CoverageEnv {
            profraw_base: tmp.path().to_path_buf(),
            results_dir: tmp.path().to_path_buf(),
            // Never invoked — there's no profraw to merge.
            llvm_profdata: PathBuf::from("llvm-profdata"),
            function_maps_dir: tmp.path().to_path_buf(),
        };
        let outcome = extract(tmp.path(), "test-bin", Path::new("test-bin"), &env);
        match outcome {
            TestOutcome::Skipped { reason } => assert_eq!(reason, "no profraw generated"),
            TestOutcome::Collected { .. } => panic!("expected Skipped, got Collected"),
        }
    }

    #[test]
    fn write_result_preserves_distinct_rust_test_names() {
        let tmp = tempfile::tempdir().unwrap();
        let names = ["a::b", "a__b", "café", "cafè"];
        for name in names {
            write_result(
                tmp.path(),
                "sample",
                name,
                TestOutcome::Skipped {
                    reason: name.into(),
                },
            );
        }
        let dir = tmp.path().join(storage_key("sample"));
        let mut stored: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let raw = std::fs::read(entry.unwrap().path()).unwrap();
                serde_json::from_slice::<TestResult>(&raw)
                    .unwrap()
                    .test_name
            })
            .collect();
        stored.sort();
        let mut expected = names.to_vec();
        expected.sort();
        assert_eq!(stored, expected, "every test needs its own result file");
    }

    /// Build a scratch binary file, a profraw beside it, and the shim env
    /// pointing at both. Returns `(env, profraw_dir, binary)`.
    fn extract_fixture(tmp: &Path) -> (CoverageEnv, PathBuf, PathBuf) {
        let profraw_dir = tmp.join("profraw");
        std::fs::create_dir_all(&profraw_dir).unwrap();
        std::fs::write(profraw_dir.join("a.profraw"), b"").unwrap();
        let binary = tmp.join("integration-abc123");
        std::fs::write(&binary, b"not really a binary").unwrap();
        let env = CoverageEnv {
            profraw_base: tmp.to_path_buf(),
            results_dir: tmp.to_path_buf(),
            // Never invoked — every case here fails at the map lookup first.
            llvm_profdata: PathBuf::from("llvm-profdata"),
            function_maps_dir: tmp.to_path_buf(),
        };
        (env, profraw_dir, binary)
    }

    fn skip_reason(outcome: TestOutcome) -> String {
        match outcome {
            TestOutcome::Skipped { reason } => reason,
            TestOutcome::Collected { .. } => panic!("expected Skipped, got Collected"),
        }
    }

    /// A profraw with no function map to join it against skips with a reason
    /// naming the file it looked for.
    #[test]
    fn extract_without_function_map_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let (env, profraw_dir, binary) = extract_fixture(tmp.path());
        let reason = skip_reason(extract(&profraw_dir, "sample::integration", &binary, &env));
        assert!(
            reason.contains(&format!("{}.json", storage_key("sample::integration"))),
            "reason should name the missing map: {reason}",
        );
    }

    /// The map that matters: a binary rebuilt after its map was exported keeps
    /// its cargo-hashed name — the hash tracks build metadata, not contents —
    /// so the *stamp* is what catches it. Without this the shim would file the
    /// test's coverage against the previous build's line numbers, which no
    /// later step could detect.
    #[test]
    fn extract_with_a_map_for_another_build_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let (env, profraw_dir, binary) = extract_fixture(tmp.path());

        let map = coverage::BinaryStamp::of(&binary).unwrap();
        let path = stamp_path(&env.function_maps_dir, "sample::integration");
        std::fs::write(&path, serde_json::to_vec(&map).unwrap()).unwrap();

        // Same name, different build.
        std::fs::write(&binary, b"a longer pretend binary than before").unwrap();

        let reason = skip_reason(extract(&profraw_dir, "sample::integration", &binary, &env));
        assert!(
            reason.contains("rebuilt after its function map was exported"),
            "reason should name the stale map: {reason}",
        );
    }

    #[test]
    fn stamp_paths_use_full_nextest_binary_identity() {
        let dir = Path::new("/tmp/maps");
        assert_eq!(
            stamp_path(dir, "package-a::integration"),
            dir.join(format!("{}.json", storage_key("package-a::integration"))),
        );
        assert_ne!(
            stamp_path(dir, "package-a::integration"),
            stamp_path(dir, "package-b::integration"),
        );
    }

    /// `write_result` round-trips through the same JSON `collect` reads back,
    /// laid out two levels deep so distinct `(binary_id, test_name)` pairs
    /// remain distinct in the staging directory.
    #[test]
    fn write_result_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let results = tmp.path();
        write_result(
            results,
            "my-crate::tests",
            "math::adds",
            TestOutcome::Collected {
                functions: vec!["adds".into()],
                profile_count: 2,
            },
        );

        let path = results
            .join(storage_key("my-crate::tests"))
            .join(format!("{}.json", storage_key("math::adds")));
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: TestResult = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.binary_id, "my-crate::tests");
        assert_eq!(parsed.test_name, "math::adds");
        match parsed.outcome {
            TestOutcome::Collected {
                functions,
                profile_count,
            } => {
                assert_eq!(functions, ["adds"]);
                assert_eq!(profile_count, 2);
            }
            TestOutcome::Skipped { .. } => panic!("expected Collected"),
        }
    }
}
