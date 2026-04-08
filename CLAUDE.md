# Server Meshing Prototype

True server meshing: multiple Rust simulation servers share authority over the SAME spatial area. Entities are distributed across servers at the entity level, not by zones.

## Architecture

- `crates/shared/` — Shared types: entities, protocol messages, spatial grid, tick constants
- `crates/sim-server/` — Rust game simulation server (run N instances, each with unique `--server-id`)
- `crates/coordinator/` — Mesh coordinator: server registry, peer discovery, health monitoring
- `gateway/` — Node.js client gateway: aggregates state from all sim-servers, routes client inputs
- `client/` — Three.js WebGPU browser client: renders entities, debug overlay

## Build & Run

```sh
./start.sh        # launch cluster: coordinator + 3 sim-servers + gateway + client
./start.sh 5      # launch with 5 sim-servers instead
./stop.sh         # kill everything
```

### Ports
| Service       | Port  |
|---------------|-------|
| Coordinator   | 7870  |
| Sim-server N  | 7878+N (ws), 9100+N (udp) |
| Gateway       | 4322  |
| Client (Vite) | 4321  |

### Manual run
```sh
cargo run -p coordinator -- --port 7870
cargo run -p sim-server -- --server-id 1 --port 7878 --udp-port 9101 --coordinator 127.0.0.1:7870
cargo run -p sim-server -- --server-id 2 --port 7879 --udp-port 9102 --coordinator 127.0.0.1:7870
cd gateway && COORDINATOR_URL=ws://127.0.0.1:7870 GATEWAY_PORT=4322 npx tsx src/index.ts
cd client && npm run dev
```

### Controls
- Click: spawn NPC at cursor position (round-robin to random server)
- Shift+Click: spawn Player entity
- Scroll: zoom | Drag: orbit camera

## Key Design Decisions
- HashMap<EntityId, EntityState> for entity storage (not a full ECS — serialization simplicity)
- UDP + bincode for server-to-server entity replication
- JSON over WebSocket for server-to-client and coordinator protocol
- 20 Hz tick rate (50ms per tick)
- Each sim-server broadcasts only authoritative entities; gateway aggregates from all servers
- Entities color-coded by authority server in the client

## Conventions
- Rust: standard formatting (`cargo fmt`), no unsafe unless justified
- TypeScript: strict mode, ES2022 target
- Entity IDs are UUIDs, Server IDs are u16 starting at 1
- Server ID 0 is reserved for the gateway
- World bounds: -500 to 500 on X and Z axes
