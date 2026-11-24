---
name: ship
description: Prepare and ship a requested mesh branch through the repository’s Git and pull request workflow.
---

Inspect the branch, upstream, working tree and commits for the requested change. Keep unrelated user edits out of commits. Run the corresponding local checks in `AGENTS.md` and summarize their actual results in the pull request.

Use the requested base and remote. When the request includes publication or merge, follow repository checks and branch protection, report the PR URL and resulting branch. Use the squash convention for a requested merge unless the repository or user specifies another strategy.
