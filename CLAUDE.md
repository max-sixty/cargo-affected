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

- `main.rs` — CLI via clap. Three subcommands: `collect`, `run`, `status`.
- `project.rs` — Project root detection via `cargo metadata` and git queries. `find_project_root()` returns a `ProjectRoot` with the workspace root for git ops and DB storage. `git_changed_files()` returns changed files via git diff or three-dot diff.
- `collect.rs` — Coverage pipeline. Builds with `-C instrument-coverage`, discovers test binaries from cargo JSON output, runs tests in parallel (one process per test with per-test profraw dir), merges profraw via `llvm-profdata`, exports coverage via `llvm-cov`, stores test-to-file mappings in SQLite.
- `coverage.rs` — Parses `llvm-cov export` JSON to extract covered source file paths.
- `db.rs` — SQLite storage at `.difftest.db`. Schema: `test_files(test_name, source_file)` with index on `source_file` for fast lookups. `meta` table for timestamps.
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
