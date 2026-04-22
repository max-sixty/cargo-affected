# TODO

## Features

- [ ] **Line/function-level granularity** — Use llvm-cov region data to track which functions each test calls, not just which files. Reduces false-positive reruns (e.g., 565 tests for any change to `config.rs`).
- [ ] **Watch mode** — `cargo affected watch` to re-run affected tests on file save (notify crate).
- [ ] **Tier B fingerprint signals** — extend `src/fingerprint.rs` if false-negatives appear. Candidates: `.cargo/config.toml` (workspace + `$CARGO_HOME`), `rust-toolchain.toml`, `build.rs` contents. Tier A (Cargo.lock, Cargo.toml, `rustc -vV`, RUSTFLAGS, CARGO_BUILD_TARGET) was chosen to minimize false invalidations; add these only when a missed invalidation is observed in practice.

## Known limitations

- **Same-fingerprint branches share a cache slot.** Coverage is keyed by build environment (Cargo.lock, workspace Cargo.toml files, rustc version, RUSTFLAGS, CARGO_BUILD_TARGET), not by source tree. Two branches that differ only in source — same deps, same toolchain — hash to the same fingerprint, so a full `collect` on one wipes the other's data. Branches with different deps or toolchains are unaffected (they get separate fingerprints). Sidestep: use separate git worktrees — each has its own `target/affected/coverage.db`. `collect --diff-base` also mitigates it by only replacing rows for reran tests.
