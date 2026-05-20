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
- `collect.rs` — Coverage pipeline: `cargo nextest list` → instrumented `nextest run` via the runner shim → per-test profraw extraction with `llvm-profdata`/`llvm-cov` → SQLite write. `--diff` reuses the same pipeline, restricting the run to the affected + new test set. Also owns the test-selection plumbing shared with `run.rs`: `nextest_filter_expr` builds the filterset, `write_nextest_config` hands it to nextest as a `default-filter` in a generated config file (passed via `--config-file`) so an arbitrarily large selection never overflows the OS command-line limit.
- `shim.rs` — Hidden `runner-shim` invoked per test by nextest. Sets a per-test `LLVM_PROFILE_FILE`, writes a meta sidecar, `execvp`s the real binary. Unix-only.
- `coverage.rs` — Parses `llvm-cov export` JSON into `(file, line_start, line_end)` ranges per hit function.
- `fingerprint.rs` — SHA-256 of `Cargo.lock`, workspace `Cargo.toml`s, `rustc -vV`, `RUSTFLAGS`, `CARGO_BUILD_TARGET`. Queries scoped to the current fingerprint naturally miss when any tracked input changes — no explicit invalidation path.
- `db.rs` — SQLite at `target/affected/coverage.db`. `test_regions` rows carry a per-row `collect_sha` so `--diff` can leave unaffected tests anchored at their original sha while re-anchoring rerun tests at the new HEAD; multiple shas can coexist for one fingerprint. Diverged-sha rows linger until `cargo affected clean`. Crate roots ride the same table with sentinel `(1, i64::MAX)` ranges (the structural-edit backstop). Legacy schemas drop on open.
- `selection.rs` — Shared between `run`, `status`, and `collect --diff`. Owns reachability classification (`check_shas_reachable`), per-sha diff collection (`changed_ranges_per_sha`), the divergence notice, and the affected + new + listed selection itself.
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
