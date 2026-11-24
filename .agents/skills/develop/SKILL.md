---
name: develop
description: Design and deliver a mesh feature spanning the coordinator, simulation, wire protocols or browser client.
---

Trace the feature from the client or control message through ECS and replication. Check existing designs and work items under `docs/mesh-architect/` before proposing a new protocol or transfer policy. Identify how old peers, reconnections, shadow adoption and input acknowledgement affect the change.

For an architecture request, record the chosen design and implementation steps where the project keeps comparable work. For an implementation request, carry the change through producers, consumers and observability. Resolve significant design choices with the user when they affect the requested behavior. Run the relevant crate tests, `npm test` and a local scenario covering the changed path. See the root `AGENTS.md` for commands.
