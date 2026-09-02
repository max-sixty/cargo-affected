# Functional Tests

Everything under `tests/` is one integration binary: `tests/functional/main.rs`
declares each scenario file as a `mod` and owns the shared helpers. The layout
follows cargo-insta's, which solves the same problem — many scenarios that each
drive a real CLI against a real scratch project.

These are end-to-end. Each scenario writes a small cargo project into a
`tempfile::tempdir()`, `git init`s it, and runs the actual `cargo-affected`
binary through `collect`/`run`/`status`. `llvm-tools` and `cargo-nextest` are
hard requirements; there is no mocking layer and no fallback, so a host missing
either fails loudly with the binary's own install hint.

## Writing a scenario

Use a **distinct package name** per scenario. Cargo's incremental cache is keyed
on `(workspace_root, package_name)` and misbehaves when same-named packages live
in different temporary roots — cargo-insta hit this too. `sample_<scenario>` is
the convention.

Build the scratch repo with `init_git_with_initial_commit`. It sets a local
identity and disables two things that otherwise depend on the developer's global
config: `core.autocrlf` (Windows would rewrite `\n` on checkout and break the
byte-exact edits `replace_in_file` makes) and `commit.gpgsign` (a host that signs
by default can't sign as `test@example.com`, so every scenario fails at its
initial commit — and CI, which signs nothing, never sees it).

Helpers in `main.rs`: `cargo_affected`, `cargo_affected_with_env` (for scenarios
that need to influence the build, e.g. `RUSTFLAGS`), `git`, `git_head`,
`combined_output`, `replace_in_file`, `init_git_with_initial_commit`,
`write_two_module_project`. Scenario-specific project shapes stay private to the
scenario file that needs them.

## Assertions

Assert **semantic properties**, via `combined_output(&out).contains(…)` —
"`test_extra` is tagged `(new)`", "`test_base` did not run". Not whole-output
snapshots: this CLI's output embeds temp directory paths, git shas and elapsed
seconds, so a snapshot would need enough redaction to be worth less than the
targeted assertion it replaced. That is why there is no `insta` dependency here,
notwithstanding the general Rust preference for it.

Stderr carries selection summaries and notices — `run`'s and `collect`'s — *and*
nextest's own PASS/FAIL lines, since nextest writes its entire human-readable
run to stderr. `status` splits the other way: only its `cargo-affected: cache=…`
summary line and `checking for new tests...` go to stderr, and everything else
it prints goes to stdout — the inventory, the full-suite miss explanations, the
stale-sha and divergence `note:` lines, the changed-files list, the
conditional-tense plan. `combined_output` concatenates stderr-then-stdout so a
scenario can grep both without caring which side a message landed on; reach for
`out.stdout` directly only when the stream itself is the thing under test.

## Regression tests

A test written for a bug must be **seen failing against the pre-fix behavior**
before it's worth keeping. Restore the old expression, run the one test, confirm
it fails for the stated reason, then restore the fix. A regression test that has
never failed is a test that pins nothing —
`status_prediction_uses_run_features` was verified this way against the old
`&[]`.

## Tripwires

Two scenarios exist to catch silent *under*-selection, the one failure mode this
tool cannot detect downstream — a green collect whose database then selects no
test for a real change:

- `db_has_function_ranges.rs` — the database must hold at least one non-sentinel
  row. A sentinel-only database looks healthy (exit 0, plausible row count) but
  only ever selects for crate-root edits.
- `remapped_paths.rs` — a build whose recorded source paths fall outside the
  project root must fail the collect rather than produce a coverage-free one.

Treat both as load-bearing. If a change makes either awkward, that is evidence
about the change.
