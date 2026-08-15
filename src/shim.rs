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
//! `llvm-profdata`, join the functions it names against the binary's
//! function map, write a small per-test [`TestResult`] JSON file under
//! `CARGO_AFFECTED_RESULTS_DIR`, and delete the per-test profraw dir before
//! exiting. `collect` reads those result files once nextest finishes.
//!
//! The function map is the half of the answer that doesn't vary per test —
//! where each instrumented function lives in the source — so `collect` derives
//! it once per binary and leaves it under `CARGO_AFFECTED_FUNCTION_MAPS_DIR` for
//! every shim to read. What the shim contributes is the other half: which
//! functions this test reached. See [`crate::coverage`] for what the join
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
//! checks it. See [`load_function_map`].
//!
//! Storage layout under `CARGO_AFFECTED_PROFRAW_BASE` (and, mirrored, under
//! `CARGO_AFFECTED_RESULTS_DIR`) is two levels:
//! `<sanitized_binary_id>/<sanitized_test_name>/`. Two levels (rather than a
//! single concatenated component) keep names unique even after sanitization
//! collapses `::` to `_`: `(foo, a::b)` and `(foo::a, b)` would otherwise both
//! produce `foo__a__b` and clobber each other on Windows where `:` is
//! filesystem-illegal.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::coverage::{self, BinaryFunctionMap, BinaryStamp, HitRange};

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
/// [`sanitize`] from the filesystem path.
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
    Collected { ranges: BTreeSet<HitRange> },
    Skipped { reason: String },
}

/// Entry point. `args` is everything after `runner-shim` on argv:
/// `[<test-binary>, <test-args…>]`.
///
/// Never returns — runs the test binary, extracts its coverage, and exits with
/// the test's exit code.
pub(crate) fn run(args: &[String]) -> ! {
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
        .join(sanitize(&binary_id))
        .join(sanitize(&test_name));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "cargo-affected runner-shim: failed to create {}: {e}",
            dir.display()
        );
        std::process::exit(2);
    }
    std::env::set_var("LLVM_PROFILE_FILE", dir.join("%p-%m.profraw"));

    let code = run_test(binary, rest);

    // The test has exited, so its profile is flushed. Extract now, record the
    // result, and free the bundle — the delete is what keeps peak disk
    // bounded to nextest's concurrency.
    let outcome = extract(&dir, Path::new(binary), &env);
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

/// The llvm-profdata path, the function maps to join against, and where to
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

/// Merge the profraws in `dir` and join the functions they name against
/// `binary`'s function map, returning the hit ranges or a `Skipped` reason.
///
/// `--sparse` drops all-zero records, so the merged profile is already close
/// to the set of functions the test reached; the counter values decide the
/// rest. `--text` is what makes the merge the *only* llvm-tool invocation
/// here: the binary side of the join was resolved once, by `collect`.
fn extract(dir: &Path, binary: &Path, env: &CoverageEnv) -> TestOutcome {
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

    let map = match load_function_map(&env.function_maps_dir, binary) {
        Ok(map) => map,
        Err(e) => return TestOutcome::Skipped { reason: e },
    };

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

    match coverage::hit_ranges(&executed, &map.functions)
        .with_context(|| format!("joining against {}", binary.display()))
    {
        Ok(ranges) => TestOutcome::Collected { ranges },
        Err(e) => TestOutcome::Skipped {
            reason: format!("{e:#}"),
        },
    }
}

/// Read the function map `collect` wrote for `binary`, and check it describes
/// the binary we were actually handed.
///
/// The filename can't carry that check on its own: cargo's hash suffix is
/// derived from build metadata, not from the binary's contents, so a rebuild
/// between `collect`'s listing pass and the run produces the *same* name over
/// different line numbers. The stamp inside the file is what distinguishes
/// them ([`coverage::BinaryStamp`]). A mismatch skips the test rather than
/// filing it under stale lines; if it happens to every test, `collect` bails
/// instead of overwriting stored coverage.
fn load_function_map(function_maps_dir: &Path, binary: &Path) -> Result<BinaryFunctionMap, String> {
    let path = map_path(function_maps_dir, binary)
        .ok_or_else(|| format!("{} has no file name to look a map up by", binary.display()))?;
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("reading function map {}: {e}", path.display()))?;
    let map: BinaryFunctionMap = serde_json::from_str(&raw)
        .map_err(|e| format!("parsing function map {}: {e}", path.display()))?;
    let stamp = BinaryStamp::of(binary).map_err(|e| format!("{e:#}"))?;
    if stamp != map.binary {
        return Err(format!(
            "{} was rebuilt after its function map was exported; the map \
             describes a different build and its line numbers no longer apply",
            binary.display(),
        ));
    }
    Ok(map)
}

/// Where a binary's function map lives under the function-maps directory.
/// Shared with `collect`, which writes it, so the two can't drift. `None` for
/// a path with no file name — impossible for a binary nextest ran, and
/// refused rather than quietly turned into a shared default name.
pub(crate) fn map_path(function_maps_dir: &Path, binary: &Path) -> Option<PathBuf> {
    let name = sanitize(&binary.file_name()?.to_string_lossy());
    Some(function_maps_dir.join(format!("{name}.json")))
}

/// Write the per-test result to `<results_dir>/<binary_id>/<test_name>.json`.
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
    let dir = results_dir.join(sanitize(binary_id));
    let path = dir.join(format!("{}.json", sanitize(test_name)));
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

/// Make a test name or binary id safe for use as a single filesystem directory
/// component.
///
/// Keeps alphanumerics, `_`, `-`, `.`. Replaces everything else (including
/// `:` and path separators) with `_`. `:` is forbidden in Windows path
/// components — drive letters and alternate data streams reserve it — so
/// the `::`-joined nextest ids and Rust test names that occur in practice
/// have to collapse to underscores. Sanitize output is never reversed; the
/// per-test [`TestResult`] carries the verbatim values, so name collisions
/// inside one binary_id are the only risk, and they don't occur with real Rust
/// test names (no two tests in the same binary share a sanitized form).
pub(crate) fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
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
        assert_eq!(sanitize("plain_name"), "plain_name");
        assert_eq!(sanitize("dotted.name-1"), "dotted.name-1");
    }

    #[test]
    fn sanitize_replaces_hostile_chars() {
        // `:` and path separators are filesystem-illegal on Windows; spaces
        // and other punctuation are merely ugly. All collapse to `_`.
        assert_eq!(sanitize("math::tests::test_add"), "math__tests__test_add");
        assert_eq!(sanitize("mock-stub::builds"), "mock-stub__builds");
        assert_eq!(sanitize("a/b"), "a_b");
        assert_eq!(sanitize("a\\b"), "a_b");
        assert_eq!(sanitize("a b"), "a_b");
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
        let outcome = extract(tmp.path(), Path::new("test-bin"), &env);
        match outcome {
            TestOutcome::Skipped { reason } => assert_eq!(reason, "no profraw generated"),
            TestOutcome::Collected { .. } => panic!("expected Skipped, got Collected"),
        }
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
        let reason = skip_reason(extract(&profraw_dir, &binary, &env));
        assert!(
            reason.contains("integration-abc123.json"),
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

        let map = coverage::BinaryFunctionMap {
            binary: coverage::BinaryStamp::of(&binary).unwrap(),
            functions: [("f".to_string(), Vec::new())].into_iter().collect(),
        };
        let path = map_path(&env.function_maps_dir, &binary).unwrap();
        std::fs::write(&path, serde_json::to_vec(&map).unwrap()).unwrap();

        // Same name, different build.
        std::fs::write(&binary, b"a longer pretend binary than before").unwrap();

        let reason = skip_reason(extract(&profraw_dir, &binary, &env));
        assert!(
            reason.contains("rebuilt after its function map was exported"),
            "reason should name the stale map: {reason}",
        );
    }

    /// A binary path with no file name has no map to look up, and gets an
    /// error rather than a default name every such binary would share.
    #[test]
    fn map_path_is_named_for_the_binary() {
        assert_eq!(
            map_path(
                Path::new("/tmp/maps"),
                Path::new("/t/debug/deps/integration-9f2a")
            ),
            Some(PathBuf::from("/tmp/maps/integration-9f2a.json")),
        );
        assert_eq!(map_path(Path::new("/tmp/maps"), Path::new("..")), None);
    }

    /// `write_result` round-trips through the same JSON `collect` reads back,
    /// laid out two levels deep so distinct `(binary_id, test_name)` pairs
    /// never collide after sanitization.
    #[test]
    fn write_result_round_trips() {
        use camino::Utf8PathBuf;

        let tmp = tempfile::tempdir().unwrap();
        let results = tmp.path();
        let ranges: BTreeSet<HitRange> = [HitRange {
            file: Utf8PathBuf::from("src/lib.rs"),
            line_start: 3,
            line_end: 7,
        }]
        .into_iter()
        .collect();
        write_result(
            results,
            "my-crate::tests",
            "math::adds",
            TestOutcome::Collected { ranges },
        );

        let path = results.join("my-crate__tests").join("math__adds.json");
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: TestResult = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.binary_id, "my-crate::tests");
        assert_eq!(parsed.test_name, "math::adds");
        match parsed.outcome {
            TestOutcome::Collected { ranges } => {
                assert_eq!(ranges.len(), 1);
                let r = ranges.iter().next().unwrap();
                assert_eq!(r.file, "src/lib.rs");
                assert_eq!((r.line_start, r.line_end), (3, 7));
            }
            TestOutcome::Skipped { .. } => panic!("expected Collected"),
        }
    }
}
