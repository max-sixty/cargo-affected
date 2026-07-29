//! Wall-clock benchmark for `cargo affected collect`.
//!
//! ```sh
//! cargo bench --bench collect
//! ```
//!
//! ## What it measures, and why the fixture is shaped the way it is
//!
//! Collect spends its time turning each test's profile into source ranges, and
//! the thing that has historically made that expensive is the *binary's*
//! coverage-map size — the number of instrumented functions — rather than
//! anything about the test. Work proportional to the whole binary, repeated
//! per test, is the shape this benchmark exists to catch: it multiplies two
//! dimensions that a small crate has neither of.
//!
//! So the fixture is deliberately wide rather than deep: [`MODULES`] ×
//! [`FUNCTIONS_PER_MODULE`] trivial functions produce a coverage map of
//! realistic size, while [`TESTS`] tests each touch only
//! [`MODULES_PER_TEST`] modules' worth of them.
//!
//! Real numbers for calibration: worktrunk's `integration` test binary carries
//! 63,021 functions, of which a typical test hits ~2,000 (3%).
//!
//! ## Fixed vs marginal
//!
//! Not all of a collect scales with the test count: `cargo nextest list`, the
//! per-binary function-map export and cargo's own freshness check happen once
//! however many tests run. Dividing the total by [`TESTS`] would fold that in
//! and report a per-test cost that falls as `TESTS` rises with nothing else
//! changing. So the fixed part is measured directly — a collect whose filter
//! matches no test at all — and reported separately from the marginal cost the
//! change is actually about.
//!
//! ## Method
//!
//! The fixture is a self-contained crate under `target/affected-bench/fixture`
//! with its own git repo (collect refuses to run on a dirty tree). It is
//! rewritten only when its content changes, so cargo's build cache survives
//! between runs.
//!
//! The first `collect` is an untimed warm-up. It's a full collect rather than
//! a cheaper `cargo build` so it primes exactly the artifacts the timed runs
//! consume — replicating collect's RUSTFLAGS and `--target-dir` here would
//! duplicate a contract that lives in `collect.rs`.
//!
//! The timed runs are serial (`--test-threads=1`). Extraction happens inside
//! the runner shim, so nextest's concurrency divides it away — a parallel run
//! reports roughly the CPU cost over the core count, which makes the number
//! track the machine as much as the code. Serialising makes the timed number
//! the per-test cost itself: comparable across machines, and the figure that
//! extrapolates to a two-core CI runner.
//!
//! Each figure is the **minimum** of [`SAMPLES`] runs. Contention only ever
//! adds time, so the minimum is the estimator least polluted by whatever else
//! the machine is doing; every sample is printed so a spread between them
//! shows the measurement was taken under load and should be repeated.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

/// Fixture shape. The product of the first two is the coverage-map size, which
/// is the axis extraction cost scales on; `TESTS` multiplies it.
const MODULES: usize = 80;
const FUNCTIONS_PER_MODULE: usize = 250;
const TESTS: usize = 120;
/// How many modules each test exercises — sets the fraction of the coverage
/// map a single test hits (4/80 = 5%, against ~3% for a real suite).
const MODULES_PER_TEST: usize = 4;
/// Timed runs; the fastest is reported.
const SAMPLES: usize = 3;

fn main() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("affected-bench")
        .join("fixture");
    write_fixture(&fixture);
    commit_fixture(&fixture);

    println!(
        "fixture:       {} functions across {MODULES} modules, {TESTS} tests",
        MODULES * FUNCTIONS_PER_MODULE,
    );

    let warm = collect(&fixture, &[]);
    println!("warm-up:       {warm:.1}s (parallel, primes the build cache)");

    // `none()` matches no test, so this run pays everything a collect pays
    // except the per-test work: the listing, the function-map export, the
    // freshness check. `--no-tests=warn` keeps it a success.
    let (fixed, fixed_spread) = best_of(&fixture, &["--", "--test-threads=1", "-E", "none()"]);
    println!("fixed:         {fixed:.1}s list + map export, no tests — samples: {fixed_spread}");

    let (total, total_spread) = best_of(&fixture, &["--", "--test-threads=1"]);
    println!("collect:       {total:.1}s serial — samples: {total_spread}");
    println!(
        "per test:      {:.0} ms marginal, {:.0} ms amortized",
        (total - fixed) * 1000.0 / TESTS as f64,
        total * 1000.0 / TESTS as f64,
    );
}

/// Time [`SAMPLES`] collects; return the fastest and every sample formatted,
/// so a spread between them shows the machine was busy and the run should be
/// repeated.
fn best_of(dir: &Path, extra: &[&str]) -> (f64, String) {
    let samples: Vec<f64> = (0..SAMPLES).map(|_| collect(dir, extra)).collect();
    let spread = samples
        .iter()
        .map(|s| format!("{s:.1}"))
        .collect::<Vec<_>>()
        .join(", ");
    let best = samples.into_iter().fold(f64::INFINITY, f64::min);
    (best, spread)
}

/// Run `cargo affected collect` in `dir`, returning wall-clock seconds. Its
/// own progress output is suppressed; a failure is fatal, since a benchmark
/// that silently times an error path is worse than no benchmark.
fn collect(dir: &Path, extra: &[&str]) -> f64 {
    let start = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-affected"))
        .args(["affected", "collect"])
        .args(extra)
        .current_dir(dir)
        .output()
        .expect("failed to run cargo-affected");
    let elapsed = start.elapsed().as_secs_f64();
    assert!(
        output.status.success(),
        "collect failed:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    elapsed
}

/// Write the generated crate, touching only files whose content changed so
/// cargo's mtime-based fingerprints stay valid across runs.
fn write_fixture(dir: &Path) {
    let src = dir.join("src");
    std::fs::create_dir_all(&src).expect("failed to create fixture src dir");

    // `[workspace]` makes the fixture its own workspace root — without it
    // cargo walks up into cargo-affected's manifest and rejects the nested
    // package. `/target` and `/Cargo.lock` are ignored so collect sees a clean
    // tree on the second run.
    write_if_changed(
        &dir.join("Cargo.toml"),
        "[package]\n\
         name = \"affected_bench_fixture\"\n\
         version = \"0.1.0\"\n\
         edition = \"2021\"\n\
         \n\
         [workspace]\n",
    );
    write_if_changed(&dir.join(".gitignore"), "/target\n/Cargo.lock\n");

    let mut lib = String::new();
    for m in 0..MODULES {
        lib.push_str(&format!("pub mod m{m};\n"));
    }
    lib.push_str("pub mod tests;\n");
    write_if_changed(&src.join("lib.rs"), &lib);

    for m in 0..MODULES {
        let mut body = String::new();
        for f in 0..FUNCTIONS_PER_MODULE {
            // A branch and a loop per function so each one contributes several
            // coverage regions, as real code does.
            body.push_str(&format!(
                r#"pub fn f{f}(x: i64) -> i64 {{
    if x > {f} {{
        return x - {f};
    }}
    let mut t = 0;
    for i in 0..(x % 4) {{
        t += i * {};
    }}
    t
}}

"#,
                f + 1,
            ));
        }
        // One call site per function, so a test can reach a whole module's
        // worth of coverage map in a single line.
        body.push_str("pub fn all(x: i64) -> i64 {\n    let mut t = 0;\n");
        for f in 0..FUNCTIONS_PER_MODULE {
            body.push_str(&format!("    t += f{f}(x);\n"));
        }
        body.push_str("    t\n}\n");
        write_if_changed(&src.join(format!("m{m}.rs")), &body);
    }

    // A smaller MODULES than last run would otherwise leave orphaned m*.rs
    // files in the fixture's git repo forever.
    for entry in std::fs::read_dir(&src).expect("failed to read fixture src dir") {
        let path = entry.expect("failed to read fixture src entry").path();
        let orphan = path
            .file_stem()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix('m'))
            .and_then(|n| n.parse::<usize>().ok())
            .is_some_and(|m| m >= MODULES);
        if orphan {
            std::fs::remove_file(&path)
                .unwrap_or_else(|e| panic!("failed to remove {}: {e}", path.display()));
        }
    }

    let mut tests = String::from("#![cfg(test)]\n\n");
    for t in 0..TESTS {
        tests.push_str(&format!("#[test]\nfn test_{t}() {{\n    let mut t = 0;\n"));
        for k in 0..MODULES_PER_TEST {
            let m = (t * MODULES_PER_TEST + k) % MODULES;
            tests.push_str(&format!("    t += crate::m{m}::all({});\n", t as i64 % 7));
        }
        // The sum is never asserted on — the test exists to touch functions,
        // and black_box keeps the calls from being optimised away.
        tests.push_str("    std::hint::black_box(t);\n}\n\n");
    }
    write_if_changed(&src.join("tests.rs"), &tests);
}

/// Write `content` to `path` unless it's already there verbatim.
fn write_if_changed(path: &Path, content: &str) {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == content) {
        return;
    }
    std::fs::write(path, content)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
}

/// Initialise the fixture's git repo if needed and commit any pending change.
/// collect anchors its coverage at HEAD and refuses to run on a dirty tree, so
/// the fixture has to be a committed working tree.
fn commit_fixture(dir: &Path) {
    if !dir.join(".git").exists() {
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "bench@example.com"]);
        git(dir, &["config", "user.name", "Bench"]);
        git(dir, &["config", "core.autocrlf", "false"]);
        // A host that signs commits by default can't sign as this identity.
        git(dir, &["config", "commit.gpgsign", "false"]);
    }
    git(dir, &["add", "."]);
    let dirty = !Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(dir)
        .status()
        .expect("failed to run git diff")
        .success();
    if dirty {
        git(dir, &["commit", "-q", "-m", "fixture"]);
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| panic!("failed to run git {}: {e}", args.join(" ")));
    assert!(status.success(), "git {} failed", args.join(" "));
}
