# Server Meshing Prototype

True server meshing: multiple Rust simulation servers share authority over the SAME spatial area. Entities are distributed across servers at the entity level, not by zones.

## Architecture

- `crates/shared/` — Shared types: entities, protocol messages, spatial grid, tick constants
- `crates/sim-server/` — Rust game simulation server (run N instances, each with unique `--server-id`)
- `crates/coordinator/` — Mesh coordinator: server registry, peer discovery, health monitoring, client assignment
- `client/` — Three.js WebGPU browser client: renders entities, debug overlay

Clients connect directly to sim-servers (true peer-to-peer mesh). Each sim-server broadcasts the full world view (authoritative + shadow entities) to its clients. Inputs for entities owned by other servers are forwarded via UDP. The coordinator provides an HTTP assignment endpoint for load-balanced server selection.

## Build & Run

```sh
./start.sh        # launch cluster: coordinator + 3 sim-servers + client
./start.sh 5      # launch with 5 sim-servers instead
./stop.sh         # kill everything
```

### Ports
| Service            | Port  |
|--------------------|-------|
| Coordinator (ws)   | 7870  |
| Coordinator (http) | 7871  |
| Sim-server N       | 7878+N (ws), 9100+N (udp), 8100+N (metrics) |
| Client (Vite)      | 4321  |

### Manual run
```sh
cargo run -p coordinator -- --port 7870 --http-port 7871
cargo run -p sim-server -- --server-id 1 --port 7878 --udp-port 9101 --coordinator 127.0.0.1:7870
cargo run -p sim-server -- --server-id 2 --port 7879 --udp-port 9102 --coordinator 127.0.0.1:7870
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
- Each sim-server broadcasts authoritative + shadow entities directly to connected clients
- Inputs for non-local entities are forwarded between peers via UDP
- Entities color-coded by authority server in the client

## Deployment Modes

### Local (Mac only)
Everything runs on the dev Mac. No TLS, plain `ws://`. WebGPU works because localhost is a secure context.

```sh
./start.sh                             # coordinator + 3 sim-servers + client
./start.sh 5                           # 5 sim-servers instead
./stop.sh                              # kill everything
# Client: http://localhost:4321
# Dashboard: http://localhost:7871
```

### UNO Q Boards (embedded hardware mesh)
Full mesh on embedded hardware — no Mac processes required. Coordinator + client on a Pi Zero W2, sim-servers on 3 Arduino UNO Q boards. All connections use TLS (self-signed cert).

| Device | Role | Host | IP |
|--------|------|------|----|
| zzttpi | Coordinator + client | zzttpi.local | 192.168.0.208 |
| unoq   | Sim-server 1 | unoq.local | 192.168.0.15 |
| unoq2  | Sim-server 2 | unoq2.local | 192.168.0.173 |
| unoq3  | Sim-server 3 | unoq3.local | 192.168.0.86 |

```sh
./start-unoq.sh                       # cross-compile, deploy, start on boards
./stop-unoq.sh                        # stop all
# Client: https://192.168.0.208:7871
# Dashboard: https://192.168.0.208:7871/dashboard
# Board dashboards: http://<board-ip>:3000
```

Both deployments can run simultaneously — same ports, different hosts.

#### Cross-compilation
Two targets are needed:
- `aarch64-unknown-linux-gnu` — UNO Q boards (sim-server, board-dash)
- `armv7-unknown-linux-gnueabihf` — Pi Zero W2 (coordinator)

Built via `cross` (Docker-based cross-compilation). Binaries deployed via SCP.

#### TLS
Self-signed cert generated on first run, stored in `certs/` (gitignored). Covers all board IPs via SANs. Both coordinator (HTTPS) and sim-servers (WSS) use it.

To trust the cert (eliminates browser warnings):
```sh
sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain certs/mesh-coordinator.crt
```

To regenerate (e.g., new board IP): `rm certs/mesh-coordinator.*` then run `start-unoq.sh`.

#### Coordinator auto-start
The coordinator runs as a systemd user service on zzttpi (`mesh-coordinator.service`). It starts on boot and restarts on crash. The coordinator is stateless — sim-servers reconnect automatically when it restarts.

#### Connecting local client to board mesh
```sh
VITE_COORDINATOR_URL="https://192.168.0.208:7871" npx vite --port 4321
```
The Vite proxy in `vite.config.ts` forwards `/assign`, `/servers`, `/dashboard` to the coordinator URL (defaults to `http://localhost:7871` for local use).

### Coordinator HTTP API
The coordinator serves these endpoints on its HTTP port (7871):

| Endpoint | Description |
|----------|-------------|
| `/assign` | Returns `{"ws_url":"ws[s]://..."}` for the least-loaded server |
| `/servers` | Full server list with entity counts, ports, and hosts |
| `/dashboard` | Live HTML dashboard with gauges and phase bars |
| `/` | Client app (when `--static-dir` is set), otherwise redirects to dashboard |

#### Coordinator CLI flags
| Flag | Description |
|------|-------------|
| `--port` | WebSocket port for sim-server connections (default: 7870) |
| `--http-port` | HTTP(S) port for assignment API and dashboard (default: 7871) |
| `--static-dir` | Directory to serve client files from (e.g., built Vite output) |
| `--tls-cert` | Path to TLS certificate file (PEM) |
| `--tls-key` | Path to TLS private key file (PEM) |

#### Sim-server CLI flags (TLS)
| Flag | Description |
|------|-------------|
| `--tls-cert` | Path to TLS certificate file (PEM) — enables WSS |
| `--tls-key` | Path to TLS private key file (PEM) |

When TLS is enabled, the sim-server registers with `tls: true` and the coordinator returns `wss://` URLs in `/assign`.

## Testing Workflow

Two-stage verification for all changes:
1. **Local cluster first** — quick smoke test on the dev Mac (`./start.sh` + `./test.sh`)
2. **UNO Q cluster second** — real deployment to the boards (`./start-unoq.sh`)

```sh
# Stage 1: Local verification
./start.sh                             # 3 local sim-servers
./test.sh                              # run test suite
./stop.sh

# Stage 2: UNO Q verification
./start-unoq.sh                       # cross-compile, deploy, start on boards
./stop-unoq.sh
```

## Test Suite

Single-command test runner that starts the cluster, runs scenarios, and reports results.

```sh
./test.sh                              # all scenarios, local cluster
./test.sh --json                       # JSON output (for automated parsing)
./test.sh --json spawn-burst           # single scenario with JSON
./test.sh --docker                     # run in Docker with resource limits
./test.sh --no-start                   # use already-running cluster
./test.sh spawn-burst,steady-scale     # run specific scenarios only
```

### Metrics Endpoint
Each sim-server exposes `http://localhost:810N/metrics` with per-tick phase timing (p50/p95/p99):
```sh
curl localhost:8101/metrics | jq .      # server 1 metrics
```

### Scenarios
| Scenario | What it tests |
|----------|--------------|
| spawn-burst | 500 entities in 2s, peak budget + recovery |
| steady-scale | +10/sec until 80% budget, finds entity ceiling |
| crash-recovery | Kill server, verify entity adoption |
| rebalance-storm | 300 entities on one server, convergence |
| spatial-hotspot | 200 clustered entities, UDP packet stress |
| input-latency | MoveInput round-trip p50/p95/p99 |
| mesh-verify | Binary frames, entity fields, multi-server visibility, tick monotonicity |
| input-seq-echo | Prediction protocol: send seq 1-5, verify last_input_seq echoed |
| peer-replication | Shadow counts, peer connectivity, cross-server visibility |
| forwarded-input | MoveInput for shadow entity forwarded to authority and processed |
| multi-client | Two clients on different servers see consistent world |
| entity-lifecycle | Spawn → move → verify authority → remove |
| authority-transfer | Overload server, verify rebalance transfers entities |

### Scene Tests
Deep mesh verification tests — run against a live cluster:
```sh
./test-scene.sh                        # start cluster + run scene tests
./test-scene.sh --no-start             # use existing cluster
```

### Docker (Pi Zero W2 simulation)
Each container is limited to 1.0 CPU / 512MB RAM to simulate Raspberry Pi Zero W2 hardware.
```sh
docker compose -f docker-compose.test.yml up --build   # start constrained cluster
docker compose -f docker-compose.test.yml down          # stop
```

## Agents

### @mesh-dev
Specialized agent for developing server mesh netcode. Use for any work on sim-server game loop, entity replication, authority transfers, binary protocol, prediction, peer communication, or WebSocket server code. Has full context on the UNO Q boards, testing workflow, protocol formats, and all key constants. Automatically tests on the boards.

Use it for:
- "Add a health field to EntityState"
- "Implement peer health monitoring"
- "Fix the authority transfer to preserve velocity"
- "Add UDP chunking for large entity counts"

Defined in `.claude/agents/mesh-dev.md`. Auto-triggered by `.claude/rules/mesh-dev-agent.md`.

### @mesh-architect
Architects the mesh networking system. Designs features, breaks them into implementable work items, and maintains the technical roadmap. Produces design docs and ordered work item lists in `docs/mesh-architect/` that mesh-dev follows.

Use it for:
- "Design peer health monitoring"
- "What should we build next?"
- "Plan out interest management properly"
- "Break down player persistence into tasks"

Defined in `.claude/agents/mesh-architect.md`. Writes to `docs/mesh-architect/`.

### @mesh-tester
Tests and verifies the mesh. Writes unit tests, integration tests, and scene tests. Runs them against the UNO Q boards. Finds coverage gaps and fills them. Reads the codebase thoroughly before writing tests.

Use it for:
- "Add tests for the replication code"
- "Run the test suite on the boards"
- "Find what's not tested"
- "Write a scene test for crash recovery"

Defined in `.claude/agents/mesh-tester.md`.

### @mesh-auditor
Audits the mesh codebase for bottlenecks, single points of failure, race conditions, data loss risks, and scalability limits. Writes finding reports to `docs/mesh-auditor/` that mesh-dev can implement against. Each finding has severity, evidence, impact, and a recommended fix.

Use it for:
- "Audit the mesh for problems"
- "What breaks at 10,000 entities?"
- "Find race conditions in replication"
- "Are there data loss risks in authority transfers?"

Defined in `.claude/agents/mesh-auditor.md`. Writes to `docs/mesh-auditor/`.

### @mesh-cleanup
Implements codebase simplification work items from design 012. Removes dead code, splits monoliths, deduplicates, cleans abstractions. Pure refactoring with zero behavior change. Follows the work items in `docs/mesh-architect/work-items/012-codebase-simplification.md`.

Use it for:
- "Remove dead code (WI-120 to WI-125)"
- "Split systems.rs into modules"
- "Make the protocol types type-safe"
- "Continue the simplification work"
- "Run the next batch of cleanup"

Defined in `.claude/agents/mesh-cleanup.md`. Writes worklogs to `worklogs/mesh-cleanup/`.

### @cluster-ops
Manages the Pi/UNO Q hardware cluster. SSHs into boards, monitors CPU/memory/network/temperature, deploys binaries, checks logs, diagnoses issues, and verifies fixes on live hardware. Knows all board IPs, systemd services, and deployment layout.

Use it for:
- "Deploy to the boards"
- "Check if the boards are running"
- "What's the CPU usage on unoq?"
- "Why is S2 lagging?"
- "Monitor the cluster while I test"

Defined in `.claude/agents/cluster-ops.md`. Writes worklogs to `worklogs/cluster-ops/`.

### @codebase-oracle
Read-only agent that answers questions about how the codebase works. Treats code and tests as the ultimate source of truth — reads actual implementations before answering, cites `file:line`, and flags doc/code discrepancies. Use it for:
- "How does authority transfer work?"
- "What happens when a sim-server crashes?"
- "Where is entity replication implemented?"
- "What does the rebalance algorithm actually do?"

Defined in `.claude/agents/codebase-oracle.md`.

### MCP Oracle Server
Exposes the codebase-oracle as an MCP tool (`ask_server_meshing`) that other Claude Code instances can call to ask questions about this codebase.

**Build:**
```sh
cd mcp-oracle && npm install && npm run build
```

**Add to another project's `.mcp.json`:**
```json
{
  "mcpServers": {
    "server-meshing-oracle": {
      "command": "node",
      "args": ["/Users/dz/sources/ZZServerMeshing/mcp-oracle/dist/server.js"]
    }
  }
}
```

Then use `mcp__server-meshing-oracle__ask_server_meshing` from that project, or just ask about server meshing and Claude will route to it.

## Commands

### `/develop <feature>`
Full development pipeline. Clarifies requirements, architects the feature, presents the design for approval, then implements → tests → audits using the mesh agent team. Example:
```
/develop Add peer health monitoring so servers detect struggling peers
```

### `/ship`
Ship the current branch to main — commit, PR, merge, new branch.

## Conventions
- Rust: standard formatting (`cargo fmt`), no unsafe unless justified
- TypeScript: strict mode, ES2022 target
- Entity IDs are UUIDs, Server IDs are u16 starting at 1
- Server IDs are u16, starting at 1 (0 was historically reserved for the gateway, now removed)
- World bounds: -5000 to 5000 on X and Z axes
