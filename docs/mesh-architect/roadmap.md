# Mesh Architect Roadmap

Last updated: 2026-04-15

## Active

### 011: Network Saturation Telemetry
**Status:** Designed, ready for implementation
**Priority:** High
**Design:** `designs/011-network-saturation-telemetry.md`
**Work Items:** `work-items/011-network-saturation-telemetry.md`
**Summary:** Surface network health telemetry in the `/metrics` JSON endpoint so WiFi/UDP saturation can be detected before it causes functional problems (shadow loss, peer disconnects). Exposes UDP outbound drop counters (already tracked but only logged), outbound bytes per tick, and per-peer inbound health: shadow freshness (staleness in ticks), receive rate (packets/second), and chunk reassembly evictions. All data added to existing MetricsSnapshot JSON. Per-peer data keyed by server_id. Adds a PeerHealthAccumulator shared structure between the UDP receiver and game loop. 8 work items across 3 phases, estimated 4-6 hours. No breaking changes to existing consumers.

### 010: Tiered AoI-Aware Shadow Replication
**Status:** Designed, ready for implementation
**Priority:** High
**Design:** `designs/010-tiered-aoi-shadow-replication.md`
**Work Items:** `work-items/010-tiered-aoi-shadow-replication.md`
**Summary:** Split server-to-server replication into hot tier (20Hz, entities near peer players) and cold tier (5Hz, everything else). Currently all spatially-relevant entities are replicated at 20Hz, wasting bandwidth on entities no player will see. The hot tier uses player shadow positions (not all shadows) and squared distance checks for precise classification. The cold tier replaces the 1-per-second full snapshot for crash recovery. Full snapshots are disabled by default (configurable via --full-snapshot flag), with interval changed from 1s to 30s. Peers without player shadows fall back to the existing PeerBounds approach. 11 work items, estimated 7-8 hours.

### 007: Peer Replication AoI (Spatial Filtering for Server-to-Server UDP)
**Status:** Implemented
**Priority:** High
**Design:** `designs/007-peer-replication-aoi.md`
**Work Items:** `work-items/007-peer-replication-aoi.md`
**Summary:** Server-to-server UDP replication currently sends ALL authoritative entities to ALL peers every tick, saturating the UDP channel at ~600-800 entities on the UNO Q boards. This design filters replication per-peer using spatial bounding boxes: each server computes the spatial footprint of each peer's entities (from shadow positions) and only replicates entities within a 750-unit expanded radius of that footprint. Periodic full replication (once per second) ensures crash recovery still has complete shadow state. Uses axis-aligned bounding box checks (4 float comparisons per entity per peer) instead of expensive cell-set expansion. 10 work items across 4 phases, estimated 12-18 hours. Includes a rollback knob (--full-repl-interval 1 disables filtering).

### 006: Area of Interest (AoI) Filtering for Server-to-Client Snapshots
**Status:** Designed, ready for implementation
**Priority:** High
**Design:** `designs/006-aoi-filtering.md`
**Work Items:** `work-items/006-aoi-filtering.md`
**Summary:** Binary in/out AoI filter at 500 units for client snapshots. Entities within 500 units of the client's player are included every tick; entities beyond are excluded. No tiers, no variable rates. Prerequisites: expand WORLD_EXTENT from 500 to 5000 (10k x 10k world), update client visuals, update test infrastructure with spawnCluster/spawnAtPositions helpers. The AoI filter integrates with existing delta compression -- entities leaving AoI produce delta removals, entering produces delta updates. Player's own entity always included. No wire format changes. 10 work items across 4 phases, estimated 8-12 hours.

### 005: Tick Loop Health (Mutex Swap, UDP Sender Task, Clean Exit)
**Status:** Designed, ready for implementation
**Priority:** High
**Design:** `designs/005-tick-loop-health.md`
**Work Items:** `work-items/005-tick-loop-health.md`
**Summary:** Fixes 3 audit findings (#003, #013, #009) that introduce unpredictable latency and unsafe behavior in the 50ms game loop. (1) Replace 3 tokio::Mutex locks with std::sync::Mutex -- eliminates scheduler yields on sub-microsecond critical sections. (2) Move all UDP send_to calls off the tick loop into a dedicated sender task with an mpsc channel -- removes 6-15 awaited syscalls per tick. (3) Replace std::process::exit(0) on drain-complete with a clean loop break -- enables graceful runtime shutdown. 7 work items, estimated 5-8 hours.

### 004: Data Integrity Fixes (Delta Compression and Baseline Management)
**Status:** Designed, ready for implementation
**Priority:** Medium-High
**Design:** `designs/004-data-integrity-fixes.md`
**Work Items:** `work-items/004-data-integrity-fixes.md`
**Summary:** Fixes for 4 open audit findings in the delta compression system. (1) Wire ACKs into baseline management so stale clients get full snapshots within 2s instead of 5s. (2) Use vector distance instead of per-component threshold for change detection. (3) Fall back to full snapshot when >80% of entities changed (NPCs defeat delta). (4) Validate ACK tick values for monotonicity and server-tick bounds. Also confirms 3 findings (#018, #023, #024) are already resolved in code. 5 work items, estimated 4-6 hours.

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
- Shadow extrapolation
- Smart rebalancing
- Graceful degradation
- Horizontal scaling
- Security hardening
- Observability improvements
