# cargo-affected

Like pytest-testmon for Rust. Uses LLVM coverage to map each test to the source-line ranges it touches, then reruns only tests whose ranges overlap git diff hunks.

## Build and test

```sh
cargo clippy --all-targets   # lint
cargo test                   # unit + functional integration tests
cargo bench --bench collect  # wall-clock benchmark for `collect`
```

Requires `rustup component add llvm-tools` and `cargo-nextest` — both used by
the functional test suite.

`benches/collect.rs` generates a deliberately *wide* crate (20,000 functions,
120 tests) under `target/affected-bench/` and times `collect` over it, because
collect's per-test cost scales with the binary's coverage-map size rather than
with the test. A benchmark on a small crate measures nothing. It reports the
fastest of several serial runs; see the file's module docs for why.

## Architecture

- `main.rs` — CLI via clap. Subcommands: `collect` (with `--diff`), `run`, `status`, `clean`. Also dispatches the hidden `runner-shim` argv before clap sees it.
- `project.rs` — Project root detection (`cargo metadata`) and git queries (changed files, line ranges per sha, HEAD reachability).
- `collect.rs` — Coverage pipeline: `cargo nextest list` → per-binary coverage maps → instrumented `nextest run` via the runner shim → SQLite write. Extraction is split by what varies: `write_function_maps` exports each participating binary's function→line map **once** (`llvm-cov export` against an empty profile — the whole-binary walk that used to run per test), and the per-test half is **inlined into the shim** (see `shim.rs`), which merges its own profraw, joins it against the map and deletes the bundle before exiting, so peak disk usage is bounded by nextest's own concurrency (O(test-threads × per-test size) instead of O(whole-suite size)) with no external watcher or completion heuristic. After `nextest run` exits, `collect` reads the per-test `TestResult` files the shims wrote under `CARGO_AFFECTED_RESULTS_DIR`, folds in each test's crate-root sentinels, and writes the rows. `--diff` reuses the same pipeline, restricting the run to the affected + new test set. Also owns the test-selection plumbing shared with `run.rs`: `nextest_filter_expr` builds the filterset, `write_nextest_config` hands it to nextest as a `default-filter` in a generated config file (passed via `--config-file`) so an arbitrarily large selection never overflows the OS command-line limit.
- `shim.rs` — Hidden `runner-shim` invoked per test by nextest. Sets a per-test `LLVM_PROFILE_FILE`, spawns and waits for the real binary, then extracts its coverage (`llvm-profdata merge --text`, joined against the binary's map from `CARGO_AFFECTED_FUNCTION_MAPS_DIR`), writes a `TestResult` JSON file, and deletes the per-test profraw dir. Waiting (rather than `execvp`) is what lets extraction run in-process; nextest signals each test's process group on cancellation, so the spawned child is reaped without the shim forwarding signals.
- `coverage.rs` — Both halves of the join: `build_function_map` parses `llvm-cov export` JSON into a per-binary `{function → (file, line_start, line_end)}` map, `executed_functions` parses `llvm-profdata merge --text` into the names one test reached, `hit_ranges` joins them. The map holds each function's full extent where a per-test export would hold its executed extent, so ranges are a superset — the module docs quantify how much (7 lines across worktrunk's 34,175) and why the direction is the safe one.
- `fingerprint.rs` — SHA-256 of `Cargo.lock`, workspace `Cargo.toml`s, `rustc -vV`, `RUSTFLAGS`, `CARGO_BUILD_TARGET`. Queries scoped to the current fingerprint naturally miss when any tracked input changes — no explicit invalidation path. `[*.metadata]` tables are stripped from each manifest before hashing (cargo ignores them for builds), so editing `[workspace.metadata.affected]` rules is cache-neutral; metadata-free manifests hash by raw bytes so there's no churn on upgrade.
- `db.rs` — SQLite at `target/affected/coverage.db`. `test_regions` rows carry a per-row `collect_sha` so `--diff` can leave unaffected tests anchored at their original sha while re-anchoring rerun tests at the new HEAD; multiple shas can coexist for one fingerprint. Diverged-sha rows linger until `cargo affected clean`. Crate roots ride the same table with sentinel `(1, i64::MAX)` ranges (the structural-edit backstop). Legacy schemas drop on open.
- `selection.rs` — Shared between `run`, `status`, and `collect --diff`. Owns reachability classification (`check_shas_reachable`), per-sha diff collection (`changed_ranges_per_sha`), the divergence notice, and the selection itself: affected + config + new + stranded, where config hits (from `config.rs`) are a disjoint category that never inflates the coverage-overlap counts.
- `config.rs` — Declarative input→test rules from `[workspace.metadata.affected]` (or `[package.metadata.affected]` for single-crate projects). Each `[[rule]]` pairs input globs with a nextest filterset; when a changed path matches, `cargo nextest list -E` resolves the filterset and those tests are force-selected. Closes the blind spot where a test reads a non-Rust file at runtime (an insta `.snap`, a doc `.md`) that has no coverage row, so a change to it would otherwise select no test. No rules means no extra `nextest list` call.
- `plan.rs` — The decision `run` and `status` share, held in one place so a dry run can't predict something other than what runs: list tests, diff against every reachable `collect_sha`, apply config rules, select, classify `hit-exact` vs `hit-with-divergence`, and assemble either report shape. It was two hand-maintained copies annotated "mirrors the other", and they had drifted — `run` listed with the caller's build flags while `status` listed with none, so a feature-gated test was invisible to `status` and visible to `run`. `run.rs` and `status.rs` now hold only what differs for real: which stream, which tense, and whether anything gets executed at the end.
- `run.rs` — `collect_shas` → reachability → [`plan`] → `nextest run` against the generated filter config. Widens to all tests only when every sha is diverged.
- `status.rs` — The conditional-tense rendering of the same plan, plus a database inventory on stdout. Takes the same post-`--` passthrough as `run`, because the build flags decide which tests exist to predict about.

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
