# cargo-affected

Run only the tests affected by your changes. Uses LLVM coverage to map each
test to the source files it touches, then reruns only tests affected by git
changes.

## Accuracy model

`cargo affected run` is an approximation — it trades correctness for speed.
CI should still run the full suite.

### False positives (tests selected that didn't need to run)

- **File-level granularity.** Coverage is tracked per source file, not per
  function or region. Any edit to a file reruns every test that touched it,
  even if the edit is unreachable from those tests.
- **Comment- and whitespace-only edits.** We key off `git diff --name-only`;
  we don't inspect contents.

### False negatives (tests skipped that should have run)

- **Non-Rust sources.** `include_str!` / `include_bytes!` targets, SQL files,
  migrations, assets, and templates aren't seen by llvm-cov. Editing them
  won't affect test selection.
- **Build-time inputs not in the fingerprint.** The fingerprint hashes
  `Cargo.lock`, workspace `Cargo.toml` files, `rustc -vV`, `RUSTFLAGS`, and
  `CARGO_BUILD_TARGET`. Changes to `build.rs`, `rust-toolchain.toml`, or
  `.cargo/config.toml` don't currently invalidate the cache (see TODO.md for
  the Tier B list).
- **Proc-macro crate source.** A proc-macro's own source files are compiled
  into a host dylib, not the test binary, so they don't appear in the test's
  coverage. Editing a proc-macro crate won't reselect its downstream tests.
- **External state.** Tests that read env vars, filesystem state, or the
  network can change outcome without any source file changing.

When in doubt, `cargo affected collect` to refresh coverage, or skip
cargo-affected and run the full suite.
