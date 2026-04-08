# Codebase Reference

## Project Structure

```
ZZServerMeshing/
├── Cargo.toml                      # Rust workspace root
├── start.sh / stop.sh              # Cluster launch scripts
├── CLAUDE.md                       # AI assistant guidelines
├── PLAN.md                         # Phase tracking
│
├── crates/
│   ├── shared/                     # Shared types (217 LOC)
│   │   ├── entity.rs               # EntityId, EntityState, EntityType, ServerId
│   │   ├── protocol.rs             # All message types (8 enums)
│   │   ├── spatial.rs              # Grid: world_to_cell(), clamp_to_world()
│   │   └── tick.rs                 # TICK_RATE=20, TICK_DURATION_MS=50
│   │
│   ├── sim-server/                 # Game simulation server (850 LOC)
│   │   ├── main.rs                 # CLI args, tokio runtime, service startup
│   │   ├── world.rs                # Entity storage, shadow entities, transfers
│   │   ├── simulation.rs           # Game loop, NPC AI, rebalance handling
│   │   ├── network.rs              # WebSocket server + broadcast
│   │   ├── replication.rs          # UDP outbound to peers (bincode)
│   │   ├── receiver.rs             # UDP inbound, shadow merge, transfer accept
│   │   └── coordinator_client.rs   # Registration, heartbeats, directive handling
│   │
│   └── coordinator/                # Mesh coordinator (391 LOC)
│       ├── main.rs                 # WebSocket listener, event loop, timers
│       └── registry.rs             # Server registry, rebalance algo, crash recovery
│
├── gateway/                        # Node.js gateway (365 LOC)
│   ├── src/
│   │   ├── index.ts                # Main server, peer/client connections
│   │   ├── aggregator.ts           # Snapshot merge, metrics, authority lookup
│   │   └── router.ts               # Input routing to authority servers
│   ├── package.json
│   └── tsconfig.json
│
├── client/                         # Three.js WebGPU client (488 LOC)
│   ├── src/
│   │   ├── main.ts                 # Entry point
│   │   ├── renderer.ts             # WebGPU renderer, mesh pools, camera
│   │   ├── network.ts              # WebSocket client, reconnect
│   │   ├── world-view.ts           # Entity state, interpolation, colors
│   │   └── debug-overlay.ts        # FPS, server bars, network metrics
│   ├── index.html
│   ├── vite.config.ts
│   ├── package.json
│   └── tsconfig.json
│
├── tools/                          # Dev tools (152 LOC)
│   ├── stress-test.ts              # Load generator + metrics
│   └── package.json
│
└── docs/                           # Documentation
    ├── architecture.md
    ├── protocol.md
    ├── quickstart.md
    └── codebase.md                 # This file
```

**Total**: ~2,500 lines of code across 19 source files.

## Dependencies

### Rust (Cargo.toml)

| Crate | Version | Used For |
|-------|---------|----------|
| `tokio` | 1 | Async runtime (full features) |
| `tokio-tungstenite` | 0.24 | WebSocket server/client |
| `serde` + `serde_json` | 1 | JSON serialization |
| `bincode` | 1 | Binary serialization (UDP) |
| `glam` | 0.29 | Vec3 math (with serde feature) |
| `uuid` | 1 | Entity IDs (v4 + serde) |
| `clap` | 4 | CLI argument parsing |
| `tracing` + `tracing-subscriber` | 0.1/0.3 | Structured logging |
| `rand` | 0.8 | NPC behavior randomization |
| `futures-util` | 0.3 | Stream/Sink utilities |

### Node.js (package.json)

| Package | Used In | Purpose |
|---------|---------|---------|
| `ws` | gateway, tools | WebSocket server/client |
| `three` | client | Three.js WebGPU rendering |
| `vite` | client | Dev server + bundler |
| `tsx` | gateway, tools | TypeScript execution |
| `typescript` | all | Type checking |

## Sim-Server CLI Arguments

```
sim-server [OPTIONS]

Options:
  --server-id <ID>           Unique server ID [default: 1]
  --port <PORT>              WebSocket port [default: 7878]
  --udp-port <PORT>          UDP port for peers [default: 9101]
  --coordinator <ADDR>       Coordinator address [default: 127.0.0.1:7870]
  --initial-npcs <COUNT>     NPCs to spawn at start [default: 20]
```

## Coordinator CLI Arguments

```
coordinator [OPTIONS]

Options:
  --port <PORT>              WebSocket listen port [default: 7870]
```

## Gateway Environment Variables

```
COORDINATOR_URL=ws://127.0.0.1:7870    # Coordinator WebSocket URL
GATEWAY_PORT=4322                       # Client-facing WebSocket port
```

## Key Data Structures

### World (sim-server/world.rs)
```rust
World {
    server_id: ServerId,
    tick: u64,
    entities: HashMap<EntityId, EntityState>,        // Authoritative
    shadow_entities: HashMap<EntityId, EntityState>,  // From peers
    shadow_last_seen: HashMap<EntityId, u64>,         // Staleness
    peer_addrs: HashMap<u16, SocketAddr>,             // Peer UDP
    pending_transfers: Option<(Vec<String>, u16)>,    // Rebalance
    draining: bool,                                   // Drain mode
    drain_targets: Option<Vec<u16>>,
}
```

### EntityState (shared/entity.rs)
```rust
EntityState {
    id: EntityId,              // UUID
    position: Vec3,            // [x, 0, z]
    velocity: Vec3,            // [vx, 0, vz]
    entity_type: EntityType,   // Player | Npc
    authority_server: ServerId, // Owner
}
```

### Client Entity (world-view.ts)
```typescript
EntityData {
    id: string;
    position: Vector3;          // Interpolated (rendered)
    previousPosition: Vector3;  // Server tick N-1
    currentPosition: Vector3;   // Server tick N
    velocity: Vector3;
    entityType: 'Player' | 'Npc';
    authorityServer: number;
    hasSecondSnapshot: boolean;
}
```

## Build & Verify

```bash
# Rust
cargo build          # Build all crates
cargo fmt            # Format
cargo clippy         # Lint

# TypeScript
cd client && npx tsc --noEmit    # Type-check client
cd gateway && npx tsc --noEmit   # Type-check gateway (if tsconfig setup)

# Run stress test
cd tools && npx tsx stress-test.ts ws://localhost:4322 200 10
```
