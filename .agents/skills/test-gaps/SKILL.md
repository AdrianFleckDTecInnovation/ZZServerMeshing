---
name: test-gaps
description: Find missing regression coverage in mesh protocols, ownership transitions, simulation and client behavior, then exercise the affected production path.
---

Trace a concrete invariant through its producer and consumer. Good candidates are ownership during handoff, session changes after peer restart, ACK and baseline transitions, and Rust/TypeScript agreement on binary fields. Check adjacent Rust tests, `crates/sim-server/tests/`, `tools/tests/` and the named live scenarios in `tools/test-suite/scenarios/`. Some TypeScript unit files contain local copies of production logic. Exercise the production implementation when a copied model could hide the regression.

Write the smallest case that distinguishes the broken behavior, then run the focused tests and the relevant local mesh scenario. From the root, `npm test` covers the workspace and client build, and `npm run smoke` starts its own mesh. The earlier [coverage pass log](../../../.claude/skills/test-gaps/PASS_LOG.md) provides historical leads. Verify that the modules and assumptions still exist.
