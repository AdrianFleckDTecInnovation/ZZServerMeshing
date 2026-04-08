# Test Gap Hunter Pass Log

Last 5 runs. Read at start of `/test-gaps`, written at end.

## Run 2 — 2026-04-08 — Metrics, simulation, delta logic, TS binary decoder

- **Files tested**:
  - `crates/sim-server/src/metrics.rs` — 8 tests (NEW)
  - `crates/sim-server/src/simulation.rs` — 16 tests (NEW)
  - `crates/gateway/src/aggregator.rs` — 10 tests added (24 total)
  - `tools/tests/binary-protocol.test.ts` — 18 tests (NEW)
- **Tests added**: 52 new (138 total: 120 Rust + 18 TypeScript)
- **Gaps found**: metrics ring buffer, simulation tick logic, process_input, delta compression helpers, and client binary decoder were all untested
- **Coverage areas**:
  - Metrics: ring buffer record/eviction, percentile computation (p50/p95/p99 ordering), tick budget percentage, phase timing isolation, snapshot JSON serialization, empty state
  - Simulation: process_input for all ClientMessage variants (SpawnEntity, SpawnPlayer, MoveInput, RemoveEntity, RequestSnapshot), invalid UUID handling, unknown entity type defaults to NPC, tick advancement, position integration, world bounds clamping, NPC direction changes over time
  - Gateway delta: entity_changed detection (new entity, position moved, authority changed, below threshold), broadcast_tick increment, last_broadcast state tracking, snapshot/bandwidth counters, metrics update
  - TypeScript binary protocol: full snapshot decoding (tick, metrics, entities, empty), delta frame decoding (updates, removals, combined), UUID byte-to-string conversion (known UUID, all zeros, all ones), frame type detection (unknown returns null, empty returns null), large u64 tick values, frame size validation, entity type mapping (Npc=0, Player=1)
- **Gaps remaining**:
  - `sim-server/replication.rs` — UDP send (async + network; would need integration test)
  - `sim-server/receiver.rs` — UDP receive loop + authority transfer acceptance (async + network)
  - `sim-server/network.rs` — WebSocket server (integration test)
  - `sim-server/coordinator_client.rs` — WebSocket coordinator connection
  - `gateway/main.rs` — broadcast loop, server connection management
  - `coordinator/main.rs` — WebSocket accept loop, periodic tasks
  - `sim-server/metrics_server.rs` — HTTP metrics endpoint
  - `client/src/world-view.ts` — interpolation, warmup, applyDelta (requires THREE.js mock or extraction)
- **All tests pass**: YES (120 Rust + 18 TypeScript = 138 total)
- **Notes**: No bugs found. The remaining gaps are all async network code (UDP/WebSocket loops) that would need integration tests with real sockets, and the client WorldView which depends on THREE.js.

## Run 1 — 2026-04-08 — First pass: all crates from zero

- **Files tested**:
  - `crates/shared/src/entity.rs` — 9 tests
  - `crates/shared/src/protocol.rs` — 12 tests
  - `crates/shared/src/spatial.rs` — 12 tests
  - `crates/sim-server/src/world.rs` — 21 tests
  - `crates/coordinator/src/registry.rs` — 19 tests
  - `crates/gateway/src/aggregator.rs` — 14 tests
- **Tests added**: 87 total
- **Gaps found**: Zero tests existed across the entire codebase
- **Coverage areas**:
  - Protocol correctness: JSON roundtrip (all ServerMessage, ClientMessage, CoordClientMsg, CoordServerMsg variants), bincode roundtrip (PeerMessage), empty snapshots, optional field defaults
  - Entity authority & transfers: shadow entity staleness, stale tick rejection, auth entity protection, recovery dedup (UUID hash distribution across N survivors, no duplicates), recovery edge cases (not in survivor list, wrong dead server)
  - Gateway aggregation: snapshot merging from N servers, entity dedup preferring authority match, binary frame encoding/decoding roundtrip (43 bytes/entity), rebalance event detection, authority lookup, least-loaded server selection
  - Simulation logic: spawn/remove lifecycle, move input (player speed 5.0, NPC speed 2.0, zero direction), world bounds clamping, initial NPC spawn bounds
  - Coordinator logic: register/unregister, heartbeat tracking, dead server detection (skips gateway), rebalance thresholds (min 10 entities AND 20% of avg), peer list broadcast (excludes self, excludes gateway from sim peers), drain directive, recovery broadcast with sorted survivor list
  - Spatial grid: world_to_cell conversion (positive, negative, boundary), cell_and_neighbors (9 cells), clamp_to_world (preserves Y, enforces X/Z bounds)
- **All tests pass**: YES (87/87)
- **Notes**: No bugs found by tests — code is well-written. The recovery dedup test confirms UUID hash distribution works correctly across any number of survivors.
