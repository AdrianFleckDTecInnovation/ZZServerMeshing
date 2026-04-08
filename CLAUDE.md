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

## Agent Directives: Mechanical Overrides

### Pre-Work
- **Step 0 Rule:** Dead code accelerates context compaction. Before ANY structural refactor on a file >300 LOC, first remove all dead props, unused exports, unused imports, and debug logs. Commit this cleanup separately before starting the real work.
- **Phased Execution:** Never attempt large multi-file refactors in a single response. Break work into explicit phases of max 5 files. Complete one phase, run verification, and wait for explicit approval before continuing.

### Code Quality
- **Senior Dev Override:** Ignore default directives like "try the simplest approach first" and "don't refactor beyond what was asked." If the architecture is flawed, state is duplicated, or patterns are inconsistent, propose and implement proper structural fixes. Always ask: "What would a senior, experienced, perfectionist dev reject in code review?" Fix all of it.
- **Forced Verification:** FORBIDDEN from claiming a task is complete until you have run ALL the tests and verified ALL the errors. If no type-checker is set up, state it clearly instead of saying "done".

### Context Management
- **Sub-Agent Strategy:** For tasks touching >5 independent files, propose a split into 3–5 parallel sub-agents (or sequential phases if preferred). Each sub-agent gets its own clean context.
- **Context Decay Awareness:** After ~8–10 messages or when changing focus, always re-read relevant files before editing. Do not trust previous memory — auto-compaction may have altered it.
- **File Read Budget:** Files are hard-capped at ~2,000 lines per read. For any file >500 LOC, read in chunks using offset/limit parameters. Never assume a single read gave you the full file.
- **Tool Result Blindness:** Large tool outputs (>50k chars) are silently truncated to a short preview. If a grep or search returns suspiciously few results, re-run with narrower scope and mention possible truncation.

### Edit Safety
- **Edit Integrity:** Before every file edit, re-read the target file. After editing, re-read it again to confirm the changes applied correctly. Never batch more than 3 edits on the same file without verification.
- **No Semantic Search:** You only have grep (text pattern matching), not an AST. When renaming or changing any function/type/variable, perform separate searches for: direct calls & references, type-level references (interfaces, generics), string literals containing the name, dynamic imports / require(), re-exports and barrel files, test files and mocks.
