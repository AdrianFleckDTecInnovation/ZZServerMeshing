---
name: develop
description: Full development pipeline — architect a feature, get user approval, then implement, test, and audit it using the mesh agent team.
user-invocable: true
---

# /develop — Full Mesh Development Pipeline

Orchestrates the full development lifecycle for a mesh networking feature: architecture → approval → implementation → testing → audit.

## Arguments

- `/develop <feature description>` — describe what to build

## Process

### Phase 1: Clarify

Before doing anything, make sure the direction is clear. Read the user's prompt and:

1. Read `CLAUDE.md`, `docs/mesh-improvements.md`, and `docs/mesh-architect/roadmap.md` (if it exists) for context
2. Identify any ambiguities, missing requirements, or conflicting constraints
3. Ask the user **up to 3 focused questions** to clarify scope, priorities, or constraints
4. If the request is already clear, skip questions and say so

Keep questions short and specific. Don't ask obvious things you can derive from the codebase.

### Phase 2: Architect

Spawn `@mesh-architect` to produce a design document and work items:

```
Agent(subagent_type="mesh-architect", prompt="Design and create work items for: <feature>. Write the design to docs/mesh-architect/designs/ and work items to docs/mesh-architect/work-items/. Read the current codebase first to understand what exists.")
```

When the architect finishes, **present the design to the user for review**:

1. Summarize the design in 5-10 bullet points (don't dump the whole doc)
2. List the work items with their sizes
3. Call out any important tradeoffs or decisions the architect made
4. Ask: "Ready to implement, or want changes?"

**STOP HERE AND WAIT.** Do not proceed until the user confirms.

### Phase 3: Implement

After user approval, spawn `@mesh-dev` with the work items:

```
Agent(subagent_type="mesh-dev", prompt="Implement the following work items from docs/mesh-architect/work-items/<file>. Read the design doc at docs/mesh-architect/designs/<file> for full context. Follow the work items in order. Test each item on the UNO Q boards before moving to the next.")
```

When mesh-dev finishes, report what was implemented and any issues encountered.

### Phase 4: Test

Spawn `@mesh-tester` to verify the implementation:

```
Agent(subagent_type="mesh-tester", prompt="The following feature was just implemented: <summary>. Files changed: <list>. Write tests to verify it works correctly — unit tests for new functions, integration tests for cross-module flows, and scene tests that run against the UNO Q boards. Check for edge cases and error paths. Run all tests and report results.")
```

Report test results. If tests fail, go back to mesh-dev to fix.

### Phase 5: Audit

Spawn `@mesh-auditor` to review the implementation:

```
Agent(subagent_type="mesh-auditor", prompt="Audit the following recently implemented feature: <summary>. Files changed: <list>. Check for bottlenecks, race conditions, data loss risks, and scalability issues introduced by these changes. Write findings to docs/mesh-auditor/.")
```

Report audit findings. If critical issues are found, go back to mesh-dev to fix, then re-test.

### Phase 6: Report

Summarize the entire pipeline:

1. **What was built** — feature summary
2. **Files changed** — list of modified/created files
3. **Tests added** — count and what they cover
4. **Audit findings** — any issues found and whether they were fixed
5. **Status** — ready to ship, or has open issues

## Rules

- **Always get user approval after the architecture phase.** Never implement without confirmation.
- **Run agents sequentially, not in parallel.** Each phase depends on the previous one.
- **If mesh-dev encounters a design problem,** report it to the user and suggest going back to the architect rather than making ad-hoc design decisions.
- **If tests fail,** have mesh-dev fix the code first, then re-run tests. Don't ship with failing tests.
- **If the auditor finds critical issues,** have mesh-dev fix them and mesh-tester re-verify before reporting done.
- **Keep the user informed** between phases with short status updates.

## Example

```
User: /develop Add peer health monitoring so servers can detect when a peer is struggling

Phase 1: "I have a couple questions before we architect this:
  1. Should degraded peers get fewer entities rebalanced TO them, or should we actively migrate entities AWAY?
  2. What's the threshold — RTT-based, packet loss, or tick budget?"

User: "Both — stop sending new entities and start migrating away. Use a combo of RTT and packet loss."

Phase 2: [spawns mesh-architect, gets design doc]
"Here's the design:
  - New PeerHealth struct tracking RTT, packet loss, staleness
  - Ping/pong UDP messages every 1 second
  - Three states: Healthy, Degraded, Unresponsive
  - Degraded peers get reduced replication rate + coordinator notified
  - 6 work items (2 small, 3 medium, 1 large)
  Ready to implement?"

User: "Go"

Phase 3: [spawns mesh-dev with work items]
Phase 4: [spawns mesh-tester]
Phase 5: [spawns mesh-auditor]
Phase 6: "Done. 8 files changed, 12 tests added, 1 medium audit finding (race between ping timeout and heartbeat). Ready to /ship."
```
