# Server Meshing Architecture

## Overview

True server meshing prototype: **multiple Rust simulation servers share authority over the same spatial area**. Unlike zone-based sharding (each server owns a region), entities are distributed across servers at the entity level. Any server can own any entity anywhere in the world. Load balancing happens per-entity, not per-zone.

```
Browser ←─WSS─→ Sim-Server 1 ←─UDP─→ Sim-Server 2
                      │                     │
                      └──────UDP──────→ Sim-Server 3
                                            │
                 Coordinator ←──────WS──────┘
                 (assignment, peer discovery, rebalancing)
```

Clients connect directly to sim-servers (true peer-to-peer mesh). The coordinator handles server registration, peer discovery, load-based rebalancing, and client assignment — but is not in the data path.

## Services

| Service | Language | Port(s) | Role |
|---------|----------|---------|------|
| **Coordinator** | Rust | 7870 (WS), 7871 (HTTP, optional TLS) | Server registry, peer discovery, load-based rebalancing, crash detection, client assignment and dashboard |
| **Sim-Server N** | Rust | 7877+N (WS, optional TLS), 9100+N (UDP), 8100+N (HTTP metrics) | Game simulation (Bevy ECS), entity authority, peer replication, direct client connections |
| **Board Dashboard** | Rust | 3000 | Per-board system health + sim-server control (UNO Q boards only) |
| **Client** | Three.js WebGPU | 4321 (dev) or via coordinator | Renders entities, player controls (WASD), debug overlay |

## Data Flow

### Game Loop (20 Hz, 50ms per tick)

```
  1. Input Processing
     ├─ Client sends MoveInput/SpawnEntity directly to its sim-server
     ├─ If entity is local: process directly
     └─ If entity is on another server: forward input via UDP to authority

  2. Simulation (Bevy ECS)
     ├─ NPC AI: random direction change every 3 seconds
     ├─ Integration: position += velocity × dt
     └─ Bounds clamping: [-5000, 5000] on X and Z

  3. Authority Transfers
     ├─ Coordinator issues RebalanceDirective to overloaded server
     ├─ Server selects peripheral NPCs, sends AuthorityTransfer via UDP
     └─ Target server accepts, promotes shadow → authoritative

  4. Replication
     ├─ Each server broadcasts authoritative entities to all peers (UDP/bincode)
     └─ Peers store as shadow entities (read-only copies)

  5. Client Broadcast
     ├─ Each server collects authoritative and shadow entities within client interest
     └─ Clients receive binary full or delta snapshots and acknowledge the tick
```

### Entity Authority Model

Every entity has exactly one authoritative server at any time. The `authority_server` field on `EntityState` tracks this.

- **Authoritative entities**: simulated, replicated to peers, broadcast to connected clients
- **Shadow entities**: received from peers, read-only, used for spatial awareness and crash recovery
- **Authority transfer**: coordinator directs overloaded server to send entities to underloaded server via UDP `AuthorityTransfer` message

## Entity Types

| Type | Shape (Client) | Behavior | Speed |
|------|----------------|----------|-------|
| **NPC** | Sphere (r=4) | Random wandering, direction change every 3s | 1.0–3.0 u/s |
| **Player** | Box (6×10×6) | Controlled by client WASD input | 5.0 u/s |

Entity IDs are UUIDs. Server IDs are u16 (1-65535).

## Rebalancing Algorithm

The coordinator checks weighted load every five seconds. A player contributes three units and an NPC contributes one. It selects an eligible source and target when their difference reaches 20% of average load and a size-dependent minimum. A source with transfers in flight waits. A new target becomes eligible ten seconds after registration. [Coordinator logic](../crates/coordinator/src/registry.rs)

The source sim-server selects a batch of roughly 10% of its eligible entities, with a minimum of one. It prefers NPCs and sorts each entity type by descending distance from the origin. A player transfer sends the owning client a reconnect message for the target server. [Transfer logic](../crates/sim-server/src/systems/transfers.rs)

## Crash Recovery

1. Coordinator detects a disconnect or missing heartbeats (15-second timeout)
2. Dead server is unregistered, peer list updated
3. Coordinator broadcasts `RecoverEntities` with the dead server ID and the sorted list of survivors
4. Each survivor hashes the entity IDs and promotes the shadows assigned to it
5. Subsequent rebalance checks use the surviving servers' reported loads

## Graceful Drain

1. Server sends `Draining` message to coordinator
2. Coordinator responds with `DrainDirective { target_servers }`
3. Server transfers ALL entities round-robin to targets via UDP `AuthorityTransfer`
4. Server exits when entity list is empty

## TLS

Both the coordinator (HTTPS) and sim-servers (WSS) support optional TLS via `--tls-cert` and `--tls-key` flags. When TLS is enabled, sim-servers register with `tls: true` and the coordinator returns `wss://` URLs in `/assign`. This is required for WebGPU (browsers require secure contexts).
