# Server Meshing Prototype

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

**Sim-Server** (Rust, Bevy ECS) -- The game server. Run N instances. Each runs a 20 Hz game loop simulating its authoritative entities. Replicates entity state to peers via UDP/bincode. Receives shadow copies of peer entities. Broadcasts the full world view to connected clients via binary WebSocket frames.

**Client** (TypeScript, Three.js WebGPU) -- Browser app rendering entities as colored spheres (NPCs) and boxes (players). Color indicates which server owns the entity. WASD movement, click to spawn, debug overlay with per-server load bars and network metrics.

**Shared** (Rust) -- Types shared across Rust crates: entity definitions, all protocol message enums, spatial grid, tick constants.

## Key Features

### Entity-Level Authority
Every entity has exactly one authoritative server at any time. The owning server simulates it, replicates it to peers, and broadcasts it to clients. Other servers hold read-only shadow copies for spatial awareness and crash recovery. Authority transfers seamlessly between servers -- entities change color in the client as they move between servers.

### Automatic Load Balancing
The coordinator monitors per-server load (weighted: players 3x, NPCs 1x) and issues rebalance directives to redistribute entities from overloaded servers to underloaded ones. Two-phase authority transfer with ACKs prevents entity duplication or loss during handoff.

### Crash Recovery
When a server disappears (5-second heartbeat timeout), the coordinator issues `RecoverEntities` to survivors. Shadow copies are promoted to authoritative, preserving position and velocity. No entity is lost.

### Full-Mesh Peer Replication
Every sim-server replicates its authoritative entities to every peer via UDP/bincode every tick. Area-of-Interest (AoI) filtering and tiered shadow replication reduce bandwidth -- entities outside a peer's relevant area are sent at lower frequency or skipped entirely.

### Client-Side Prediction
Players see instant movement feedback. The client applies inputs locally, then reconciles with the server's authoritative state using sequence-number-based replay and exponential correction blending.

### Delta Compression
Binary WebSocket frames use delta encoding -- only changed entities are sent after the initial full snapshot. The server tracks per-client baselines and falls back to full snapshots when the change ratio makes deltas inefficient.

### Binary Protocol
Server-to-client communication uses a compact binary format: 27-byte header + 47 bytes per entity. Two frame types: `0x01` full snapshot, `0x02` delta. Client-to-server and coordinator messages use JSON over WebSocket. Server-to-server uses bincode over UDP.

### Network Saturation Telemetry
Per-server metrics endpoint exposes tick phase timing (input, simulation, transfers, replication, shadow cleanup) at p50/p95/p99. A live coordinator dashboard visualizes server load, entity distribution, and mesh health.

### TLS Support
Both the coordinator (HTTPS) and sim-servers (WSS) support TLS via self-signed certificates, enabling secure connections for the embedded hardware deployment where WebGPU requires a secure context.

## Deployment Modes

### Local (Mac)

Everything runs on a single machine. No TLS needed (localhost is a secure context for WebGPU).

```sh
./start.sh          # coordinator + 3 sim-servers + client dev server
./start.sh 5        # 5 sim-servers instead
./stop.sh           # kill everything
# Client: http://localhost:4321
# Dashboard: http://localhost:7871
```

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

Two-stage verification: local cluster first, then embedded hardware.

```sh
# Local
./start.sh && ./test.sh && ./stop.sh

# Embedded
./start-unoq.sh
```

The test suite covers 13 scenarios including spawn bursts, crash recovery, authority transfers, input latency, spatial hotspots, and multi-client consistency. Scene tests provide deep mesh verification against a live cluster.

```sh
./test.sh                              # all scenarios
./test.sh spawn-burst,crash-recovery   # specific scenarios
./test.sh --docker                     # Docker with Pi-like resource limits (1 CPU / 512MB per server)
./test-scene.sh                        # deep mesh verification
```

## Tech Stack

| Layer | Technology |
|-------|-----------|
| Sim-server | Rust, Bevy ECS, tokio, bincode |
| Coordinator | Rust, tokio, warp |
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
| Sim-server N (WebSocket) | 7878+N |
| Sim-server N (UDP) | 9100+N |
| Sim-server N (Metrics) | 8100+N |
| Client (Vite dev) | 4321 |
