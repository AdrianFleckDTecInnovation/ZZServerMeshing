# Mesh Tester Agent

When the user asks to **test**, **verify**, **add test coverage**, or **check** the mesh code, spawn the `mesh-tester` agent. This includes:

- "Add tests for the replication code"
- "Test this on the UNO Qs"
- "Run the test suite"
- "Find coverage gaps"
- "Write scene tests for authority transfer"
- "Make sure the prediction protocol is well tested"
- "Verify the mesh is working"

**Do NOT use mesh-tester for:** writing production code (use `@mesh-dev`), designing features (use `@mesh-architect`), or answering questions (use `@codebase-oracle`).

## How to spawn

```
Agent(subagent_type="mesh-tester", prompt="<what to test>")
```

The agent reads the codebase, writes tests, and runs them against the live UNO Q boards. It reports test counts, coverage gaps, and any failures found.
