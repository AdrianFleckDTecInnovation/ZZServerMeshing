---
name: ship
description: Ship the current branch — commit, rename, push, create PR, merge to main, and create a new branch for the next piece of work.
user-invocable: true
---

# Ship It

Ship the current branch to main and set up for the next piece of work. Never ask the user questions — derive everything from the code and commits. Just do it.

## Arguments

- `/ship` — derive branch name from changes, next branch is `dev`
- `/ship <branch-name>` — use the given name, next branch is `dev`
- `/ship <branch-name> <next-branch>` — use both names as given

## Steps

### 1. Assess current state

```bash
git status
git log --oneline main..HEAD
git diff --stat main..HEAD
```

### 2. Commit uncommitted changes (if any)

If there are staged, unstaged, or untracked files (excluding secrets/build artifacts):
- Stage relevant files
- Create a commit with a clear message derived from the changes
- Follow the repo's commit message style from `git log`

### 3. Rename the branch (if needed)

If the user provided a branch name, use it. Otherwise derive one from the commits (e.g., `feat/add-test-coverage`, `fix/rebalance-threshold`). Rename without asking.

```bash
git branch -m <current-branch> <new-branch-name>
```

### 4. Push the branch

```bash
git push -u origin <branch-name>
```

### 5. Create the PR

Derive title and body from the commits. Don't ask — just create it.

```bash
gh pr create --title "<title>" --body "$(cat <<'EOF'
## Summary
<bullet points>

## Test plan
<how to verify>

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

### 6. Merge the PR

```bash
gh pr merge --squash --delete-branch
```

Use `--squash` to keep main history clean.

### 7. Update local main

```bash
git checkout main
git pull origin main
```

### 8. Create the next branch

Use the user-provided name, or default to `dev`.

```bash
git checkout -b <next-branch>
```

### 9. Print summary

- PR URL
- What was shipped (one line)
- Current branch

## Rules

- **Never ask the user questions** — derive branch names, PR titles, and commit messages from the code
- Never force-push
- If the merge fails (conflicts, checks), stop and report — don't force it
- If there's nothing to ship (no changes and no commits beyond main), say so and stop
