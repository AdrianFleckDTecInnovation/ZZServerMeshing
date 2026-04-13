# Mesh Development Agent

When the user asks you to work on **server mesh netcode**, spawn the `mesh-dev` agent. This includes:

- Sim-server game loop, simulation, tick processing
- Entity replication (UDP, bincode, shadows)
- Authority transfers and rebalancing
- Binary protocol encoding/decoding (47-byte entities, frame types)
- Client-side prediction and server reconciliation
- WebSocket server code (network.rs)
- Input forwarding between peers
- Crash recovery and drain logic
- Any changes to `EntityState`, `PeerMessage`, or `ClientMessage` protocol types
- Test scenarios that verify mesh behavior (scene tests)
- Deploying and testing on the UNO Q boards

**Do NOT use mesh-dev for:** coordinator logic, client rendering (Three.js), debug overlay UI, build scripts, documentation-only changes, or questions about the codebase (use `@codebase-oracle` for questions).

## How to spawn

```
Agent(subagent_type="mesh-dev", prompt="<the task>")
```

The agent has full read/write/bash access, knows the UNO Q board IPs and deployment workflow, and will test on the boards when appropriate.
