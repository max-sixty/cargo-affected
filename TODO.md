# TODO

## Features

- [ ] **Line/function-level granularity** — Use llvm-cov region data to track which functions each test calls, not just which files. Reduces false-positive reruns (e.g., 565 tests for any change to `config.rs`).
- [ ] **Watch mode** — `cargo difftest watch` to re-run affected tests on file save (notify crate).
