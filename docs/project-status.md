# Project Status — Server Meshing Prototype

*Last updated: 2026-04-13*

## Overview

A working prototype of **true server meshing** — the approach Star Citizen is attempting for large-scale multiplayer. Multiple Rust game servers share authority over the same spatial area, distributing entities across servers at the entity level, not by zones.

**Stack**: Rust (sim-servers, coordinator, board-dash) | Three.js WebGPU (browser client)

**Deployment**: 3 Arduino UNO Q boards (sim-servers) + Pi Zero W2 (coordinator) — entire mesh runs on embedded hardware. Also runs locally on Mac for development.

---

## Phase Completion

All planned phases are complete, plus true peer-to-peer mesh and hardware deployment.

### Phase 1 — Single Server + Renderer
- Rust workspace scaffold (shared, sim-server, coordinator crates)
- Shared types: `EntityId`, `EntityState`, `ServerId`, protocol messages, spatial grid, tick constants
- Sim-server game loop: 20 Hz fixed timestep, NPC wandering, WebSocket broadcast
- Three.js WebGPU client: renderer, network client, world-view, debug overlay
- `start.sh` / `stop.sh` cluster scripts

### Phase 2 — Multi-Server Mesh
- Coordinator: server registration, peer discovery, heartbeat monitoring, peer list broadcast
- UDP + bincode peer-to-peer replication (server-to-server each tick)
- Shadow entities: inbound UDP merged into read-only copies for spatial awareness
- Client entities color-coded by authority server

### Phase 3 — Dynamic Authority Transfer
- Player assignment to least-loaded server on connect
- WASD movement with input routing directly to authority server
- Camera follow mode (F key)
- Coordinator rebalancing: triggers on >30% imbalance, transfers peripheral NPCs
- Authority transfer via UDP with full entity state
- Player cleanup on disconnect

### Phase 4 — Polish & Stress Testing
- Tick-based conflict resolution (discard stale UDP packets)
- Entity interpolation (buffer-based, 60fps from 20Hz ticks)
- Client performance: shared material cache, mesh pooling
- Network metrics in debug overlay (snapshots/sec, bandwidth, rebalance events)
- Crash detection (5s heartbeat timeout) and recovery (promote shadow entities)
- Graceful drain protocol (transfer all entities before shutdown)
- Stress test tool: verified 200 entities at 20 snapshots/sec

### Phase 5 — True Peer-to-Peer Mesh
- Removed gateway — clients connect directly to sim-servers
- Each sim-server broadcasts full world view (auth + shadow entities) to its clients
- Coordinator HTTP endpoint for client assignment (`/assign`)
- Input forwarding between peers (MoveInput for non-local entities forwarded via UDP)
- Binary WebSocket protocol (47 bytes per entity, 27-byte header)
- Client-side prediction with server reconciliation

### Phase 6 — Bevy ECS Migration
- Migrated sim-server entity storage from `HashMap<EntityId, EntityState>` to Bevy ECS
- Components: Position, Velocity, EntityTypeComp, AuthorityServer, NetworkId, LastInputSeq
- ECS systems for simulation, NPC AI, input processing
- Conversion layer between ECS components and network EntityState

### Phase 7 — Hardware Deployment & TLS
- Deployed on 3 Arduino UNO Q boards + Pi Zero W2
- Cross-compilation for aarch64 (UNO Q) and armv7 (Pi Zero W2)
- TLS support on coordinator (HTTPS) and sim-servers (WSS) via rustls
- Self-signed certificate with SANs covering all board IPs
- Coordinator serves built client app via `--static-dir`
- Coordinator runs as systemd service on Pi (auto-start on boot)
- Board dashboards on port 3000 with sim-server control
- Both local (Mac) and board meshes can run simultaneously

---

## What's Deployed

### Local
Single command (`./start.sh`) launches the full cluster: coordinator, N sim-servers, and Vite dev client. Configurable server count via `./start.sh N`. No TLS (localhost is a secure context for WebGPU).

### UNO Q Boards
`./start-unoq.sh` cross-compiles, deploys, and starts the full mesh on embedded hardware:
- **zzttpi** (Pi Zero W2) — coordinator + client serving over HTTPS
- **unoq, unoq2, unoq3** (UNO Q boards) — 3 sim-servers with WSS + UDP replication

### Docker
`docker-compose.test.yml` runs the cluster with Pi Zero W2 resource constraints (1.0 CPU / 512MB per container) to simulate edge hardware.

---

## Test Infrastructure

13 automated scenarios run via `./test.sh`:

| Scenario | What it validates |
|----------|-------------------|
| spawn-burst | 500 entities in 2s, peak tick budget + recovery |
| steady-scale | +10/sec until 80% budget, finds entity ceiling |
| crash-recovery | Kill server, verify entity adoption by survivors |
| rebalance-storm | 300 entities on one server, convergence |
| spatial-hotspot | 200 clustered entities, UDP stress |
| input-latency | MoveInput round-trip p50/p95/p99 |
| mesh-verify | Binary frames, entity fields, multi-server visibility |
| input-seq-echo | Prediction protocol: send seq 1-5, verify echoed |
| peer-replication | Shadow counts, peer connectivity, cross-server visibility |
| forwarded-input | MoveInput for shadow entity forwarded to authority |
| multi-client | Two clients on different servers see consistent world |
| entity-lifecycle | Spawn → move → verify authority → remove |
| authority-transfer | Overload server, verify rebalance transfers entities |

Scene tests (`./test-scene.sh`) for deep mesh verification against live clusters.

---

## Architecture at a Glance

```
Browser ←─WSS─→ Sim-Server 1 ←─UDP─→ Sim-Server 2 ←─WSS─→ Browser
                      │                     │
                      └──UDP──→ Sim-Server 3 ←──UDP──┘
                                     │
                              Coordinator
                           (assignment + control)
```

**Coordinator** (port 7870 ws, 7871 https) — Server registry, peer lists, heartbeats, rebalance/drain directives, crash detection. Client assignment via `/assign`. Serves client app via `--static-dir`. Stateless — sim-servers auto-reconnect if it restarts.

**Sim-Server** (ports 7878+N wss, 9100+N udp, 8100+N metrics) — 20Hz Bevy ECS game loop with 5 phases: input drain, simulation, authority transfers, peer replication, shadow cleanup. Each instance owns a subset of entities and broadcasts the full world to its connected clients.

**Client** (port 4321 dev, or served from coordinator) — Three.js WebGPU renderer. Entities are colored spheres (NPCs) and boxes (players). Debug overlay shows per-server load, network stats, and entity counts.

---

## Key Design Decisions

| Decision | Rationale |
|----------|-----------|
| Bevy ECS for entities | Scalable entity management; component queries outperform HashMap iteration at high entity counts |
| Direct client connections | True peer-to-peer mesh; no gateway bottleneck or SPOF in the data path |
| UDP + bincode for replication | Minimal latency; no packet loss risk on LAN |
| JSON for coordinator protocol | Human-readable; coordinator isn't in the hot path |
| 20 Hz tick rate | Industry standard; client interpolates to 60fps |
| Binary WebSocket frames | 47 bytes/entity vs ~200 bytes JSON; critical for bandwidth at scale |
| Entity-level distribution | Core innovation — no zones, entities balanced by load |
| TLS via rustls | WebGPU requires secure context; no OpenSSL dependency for cross-compilation |

---

## Not Implemented (Descoped)

These were considered but descoped as unnecessary for the prototype:

- Cross-server interaction RPC (collision/push between entities on different servers)
- 3-step authority transfer handshake (simplified to direct send+accept)
- Network time synchronization across servers
- Delta compression for entity state updates
- QUIC transport
- Interest management (distance-based filtering)

---

## Known Risks & Limitations

| Risk | Current mitigation |
|------|--------------------|
| Authority transfer atomicity | Direct send+accept; rebalancer self-corrects within 10s |
| UDP packet ordering | Tick-tagged; receiver discards stale updates |
| Full world broadcast | Every client gets every entity; scales to ~500 entities before bandwidth limits |
| Crash recovery duplication | Multiple survivors may adopt same shadows; rebalancer evens out |
| No clock sync | Servers assume similar system clocks; fine for LAN |
| Coordinator as SPOF | Control plane only; data plane continues without it |

---

## Metrics & Observability

Each sim-server exposes `http://host:810N/metrics` with per-tick phase timing:
- **Phases tracked**: input drain, simulation, transfers, replication, shadow cleanup
- **Percentiles**: p50, p95, p99 for each phase
- **Tick budget**: total phase time / tick duration (50ms)

Coordinator dashboard at `/dashboard` shows all servers with live gauges, entity counts, and phase breakdowns.

Board dashboards at `http://board-ip:3000` show system health (CPU, memory, temperature) and sim-server control.
