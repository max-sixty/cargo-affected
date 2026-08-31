---
name: running-tend
description: Project-specific guidance loaded by tend workflows alongside CLAUDE.md.
---

## Filing issues in other repos

Standing exception granted: file directly in agent-equipped targets (per
**Filing Issues in Other Repos** in the bundled `running-in-ci` skill) without
asking permission here first. The default rule (open an issue here asking
permission first) still applies when the target shows no agent signals.

## The review-runs evidence log is not simply the newest bot comment

The monthly `review-runs-tracking` issue carries more than one bot comment —
`tend-nightly` routes its own below-threshold findings there too. The evidence
log is specifically **the comment carrying a `## Run ` heading at the start of
a line**. Resolve it that way, not with the bare `| last` that `review-runs`
bundles:

```bash
EXISTING_COMMENT=$(gh api "repos/$REPO/issues/$TRACKING_NUMBER/comments?per_page=100" \
  --jq "[.[] | select(.user.login == \"$BOT_LOGIN\" and (.body | test(\"(^|\\n)## Run \")))] | last | .id // empty")
```

Match on a line start, not `startswith`. The log rolls into a fresh comment as
it nears GitHub's 65536-byte body limit, and a rollover comment opens with the
appended entry's own leading blank line or `---` separator rather than the
heading — on tracker #62 the four log comments begin `## Run `, `\n## Run `,
and twice `\n\n---\n\n## Run `. `startswith` matches only the first of those
and `| last` then picks the 50513-byte comment that was superseded precisely
*because* it was near the limit.

`| last` over all bot comments picked a nightly finding instead of the log on
2026-08-05 and again on 2026-08-07, each time appending a full run entry inside
an unrelated comment; nothing errors when this happens, so re-read the target
comment after posting to confirm the entry landed where it was meant to.

## Page the review-runs census — one day here is more than one API page

`tend-notifications` runs on `*/15`, so a 24-hour window holds ~75 completed
runs of that workflow alone. The GitHub API returns 30 per page by default and
`review-runs` Step 1 asks for no more, so the unpaged query silently returns
only the newest ~12 hours and reports that as the day:

`$SINCE` is the anchored window opening that `review-runs` Step 1 writes to
`/tmp/review-runs-since` — not a fresh `now - 24h`. Both blocks below re-read
it rather than inheriting it: each Bash call is its own shell, and an empty
`$SINCE` here does not fail loudly. `date -u -d " - 24 hours"` parses as a
relative offset from *now* and exits 0, so `FETCH_FROM` silently becomes the
un-anchored `now - 24h` these rules exist to replace, and
`select(.updated_at >= "")` admits every row instead of trimming.

```bash
# Over-fetch by a day and window on completion: a run created before $SINCE may
# still have been running at the predecessor's census, so `status=completed`
# dropped it there and a `created` filter would drop it again here.
SINCE=$(cat /tmp/review-runs-since)
FETCH_FROM=$(date -u -d "$SINCE - 24 hours" +%Y-%m-%dT%H:%M:%SZ)
gh api --paginate "repos/$REPO/actions/workflows/$workflow/runs?created=>=$FETCH_FROM&status=completed&per_page=100" \
  --jq ".workflow_runs[] | select(.updated_at >= \"$SINCE\") | {databaseId: .id, conclusion, createdAt: .created_at, updatedAt: .updated_at, name: .name}"
```

Completion is the axis that tiles: consecutive windows butt against each other
without a seam, where `created` leaves one. The floor is a run's whole
lifetime, not its job cap — `created_at` starts at queue time, and a
`cancel-in-progress: false` group can hold a run queued for hours before its
execution begins. This repo's longest completed run is `tend-review`
[`25516847093`](https://github.com/max-sixty/cargo-affected/actions/runs/25516847093)
at 20h47m, which is exactly the near-timeout shape Step 3 exists to find.

Cross-check the row count against `.total_count`, which is the one symptom of a
dropped page visible without re-querying. It counts the wider `$FETCH_FROM`
fetch, so it bounds the census from above rather than matching it — but a
census landing on a round page boundary (30, 100) is still the signature of a
page that was never followed. It needs its own call — the projection above
discards it, and `--paginate` re-applies the filter per page. Derive the floor
from the anchor file again — an unset `$FETCH_FROM` sends `created=>=`, which
the API answers `0` rather than rejecting, so the cross-check reports zero
against a non-empty census and inverts the signal it exists to give:

```bash
FETCH_FROM=$(date -u -d "$(cat /tmp/review-runs-since) - 24 hours" +%Y-%m-%dT%H:%M:%SZ)
gh api "repos/$REPO/actions/workflows/$workflow/runs?created=>=$FETCH_FROM&status=completed&per_page=1" \
  --jq '.total_count'
```

Measured on 2026-08-09: the unpaged query returned 30 of 75 `tend-notifications`
runs, oldest `20:41Z` against a window opening at `08:03Z`. The single run in
that window that started a session — [`31266801931`](https://github.com/max-sixty/cargo-affected/actions/runs/31266801931)
at `16:22Z` — was inside the hidden half, and surfaced only because Step 2's
`token-report.sh` fetches with `--limit 100`. Runs that leave no artifact (a
red pre-flight, a runner-acquisition failure) have no such second chance.

## Anchor the review-runs window at the predecessor run, not `now - 24h`

Fixed upstream in [max-sixty/tend#939](https://github.com/max-sixty/tend/pull/939)
(merged 2026-08-12, after `0.1.14`), so drop this section once this repo's pin
moves past the release that carries it. The recipes below match what landed
there.

`review-runs` computes `SINCE` as `now - 24h`, evaluated when the *agent* runs
the command — which is the run's own start plus container boot and skill
loading. The predecessor started at *its* cron time, earlier by however much
drift it saw. So a strict 24-hour window opens **after** the predecessor and
clips everything in between, and the clip is not a coin flip: it widens with
each minute of this session's startup latency and each minute of the
predecessor's negative drift. Anchor on the predecessor's start instead:

```bash
REPO=$(gh repo view --json nameWithOwner --jq '.nameWithOwner')
WF_ID=$(gh api "repos/$REPO/actions/runs/$GITHUB_RUN_ID" --jq '.workflow_id')
PREV_START=$(gh api "repos/$REPO/actions/workflows/$WF_ID/runs?status=success&per_page=10" \
  --jq "[.workflow_runs[] | select(.id != ${GITHUB_RUN_ID:-0}) | .created_at] | max // empty")
SINCE=${PREV_START:-$(date -u -d '25 hours ago' +%Y-%m-%dT%H:%M:%SZ)}
FLOOR=$(date -u -d '49 hours ago' +%Y-%m-%dT%H:%M:%SZ)
if [[ "$SINCE" < "$FLOOR" ]]; then SINCE=$FLOOR; fi
echo "$SINCE" > /tmp/review-runs-since
```

Derive the workflow id from `$GITHUB_RUN_ID` rather than naming the file: the
id is what the run itself reports, so it survives a workflow rename that a
hardcoded `tend-review-runs.yaml` would not — and a wrong file name doesn't
error, it returns no runs and drops silently through to the duration fallback.
The 49h clamp bounds a stale anchor: after an outage, or on the first run of a
fresh repo, the predecessor can be days back, and without a floor the window
pulls in a week. One skipped day still recovers; Step 5's dedup absorbs
whatever a widened window reports twice.

`status=success`, not `status=completed`: a predecessor that died before it
audited anything — a `startup_failure`, a runner-acquisition failure, a manual
cancel — is still `completed`, so anchoring on it skips the day that run never
covered. That is the same silent clip this rule exists to close, reached
through a red predecessor rather than through drift, and it is the one case
where the anchor would be *worse* than `now - 24h`. It is not hypothetical:
`tend-review-runs` has three `failure` runs already
([`25425083988`](https://github.com/max-sixty/cargo-affected/actions/runs/25425083988),
[`25485397409`](https://github.com/max-sixty/cargo-affected/actions/runs/25485397409),
[`25544958919`](https://github.com/max-sixty/cargo-affected/actions/runs/25544958919),
2026-05-06 through 05-08). Anchoring at the last run that actually finished an
audit widens the window to cover the missed day instead; the cost is
re-reporting a day when a run dies *after* posting its tracker comment, which
Step 5's dedup absorbs.

Excluding `$GITHUB_RUN_ID` matters — this run is `success` from the API's
point of view only after it ends, but a resumed or re-run attempt can surface
it early, and anchoring on itself would collapse the window to zero.

Step 2 takes whole hours, so derive them from the same anchor rather than
passing a literal `24`. Read the anchor back from the file rather than reusing
`$SINCE` — each Bash call is its own shell, so a variable set in Step 1 is
empty by Step 2, and `date -u -d ""` returns today's midnight with exit 0
rather than failing. That silently makes `HOURS` the hours since midnight,
which near the top of the day is a *narrower* window than the literal `24` this
rule replaces:

```bash
SINCE=$(cat /tmp/review-runs-since)
HOURS=$(( ( $(date -u +%s) - $(date -u -d "$SINCE" +%s) + 3599 ) / 3600 ))
"${CLAUDE_PLUGIN_ROOT}/scripts/token-report.sh" "$HOURS" > /tmp/token-report.json
```

Any later step that windows on `$SINCE` — Step 4's `closedAt` filter on bot PR
dispositions — re-reads it the same way. An empty string there compares less
than every non-null timestamp, so the filter stops windowing at all instead of
erroring.

Measured on 2026-08-10: the predecessor
[`31302553802`](https://github.com/max-sixty/cargo-affected/actions/runs/31302553802)
started `08:02:16Z`; this run started `08:16:49Z` and its first `now - 24h`
resolved to `08:18:19Z`. Seven runs sat in that 16-minute band, two of them
full sessions the audit exists to read — `tend-review`
[`31302992086`](https://github.com/max-sixty/cargo-affected/actions/runs/31302992086)
($1.57, the review that shaped this very PR) and `tend-mention`
[`31303194553`](https://github.com/max-sixty/cargo-affected/actions/runs/31303194553)
($1.40). The band grows during the session: a later `date -u -d '24 hours ago'`
in the same run resolved to `08:23:54Z`, by then hiding 15 runs. Step 2 hides
them too, because `token-report.sh 24` measures from its own invocation — so
unlike a dropped page, there is no second listing that recovers them.

## `gh` list commands truncate silently — pass `--limit` when the set is the answer

`gh pr list` and `gh issue list` return 30 rows by default and `gh run list`
20; all say nothing when there are more. The response is well-formed, exits 0,
and `--json` exposes no total to cross-check against, so the only defence is
asking for a bound up front. Keep the two defaults apart, because the tell for
a silent truncation is a row count landing exactly on one of them — a 20-row
`gh run list` is already at its cap, and a session watching for 30 will never
see it. This repo's open-PR queue passed 30 on 2026-08-26 and has not come
back down, so on this repo an unbounded `gh pr list --state open` is now
*always* truncated:

```bash
gh pr list --state open --limit 100 --json number,author --jq 'length'
```

Two figures have already been published wrong. `tend-weekly`
[`33303989079`](https://github.com/max-sixty/cargo-affected/actions/runs/33303989079)
(2026-08-30) reported "all 29 open PRs are authored by `cargo-affected-bot`"
from a list that stopped at 30 of 33; the 2026-08-29 nightly reported 32
against the same 33 (recorded on tracker #73). Two more sessions —
`tend-nightly` [`33297300065`](https://github.com/max-sixty/cargo-affected/actions/runs/33297300065)
and [`33365823356`](https://github.com/max-sixty/cargo-affected/actions/runs/33365823356)
— opened on "30 open bot PRs", noticed, and re-queried at `--limit 100`
before publishing. Catching it costs turns; missing it publishes a wrong
number.

The count is the visible symptom, not the worst one. `gh pr list` returns
newest-first, so truncation drops the *oldest* PRs — exactly the ones a
duplicate is most likely to re-derive. Two of the pinned bundled dedup
recipes are unbounded (`running-in-ci/SKILL.md:140`, the pre-branch "check
for existing work" scan, and `:702`, the skill-PR dedup), and the
pre-`gh pr create` recheck at `:165` passes `--limit 30` against
`--state all`, which against this repo's 84 all-state PRs reaches back only
to #66 (2026-07-23). A truncated read at any of those three does not produce
a wrong figure — it produces a duplicate PR. Bound them at `--limit 200`
here.

Fixed upstream in tend `0.1.22`
(`running-in-ci/references/grounded-analysis.md`, plus explicit `--limit`
bounds at every listing site); this repo pins `0.1.14`. Drop this section
once the pin moves past `0.1.22`.
