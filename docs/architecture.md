# Server Meshing Architecture

## Overview

True server meshing prototype: **multiple Rust simulation servers share authority over the same spatial area**. Unlike zone-based sharding (each server owns a region), entities are distributed across servers at the entity level. Any server can own any entity anywhere in the world. Load balancing happens per-entity, not per-zone.

```
  Browser ←─WS──→ Gateway ←─WS──→ Sim-Server 1 ←─UDP──→ Sim-Server 2
                     │                   │                     │
                     │                   └──────UDP──────→ Sim-Server 3
                     │                                         │
                     └──────────WS──→ Coordinator ←────WS──────┘
```

## Services

| Service | Language | Port(s) | Role |
|---------|----------|---------|------|
| **Coordinator** | Rust | 7870 (WS) | Server registry, peer discovery, load-based rebalancing, crash detection |
| **Sim-Server N** | Rust | 7877+N (WS), 9100+N (UDP) | Game simulation, entity authority, peer replication |
| **Gateway** | Node.js | 4322 (WS) | Aggregates state from all servers, routes client inputs, player assignment |
| **Client** | Three.js WebGPU | 4321 (HTTP) | Renders entities, player controls (WASD), debug overlay |

## Data Flow

### Game Loop (20 Hz, 50ms per tick)

```
  1. Input Processing
     ├─ Client sends MoveInput/SpawnEntity → Gateway → authority server
     └─ Server processes inputs, updates entity velocities

  2. Simulation
     ├─ NPC AI: random direction change every 3 seconds
     ├─ Integration: position += velocity × dt
     └─ Bounds clamping: [-500, 500] on X and Z

  3. Authority Transfers
     ├─ Coordinator issues RebalanceDirective to overloaded server
     ├─ Server selects peripheral NPCs, sends AuthorityTransfer via UDP
     └─ Target server accepts, promotes shadow → authoritative

  4. Replication
     ├─ Each server broadcasts authoritative entities to all peers (UDP/bincode)
     └─ Peers store as shadow entities (read-only copies)

  5. Client Broadcast
     ├─ Each server sends WorldSnapshot (auth entities only) via WebSocket
     ├─ Gateway aggregates snapshots from all servers
     └─ Gateway broadcasts merged snapshot to browser clients (20 Hz)
```

### Entity Authority Model

Every entity has exactly one authoritative server at any time. The `authority_server` field on `EntityState` tracks this.

- **Authoritative entities**: simulated, replicated to peers, broadcast to gateway
- **Shadow entities**: received from peers, read-only, used for spatial awareness and crash recovery
- **Authority transfer**: coordinator directs overloaded server to send entities to underloaded server via UDP `AuthorityTransfer` message

## Entity Types

| Type | Shape (Client) | Behavior | Speed |
|------|----------------|----------|-------|
| **NPC** | Sphere (r=4) | Random wandering, direction change every 3s | 1.0–3.0 u/s |
| **Player** | Box (6×10×6) | Controlled by client WASD input | 5.0 u/s |

Entity IDs are UUIDs. Server IDs are u16 (1-65535, 0 reserved for gateway).

## Rebalancing Algorithm

Runs every 5 seconds in the coordinator:

1. Compute average entity count across all sim-servers
2. Find most overloaded and most underloaded server
3. Trigger if: `difference ≥ 5 entities AND difference ≥ 30% of average`
4. Transfer count: `(difference / 2), capped at 10% of overloaded server's entities, minimum 1`
5. Cooldown: 5 seconds between rebalances

The overloaded server picks which entities to transfer — selects NPCs farthest from origin (peripheral entities are cheapest to move). Players are never transferred automatically.

## Crash Recovery

1. Coordinator detects missing heartbeats (5-second timeout)
2. Dead server is unregistered, peer list updated
3. Coordinator broadcasts `RecoverEntities { dead_server_id }` to all survivors
4. Each survivor promotes shadow entities from the dead server to authoritative
5. Rebalancer evens out distribution within 10 seconds

## Graceful Drain

1. Server sends `Draining` message to coordinator
2. Coordinator responds with `DrainDirective { target_servers }`
3. Server transfers ALL entities round-robin to targets via UDP `AuthorityTransfer`
4. Server exits when entity list is empty
