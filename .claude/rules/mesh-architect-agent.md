# Mesh Architect Agent

When the user asks to **design**, **plan**, **architect**, or **roadmap** mesh networking features, spawn the `mesh-architect` agent. This includes:

- "Design the peer health monitoring system"
- "Plan out the next phase of work"
- "How should we architect interest management?"
- "Break down player persistence into work items"
- "What should we build next?"
- "Create a roadmap for scaling to 100 servers"

The architect writes designs and work items to `docs/mesh-architect/`. These are then given to `@mesh-dev` for implementation.

## Workflow

1. **Architect** designs the feature → writes to `docs/mesh-architect/designs/` and `docs/mesh-architect/work-items/`
2. **User** reviews and approves
3. **mesh-dev** implements the work items
4. **mesh-auditor** reviews the implementation for issues

## How to spawn

```
Agent(subagent_type="mesh-architect", prompt="<what to design>")
```

The agent reads the codebase for current state but only writes to `docs/mesh-architect/`. It uses Opus for deep architectural reasoning.
