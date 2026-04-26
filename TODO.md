# TODO

## Features

- [ ] **Function-level coverage tracking** — replace file-level mappings with per-function line ranges to cut false-positive reruns. Plan below.
- [ ] **Watch mode** — `cargo affected watch` to re-run affected tests on file save (notify crate).
- [ ] **Tier B fingerprint signals** — extend `src/fingerprint.rs` if false-negatives appear. Candidates: `.cargo/config.toml` (workspace + `$CARGO_HOME`), `rust-toolchain.toml`, `build.rs` contents. Tier A (Cargo.lock, Cargo.toml, `rustc -vV`, RUSTFLAGS, CARGO_BUILD_TARGET) was chosen to minimize false invalidations; add these only when a missed invalidation is observed in practice.

## Function-level coverage tracking (plan)

Today every test row stores `(binary_id, test_name, source_file, env_fingerprint)`. Any edit to a tracked file reruns every test that touched it. On worktrunk-scale projects, a single edit to `config.rs` reruns ~565 tests, most of which don't touch the changed function.

Plan: store function-level line ranges instead. Realistic expected reduction on shared files is **~5–8×** — less than a naive raw multiplier suggests, because structural edits (struct fields, derives, consts, `mod`/`use`) fall outside any LLVM region and trigger a file-level backstop.

### Scope

Function-level only, not line-level. Function-level captures the bulk of the win at a fraction of the complexity: no per-line bitmap, no sub-function hunk parsing, and line drift across commits becomes benign over-selection rather than a correctness problem. Line-level can be revisited if measurement shows function-level is insufficient.

### Empirical anchor

Sampled `llvm-cov export` for one real test from worktrunk (`cli::hook::tests::test_parse_errors`):

- 13 project functions hit out of 3,892 — typical tests touch a tiny slice
- Per-test JSON: 56 MB default, 38 MB with `--ignore-filename-regex`, 0.7 MB with `--summary-only` (loses what we need)
- llvm-cov export: ~1.4 s wall (already the bottleneck; reading more of the same JSON is ~free)

Reference DB at file-granularity (older fingerprint-free schema): 9.6 MB, 40 K rows, 3,141 tests, avg 12.8 files/test. Function-level extrapolation: ~150–250 K rows, 40–60 MB on disk.

`functions[i].regions[]` (with `count > 0`) is the field we want; `min(line_start)..max(line_end)` per function gives the source extent. No Rust parser (`syn`, `rust-analyzer`) needed.

### Schema sketch

```sql
test_regions(
    binary_id TEXT NOT NULL,
    test_name TEXT NOT NULL,
    source_file TEXT NOT NULL,
    line_start INTEGER NOT NULL,
    line_end INTEGER NOT NULL,
    env_fingerprint TEXT NOT NULL
);
CREATE INDEX idx_source_fp_range
    ON test_regions(source_file, env_fingerprint, line_start, line_end);
```

One row per hit function per test. Dedupe by `(file, line_start, line_end)` at collect time so generic monomorphizations collapse to a single row. Migration drops the old `test_files` table — same idiom as the existing `migrate_pre_fingerprint_schema`.

### Pipeline changes

- `coverage.rs` — return `Vec<(file, line_start, line_end)>` derived from hit functions; replace the filename-set extraction.
- `db.rs` — schema migration; new range-overlap query (`source_file=? AND line_start<=? AND line_end>=?` per changed hunk).
- `project.rs` — add `git_changed_line_ranges` parsing `git diff -U0 --no-color --no-ext-diff --`. Hard-error on git failures rather than swallowing exit codes (see `git_changed_files` for the current pattern to *not* repeat).
- `run.rs` / `status.rs` / `selection.rs` — for each changed file, union stored function ranges overlapping changed hunks → tests to run.

Estimated delta: ~500 LOC across 4–5 files.

### Line-drift across commits

Store `collect_sha` so changed-line ranges are computed against the snapshot the DB was written for, not against current HEAD:

- At collect: record `git rev-parse HEAD`.
- At run: diff `<collect_sha>..workingtree`. Resulting hunks are in the same coordinate system as stored function ranges.

Function moves under this scheme:

| edit | result |
|---|---|
| 10 lines inserted above `foo` | diff hits top of file; no overlap with `foo`'s stored range; T not selected (correct — `foo` unchanged) |
| edit inside `foo` body | diff overlaps stored range; T selected |
| `foo` moved within file | diff shows delete + insert at old/new sites; overlap on old site selects T (over-select, safe) |
| `foo` renamed (signature line edited) | overlap on signature → T selected |
| helper extracted from `foo` | edit lands inside `foo` → T selected; next collect adds helper row |
| extra generic monomorphization | dedupes to same source range → no DB change |

If `collect_sha` is unreachable (rebased away, shallow clone): refuse function-level selection and tell the user to recollect. No silent fallback — consistent with the project's fail-loud principle.

### Open issues to resolve before implementing

1. **`collect --diff-base` (incremental) breaks per-fingerprint `collect_sha`.** Incremental collect updates only selected test rows; remaining rows keep their original snapshot coordinates, so a single per-fingerprint SHA would be wrong for them. Options: (a) disable incremental for v1, (b) force full recollect when SHA would change, (c) store SHA per row.

2. **File-level backstop for non-region edits.** Edits to struct fields, `#[derive]`, consts, `use` statements, `mod` declarations, and signatures outside any LLVM region yield no function overlap. Without a backstop those would select zero tests — a regression vs. today. Per-hunk rule: if a hunk overlaps no stored function range for the file, union in all tests that ever covered the file. Costs the win on structural edits; guarantees we never miss. Net effect: function-level wins on body edits, equals file-level on structural-only edits.

3. **Diff coordinate side per mode.** Old-side vs. new-side matters and isn't symmetric across default working-tree `run`, `run --diff-base`, and `collect --diff-base`. Specify each before implementing.

4. **`--ignore-filename-regex` is POSIX ERE** — no negative lookahead, so "everything outside the project root" must be enumerated (e.g., `/rustc/|.cargo/|target/`) or replaced with an inclusive include-path filter. Worth verifying that the filter shrinks `functions[]` not just `files[]`. Orthogonal to function-level itself but a ~50× JSON-size win on the sample test (56 MB → ~1 MB).

5. **Crate roots stay coarse.** `lib.rs` / `main.rs` / `tests/*.rs` are added as implicit deps for every test today (`collect.rs`). They're almost entirely structural; function-level would gain nothing. Keep the implicit-dep behavior unchanged for these files.

6. **Index efficiency.** A composite index gives equality prefix + one range bound, not two simultaneously. Benchmark with a synthetic 500K–1M-row table before trusting `status`/`run` latency.

## Known limitations

- **Same-fingerprint branches share a cache slot.** Coverage is keyed by build environment (Cargo.lock, workspace Cargo.toml files, rustc version, RUSTFLAGS, CARGO_BUILD_TARGET), not by source tree. Two branches that differ only in source — same deps, same toolchain — hash to the same fingerprint, so a full `collect` on one wipes the other's data. Branches with different deps or toolchains are unaffected (they get separate fingerprints). Sidestep: use separate git worktrees — each has its own `target/affected/coverage.db`. `collect --diff-base` also mitigates it by only replacing rows for reran tests.
