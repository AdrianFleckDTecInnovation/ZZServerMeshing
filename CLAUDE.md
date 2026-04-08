# Server Meshing Prototype

True server meshing: multiple Rust simulation servers share authority over the SAME spatial area. Entities are distributed across servers at the entity level, not by zones.

## Architecture

- `crates/shared/` — Shared types: entities, protocol messages, spatial grid, tick constants
- `crates/sim-server/` — Rust game simulation server (run N instances, each with unique `--server-id`)
- `crates/coordinator/` — Mesh coordinator: server registry, health, load balancing (Phase 2+)
- `gateway/` — Node.js client gateway: aggregates state from all sim-servers for browser clients (Phase 2+)
- `client/` — Three.js WebGPU browser client: renders entities, debug overlay

## Build & Run

```sh
./start.sh    # builds Rust, starts sim-server + client dev server
./stop.sh     # kills both
```

Sim-server WebSocket: ws://localhost:7878
Client dev server: http://localhost:4321

### Manual run
```sh
cargo run -p sim-server -- --server-id 1 --port 7878 --initial-npcs 30
cd client && npm run dev
```

### Controls
- Click: spawn NPC at cursor position
- Shift+Click: spawn Player entity
- Scroll: zoom
- Drag: orbit camera

## Key Design Decisions
- HashMap<EntityId, EntityState> for entity storage (not a full ECS — serialization simplicity)
- UDP + bincode for server-to-server (Phase 2+)
- JSON over WebSocket for server-to-client
- 20 Hz tick rate (50ms per tick)
- InstancedMesh for rendering (single draw call for thousands of entities)

## Conventions
- Rust: standard formatting (`cargo fmt`), no unsafe unless justified
- TypeScript: strict mode, ES2022 target
- Entity IDs are UUIDs
- Server IDs are u16 starting at 1
- World bounds: -500 to 500 on X and Z axes
