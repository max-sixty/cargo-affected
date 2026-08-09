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
