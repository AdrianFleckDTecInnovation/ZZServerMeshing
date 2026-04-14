# Mesh Architect Roadmap

Last updated: 2026-04-13

## Active

### 003: Critical/High Audit Fixes (Two-Phase Transfer, UDP Chunking, Persistence Race)
**Status:** Designed, ready for implementation
**Priority:** Critical/High
**Design:** `designs/003-critical-high-fixes.md`
**Work Items:** `work-items/003-critical-high-fixes.md`
**Summary:** Three fixes from the architecture audit. (1) Two-phase authority transfer with ACK/timeout/retry to prevent entity loss on UDP packet drop -- the most dangerous flaw in the system. (2) UDP replication chunking to break through the 1,000-entity hard wall where replication silently stops. (3) Atomic PlayerTransfer message to fix the race condition in the persistence design where check-in then check-out creates a duplicate-login window. 14 work items, estimated 18-27 hours. Issues are independent and can be parallelized.

### 002: Delta Compression for Server-to-Client Snapshots
**Status:** Designed, ready for implementation
**Design:** `designs/002-delta-compression.md`
**Work Items:** `work-items/002-delta-compression.md`
**Summary:** Replace full entity snapshots with per-client delta frames. Tracks per-client baselines, computes diffs (new/changed/removed entities), sends only changes. Client sends tick ACKs. Periodic full snapshots as safety net. Prerequisite: per-entity interpolation timestamps to fix the previous delta compression failure. 10 work items across 4 phases, estimated 15-22 hours. Replaces the broadcast channel with per-client mpsc channels.

## Completed

### 001: Bevy ECS Migration
**Status:** Implemented
**Design:** `designs/001-bevy-ecs-migration.md`
**Work Items:** `work-items/001-bevy-ecs-migration.md`
**Summary:** Replaced HashMap-based entity storage and hand-rolled game loop with bevy_ecs. Component decomposition, system functions for each simulation phase, ordered schedules, async-to-ECS bridge.

## Planned

(Future designs will be added here as they are produced.)

- Player persistence (design exists in `docs/player-persistence-design.md`, needs work items)
- Peer health monitoring
- Interest management (infrastructure exists but disabled, needs redesign for ECS)
- Shadow extrapolation
- Smart rebalancing
- Graceful degradation
- Horizontal scaling
- Security hardening
- Observability improvements
