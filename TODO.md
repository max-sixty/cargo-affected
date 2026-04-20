# TODO

## Features

- [ ] **Line/function-level granularity** — Use llvm-cov region data to track which functions each test calls, not just which files. Reduces false-positive reruns (e.g., 565 tests for any change to `config.rs`).
- [ ] **Watch mode** — `cargo difftest watch` to re-run affected tests on file save (notify crate).
- [ ] **Tier B fingerprint signals** — extend `src/fingerprint.rs` if false-negatives appear. Candidates: `.cargo/config.toml` (workspace + `$CARGO_HOME`), `rust-toolchain.toml`, `build.rs` contents. Tier A (Cargo.lock, Cargo.toml, `rustc -vV`, RUSTFLAGS, CARGO_BUILD_TARGET) was chosen to minimize false invalidations; add these only when a missed invalidation is observed in practice.
