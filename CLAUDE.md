# cargo-affected

Like pytest-testmon for Rust. Uses LLVM coverage to map each test to the source-line ranges it touches, then reruns only tests whose ranges overlap git diff hunks.

## Build and test

```sh
cargo clippy --all-targets   # lint
cargo test                   # unit + functional integration tests
```

Requires `rustup component add llvm-tools` and `cargo-nextest` — both used by
the functional test suite.

## Architecture

- `main.rs` — CLI via clap. Four subcommands: `collect`, `run`, `status`, `clean`. Also dispatches the hidden `runner-shim` argv before clap sees it (see `shim.rs`).
- `project.rs` — Project root detection via `cargo metadata` and git queries. `find_project_root()` returns a `ProjectRoot` with the workspace root. `git_changed_files()` returns the working-tree-vs-HEAD file list. `git_changed_line_ranges(project_root, collect_sha)` runs `git diff -U0 <collect_sha>` against the working tree and returns OLD-side line ranges per file — these are in the same coordinate system as the ranges stored in the DB. `git_head_sha` / `git_sha_exists` anchor the "is collect_sha still reachable?" check.
- `collect.rs` — Coverage pipeline. Captures HEAD sha up front (anchor for future diffs). `cargo nextest list --message-format json` enumerates every testcase and gives us a `binary_path → binary_id` map (written to `binary_map.json`). Then `cargo nextest run` with `-C instrument-coverage` runs the tests via `CARGO_TARGET_<TRIPLE>_RUNNER` set to `cargo-affected runner-shim`, with `CARGO_AFFECTED_BINARY_MAP` pointing at that JSON. A post-run worker pool reads each per-test `meta` sidecar, merges profraws via `llvm-profdata`, exports coverage via `llvm-cov`, parses per-function hit ranges, adds per-target crate-root sentinel ranges (own crate root + own package's lib for non-lib targets + lib roots of transitively-depended workspace packages), and writes everything to SQLite.
- `shim.rs` — Hidden `runner-shim` subcommand invoked per test by nextest: recovers the test name from `--exact`, looks up the invoking binary's `binary_id` in the map at `CARGO_AFFECTED_BINARY_MAP`, points `LLVM_PROFILE_FILE` at a per-test subdirectory (named `<binary_id>__<test_name>`) under `CARGO_AFFECTED_PROFRAW_BASE`, writes a `meta` sidecar (test name + binary path + binary_id), and `execvp`s the real test binary. The binary_id in the subdir name is load-bearing: two tests sharing a name across different binaries would otherwise collide on disk and lose coverage. Unix-only.
- `coverage.rs` — Parses `llvm-cov export` JSON to extract `(file, line_start, line_end)` ranges per hit function. Per `(function, file_id)` we take `min(line_start)..max(line_end)` over hit regions; multiple monomorphizations dedupe to the same tuple.
- `fingerprint.rs` — SHA-256 hex of `Cargo.lock`, every workspace `Cargo.toml`, `rustc -vV`, `RUSTFLAGS`, and `CARGO_BUILD_TARGET`; stored alongside each mapping so queries scoped to the current fingerprint naturally miss when any tracked input changes — no explicit invalidation path.
- `db.rs` — SQLite storage at `target/affected/coverage.db`. All cargo-affected artifacts (DB + profraw dirs) live under `target/affected/`, which cargo clean wipes. Schema: `test_regions(binary_id, test_name, source_file, line_start, line_end, env_fingerprint)` with `idx_test_regions_lookup(source_file, env_fingerprint, line_start, line_end)`. `fingerprints(fingerprint, last_seen, collect_sha)` tracks the git sha each fingerprint was collected at — diffs are computed against that sha so stored line numbers line up. `tests_covering_ranges` does range-overlap first; if a hunk overlaps no stored range it falls back to "any test with rows for the file" (the structural-edit backstop for struct fields, derives, consts, use/mod statements). Crate roots ride the same table with sentinel `(1, i64::MAX)` ranges. Legacy schemas (pre-fingerprint, pre-binary_id, pre-range) are dropped on open. `meta` table for timestamps.
- `selection.rs` — Shared between `run` and `status`. Calls `cargo nextest list` for new-test detection and `db.tests_covering_ranges` per changed file with hunk ranges from `git_changed_line_ranges`.
- `run.rs` — Looks up `collect_sha` from the DB, verifies it's still reachable in the repo, computes per-file changed line ranges via `git diff -U0 <collect_sha>`, queries the DB for overlapping tests, and runs them via `cargo nextest run` with an exact-match `-E` filter expression.
- `status.rs` — Dry-run variant of `run` — shows what would run without executing. Reports `collect_sha` along with tests/regions counts.

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
cargo affected collect
# modify a file
cargo affected status
cargo affected run
```

## Workspace support

For workspace projects, `find_project_root()` uses `cargo metadata` to determine the workspace root. The workspace root is used for git operations and the DB. `cargo test --no-run --message-format=json` naturally lists binaries from all member crates.
