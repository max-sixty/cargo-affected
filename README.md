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

When the coverage cache can't anchor a precise selection — no coverage yet,
environment fingerprint changed, missing or unreachable `collect_sha` — `run`
emits a stderr notice and runs every test. `cargo affected run` is therefore
a strict superset of `cargo nextest run`: always at least as safe.

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

## Comparison with similar tools

The biggest design choice is *how* a tool decides what changed. The headline
difference vs. [pytest-testmon] (the closest analogue) is that
cargo-affected anchors selection on a git SHA: `collect` records the HEAD
sha alongside the coverage data, and `run` asks git for the diff against
it. testmon is VCS-agnostic — it stores a per-block checksum and compares
the current source's checksums against the stored ones on every test run.

|                         | cargo-affected                                                              | [pytest-testmon]                                                  | [`jest --changedSince`]                              | [Bazel] / [Buck]                       |
| ----------------------- | --------------------------------------------------------------------------- | ----------------------------------------------------------------- | ---------------------------------------------------- | -------------------------------------- |
| Test-to-code mapping    | LLVM source-based coverage                                                  | `coverage.py`                                                     | Static module-import graph                           | Declared `BUILD` deps                  |
| Granularity             | Function-level source line ranges                                           | AST blocks (function / method / class)                            | File                                                 | Target                                 |
| Change detection        | `git diff -U0 <collect_sha>` (text)                                         | AST-block checksum mismatch                                       | `git`/`hg` diff of changed files                     | Build-graph reachability               |
| Uses VCS commit data    | Yes — records HEAD sha at `collect`, diffs against it on every `run`        | No — works independently of VCS                                   | Yes — at runtime only, no stored sha                 | No                                     |
| Persistent state        | SQLite at `target/affected/coverage.db` (per-test line ranges + env fingerprint + collect_sha) | SQLite at `.testmondata` (per-test block checksums)               | None                                                 | Build graph + remote cache             |
| When state updates      | Explicit `cargo affected collect`                                           | Silently after every test run                                     | n/a                                                  | On every build                         |
| Whitespace/comment edits | Count as changes (text diff)                                                | Ignored (checksums stable across formatting)                      | Count (file mtime / diff)                            | Ignored (no source diff)               |
| Env invalidation        | Fingerprint: `Cargo.lock`, workspace `Cargo.toml`s, `rustc -vV`, `RUSTFLAGS`, `CARGO_BUILD_TARGET` | Python version, env vars, installed package versions             | n/a                                                  | Toolchain + declared inputs            |
| Falls back to full run when | Fingerprint mismatch, missing/unreachable `collect_sha`, no coverage yet | DB schema mismatch                                                | No git repo / no merge base                          | n/a                                    |

The trade-off:

- **Anchoring on a SHA** (cargo-affected) means `collect` is a separate,
  explicit step and `run` does cheap text diffs — but it depends on the
  recorded `collect_sha` still being reachable, and any commit since
  `collect` widens the diff. Whitespace and comment edits look like real
  changes because we diff text, not AST.
- **Recomputing checksums every run** (testmon) is VCS-agnostic and
  ignores cosmetic edits, at the cost of reparsing all source on every
  invocation and silently mutating the DB on every run.
- **Static-graph approaches** ([jest], [Bazel], [Buck]) skip dynamic
  coverage entirely — fast and deterministic, but conservative on
  reflection, plugin loading, and runtime dispatch, where coverage-based
  tools see the actual edges.

### Why git instead of content hashes

The obvious alternative — testmon's design — is to hash each item and
rerun any test whose dependencies' hashes changed. We track line ranges
instead because of coordinates: stored data is keyed to OLD-side line
numbers, and after any edit those don't point at the same code in the
working tree.

Bridging the two coordinate systems takes either:

- A diff in OLD-side coordinates (`git diff -U0 <collect_sha>`,
  language-agnostic), or
- An AST parse to re-find each item in current source by stable
  identity and rehash (`syn` for Rust).

Tests themselves don't need stable identity — nextest gives canonical
names, and "rerun any test in a file that changed" is a fine
concession. The coordinate problem is on the *source* side, where
dropping git means choosing between a parser and a precision drop:

|                                    | Precision | Needs parser | Needs git |
| ---------------------------------- | --------- | ------------ | --------- |
| Line ranges + `git diff` (today)   | Function  | No           | Yes       |
| Per-file content hash              | File      | No           | No        |
| Per-item content hash via `syn`    | Function  | Yes          | No        |

Git is the cheapest bridge that keeps function-level precision without
a parser. If the git dependency becomes a real constraint, per-item
hashes via `syn` are the natural next step — strictly more work, but
VCS-agnostic and robust to whitespace and comment edits.

[pytest-testmon]: https://testmon.org/
[`jest --changedSince`]: https://jestjs.io/docs/cli#--changedsince
[Bazel]: https://bazel.build/
[Buck]: https://buck2.build/
[jest]: https://jestjs.io/

## License

Dual-licensed under MIT or Apache-2.0 at your option.
