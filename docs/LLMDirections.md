# LLM Developer Onboarding

You are working on a **server meshing prototype** — the approach Star Citizen is attempting for large-scale multiplayer. The core idea: **multiple game servers share authority over the SAME spatial area**, distributing entities (players, NPCs) across servers at the entity level, not by zones.

## What This Project Is

A working prototype with ~2,500 LOC across Rust, TypeScript, and Three.js WebGPU. You can `./start.sh` to launch 3 game servers sharing one world, open a browser, see entities moving, spawn more, and watch them rebalance across servers in real time.

## The Five Services

```
Browser ←─WS─→ Gateway ←─WS─→ Sim-Servers ←─UDP─→ each other
                  │                  │
                  └──WS──→ Coordinator ←──WS──┘
```

1. **Coordinator** (`crates/coordinator/`, Rust, port 7870) — Servers register here on startup. It broadcasts peer lists, monitors heartbeats, detects crashes, and issues rebalance directives.

2. **Sim-Server** (`crates/sim-server/`, Rust, ports 7878+N ws, 9100+N udp) — The game server. Run N instances. Each runs a 20Hz game loop simulating its authoritative entities. Replicates entity state to peers via UDP/bincode. Receives shadow copies of peer entities. Handles authority transfers.

3. **Gateway** (`gateway/`, Node.js, port 4322) — Connects to all sim-servers via WebSocket. Merges their entity snapshots into one unified view. Serves browser clients. Routes player inputs to the correct authority server. Assigns players to the least-loaded server on connect.

4. **Client** (`client/`, Three.js WebGPU, port 4321) — Browser app. Renders entities as colored spheres (NPCs) and boxes (players). Color = which server owns the entity. WASD moves your player. Click spawns NPCs. Debug overlay shows per-server load bars and network metrics.

5. **Shared** (`crates/shared/`) — Types used by both Rust crates: `EntityId`, `EntityState`, `ServerId`, all protocol message enums, spatial grid helpers, tick constants.

## The Core Abstraction: Entity Authority

Every entity has exactly one authoritative server at any time, tracked by `EntityState.authority_server`. The sim-server that owns an entity simulates it, replicates it to peers, and broadcasts it to the gateway. Other servers hold read-only "shadow copies" for spatial awareness and crash recovery.

Authority can transfer between servers. The coordinator detects load imbalance and tells the overloaded server to send entities to an underloaded one. The transfer is a UDP message with the full entity state. The target accepts, the source removes. The entity seamlessly changes color in the client.

## Where Things Live

| What | Where | Key facts |
|------|-------|-----------|
| Entity types & state | `crates/shared/src/entity.rs` | `EntityId(Uuid)`, `ServerId(u16)`, `EntityState { id, position, velocity, entity_type, authority_server }` |
| All protocol messages | `crates/shared/src/protocol.rs` | 5 enums: `ServerMessage`, `ClientMessage`, `PeerMessage`, `CoordClientMsg`, `CoordServerMsg` |
| Game loop & simulation | `crates/sim-server/src/simulation.rs` | 20Hz tick, NPC wandering, authority transfers, drain logic |
| Entity storage | `crates/sim-server/src/world.rs` | `HashMap<EntityId, EntityState>` for auth + shadow entities |
| UDP replication | `crates/sim-server/src/replication.rs` + `receiver.rs` | Outbound: bincode serialize all auth entities to peers. Inbound: merge into shadows with tick-based conflict resolution |
| Coordinator client | `crates/sim-server/src/coordinator_client.rs` | Registration, heartbeats, handling PeerList/RebalanceDirective/DrainDirective/RecoverEntities |
| WebSocket server | `crates/sim-server/src/network.rs` | Broadcasts `WorldSnapshot` each tick, receives `ClientMessage` |
| Server registry | `crates/coordinator/src/registry.rs` | Peer tracking, rebalance algorithm, crash detection (5s heartbeat timeout), drain orchestration |
| Snapshot aggregation | `gateway/src/aggregator.ts` | Merges snapshots from all servers, tracks metrics, authority lookup |
| Input routing | `gateway/src/router.ts` | `MoveInput` → authority server, `SpawnEntity` → random server |
| Client rendering | `client/src/renderer.ts` | Three.js WebGPU, shared material cache, mesh pooling, camera follow |
| Client entity state | `client/src/world-view.ts` | Tick-based interpolation (previousPosition/currentPosition with alpha), server color mapping |

## How to Build and Run

```bash
cargo build                          # All Rust crates
cd client && npm install && cd ..    # Client deps
cd gateway && npm install && cd ..   # Gateway deps
./start.sh                           # Launch everything (3 servers)
./stop.sh                            # Kill everything
```

Verify: `cargo build` for Rust, `cd client && npx tsc --noEmit` for TypeScript.

## Protocol Quick Reference

**JSON over WebSocket** (human-readable, server↔client and server↔coordinator):
- `WorldSnapshot { tick, entities[] }` — server→gateway, every tick
- `SpawnEntity { position, entity_type }` — client→gateway→server
- `MoveInput { entity_id, direction }` — client→gateway→authority server
- `PlayerAssigned { player_id }` — gateway→client on connect
- `PeerList { peers[] }` — coordinator→servers on topology change
- `RebalanceDirective { entity_ids, target_server }` — coordinator→overloaded server
- `RecoverEntities { dead_server_id }` — coordinator→all survivors

**bincode over UDP** (binary, server↔server, every tick):
- `EntityStateUpdate { tick, server_id, entities[] }` — replication
- `AuthorityTransfer { tick, entity_id, from_server, to_server, full_state }` — ownership handoff

## Key Constants

| Name | Value | Where |
|------|-------|-------|
| `TICK_RATE` | 20 Hz | `shared/tick.rs` |
| `TICK_DURATION_MS` | 50 ms | `shared/tick.rs` |
| `WORLD_EXTENT` | 500 | `shared/spatial.rs` — bounds are [-500, 500] |
| `CELL_SIZE` | 50 | `shared/spatial.rs` — spatial grid |
| `SHADOW_STALE_TICKS` | 100 (5s) | `sim-server/world.rs` — shadow cleanup |
| Rebalance interval | 5s | `coordinator/main.rs` |
| Heartbeat timeout | 5s | `coordinator/registry.rs` |

## Conventions

- Rust: `cargo fmt`, no unsafe. Entity storage is `HashMap` not ECS.
- TypeScript: strict mode, ES2022. Imports: `three/webgpu` for WebGPU, `three/tsl` for shaders.
- Entity IDs: UUID v4. Server IDs: u16, 1-indexed. 0 = gateway.
- World: flat ground plane, Y is always 0 for entities, 3D rendering with elevated camera.
- Serialization: glam `Vec3` serializes as `[x, y, z]` array in JSON. `EntityId` and `ServerId` are serde newtypes (serialize transparently as string/number).

## What's Been Built (Phases 1-4)

1. **Single server + WebGPU client** — game loop, entity rendering, click-to-spawn
2. **Multi-server mesh** — coordinator, UDP replication, shadow entities, gateway aggregation
3. **Dynamic authority** — player controls (WASD), load-based rebalancing, authority transfer
4. **Polish** — tick-based conflict resolution, entity interpolation, shared materials + mesh pooling, crash detection/recovery, graceful drain, stress test tool, network metrics overlay

## Common Tasks

**Add a new entity field**: Update `EntityState` in `shared/entity.rs`. It auto-propagates through serde to all protocols. Update `RawEntity` in `client/world-view.ts` to match.

**Add a new coordinator→server message**: Add variant to `CoordServerMsg` in `shared/protocol.rs`. Handle in `coordinator_client.rs` match arm. Handle exhaustiveness errors in any other match sites.

**Add a new client→server message**: Add variant to `ClientMessage` in `shared/protocol.rs`. Handle in `simulation.rs:process_input()`. If it needs routing, update `gateway/router.ts`.

**Change tick rate**: Update `TICK_RATE` in `shared/tick.rs`. Update `TICK_DURATION_MS` accordingly. Client interpolation in `world-view.ts` uses a hardcoded 50ms — update `TICK_DURATION_MS` there too.

**Add a new server type/service**: Register with coordinator using `CoordClientMsg::Register`. Use server_id > 0 for sim-servers, 0 for gateway. The coordinator filters server_id 0 from peer lists sent to sim-servers.
