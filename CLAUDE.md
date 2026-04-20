# cargo-difftest

Like pytest-testmon for Rust. Uses LLVM coverage to map each test to the source files it touches, then reruns only tests affected by git changes.

## Build and test

```sh
cargo clippy --all-targets   # lint
cargo test                   # unit tests only
cargo test -- --ignored      # integration test (slow — runs coverage builds)
```

Requires `rustup component add llvm-tools` for coverage collection.

## Architecture

- `main.rs` — CLI via clap. Four subcommands: `collect`, `run`, `status`, `clean`. Also dispatches the hidden `runner-shim` argv before clap sees it (see `shim.rs`).
- `project.rs` — Project root detection via `cargo metadata` and git queries. `find_project_root()` returns a `ProjectRoot` with the workspace root for git ops and DB storage. `git_changed_files()` returns changed files via git diff or three-dot diff.
- `collect.rs` — Coverage pipeline. Builds with `-C instrument-coverage` via `cargo test --no-run`, then runs tests through `cargo nextest run` with `CARGO_TARGET_<TRIPLE>_RUNNER` set to `cargo-difftest runner-shim`; a post-run worker pool reads each per-test `meta` sidecar, merges profraws via `llvm-profdata`, exports coverage via `llvm-cov`, and writes test-to-file mappings to SQLite.
- `shim.rs` — Hidden `runner-shim` subcommand invoked per test by nextest: recovers the test name from `--exact`, points `LLVM_PROFILE_FILE` at a per-test subdirectory under `DIFFTEST_PROFRAW_BASE`, writes a `meta` sidecar (test name + binary path), and `execvp`s the real test binary. Unix-only.
- `coverage.rs` — Parses `llvm-cov export` JSON to extract covered source file paths.
- `fingerprint.rs` — SHA-256 hex of `Cargo.lock`, every workspace `Cargo.toml`, `rustc -vV`, `RUSTFLAGS`, and `CARGO_BUILD_TARGET`; stored alongside each mapping so queries scoped to the current fingerprint naturally miss when any tracked input changes — no explicit invalidation path.
- `db.rs` — SQLite storage at `target/difftest/coverage.db`. All difftest artifacts (DB + profraw dirs) live under `target/difftest/`, which cargo clean wipes. Schema: `test_files(test_name, source_file, env_fingerprint)` keyed on all three, with `idx_source_file_fp(source_file, env_fingerprint)` for fast lookups; every query is scoped by fingerprint, so stale environments read as "no data". `meta` table for timestamps.
- `run.rs` — Queries DB for tests covering changed files, runs them via nextest (preferred) or cargo test.
- `status.rs` — Dry-run variant of `run` — shows what would run without executing.

## Manual testing

```sh
cd /tmp && mkdir difftest-sample && cd difftest-sample
cargo init --lib
# add some modules with tests
git init && git add . && git commit -m init
cargo-difftest difftest collect
# modify a file
cargo-difftest difftest status
cargo-difftest difftest run
```

## Workspace support

For workspace projects, `find_project_root()` uses `cargo metadata` to determine the workspace root. The workspace root is used for git operations and the DB. `cargo test --no-run --message-format=json` naturally lists binaries from all member crates.
