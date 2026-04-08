# Server Meshing Prototype - Implementation Plan

## Context

Build a prototype demonstrating **true server meshing** for large-scale online game servers. Unlike traditional zone-based sharding (each server owns a region), server meshing has **multiple servers sharing authority over the SAME spatial area**. Entities (players, NPCs) are distributed across servers dynamically at the entity level, not the zone level. This is the approach Star Citizen is attempting -- one of the hardest problems in online gaming.

**Tech stack**: Rust (simulation servers + coordinator) | Node.js/TypeScript (client gateway) | Three.js WebGPU (browser renderer)

---

## Project Structure

```
ZZServerMeshing/
├── CLAUDE.md
├── Cargo.toml                    # Rust workspace root
├── crates/
│   ├── shared/                   # Shared types, protocols, spatial grid
│   │   └── src/ { lib.rs, entity.rs, protocol.rs, spatial.rs, tick.rs }
│   ├── sim-server/               # Game simulation server (run N instances)
│   │   └── src/ { main.rs, world.rs, simulation.rs, replication.rs, receiver.rs, coordinator_client.rs, network.rs }
│   └── coordinator/              # Mesh coordination service
│       └── src/ { main.rs, registry.rs }
├── gateway/                      # Node.js client gateway (aggregates all sim-servers)
│   └── src/ { index.ts, aggregator.ts, router.ts }
├── client/                       # Three.js WebGPU browser client
│   └── src/ { main.ts, renderer.ts, network.ts, world-view.ts, debug-overlay.ts }
├── tools/                        # Stress testing
│   └── stress-test.ts
├── docs/                         # Documentation
│   ├── architecture.md, protocol.md, quickstart.md, codebase.md, LLMDirections.md
├── start.sh / stop.sh            # Cluster launch/stop scripts
└── PLAN.md                       # This file
```

---

## Key Architecture Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Entity storage | `HashMap<EntityId, EntityState>` | Trivially serializable for replication; full ECS (Bevy) is overkill for a network-focused prototype |
| Server-to-server transport | UDP + bincode | Simplest for local prototype; QUIC adds TLS/stream complexity with no benefit locally |
| Spatial interest mgmt | Flat 2D grid | O(1) lookup, simple subscriptions; quadtree unnecessary when rebalancing prevents heavy clustering |
| Client gateway | Separate Node.js process | Keeps sim-servers focused on simulation; WebSocket fanout + JSON merging is Node's strength |
| Tick rate | 20 Hz (50ms) | Industry standard (WoW: 20Hz, Fortnite: 30Hz); client interpolation bridges to 60fps |

---

## Phase 1: Single Server + Renderer (Foundation) ✅ DONE

- [x] Rust workspace scaffold (shared, sim-server, coordinator crates)
- [x] Shared types: EntityId, EntityState, ServerId, protocol messages, spatial grid, tick constants
- [x] Sim-server game loop: fixed timestep, NPC wandering, WebSocket broadcast
- [x] Three.js WebGPU client: renderer, WebSocket client, world-view, debug overlay
- [x] start.sh / stop.sh scripts, CLAUDE.md

---

## Phase 2: Multi-Server Mesh (Core Innovation) ✅ DONE

- [x] Coordinator service: server registration, peer discovery, peer list broadcast
- [x] Coordinator-server protocol (CoordClientMsg/CoordServerMsg)
- [x] Sim-server coordinator client: register, receive peer list, heartbeats
- [x] UDP peer replication: bincode entity state to all peers each tick
- [x] Shadow entity receiver: inbound UDP, merge into shadow_entities, stale cleanup
- [x] Node.js gateway: aggregates snapshots from all sim-servers, routes client inputs
- [x] Client connects through gateway, entities color-coded by authority server
- [x] Cluster scripts for multi-server (coordinator + N servers + gateway + client)

---

## Phase 3: Dynamic Authority Transfer ✅ DONE

- [x] Player assignment: gateway auto-spawns player on least-loaded server, sends PlayerAssigned
- [x] WASD movement: client sends MoveInput, gateway routes to authority server
- [x] Camera follow mode (F key toggle)
- [x] Rebalancing algorithm in coordinator: triggers on >30% imbalance, transfers peripheral NPCs
- [x] Authority transfer via UDP AuthorityTransfer message with full entity state
- [x] Player cleanup on disconnect (RemoveEntity)
- [x] Debug overlay: per-server load bars, player position, camera mode

**Not implemented** (descoped as unnecessary for prototype):
- Cross-server interaction RPC (collision/push)
- 3-step transfer handshake (simplified to direct send+accept)
- Spatial grid cell visualization in client

---

## Phase 4: Polish & Stress Testing ✅ DONE

- [x] **Tick-based conflict resolution** — `apply_peer_update()` discards stale UDP packets (lower tick). `AuthorityTransfer` validates target server, rejects duplicates. Added tick field to transfer message.
- [x] **Entity interpolation** — Buffer-based: `previousPosition`/`currentPosition` with tick-timed alpha (`timeSinceSnapshot / 50ms`, capped at 1.2 for slight extrapolation). Smooth 60fps from 20Hz ticks.
- [x] **Client performance** — Shared material cache (one per server color), mesh pooling (reuse on entity remove/create). No per-entity material allocation.
- [x] **Network metrics** — Gateway tracks snapshots/sec, bandwidth, rebalance events. Embedded as `_metrics` in client snapshots. Debug overlay shows live stats.
- [x] **Server elasticity** — Crash detection (5s heartbeat timeout), graceful drain protocol (DrainDirective → transfer all → exit), crash recovery (promote shadow entities from dead server).
- [x] **Stress test tool** — `tools/stress-test.ts`: configurable entity count + spawn rate, tracks distribution, snapshot rate, rebalance events. Verified: 200 entities, 20 snap/s, 50.6ms avg interval.

---

## Risk Areas

| Risk | Mitigation |
|------|------------|
| Authority transfer atomicity | Simplified to direct send+accept; rebalancer self-corrects within 10s |
| UDP packet ordering | Tick-tagged updates; receiver discards stale (lower tick) updates |
| Gateway aggregation latency | Forwards every tick (50ms); no additional batching |
| Crash recovery duplication | Multiple survivors may adopt same shadows; rebalancer evens out |
