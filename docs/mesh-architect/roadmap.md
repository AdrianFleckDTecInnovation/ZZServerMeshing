# Mesh Architect Roadmap

Last updated: 2026-04-12

## Active

### 001: Bevy ECS Migration
**Status:** Designed, ready for implementation
**Design:** `designs/001-bevy-ecs-migration.md`
**Work Items:** `work-items/001-bevy-ecs-migration.md`
**Summary:** Replace HashMap-based entity storage and hand-rolled game loop with bevy_ecs. Introduces component decomposition (Position, Velocity, EntityKind, etc.), system functions for each simulation phase, ordered schedules, and an async-to-ECS bridge for the tokio runtime. 8 work items, estimated 14-20 hours.

## Planned

(Future designs will be added here as they are produced.)

- Player persistence (design exists in `docs/player-persistence-design.md`, needs work items)
- Peer health monitoring
- Interest management (infrastructure exists but disabled, needs redesign for ECS)
- Delta compression (infrastructure exists but disabled, needs redesign for ECS)
- Shadow extrapolation
- Smart rebalancing
- Graceful degradation
- Horizontal scaling
- Security hardening
- Observability improvements

## Completed

(None yet.)
