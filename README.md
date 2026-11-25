# Server Meshing Prototype

The reusable Rust library and the standalone Infinite Stars game target Bevy 0.15. Run `cargo run -p infinite-stars` for a persistent two-worker handoff or `cargo run -p infinite-stars --example survey_probe` to try adding a new game type. Development, restart and benchmark commands are in the [Infinite Stars guide](docs/infinite-stars.md).

## Browser demo


A working implementation of true server meshing -- multiple game servers sharing authority over the same spatial area, distributing entities across servers at the entity level rather than by zones. This is the architecture Star Citizen is pursuing for large-scale multiplayer.

Launch 3 servers, open a browser, spawn entities, and watch them move and rebalance across servers in real time. The mesh also runs entirely on embedded hardware (3 Arduino UNO Q boards + a Raspberry Pi Zero W2).

## Architecture

```
Browser Client ←─ WSS ─→ Sim-Server 1 ←─ UDP ─→ Sim-Server 2
                              │                       │
                              └──── UDP ────→ Sim-Server 3
                                                  │
                                           Coordinator
                                      (assignment + control)
```

Four services make up the mesh:

**Coordinator** (Rust) -- Servers register here on startup. Broadcasts peer lists, monitors heartbeats, detects crashes, issues rebalance directives, and assigns clients to the least-loaded server. Serves a live HTML dashboard with per-server gauges.

**Sim-Server** (Rust, Bevy ECS) runs a 20 Hz simulation for the entities it owns. Each instance replicates their state to peers over UDP/bincode, holds shadow copies of peer entities and sends each connected client a view of nearby entities over WebSocket.

**Client** (TypeScript, Three.js WebGPU) -- Browser app rendering entities as colored spheres (NPCs) and boxes (players). Color indicates which server owns the entity. WASD movement, click to spawn, debug overlay with per-server load bars and network metrics.

**Shared** (Rust) -- Types shared across Rust crates: entity definitions, all protocol message enums, spatial grid, tick constants.

## Key Features

### Entity-Level Authority
Every entity has exactly one authoritative server at any time. The owning server simulates it, replicates it to peers, and broadcasts it to clients. Other servers hold read-only shadow copies for spatial awareness and crash recovery. Authority transfers seamlessly between servers -- entities change color in the client as they move between servers.

### Automatic Load Balancing
The coordinator monitors per-server load (weighted: players 3x, NPCs 1x) and issues rebalance directives to redistribute entities from overloaded servers to underloaded ones. Two-phase authority transfer with ACKs prevents entity duplication or loss during handoff.

### Crash Recovery
When a server disconnects or exceeds the 15-second heartbeat timeout, the coordinator issues `RecoverEntities` to survivors. They promote eligible shadow copies to authoritative entities with the last replicated position and velocity.

### Full-Mesh Peer Replication
Every sim-server replicates its authoritative entities to every peer via UDP/bincode every tick. Area-of-Interest (AoI) filtering and tiered shadow replication reduce bandwidth -- entities outside a peer's relevant area are sent at lower frequency or skipped entirely.

### Client-Side Prediction
Players see instant movement feedback. The client applies inputs locally, then reconciles with the server's authoritative state using sequence-number-based replay and exponential correction blending.

### Delta Compression
Binary WebSocket frames use delta encoding -- only changed entities are sent after the initial full snapshot. The server tracks per-client baselines and falls back to full snapshots when the change ratio makes deltas inefficient.

### Binary Protocol
Full snapshots (`0x01`) have a 27-byte header and 47 bytes per entity. Deltas (`0x02`) have a 31-byte header followed by updated entities and removed IDs. The client acknowledges the tick with a 9-byte binary frame (`0x03`). Gameplay input and coordinator control messages use JSON over WebSocket. Peers exchange bincode messages over UDP.

### Network Saturation Telemetry
Per-server metrics endpoint exposes tick phase timing (input, simulation, transfers, replication, shadow cleanup) at p50/p95/p99. A live coordinator dashboard visualizes server load, entity distribution, and mesh health.

### TLS Support
Both the coordinator (HTTPS) and sim-servers (WSS) support TLS via self-signed certificates, enabling secure connections for the embedded hardware deployment where WebGPU requires a secure context.

## Deployment Modes

### Local on Linux and Windows

Install Rust stable with a native C toolchain and Node.js 22.16+ with npm. Windows uses MSVC, the Visual Studio C++ Build Tools and the Windows SDK. Run these commands from the repository root.

```sh
npm run setup
npm start
```

Open the [browser client](http://127.0.0.1:4321) or the [coordinator dashboard](http://127.0.0.1:7871/dashboard). Stop the cluster with Ctrl+C or `npm stop` from a second terminal. Use `npm start -- --servers 5 --npcs 100` for a different topology. PowerShell users can invoke `npm.cmd` if script execution policy blocks `npm.ps1`. See the [quick start](docs/quickstart.md) and [Codex project instructions](AGENTS.md).

### Embedded Hardware (UNO Q Boards)

Full mesh on embedded hardware -- coordinator + client on a Pi Zero W2, sim-servers on 3 Arduino UNO Q boards. All connections use TLS. Cross-compiled via `cargo zigbuild` targeting `aarch64-unknown-linux-gnu` (boards) and `armv7-unknown-linux-gnueabihf` (Pi).

```sh
./start-unoq.sh     # cross-compile, deploy via SCP, start on boards
./stop-unoq.sh      # stop all
# Client: https://192.168.0.208:7871
# Dashboard: https://192.168.0.208:7871/dashboard
```

| Device | Role | IP |
|--------|------|----|
| zzttpi | Coordinator + client | 192.168.0.208 |
| unoq | Sim-server 1 | 192.168.0.15 |
| unoq2 | Sim-server 2 | 192.168.0.173 |
| unoq3 | Sim-server 3 | 192.168.0.86 |

Both deployments can run simultaneously.

## Testing

The local smoke test starts a fresh mesh, checks the browser assignment proxy and runs the input sequence and peer replication scenarios. The benchmark runs release binaries with one WebSocket client per sim-server and reports per-server timing and snapshot delivery.

```sh
npm test
npm run smoke
npm run bench -- --servers 3 --npcs 3000 --seconds 20
```

The [player-cluster benchmarks](docs/cluster-performance.md) cover isolated groups, increasing player density, players switching groups, progressive merges and splits, and a side-by-side run of the original browser demo.

See the [benchmark results and methodology](docs/benchmark.md). The extended suite includes spawn bursts, crash recovery, authority transfers, input latency, spatial hotspots and multi-client consistency. The older Bash, Docker and board entry points target their respective environments.

```sh
./test.sh                              # all scenarios
./test.sh spawn-burst,crash-recovery   # specific scenarios
./test-scene.sh                        # deep mesh verification
```

## Tech Stack

| Layer | Technology |
|-------|-----------|
| Sim-server | Rust, Bevy ECS, tokio, bincode |
| Coordinator | Rust, Tokio, tokio-tungstenite |
| Client | TypeScript, Three.js WebGPU, Vite |
| Server-to-server | UDP + bincode |
| Server-to-client | Binary WebSocket frames |
| Control plane | JSON over WebSocket |
| Cross-compilation | cargo zigbuild |

## Project Structure

```
crates/
  shared/         Shared types: entities, protocol messages, spatial grid, tick constants
  sim-server/     Rust game simulation server (Bevy ECS, 20 Hz tick loop)
  coordinator/    Mesh coordinator: server registry, peer discovery, health, rebalancing
  board-dash/     Per-board system health dashboard (UNO Q only)
client/           Three.js WebGPU browser client
docs/             Architecture docs, protocol reference, deployment guides
```

## Ports

| Service | Port |
|---------|------|
| Coordinator (WebSocket) | 7870 |
| Coordinator (HTTP) | 7871 |
| Sim-server N (WebSocket) | 7877+N (S1 = 7878) |
| Sim-server N (UDP) | 9100+N |
| Sim-server N (Metrics) | 8100+N |
| Client (Vite dev) | 4321 |
