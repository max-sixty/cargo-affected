---
name: running-tend
description: Project-specific guidance loaded by tend workflows alongside CLAUDE.md.
---

## Filing issues in other repos

Standing exception granted: file directly in agent-equipped targets (per
**Filing Issues in Other Repos** in the bundled `running-in-ci` skill) without
asking permission here first. The default rule (open an issue here asking
permission first) still applies when the target shows no agent signals.

## Batch prose-only fixes into one rolling sweep PR

This repo's bot PRs are reviewed in occasional batches, not continuously. One
PR per one-line comment fix therefore spends a review slot per line and makes
the queue harder to triage rather than easier — as of 2026-08-20 nine of the
open bot PRs (#29, #47, #48, #49, #50, #60, #61, #65, #87) change nothing but
documentation prose and code comments, 41 added and 27 removed lines between
them, across nine separate slots.

So when a sweep turns up a fix whose **entire diff** is prose — code comments,
module docs, `CLAUDE.md`, `README.md` — with no change to code, tests, or
assertion messages, don't give it its own PR. Add it to the rolling sweep
instead:

1. Look for an open sweep PR:
   ```bash
   gh pr list --state open --author "$BOT_LOGIN" --head docs/prose-sweep \
     --json number --jq '.[0].number // empty'
   ```
2. If one exists, `gh pr checkout` it, commit the fix on that branch, push, and
   add a bullet for it to the PR description.
3. If none exists, first check for a stale `docs/prose-sweep` branch left by an
   earlier closed sweep:
   ```bash
   git ls-remote --exit-code --heads origin docs/prose-sweep
   ```
   A surviving branch means the sweep was declined rather than merged — this
   repo squash-merges with `delete_branch_on_merge`, so a merged sweep leaves no
   branch behind. Take that as the signal batching was rejected and go back to a
   per-fix PR; pushing onto the rejected branch would be a non-fast-forward and
   the fix would be dropped with a push error. Otherwise branch
   `docs/prose-sweep` off `main` and open one PR titled `docs: prose-accuracy
   sweep`, one bullet per fix naming the file it touches.

Anything that changes behavior keeps its own PR — including an edit under
`.claude/skills/`, which changes *this bot's* behavior even though its diff is
pure markdown — as does anything that adds or edits a test or an assertion
message. Those earn an individual review slot on their own merits. Within this
prose-only scope the rule deliberately overrides the bundled **Atomic PRs**
guidance. If a prose fix corrects something that is actively misleading users
today, still put it in the sweep, but say so in its bullet so it can be
cherry-picked ahead of the rest.
