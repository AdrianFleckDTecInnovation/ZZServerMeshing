# Mesh Cleanup Agent

When the user asks to **simplify**, **clean up**, **remove dead code**, **split systems.rs**, **deduplicate**, or **refactor** the codebase, spawn the `mesh-cleanup` agent. This includes:

- "Clean up the codebase"
- "Remove dead code"
- "Split systems.rs"
- "Do WI-120 through WI-125"
- "Run the next batch of cleanup work items"
- "Simplify the protocol types"
- "Deduplicate the TLS code"
- "Extract the dashboard HTML"
- "Clean up the abstractions"
- "Continue the simplification work"

The agent follows the work items in `docs/mesh-architect/work-items/012-codebase-simplification.md`. Each work item has exact files, steps, tests, and done-when criteria.

**Do NOT use mesh-cleanup for:** new features (use `@mesh-dev`), test coverage (use `@mesh-tester`), architecture design (use `@mesh-architect`), or codebase questions (use `@codebase-oracle`).

## How to spawn

```
Agent(subagent_type="mesh-cleanup", prompt="Implement WI-120 through WI-125 (Phase 1 dead code removal, batch 1). Read the work items doc at docs/mesh-architect/work-items/012-codebase-simplification.md for full details on each item.")
```

Always tell the agent which specific WIs to implement. Don't give it the entire plan at once — batch 3-6 related WIs per invocation.

## Recommended Batches

Phase 1 (dead code) is safe to batch aggressively since items are independent:
- **Batch 1:** WI-120 to WI-125 (trivial/small dead field removals)
- **Batch 2:** WI-126 to WI-128 (ClientFrame simplification, backpressure, ServerMessage variants)
- **Batch 3:** WI-129 to WI-132 (ReplicationConfig, ClientSnapshot, CompactEntityState, GatewayMetrics)

Phase 2 onward should be one WI per invocation since they're larger.
