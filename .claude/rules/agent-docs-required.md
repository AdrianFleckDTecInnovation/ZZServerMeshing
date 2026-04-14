# Agents Require Reference Docs

Before spawning any mesh-dev, mesh-tester, or mesh-auditor agent, ensure there is a **written document** for the agent to reference. Agents without context produce poor results that require multiple fix iterations.

## Required before spawning

- **mesh-dev**: A design doc (`docs/mesh-architect/designs/`) or work items file (`docs/mesh-architect/work-items/`) describing what to implement
- **mesh-tester**: A summary of what changed and which files were modified, OR a test plan doc
- **mesh-auditor**: A summary of what was implemented and which files to audit
- **mesh-architect**: A clear problem statement (can be in the prompt since the architect produces the docs)

## If no doc exists

Either:
1. **Create the doc first** — write the design/work items, then spawn the agent pointing to it
2. **Do the work directly** — if the task is small enough to not need a doc, handle it in the main conversation instead of spawning an agent

## How to point agents to docs

Always include the doc path in the agent prompt:
```
"Read the design doc at docs/mesh-architect/designs/004-data-integrity-fixes.md for full context."
"Implement the work items from docs/mesh-architect/work-items/004-data-integrity-fixes.md."
"Read docs/mesh-auditor/049-compute-delta-before-fallback-wastes-cpu.md for the finding details."
```

## Never do this

```
Agent(subagent_type="mesh-dev", prompt="Fix the heartbeat starvation bug")
```

The agent has no context about what the bug is, where it lives, or what the fix should look like. It will read random files and produce a mediocre fix.
