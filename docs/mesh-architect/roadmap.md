# Mesh Architect Roadmap

Last updated: 2026-04-17

## Active

### 012: Codebase Simplification
**Status:** Complete (all 34 work items implemented)
**Priority:** High
**Design:** `designs/012-codebase-simplification.md`
**Work Items:** `work-items/012-codebase-simplification.md`
**Summary:** Comprehensive cleanup to make the prototype clean enough for a drop-in package. 9 phases, 34 work items: (1) Remove 12 dead code items (ClientFrame::Shared, TimingAccumulator, CompactEntityState, GatewayMetrics, ReplicationConfig, etc.). (2) Split systems.rs (8,041 lines) into 10 focused modules under systems/. (3) Type-safe protocol IDs (EntityId/ServerId instead of String/u16 in ClientMessage and coordinator messages). (4) Deduplication (TLS, TICK_DURATION, speed constants, queue swap, helpers). (5) Extract inline HTML dashboards to files. (6) Clean abstractions (PeerBounds to sim-server, DrainState collapse, Shared* wrappers to type aliases). (7) Coordinator HTTP helper. (8) Dependency cleanup (shared dev-deps, rand 0.9). (9) Doc archival. Zero behavior change throughout -- pure refactoring backed by 225+ existing tests. Based on complexity-audit.md findings.

## Implemented

Designs 001-011 and their work items have been archived to `archive/designs/` and `archive/work-items/` respectively. Auditor per-finding reports (001-073) archived to `docs/mesh-auditor/archive/`.

### 011: Network Saturation Telemetry
**Status:** Implemented
**Design:** `archive/designs/011-network-saturation-telemetry.md`
**Work Items:** `archive/work-items/011-network-saturation-telemetry.md`

### 010: Tiered AoI-Aware Shadow Replication
**Status:** Implemented
**Design:** `archive/designs/010-tiered-aoi-shadow-replication.md`
**Work Items:** `archive/work-items/010-tiered-aoi-shadow-replication.md`

### 007: Peer Replication AoI (Spatial Filtering for Server-to-Server UDP)
**Status:** Implemented
**Design:** `archive/designs/007-peer-replication-aoi.md`
**Work Items:** `archive/work-items/007-peer-replication-aoi.md`

### 006: Area of Interest (AoI) Filtering for Server-to-Client Snapshots
**Status:** Implemented
**Design:** `archive/designs/006-aoi-filtering.md`
**Work Items:** `archive/work-items/006-aoi-filtering.md`

### 005: Tick Loop Health (Mutex Swap, UDP Sender Task, Clean Exit)
**Status:** Implemented
**Design:** `archive/designs/005-tick-loop-health.md`
**Work Items:** `archive/work-items/005-tick-loop-health.md`

### 004: Data Integrity Fixes (Delta Compression and Baseline Management)
**Status:** Implemented
**Design:** `archive/designs/004-data-integrity-fixes.md`
**Work Items:** `archive/work-items/004-data-integrity-fixes.md`

### 003: Critical/High Audit Fixes (Two-Phase Transfer, UDP Chunking, Persistence Race)
**Status:** Implemented
**Design:** `archive/designs/003-critical-high-fixes.md`
**Work Items:** `archive/work-items/003-critical-high-fixes.md`

### 002: Delta Compression for Server-to-Client Snapshots
**Status:** Implemented
**Design:** `archive/designs/002-delta-compression.md`
**Work Items:** `archive/work-items/002-delta-compression.md`

### 001: Bevy ECS Migration
**Status:** Implemented
**Design:** `archive/designs/001-bevy-ecs-migration.md`
**Work Items:** `archive/work-items/001-bevy-ecs-migration.md`

## Planned

(Future designs will be added here as they are produced.)

- Player persistence (design exists in `docs/player-persistence-design.md`, needs work items)
- Peer health monitoring
- Shadow extrapolation
- Smart rebalancing
- Graceful degradation
- Horizontal scaling
- Security hardening
- Observability improvements
