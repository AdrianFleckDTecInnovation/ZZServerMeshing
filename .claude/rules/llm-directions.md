# LLM Developer Onboarding

You are working on a **server meshing prototype** — the approach Star Citizen is attempting for large-scale multiplayer. The core idea: **multiple game servers share authority over the SAME spatial area**, distributing entities (players, NPCs) across servers at the entity level, not by zones.

## What This Project Is

A working prototype across Rust, TypeScript, and Three.js WebGPU. You can `./start.sh` to launch 3 game servers sharing one world, open a browser, see entities moving, spawn more, and watch them rebalance across servers in real time. The mesh also runs entirely on embedded hardware (3 Arduino UNO Q boards + Pi Zero W2).

## The Four Services

```
Browser ←─WSS─→ Sim-Server 1 ←─UDP─→ Sim-Server 2
                      │                     │
                      └──UDP──→ Sim-Server 3
                                     │
                              Coordinator
                         (assignment + control)
```

1. **Coordinator** (`crates/coordinator/`, Rust, port 7870 ws, 7871 https) — Servers register here on startup. It broadcasts peer lists, monitors heartbeats, detects crashes, issues rebalance directives, and assigns clients to the least-loaded server via `/assign`. Can serve the built client app via `--static-dir`. Supports TLS.

2. **Sim-Server** (`crates/sim-server/`, Rust, ports 7878+N ws/wss, 9100+N udp, 8100+N metrics) — The game server. Run N instances. Each runs a 20Hz Bevy ECS game loop simulating its authoritative entities. Replicates entity state to peers via UDP/bincode. Receives shadow copies of peer entities. Broadcasts full world view (auth + shadows) directly to connected clients via binary WebSocket frames. Supports TLS (WSS).

3. **Client** (`client/`, Three.js WebGPU, port 4321 dev) — Browser app. Renders entities as colored spheres (NPCs) and boxes (players). Color = which server owns the entity. WASD moves your player. Click spawns NPCs. Debug overlay shows per-server load bars and network metrics. Connects directly to a sim-server after getting an assignment from the coordinator.

4. **Shared** (`crates/shared/`) — Types used by both Rust crates: `EntityId`, `EntityState`, `ServerId`, all protocol message enums, spatial grid helpers, tick constants, gameplay constants (`PLAYER_SPEED`, `NPC_SPEED`), and `tls::build_tls_acceptor`.

Additional: **Board Dashboard** (`crates/board-dash/`, Rust, port 3000) — Per-board system health monitoring + sim-server start/stop control. Only deployed on UNO Q boards.

## The Core Abstraction: Entity Authority

Every entity has exactly one authoritative server at any time, tracked by `EntityState.authority_server`. The sim-server that owns an entity simulates it, replicates it to peers, and broadcasts it to its connected clients. Other servers hold read-only "shadow copies" for spatial awareness and crash recovery.

Authority can transfer between servers. The coordinator detects load imbalance and tells the overloaded server to send entities to an underloaded one. The transfer is a UDP message with the full entity state. The target accepts, the source removes. The entity seamlessly changes color in the client.

## Where Things Live

| What | Where | Key facts |
|------|-------|-----------|
| Entity types & state | `crates/shared/src/entity.rs` | `EntityId(Uuid)`, `ServerId(u16)`, `EntityState { id, position, velocity, entity_type, authority_server, last_input_seq }` |
| All protocol messages | `crates/shared/src/protocol.rs` | 5 enums: `ServerMessage`, `ClientMessage`, `PeerMessage`, `CoordClientMsg`, `CoordServerMsg` |
| ECS components | `crates/sim-server/src/components.rs` | Position, Velocity, EntityTypeComp, AuthorityServer, NetworkId, LastInputSeq |
| ECS ↔ network conversion | `crates/sim-server/src/conversion.rs` | Converts between Bevy ECS components and EntityState for serialization |
| ECS systems | `crates/sim-server/src/systems/` | Directory module: `inbound.rs`, `input.rs`, `transfers.rs`, `drain.rs`, `replication.rs`, `snapshot.rs`, `simulation.rs`, `coordinator.rs`, `shadow.rs`, `metrics.rs` |
| Game loop & phases | `crates/sim-server/src/game_loop.rs` | 20Hz tick, phase timing (input, sim, transfers, replication, shadow cleanup) |
| UDP replication | `crates/sim-server/src/replication.rs` + `receiver.rs` | Outbound: bincode serialize all auth entities to peers. Inbound: merge into shadows with tick-based conflict resolution |
| Coordinator client | `crates/sim-server/src/coordinator_client.rs` | Registration (with `tls` flag), heartbeats, handling PeerList/RebalanceDirective/DrainDirective/RecoverEntities |
| WebSocket server | `crates/sim-server/src/network.rs` | WS/WSS server, broadcasts binary frames each tick, receives ClientMessage |
| Metrics server | `crates/sim-server/src/metrics_server.rs` | HTTP endpoint with per-tick phase timing (p50/p95/p99) |
| Server registry | `crates/coordinator/src/registry.rs` | Peer tracking, rebalance algorithm, crash detection (15s heartbeat timeout), drain orchestration |
| TLS utilities | `crates/shared/src/tls.rs` | `build_tls_acceptor` -- single shared implementation used by both sim-server and coordinator |
| Peer spatial bounds | `crates/sim-server/src/peer_bounds.rs` | `PeerBounds` struct for per-peer replication filtering (moved from shared) |
| HTTP/HTTPS server | `crates/coordinator/src/main.rs` | `/assign` (client assignment), `/servers` (server list), `/dashboard` (HTML dashboard), static file serving |
| Client rendering | `client/src/renderer.ts` | Three.js WebGPU, shared material cache, mesh pooling, camera follow |
| Client entity state | `client/src/world-view.ts` | Tick-based interpolation (previousPosition/currentPosition with alpha), server color mapping |
| Binary protocol | `client/src/binary-protocol.ts` | Decodes 47-byte entity frames from server |
| Client prediction | `client/src/prediction.ts` | Client-side prediction with server reconciliation via `last_input_seq` |

## How to Build and Run

```bash
cargo build                          # All Rust crates
cd client && npm install && cd ..    # Client deps
./start.sh                           # Launch everything (3 servers)
./stop.sh                            # Kill everything
```

Verify: `cargo build` for Rust, `cd client && npx tsc --noEmit` for TypeScript.

### UNO Q Board Deployment
```bash
./start-unoq.sh                     # Cross-compile, deploy, start on boards
./stop-unoq.sh                      # Stop everything
```

## Protocol Quick Reference

**Binary over WebSocket** (server→client, every tick):
- 27-byte header + 47 bytes per entity (UUID, position, velocity, type, authority, input seq)
- Frame types: `0x01` full snapshot, `0x02` delta

**JSON over WebSocket** (client→server):
- `SpawnEntity { position, entity_type }` — client→server
- `MoveInput { entity_id, direction, seq }` — client→server (forwarded to authority if non-local)
- `PlayerAssigned { player_id }` — server→client on connect
- `Reconnect { url }` — server→client on authority transfer

**JSON over WebSocket** (server↔coordinator):
- `Register { server_id, udp_port, ws_port, tls }` — server→coordinator
- `PeerList { peers[] }` — coordinator→servers on topology change
- `RebalanceDirective { entity_ids, target_server }` — coordinator→overloaded server
- `RecoverEntities { dead_server_id }` — coordinator→all survivors

**bincode over UDP** (server↔server, every tick):
- `EntityStateUpdate { tick, server_id, entities[] }` — replication
- `AuthorityTransfer { tick, entity_id, from_server, to_server, full_state }` — ownership handoff
- `ForwardedInput { entity_id, direction, seq }` — input routing

## Key Constants

| Name | Value | Where |
|------|-------|-------|
| `TICK_RATE` | 20 Hz | `shared/tick.rs` |
| `TICK_DURATION_MS` | 50 ms | `shared/tick.rs` |
| `TICK_DURATION` | 50ms Duration | `shared/tick.rs` — single `std::time::Duration` constant |
| `PLAYER_SPEED` | 5.0 | `shared/tick.rs` — must match `client/src/config.ts` |
| `NPC_SPEED` | 2.0 | `shared/tick.rs` — fallback speed for non-player entities |
| `WORLD_EXTENT` | 5000 | `shared/spatial.rs` — bounds are [-5000, 5000] |
| `AOI_RADIUS` | 500 | `shared/spatial.rs` — client AoI filter radius |
| `CELL_SIZE` | 50 | `shared/spatial.rs` — spatial grid |
| `SHADOW_STALE_TICKS` | 300 (15s) | `sim-server/systems/shadow.rs` — shadow cleanup |
| Rebalance interval | 5s | `coordinator/main.rs` |
| Heartbeat timeout | 15s | `coordinator/registry.rs` |

## Conventions

- Rust: `cargo fmt`, no unsafe. Entity storage uses Bevy ECS with conversion to `EntityState` for network serialization.
- TypeScript: strict mode, ES2022. Imports: `three/webgpu` for WebGPU, `three/tsl` for shaders.
- Entity IDs: UUID v4. Server IDs: u16, 1-indexed.
- World: flat ground plane, Y is always 0 for entities, 3D rendering with elevated camera.
- Serialization: glam `Vec3` serializes as `[x, y, z]` array in JSON. `EntityId` and `ServerId` are serde newtypes (serialize transparently as string/number).

## Common Tasks

**Add a new entity field**: Add component in `components.rs`, update `EntityState` in `shared/entity.rs`, update conversion in `conversion.rs`. Update `RawEntity` in `client/world-view.ts` and binary decoder in `binary-protocol.ts` to match.

**Add a new coordinator→server message**: Add variant to `CoordServerMsg` in `shared/protocol.rs`. Handle in `coordinator_client.rs` match arm. Handle in `systems/coordinator.rs`. Handle exhaustiveness errors in any other match sites.

**Add a new client→server message**: Add variant to `ClientMessage` in `shared/protocol.rs`. Handle in `systems/input.rs` (`process_client_inputs`). If the input targets a non-local entity, add forwarding logic.

**Add a new ECS system**: Create the system function in the appropriate `systems/*.rs` module. Re-export it from `systems/mod.rs`. Add it to the schedule in `schedule.rs` and the tick loop in `game_loop.rs`.

**Change tick rate**: Update `TICK_RATE` in `shared/tick.rs`. `TICK_DURATION_MS` and `TICK_DURATION` update automatically. Client interpolation in `world-view.ts` uses a hardcoded 50ms — update `TICK_DURATION_MS` there too.
