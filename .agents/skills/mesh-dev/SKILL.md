---
name: mesh-dev
description: Implement mesh work items affecting Rust simulation, authority, replication, coordinator behavior or binary snapshots.
---

Start from the referenced work item and the current code. New simulation fields can cross the ECS component, shared entity, conversion, snapshot encoder and browser decoder. A new message can also require an exhaustive match in the coordinator client or ECS input system. For timing changes, compare the shared tick constants with client interpolation and prediction.

Exercise one sim-server for local logic and multiple peers for transfers or replication. Use `cargo test --locked -p sim-server` and `npm run smoke` from the root. Record wire compatibility or timing assumptions alongside the relevant implementation. Historical development notes are in `.claude/skills/mesh-dev/CHANGELOG.md`.
