---
name: running-tend
description: Project-specific guidance loaded by tend workflows alongside CLAUDE.md.
---

## Merging

Merge PRs autonomously when they look safe and beneficial, regardless of author
or change category. Use judgment about the improvement and its risk; small
improvements are worth merging too. Review the current diff, verify checks
against the latest head and compatibility with the current base, and address
actionable findings before merging. Follow through to the merge rather than
stopping at an approval or green CI.

Resolve uncertainty through investigation and fixes where possible. Involve the
maintainer when a material product decision or unresolved risk needs their
judgment, and coordinate with an active owner before superseding unfinished work.
Respect GitHub's required independent CODEOWNER review for protected changes.

## Filing issues in other repos

Standing exception granted: file directly in agent-equipped targets (per
**Filing issues** in the bundled `/tend-ci-runner:act-in-other-repos` skill)
without asking permission here first. The default rule (open an issue here asking
permission first) still applies when the target shows no agent signals.
