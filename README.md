# cargo-affected

Like pytest-testmon for Rust. Maps each test to the source-line ranges it
touches via LLVM coverage, then reruns only the tests whose ranges overlap
`git diff` hunks.

> **Status: extremely early.** Mac & linux only at this stage. The author is
> starting to use this in their own repos; others probably shouldn't yet.
> The schema can change without migration, behavior may break, and there is
> no support promise. CI should still run the full test suite.

## Installation

Not on crates.io yet. Install from source:

```sh
git clone https://github.com/max-sixty/cargo-affected
cd cargo-affected
cargo install --path .
rustup component add llvm-tools
```

## Quick start

```sh
# Build with coverage instrumentation and record what each test touches.
cargo affected collect

# After editing:
cargo affected run        # run only tests overlapping the diff
cargo affected status     # dry-run: show what would run
cargo affected clean      # wipe the coverage cache
```

`run` diffs the working tree against the git sha that was HEAD when
`collect` last ran. Recollect periodically — every committed change since
the last collect adds to the diff and broadens selection.

When `run` can't compute a precise selection — no coverage yet, environment
fingerprint changed, missing or unreachable `collect_sha` — it falls back to
running every test with a stderr notice. `cargo affected run` is therefore a
strict superset of `cargo nextest run`: always at least as safe.

## How it works

`collect`:

1. `cargo nextest list` enumerates every test.
2. `cargo nextest run` runs them with `-C instrument-coverage` and a
   per-test `LLVM_PROFILE_FILE`.
3. For each test, `llvm-profdata` merges its profraw and `llvm-cov export`
   lists every hit function with its source-line regions.
4. Per `(test, file, function)`, the min/max line span is stored in
   `target/affected/coverage.db` (SQLite), keyed by a fingerprint of
   `Cargo.lock`, all workspace `Cargo.toml`s, `rustc -vV`, `RUSTFLAGS`, and
   `CARGO_BUILD_TARGET`. The git HEAD sha is recorded alongside.

`run`:

1. `git diff -U0 <collect_sha>` produces OLD-side line ranges for changed
   files — same coordinate system as storage.
2. For each changed file, the DB returns stored function ranges overlapping
   any hunk; the union of matching tests is run via `cargo nextest run`.
3. If a hunk overlaps no stored range (struct fields, `#[derive]`, `const`,
   `use`, `mod`), a per-file backstop selects every test that touched the
   file. Crate roots (`lib.rs` / `main.rs` / `tests/*.rs`) are stored with a
   sentinel range covering the whole file, scoped per nextest target. An
   edit to a crate root reselects every test in that target, every test in
   the same package that links the lib (bins, integration tests), and every
   test in workspace packages that transitively depend on it.

## Accuracy model

`cargo affected run` is an approximation — it trades correctness for speed.
CI should still run the full suite.

### False positives (tests selected that didn't need to run)

- **Function-level granularity.** A hit function's full line span is treated
  as one range, so an edit anywhere inside it reruns every test that touched
  the function — even if the edited line is unreachable from those tests.
- **Structural-edit backstop.** Hunks outside any LLVM region (struct
  fields, derives, consts, `use`, `mod`) reselect every test that touched
  the file.
- **Crate roots.** Any edit to `lib.rs` / `main.rs` / `tests/*.rs` reruns
  every test in that target, every test in the same package that links the
  lib, and every test in workspace packages that transitively depend on it.
- **Comment- and whitespace-only edits.** Selection diffs lines, not
  semantics.

### False negatives (tests skipped that should have run)

- **Non-Rust sources.** `include_str!` / `include_bytes!` targets, SQL
  files, migrations, assets, and templates aren't seen by llvm-cov.
- **Build-time inputs not in the fingerprint.** The fingerprint covers
  `Cargo.lock`, workspace `Cargo.toml`s, `rustc -vV`, `RUSTFLAGS`, and
  `CARGO_BUILD_TARGET`. Changes to `build.rs`, `rust-toolchain.toml`, or
  `.cargo/config.toml` don't currently invalidate the cache.
- **Proc-macro crate source.** A proc-macro's own source files compile into
  a host dylib, not the test binary, so editing a proc-macro crate won't
  reselect its downstream tests.
- **External state.** Tests that read env vars, filesystem state, or the
  network can change outcome without any source file changing.

When in doubt, `cargo affected collect` to refresh coverage, or skip
cargo-affected and run the full suite.

## License

Dual-licensed under MIT or Apache-2.0 at your option.
