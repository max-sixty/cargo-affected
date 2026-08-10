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

```bash
gh api --paginate "repos/$REPO/actions/workflows/$workflow/runs?created=>=$SINCE&status=completed&per_page=100" \
  --jq '.workflow_runs[] | {databaseId: .id, conclusion, createdAt: .created_at, name: .name}'
```

Cross-check the row count against `.total_count`, which is the one symptom of a
dropped page visible without re-querying. It needs its own call — the projection
above discards it, and `--paginate` re-applies the filter per page:

```bash
gh api "repos/$REPO/actions/workflows/$workflow/runs?created=>=$SINCE&status=completed&per_page=1" \
  --jq '.total_count'
```

Measured on 2026-08-09: the unpaged query returned 30 of 75 `tend-notifications`
runs, oldest `20:41Z` against a window opening at `08:03Z`. The single run in
that window that started a session — [`31266801931`](https://github.com/max-sixty/cargo-affected/actions/runs/31266801931)
at `16:22Z` — was inside the hidden half, and surfaced only because Step 2's
`token-report.sh` fetches with `--limit 100`. Runs that leave no artifact (a
red pre-flight, a runner-acquisition failure) have no such second chance.

## Anchor the review-runs window at the predecessor run, not `now - 24h`

`review-runs` computes `SINCE` as `now - 24h`, evaluated when the *agent* runs
the command — which is the run's own start plus container boot and skill
loading. The predecessor started at *its* cron time, earlier by however much
drift it saw. So a strict 24-hour window opens **after** the predecessor and
clips everything in between, and the clip is not a coin flip: it widens with
each minute of this session's startup latency and each minute of the
predecessor's negative drift. Anchor on the predecessor's start instead:

```bash
REPO=$(gh repo view --json nameWithOwner --jq '.nameWithOwner')
PREV_START=$(gh api "repos/$REPO/actions/workflows/tend-review-runs.yaml/runs?status=success&per_page=10" \
  --jq "[.workflow_runs[] | select(.id != $GITHUB_RUN_ID) | .created_at] | max // empty")
SINCE=${PREV_START:-$(date -u -d '25 hours ago' +%Y-%m-%dT%H:%M:%SZ)}
```

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
passing a literal `24`:

```bash
HOURS=$(( ( $(date -u +%s) - $(date -u -d "$SINCE" +%s) + 3599 ) / 3600 ))
"${CLAUDE_PLUGIN_ROOT}/scripts/token-report.sh" "$HOURS" > /tmp/token-report.json
```

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
