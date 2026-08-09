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
log is specifically **the comment whose body starts with `## Run `**. Resolve
it that way, not with the bare `| last` that `review-runs` bundles:

```bash
EXISTING_COMMENT=$(gh api "repos/$REPO/issues/$TRACKING_NUMBER/comments" \
  --jq "[.[] | select(.user.login == \"$BOT_LOGIN\" and (.body | startswith(\"## Run \")))] | last | .id // empty")
```

`| last` picked a nightly finding instead of the log on 2026-08-05 and again on
2026-08-07, each time appending a full run entry inside an unrelated comment;
nothing errors when this happens, so re-read the target comment after posting
to confirm the entry landed where it was meant to.

## Page the review-runs census — one day here is more than one API page

`tend-notifications` runs on `*/15`, so a 24-hour window holds ~75 completed
runs of that workflow alone. The GitHub API returns 30 per page by default and
`review-runs` Step 1 asks for no more, so the unpaged query silently returns
only the newest ~12 hours and reports that as the day. Always paginate, and
cross-check the count against `.total_count` from the same query:

```bash
gh api --paginate "repos/$REPO/actions/workflows/$workflow/runs?created=>=$SINCE&status=completed&per_page=100" \
  --jq '.workflow_runs[] | {databaseId: .id, conclusion, createdAt: .created_at, name: .name}'
```

Measured on 2026-08-09: the unpaged query returned 30 of 74 `tend-notifications`
runs, oldest `20:22Z` against a window opening at `08:03Z`. The single run in
that window that started a session — [`31266801931`](https://github.com/max-sixty/cargo-affected/actions/runs/31266801931)
at `16:22Z` — was inside the hidden half, and surfaced only because Step 2's
`token-report.sh` fetches with `--limit 100`. Runs that leave no artifact (a
red pre-flight, a runner-acquisition failure) have no such second chance.
