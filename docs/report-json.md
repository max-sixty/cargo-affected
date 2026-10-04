# `--report-json` schema

`cargo affected run` and `cargo affected status` accept
`--report-json <PATH>` to write a structured diagnostic artifact
alongside their normal stderr output. The artifact's purpose is to make
test selection self-explanatory: which file pulled which test in by
which mechanism, and on cache misses, which build-input differs from
the closest stored snapshot.

The schema is versioned (`schema_version`); incompatible changes bump
the integer.

```sh
cargo affected status --report-json target/affected/report.json
cargo affected run    --report-json target/affected/report.json --report-detail full
```

## Output rules at a glance

- The `cache.status` value classifies the run. Six values, see
  [`cache.status`](#cachestatus).
- `current_fingerprint` and `current_components` are populated whenever
  the fingerprint was computed (essentially always).
- `stored_fingerprints` lists every stored fingerprint with a per-label
  diff against the current environment. Sorted by `(diff_count asc,
  last_seen desc)`. The first entry on a `miss-fingerprint` is the
  closest match.
- `collect_shas` is populated whenever the fingerprint matched.
- `selection.changed_files` and `selection.selected_tests` are only
  populated in selection mode. Full-suite paths emit `null` for both
  and `selection.summary.mode = "full-suite-no-listing"`.
- `selection.selected_tests` requires `--report-detail full` (the
  default `summary` keeps file-level counts only — bounded regardless of
  test-suite size).

## Top-level shape

```json
{
  "schema_version": 1,
  "cargo_affected_version": "0.1.x",
  "command": "run",
  "cache": { ... },
  "selection": { ... }
}
```

| Field | Type | Notes |
|---|---|---|
| `schema_version` | integer | Bumped on any incompatible change. |
| `cargo_affected_version` | string | Crate version that produced the report. |
| `command` | string | `"run"` or `"status"`. |
| `cache` | object | See [Cache](#cache). |
| `selection` | object | See [Selection](#selection). |

## Cache

```json
{
  "status": "hit-with-divergence",
  "current_fingerprint": "abc...",
  "current_components": [ {"label": "cargo_lock", "hash": "..."}, ... ],
  "stored_fingerprints": [ {"fingerprint": "...", "last_seen": "...", "diff_count": 0, "differing_labels": []}, ... ],
  "collect_shas": [ {"sha": "...", "relation": "reachable", "commits_ahead": 6, "row_count": 40123}, ... ]
}
```

### `cache.status`

Closed enum. Consumers should treat unknown values as forward-compatible.

| Value | Meaning |
|---|---|
| `hit-exact` | Fingerprint matched and every reachable collect_sha equals HEAD. Highest-precision selection. |
| `hit-with-divergence` | Fingerprint matched; at least one reachable sha is ahead of HEAD or at least one stored sha is missing alongside reachable ones. Selection still runs but is noisier. |
| `miss-fingerprint` | DB has rows under other fingerprints but the current fingerprint isn't among them. A build input changed. Run skips selection. |
| `miss-no-coverage` | DB has no rows at all. First-ever run, or after `cargo affected clean`. Run skips selection. |
| `miss-no-reachable-sha` | Fingerprint matched but every stored collect_sha is missing from the repo (rebased away). No usable diff anchor. Run skips selection. |
| `forced-all` | `--all` was passed. Selection skipped intentionally. |

### `cache.stored_fingerprints[]`

```json
{
  "fingerprint": "def...",
  "last_seen": "2026-05-04T17:11:00Z",
  "diff_count": 1,
  "differing_labels": ["manifest:tests/helpers/wt-perf/Cargo.toml"]
}
```

`diff_count` and `differing_labels` are computed against
`current_components` — a label appears in `differing_labels` whenever
its hash differs (or is absent from one side). On
`miss-fingerprint`, the first entry is the closest stored fingerprint
and its `differing_labels` answers "which input changed?".

### `cache.collect_shas[]`

```json
{"sha": "16c0f8a...", "relation": "reachable", "commits_ahead": 6, "row_count": 40123}
```

| Field | Notes |
|---|---|
| `relation` | `"equal"`, `"reachable"`, or `"missing"`. |
| `commits_ahead` | Present only on `"reachable"`. |
| `row_count` | `test_regions` rows anchored at this sha for the current fingerprint. |

## Selection

```json
{
  "summary": { ... },
  "changed_files": [ ... ],
  "selected_tests": [ ... ]
}
```

`changed_files` and `selected_tests` are `null` on full-suite paths.

### `selection.summary`

```json
{
  "selected": 2577,
  "affected": 2569,
  "config": 0,
  "new": 6,
  "stranded": 2,
  "skipped": 924,
  "total_reachable_known": 3493,
  "mode": "selection"
}
```

| Field | Notes |
|---|---|
| `mode` | `"selection"` or `"full-suite-no-listing"`. |
| `selected` | Union of affected + config + new + stranded. `null` on full-suite paths. |
| `affected` | In DB at a reachable sha AND a hunk overlapped a stored row. |
| `config` | Reachable-known and force-selected by a `[workspace.metadata.affected]` input rule (would otherwise have been skipped). Disjoint from `affected`. |
| `new` | Listed in nextest but not in the DB at all under this fingerprint. |
| `stranded` | In DB but only at currently-missing collect_shas. |
| `skipped` | `total_reachable_known - affected - config` (saturating). |
| `total_reachable_known` | Distinct test count under the fingerprint at reachable shas. |

### `selection.changed_files[]`

Sorted `(tests_pulled_total desc, path asc)`.

```json
{
  "path": "src/commands/alias.rs",
  "tracked_by_coverage": true,
  "hunks_by_sha": [{"sha": "16c0f8a...", "hunks": [{"start": 42, "end": 47}]}],
  "tests_pulled_total": 1240,
  "tests_pulled_by_reason": {
    "line_overlap": 180,
    "structural_backstop": 0,
    "crate_root_sentinel": 1060,
    "config_rule": 0
  }
}
```

`tracked_by_coverage` is `true` iff the file has at least one stored
`test_regions` row at a reachable sha. Non-Rust files (`.snap`, configs)
read `false` — and are exactly where `config_rule` selections show up.

`tests_pulled_by_reason` is deduplicated by **strongest reason** per
test per file: a test pulled in by both line overlap and a sentinel
counts once, classified by the strongest reason
(`line_overlap` > `structural_backstop` > `config_rule` >
`crate_root_sentinel`). `config_rule` counts tests force-selected by a
`[workspace.metadata.affected]` rule matching this path (non-zero only for the
non-Rust inputs such rules target). The four counters sum to
`tests_pulled_total`.

### `selection.selected_tests[]`

Only present when `--report-detail full` AND `mode == "selection"`.
Sorted `(binary_id, test_name)`.

```json
{
  "binary_id": "worktrunk::integration_tests",
  "test_name": "test_foo",
  "kind": "affected",
  "reasons": [
    {
      "collect_sha": "16c0f8a...",
      "file": "src/commands/alias.rs",
      "kind": "line_overlap",
      "stored_range": [100, 150],
      "matched_hunk": [42, 47]
    }
  ]
}
```

| Field | Notes |
|---|---|
| `kind` | `"affected"`, `"config_rule"`, `"new"`, or `"stranded"`. |
| `reasons` | Empty for `"new"` and `"stranded"`. For `"config_rule"`, names the triggering input path(s). Sorted `(file, kind, collect_sha)`. |
| `reasons[].kind` | `"line_overlap"`, `"structural_backstop"`, `"crate_root_sentinel"`, or `"config_rule"`. |
| `reasons[].stored_range` | `null` for `structural_backstop` and `config_rule` (no coverage row matched). |
| `reasons[].collect_sha` / `matched_hunk` | Empty / `[0, 0]` for `config_rule` (not anchored to a coverage hunk). |

## Stderr summary line

Independent of `--report-json`, every run/status command emits a final
grep-able line:

```
cargo-affected: cache=hit-exact selection=2577/3493 (74%)
cargo-affected: cache=hit-with-divergence selection=1444/3493 (41%) missing_shas=1 max_commits_ahead=9
cargo-affected: cache=miss-fingerprint mode=full-suite
cargo-affected: cache=miss-no-coverage mode=full-suite
cargo-affected: cache=miss-no-reachable-sha mode=full-suite missing_shas=3
cargo-affected: cache=forced-all mode=full-suite
```

Useful for tracking selection ratios over time without parsing the JSON.
