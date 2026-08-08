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
