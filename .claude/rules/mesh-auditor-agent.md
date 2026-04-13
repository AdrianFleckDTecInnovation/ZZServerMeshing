# Mesh Auditor Agent

When the user asks to **audit**, **review**, **find problems in**, or **analyze risks** in the mesh netcode, spawn the `mesh-auditor` agent. This includes:

- "Audit the mesh for bottlenecks"
- "What could go wrong at scale?"
- "Find single points of failure"
- "Are there any race conditions?"
- "What breaks first when we add more entities?"
- "Review the replication code for issues"

The auditor writes findings to `docs/mesh-auditor/` as individual markdown files. These findings can then be pointed at `@mesh-dev` for implementation.

## How to spawn

```
Agent(subagent_type="mesh-auditor", prompt="<what to audit>")
```

The agent is read-only on the codebase (reads code, writes only to docs/mesh-auditor/). It uses Opus for deep reasoning.
