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

- `main.rs` — CLI via clap. Subcommands: `collect` (with `--diff`), `run`, `status`, `clean`. Also dispatches the hidden `runner-shim` argv before clap sees it.
- `project.rs` — Project root detection (`cargo metadata`) and git queries (changed files, line ranges per sha, HEAD reachability).
- `collect.rs` — Coverage pipeline: `cargo nextest list` → instrumented `nextest run` via the runner shim → SQLite write. Extraction is **inlined into the shim** (see `shim.rs`): each test's shim merges/exports/parses its own profraw and deletes the bundle before exiting, so peak disk usage is bounded by nextest's own concurrency (O(test-threads × per-test size) instead of O(whole-suite size)) with no external watcher or completion heuristic. After `nextest run` exits, `collect` reads the per-test `TestResult` files the shims wrote under `CARGO_AFFECTED_RESULTS_DIR`, folds in each test's crate-root sentinels, and writes the rows. `--diff` reuses the same pipeline, restricting the run to the affected + new test set. Also owns the test-selection plumbing shared with `run.rs`: `nextest_filter_expr` builds the filterset, `write_nextest_config` hands it to nextest as a `default-filter` in a generated config file (passed via `--config-file`) so an arbitrarily large selection never overflows the OS command-line limit.
- `shim.rs` — Hidden `runner-shim` invoked per test by nextest. Sets a per-test `LLVM_PROFILE_FILE`, spawns and waits for the real binary, then extracts its coverage (`llvm-profdata`/`llvm-cov`), writes a `TestResult` JSON file, and deletes the per-test profraw dir. Waiting (rather than `execvp`) is what lets extraction run in-process; nextest signals each test's process group on cancellation, so the spawned child is reaped without the shim forwarding signals.
- `coverage.rs` — Parses `llvm-cov export` JSON into `(file, line_start, line_end)` ranges per hit function.
- `fingerprint.rs` — SHA-256 of `Cargo.lock`, workspace `Cargo.toml`s, `rustc -vV`, `RUSTFLAGS`, `CARGO_BUILD_TARGET`. Queries scoped to the current fingerprint naturally miss when any tracked input changes — no explicit invalidation path. `[*.metadata]` tables are stripped from each manifest before hashing (cargo ignores them for builds), so editing `[workspace.metadata.affected]` rules is cache-neutral; metadata-free manifests hash by raw bytes so there's no churn on upgrade.
- `db.rs` — SQLite at `target/affected/coverage.db`. `test_regions` rows carry a per-row `collect_sha` so `--diff` can leave unaffected tests anchored at their original sha while re-anchoring rerun tests at the new HEAD; multiple shas can coexist for one fingerprint. Diverged-sha rows linger until `cargo affected clean`. Crate roots ride the same table with sentinel `(1, i64::MAX)` ranges (the structural-edit backstop). Legacy schemas drop on open.
- `selection.rs` — Shared between `run`, `status`, and `collect --diff`. Owns reachability classification (`check_shas_reachable`), per-sha diff collection (`changed_ranges_per_sha`), the divergence notice, and the selection itself: affected + config + new + stranded, where config hits (from `config.rs`) are a disjoint category that never inflates the coverage-overlap counts.
- `config.rs` — Declarative input→test rules from `[workspace.metadata.affected]` (or `[package.metadata.affected]` for single-crate projects). Each `[[rule]]` pairs input globs with a nextest filterset; when a changed path matches, `cargo nextest list -E` resolves the filterset and those tests are force-selected. Closes the blind spot where a test reads a non-Rust file at runtime (an insta `.snap`, a doc `.md`) that has no coverage row, so a change to it would otherwise select no test. No rules means no extra `nextest list` call.
- `run.rs` — `collect_shas` → reachability → per-sha `git diff -U0` → selection → `nextest run` against the generated filter config. Widens to all tests only when every sha is diverged.
- `status.rs` — Dry-run variant of `run`.

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
# commit the change, edit a function, then update only the affected rows
git add . && git commit -m edit
cargo affected collect --diff
```

## Workspace support

For workspace projects, `find_project_root()` uses `cargo metadata` to determine the workspace root. The workspace root is used for git operations and the DB. `cargo test --no-run --message-format=json` naturally lists binaries from all member crates.
