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

Sampled `llvm-cov export` for one real test from worktrunk (`commands::alias::tests::test_parse_errors`):

- 6 project functions hit out of 67,253 total functions in the binary — typical tests touch a tiny slice; each hit function spans ~1–7 source lines
- Per-test JSON: 44 MB default, 30 MB with `--ignore-filename-regex=/rustc/|/\.cargo/|/target/` (~32% reduction). The filter shrinks `data[0].files[]` (1234 → 113 entries) but does **not** shrink `data[0].functions[]` (55,950 → 55,950). The 30 MB residual is dominated by `functions[]` (32.6 MB on this sample) — most of those have `count=0` and are filtered downstream
- llvm-cov export: ~1.4 s wall (already the bottleneck; parsing more of the same JSON is ~free)

Reference DB at file-granularity (older fingerprint-free schema): 9.6 MB, 40 K rows, 3,141 tests, avg 12.8 files/test. Function-level extrapolation: ~150–250 K rows, 40–60 MB on disk.

`functions[i].regions[]` (with `count > 0`) is the field we want; per `(function, file_id)` we take `min(line_start)..max(line_end)` to get the source extent. No Rust parser (`syn`, `rust-analyzer`) needed.

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
- `db.rs` — schema migration; new range-overlap query (`source_file=? AND line_start<=? AND line_end>=?` per changed hunk) plus a per-file fallback when no range overlaps. Add `collect_sha` column to `fingerprints` (drop+recreate on missing column, same pattern as the existing `migrate_legacy_test_files`).
- `project.rs` — add `git_changed_line_ranges` parsing `git diff -U0 --no-color --no-ext-diff --` against `<collect_sha>`. Hard-error on git failures (see `git_changed_files` for the existing pattern). Drop the `diff_base` parameter from `git_changed_files`.
- `run.rs` / `status.rs` / `selection.rs` — for each changed file, union stored function ranges overlapping changed hunks → tests to run. Delete `--diff-base` flag handling.
- `collect.rs` — capture HEAD sha at collect time and store it on the fingerprint row. Delete `--diff-base` (incremental collect) flag and `select_tests_for_incremental`.

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

If a user has committed since `collect`, ranges may have drifted off their stored coordinates and the `git diff <collect_sha>` view becomes increasingly noisy as committed changes accumulate; the cure is to recollect. We don't error in that case — we just do more work than strictly necessary.

### Resolved design decisions

1. **No `--diff-base` flag in v1.** Removed from `collect`, `run`, and `status`. The diff base is implicitly the per-fingerprint `collect_sha`; "compare against branch" is gone until we have proper SHA-translation. Keeps per-fingerprint `collect_sha` coherent without per-row tracking, range translation, or "force full recollect" logic.

2. **Per-hunk file-level backstop.** Correctness floor for struct-field, `#[derive]`, const, `use`, and `mod` edits that fall outside any LLVM region. Per hunk: if no stored range overlaps, union in every test with rows for the file under the current fingerprint. Net effect: function-level wins on body edits, equals file-level on structural-only edits. Two queries per file (range-overlap, then fallback) — the second is a strict superset of the first.

3. **One diff command, one rule.** `git diff -U0 --no-color --no-ext-diff <collect_sha> -- <files>` (working tree as new). OLD-side line numbers always (= collect_sha = storage). No mode-dependent sides; no three-dot. Pure insertions (`@@ -A,0 +B,N @@`) are treated as the single-line range `[A, A]`.

4. **Crate roots ride the same table.** `lib.rs` / `main.rs` / `tests/*.rs` are stored with sentinel range `(line_start=1, line_end=i64::MAX)` in `test_regions`. Any hunk in those files overlaps the sentinel, selecting every test that covered the crate root — same effect as the old per-test implicit dep, no special-case branch.

5. **Index `(source_file, env_fingerprint, line_start, line_end)`.** A composite index can use equality + one range bound but not two simultaneously. Ship as-is; benchmark and revisit if `status`/`run` latency becomes a problem on real workspaces.

## Known limitations

- **Same-fingerprint branches share a cache slot.** Coverage is keyed by build environment (Cargo.lock, workspace Cargo.toml files, rustc version, RUSTFLAGS, CARGO_BUILD_TARGET), not by source tree. Two branches that differ only in source — same deps, same toolchain — hash to the same fingerprint, so a full `collect` on one wipes the other's data. Branches with different deps or toolchains are unaffected (they get separate fingerprints). Sidestep: use separate git worktrees — each has its own `target/affected/coverage.db`. `collect --diff-base` also mitigates it by only replacing rows for reran tests.
