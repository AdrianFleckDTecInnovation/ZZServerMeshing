# Codebase Reference

## Project Structure

```
ZZServerMeshing/
├── Cargo.toml                      # Rust workspace root
├── start.sh / stop.sh              # Local cluster launch scripts
├── start-unoq.sh / stop-unoq.sh   # UNO Q board deployment scripts
├── CLAUDE.md                       # AI assistant guidelines
│
├── crates/
│   ├── shared/                     # Shared types
│   │   ├── entity.rs               # EntityId, EntityState, EntityType, ServerId
│   │   ├── protocol.rs             # All message types (ServerMessage, ClientMessage, PeerMessage, CoordClientMsg, CoordServerMsg)
│   │   ├── spatial.rs              # Grid: world_to_cell(), clamp_to_world()
│   │   └── tick.rs                 # TICK_RATE=20, TICK_DURATION_MS=50
│   │
│   ├── sim-server/                 # Game simulation server (Bevy ECS)
│   │   ├── main.rs                 # CLI args, tokio runtime, TLS setup, service startup
│   │   ├── components.rs           # Bevy ECS components (Position, Velocity, Authority, etc.)
│   │   ├── conversion.rs           # ECS ↔ EntityState conversion
│   │   ├── systems.rs              # ECS systems (simulation, NPC AI, input processing)
│   │   ├── game_loop.rs            # 20Hz fixed timestep loop, phase timing
│   │   ├── network.rs              # WebSocket server (WS/WSS) + binary broadcast
│   │   ├── replication.rs          # UDP outbound to peers (bincode)
│   │   ├── receiver.rs             # UDP inbound, shadow merge, transfer accept
│   │   ├── coordinator_client.rs   # Registration, heartbeats, directive handling
│   │   └── metrics_server.rs       # HTTP metrics endpoint
│   │
│   ├── coordinator/                # Mesh coordinator
│   │   ├── main.rs                 # WebSocket listener, HTTP(S) server, TLS, static file serving
│   │   └── registry.rs             # Server registry, rebalance algo, crash recovery
│   │
│   └── board-dash/                 # UNO Q board dashboard
│       ├── main.rs                 # Axum web server
│       ├── routes.rs               # API endpoints (stats, mesh status, start/stop)
│       └── static/                 # Dashboard web assets
│
├── client/                         # Three.js WebGPU client
│   ├── src/
│   │   ├── main.ts                 # Entry point, coordinator assignment
│   │   ├── renderer.ts             # WebGPU renderer, mesh pools, camera
│   │   ├── network.ts              # WebSocket client, binary frame decoding, reconnect
│   │   ├── world-view.ts           # Entity state, interpolation, colors
│   │   ├── binary-protocol.ts      # Binary frame decoder (47-byte entities)
│   │   ├── prediction.ts           # Client-side prediction manager
│   │   └── debug-overlay.ts        # FPS, server bars, network metrics
│   ├── index.html
│   ├── vite.config.ts              # Dev server + proxy config
│   └── package.json
│
├── certs/                          # TLS certificates (gitignored)
│
├── tools/                          # Dev tools
│   ├── stress-test.ts              # Load generator + metrics
│   └── test-suite/                 # Automated test scenarios
│
└── docs/                           # Documentation
```

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
| `bevy_ecs` | 0.15 | Entity Component System (sim-server) |
| `tracing` + `tracing-subscriber` | 0.1/0.3 | Structured logging |
| `rand` | 0.8 | NPC behavior randomization |
| `futures-util` | 0.3 | Stream/Sink utilities |
| `rustls` + `tokio-rustls` | 0.23/0.26 | TLS (coordinator + sim-server) |
| `axum` | 0.8 | HTTP server (board-dash) |

### Node.js (package.json)

| Package | Used In | Purpose |
|---------|---------|---------|
| `three` | client | Three.js WebGPU rendering |
| `vite` | client | Dev server + bundler |
| `typescript` | client, tools | Type checking |

## CLI Arguments

### Sim-Server

```
sim-server [OPTIONS]

Options:
  --server-id <ID>           Unique server ID (u16, starting at 1)
  --port <PORT>              WebSocket port [default: 7878]
  --udp-port <PORT>          UDP port for peers [default: 9101]
  --metrics-port <PORT>      HTTP metrics port [default: 8101]
  --coordinator <ADDR>       Coordinator address [default: 127.0.0.1:7870]
  --advertise-host <HOST>    IP to advertise to coordinator (for LAN routing)
  --initial-npcs <COUNT>     NPCs to spawn at start [default: 0]
  --tls-cert <PATH>          TLS certificate file (PEM) — enables WSS
  --tls-key <PATH>           TLS private key file (PEM)
```

### Coordinator

```
coordinator [OPTIONS]

Options:
  --port <PORT>              WebSocket listen port [default: 7870]
  --http-port <PORT>         HTTP(S) port [default: 7871]
  --static-dir <PATH>        Directory to serve client files from
  --tls-cert <PATH>          TLS certificate file (PEM) — enables HTTPS
  --tls-key <PATH>           TLS private key file (PEM)
```

## Key Data Structures

### EntityState (shared/entity.rs)
```rust
EntityState {
    id: EntityId,              // UUID
    position: Vec3,            // [x, 0, z]
    velocity: Vec3,            // [vx, 0, vz]
    entity_type: EntityType,   // Player | Npc
    authority_server: ServerId, // Owner
    last_input_seq: u32,       // For client prediction
}
```

### Bevy ECS Components (sim-server/components.rs)
```rust
// Core entity components
Position(Vec3)
Velocity(Vec3)
EntityTypeComp(EntityType)
AuthorityServer(ServerId)
NetworkId(EntityId)
LastInputSeq(u32)
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
    lastInputSeq: number;
    hasSecondSnapshot: boolean;
}
```

## Build & Verify

```bash
# Rust
cargo build          # Build all crates
cargo test           # Run tests
cargo fmt            # Format
cargo clippy         # Lint

# TypeScript
cd client && npx tsc --noEmit    # Type-check client

# Cross-compile for boards
cross build --release --target aarch64-unknown-linux-gnu -p sim-server -p board-dash
cross build --release --target armv7-unknown-linux-gnueabihf -p coordinator
```
