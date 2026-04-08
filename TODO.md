# TODO

## Simplifications

- [ ] Extract duplicated untracked-file warning — `status.rs` reimplements `warn_untracked_rs_files` from `run.rs`
- [ ] Pass `&[TestBinaryInfo]` to `discover_tests` instead of building a separate `Vec<PathBuf>`
- [ ] Unify nextest/cargo-test branching — `run_all_tests` and `run_tests` both independently check `has_nextest()` and branch; check once or merge into one function with optional filter
- [ ] Consolidate multiple `println!` calls in `status.rs` into formatted strings

## Features

- [ ] **Batch collection** — Run all tests in one nextest pass with `LLVM_PROFILE_FILE=%p-%m.profraw`, correlate PIDs to test names via `--message-format libtest-json`. Would reduce collection from O(n) sequential runs (~32 min for 2797 tests) to ~2-3 min.
- [ ] **Line/function-level granularity** — Use llvm-cov region data to track which functions each test calls, not just which files. Reduces false-positive reruns (e.g., 565 tests for any change to `config.rs`).
- [ ] **Exit code propagation** — Propagate nextest/cargo-test exit code instead of `bail!("some tests failed")`, so CI can distinguish test failures from tool errors.
- [ ] **Watch mode** — `cargo difftest watch` to re-run affected tests on file save (notify crate).
- [ ] **Parallel collection** — Even without batch mode, run N tests concurrently with separate profraw dirs. Simpler than batch but still a large speedup.
