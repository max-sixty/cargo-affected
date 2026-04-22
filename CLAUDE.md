# cargo-affected

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
- `collect.rs` — Coverage pipeline. First, `cargo nextest list --message-format json` enumerates every testcase and gives us a `binary_path → binary_id` map (written to `binary_map.json`). Then `cargo nextest run` with `-C instrument-coverage` runs the tests via `CARGO_TARGET_<TRIPLE>_RUNNER` set to `cargo-affected runner-shim`, with `CARGO_AFFECTED_BINARY_MAP` pointing at that JSON. A post-run worker pool reads each per-test `meta` sidecar, merges profraws via `llvm-profdata`, exports coverage via `llvm-cov`, and writes `(binary_id, test_name)`-keyed mappings to SQLite.
- `shim.rs` — Hidden `runner-shim` subcommand invoked per test by nextest: recovers the test name from `--exact`, looks up the invoking binary's `binary_id` in the map at `CARGO_AFFECTED_BINARY_MAP`, points `LLVM_PROFILE_FILE` at a per-test subdirectory (named `<binary_id>__<test_name>`) under `CARGO_AFFECTED_PROFRAW_BASE`, writes a `meta` sidecar (test name + binary path + binary_id), and `execvp`s the real test binary. The binary_id in the subdir name is load-bearing: two tests sharing a name across different binaries would otherwise collide on disk and lose coverage. Unix-only.
- `coverage.rs` — Parses `llvm-cov export` JSON to extract covered source file paths.
- `fingerprint.rs` — SHA-256 hex of `Cargo.lock`, every workspace `Cargo.toml`, `rustc -vV`, `RUSTFLAGS`, and `CARGO_BUILD_TARGET`; stored alongside each mapping so queries scoped to the current fingerprint naturally miss when any tracked input changes — no explicit invalidation path.
- `db.rs` — SQLite storage at `target/affected/coverage.db`. All cargo-affected artifacts (DB + profraw dirs) live under `target/affected/`, which cargo clean wipes. Schema: `test_files(binary_id, test_name, source_file, env_fingerprint)` keyed on all four (binary_id is nextest's stable package-qualified id, e.g. `mock-stub::builds`), with `idx_source_file_fp(source_file, env_fingerprint)` for fast lookups; every query is scoped by fingerprint, so stale environments read as "no data". Legacy schemas (pre-fingerprint, pre-binary_id) are dropped on open — old rows can't be retroactively tagged and `target/affected/` is cargo-clean territory. `meta` table for timestamps.
- `run.rs` — Queries DB for tests covering changed files and runs them via `cargo nextest run` with an exact-match `-E` filter expression.
- `status.rs` — Dry-run variant of `run` — shows what would run without executing.

## Principles

This is an early-stage project. Prefer failing loudly over silently degrading.
Do **not** add fallback paths (e.g., "try tool X, else tool Y") — they mask
missing dependencies and double the surface area we have to reason about.
If a required tool isn't installed, error out with an install hint and let the
user fix it.

## Manual testing

```sh
cd /tmp && mkdir affected-sample && cd affected-sample
cargo init --lib
# add some modules with tests
git init && git add . && git commit -m init
cargo-affected affected collect
# modify a file
cargo-affected affected status
cargo-affected affected run
```

## Workspace support

For workspace projects, `find_project_root()` uses `cargo metadata` to determine the workspace root. The workspace root is used for git operations and the DB. `cargo test --no-run --message-format=json` naturally lists binaries from all member crates.
