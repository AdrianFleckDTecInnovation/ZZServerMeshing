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

**Goal**: One Rust sim-server with a game loop, streaming entity state over WebSocket to a Three.js WebGPU client.

- [x] Rust workspace scaffold (shared, sim-server, coordinator crates)
- [x] Shared types: EntityId, EntityState, ServerId, protocol messages, spatial grid, tick constants
- [x] Sim-server game loop: fixed timestep, NPC wandering, WebSocket broadcast
- [x] Three.js WebGPU client: renderer, WebSocket client, world-view, debug overlay
- [x] start.sh / stop.sh scripts
- [x] CLAUDE.md

---

## Phase 2: Multi-Server Mesh (Core Innovation) ✅ DONE

**Goal**: Multiple sim-servers sharing the SAME area. Node.js gateway aggregates state. Client sees all entities seamlessly.

- [x] Coordinator service: server registration, peer discovery, peer list broadcast
- [x] Coordinator-server protocol (CoordClientMsg/CoordServerMsg in shared crate)
- [x] Sim-server coordinator client: register, receive peer list, heartbeats
- [x] UDP peer replication: each server sends authoritative entities to all peers via bincode/UDP
- [x] Shadow entity receiver: inbound UDP, upsert into world.shadow_entities, stale cleanup
- [x] Node.js gateway: connects to all sim-servers, aggregates snapshots, routes client inputs
- [x] Client connects through gateway, entities color-coded by authority server (red/blue/green)
- [x] Cluster scripts updated for multi-server (coordinator + N servers + gateway + client)

**Verified**: 3 servers running, 30 entities (10 per server), gateway aggregates all into single view.

---

## Phase 3: Dynamic Authority Transfer ✅ DONE

**Goal**: Entities move between servers at runtime based on load. The mesh adapts dynamically.

### Tasks

1. **Player experience** — Wire up proper player controls:
   - Client connects → gateway assigns a player entity on least-loaded server
   - Client receives "your player ID" and sends WASD movement via MoveInput
   - Gateway routes inputs to the entity's authority server
   - Optional: camera follow mode (toggle between overview and follow)

2. **Transfer protocol** (`sim-server/src/authority.rs`)
   - 3-step handshake: coordinator directive → source sends snapshot to target → target acknowledges
   - Single-writer invariant: source retains authority until target confirms
   - Edge case: defer transfer if entity is in an active interaction

3. **Load monitoring** (`coordinator/src/health.rs`)
   - Sliding window over heartbeats; compute avg_load, load_variance, overloaded flag

4. **Rebalancing algorithm** (`coordinator/src/balancer.rs`)
   - Trigger when server overloaded + another underloaded
   - Select entities closest to target's existing cluster (proximity clustering)
   - Cap at 10% transfer per cycle; 5s cooldown to prevent oscillation

5. **Cross-server interactions** (`sim-server/src/simulation.rs`)
   - InteractionRequest RPC via UDP for entity-to-entity across server boundaries
   - Simple collision/push: if two entities within distance D, repel each other

6. **Client smoothing** (`client/src/world-view.ts`)
   - Detect authority_server change between frames
   - Lerp over 100ms to smooth any position discontinuity during transfer

7. **Debug overlay expansion**
   - Load bars per server (entity count + CPU)
   - Transfer event log (scrolling: "Entity X: S1 → S2")
   - Spatial grid cell visualization

**Verify**: Spawn 1000 entities on one server, watch rebalancing distribute them across 3 servers. No visible glitches.

---

## Phase 4: Polish & Stress Testing ⬜ TODO

1. **Entity interpolation** — `previousPosition`/`currentPosition` lerp for smooth 60fps from 20Hz ticks
2. **Conflict resolution** — Prefer authoritative server's update; tick-number tiebreak; log warnings
3. **Server elasticity** — Hot-add (new server joins, gets entities on next rebalance); graceful drain; crash recovery via shadow entity data
4. **Stress testing** (`tools/stress-test.ts`) — Rapid entity spawning, synthetic inputs, latency measurement
5. **Performance profiling** — tracing spans in Rust, renderer.info in client; target: 2000 entities / 4 servers / 20 ticks/sec / <100ms e2e latency

---

## Risk Areas

| Risk | Mitigation |
|------|------------|
| Authority transfer atomicity | Timeout + rollback at each handshake step; source reclaims on failure |
| UDP packet ordering | Tick-tagged updates; receiver discards stale (lower tick) updates |
| Gateway aggregation latency | Forward immediately rather than batching; or run at 2x sim tick rate |
| Spatial grid subscription churn | Hysteresis: only update subscription after entity in new cell for 2+ ticks |
