# Project Status — Server Meshing Prototype

*Last updated: 2026-04-08*

## Overview

A working prototype of **true server meshing** — the approach Star Citizen is attempting for large-scale multiplayer. Multiple Rust game servers share authority over the same spatial area, distributing entities across servers at the entity level, not by zones.

**Stack**: Rust (sim-servers, coordinator, gateway) | Node.js/TypeScript (alternative gateway) | Three.js WebGPU (browser client)

**Codebase**: ~8,600 LOC across 47 source files | 7 commits | Single contributor

| Area | Language | LOC | Files |
|------|----------|-----|-------|
| Sim-server | Rust | 1,730 | 9 |
| Coordinator | Rust | 677 | 2 |
| Shared types | Rust | 613 | 5 |
| Rust gateway | Rust | ~200 | 2 |
| Node.js gateway | TypeScript | 365 | 3 |
| Client | TypeScript | 949 | 7 |
| Test suite & tools | TypeScript | 3,032 | 18 |
| Deployment | Shell/YAML | ~1,000 | ~15 |

---

## Phase Completion

All four planned phases are complete.

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
- Node.js gateway: aggregates snapshots from all sim-servers, routes client inputs
- Client entities color-coded by authority server

### Phase 3 — Dynamic Authority Transfer
- Player assignment to least-loaded server on connect
- WASD movement with input routing through gateway to authority server
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

---

## What's Deployed

### Local
Single command (`./start.sh`) launches the full cluster: coordinator, 3 sim-servers, gateway, and Vite dev server. Configurable server count via `./start.sh N`.

### Docker
`docker-compose.test.yml` runs the cluster with Pi Zero W2 resource constraints (0.25 CPU / 512MB per container) to simulate edge hardware.

### Kubernetes / Azure
Full deployment pipeline in `deploy/`:
- AKS cluster + ACR + Azure Static Web Apps
- K8s manifests: coordinator Deployment, sim-server StatefulSet (3 replicas), gateway Deployment
- Resource limits: 200m–500m CPU, 128–512Mi RAM per sim-server

---

## Test Infrastructure

7 automated scenarios run via `./test.sh`:

| Scenario | What it validates |
|----------|-------------------|
| spawn-burst | 500 entities in 2s, peak tick budget + recovery |
| steady-scale | +100/sec until 80% budget, finds entity ceiling |
| crash-recovery | Kill server, verify entity adoption by survivors |
| rebalance-storm | 300 entities on one server, convergence time |
| spatial-hotspot | 200 clustered entities, UDP stress |
| input-latency | MoveInput round-trip p50/p95/p99 |
| smoothness | Entity interpolation quality at 60fps |

138 unit tests covering shared types, protocol serialization, spatial grid, and simulation logic.

---

## Architecture at a Glance

```
Browser ←─WS─→ Gateway ←─WS─→ Sim-Servers ←─UDP─→ each other
                  │                  │
                  └──WS──→ Coordinator ←──WS──┘
```

**Coordinator** (port 7870) — Server registry, peer lists, heartbeats, rebalance/drain directives, crash detection.

**Sim-Server** (ports 7878+N ws, 9100+N udp, 8100+N metrics) — 20Hz game loop with 5 phases: input drain, simulation, authority transfers, peer replication, shadow cleanup. Each instance owns a subset of entities.

**Gateway** (port 4322) — Aggregates WorldSnapshot from all sim-servers into unified client view. Routes player inputs to correct authority server. Assigns players on connect.

**Client** (port 4321) — Three.js WebGPU renderer. Entities are colored spheres (NPCs) and boxes (players). Debug overlay shows per-server load, network stats, and entity counts.

---

## Key Design Decisions

| Decision | Rationale |
|----------|-----------|
| HashMap for entities, not ECS | Trivially serializable; full ECS overkill for network prototype |
| UDP + bincode for replication | Minimal latency; no packet loss risk locally |
| JSON for coordinator protocol | Human-readable; coordinator isn't in the hot path |
| 20 Hz tick rate | Industry standard; client interpolates to 60fps |
| Separate gateway process | Keeps sim-servers focused; Node.js excels at WebSocket fanout |
| Entity-level distribution | Core innovation — no zones, entities balanced by load |

---

## Not Implemented (Descoped)

These were considered but descoped as unnecessary for the prototype:

- Cross-server interaction RPC (collision/push between entities on different servers)
- 3-step authority transfer handshake (simplified to direct send+accept)
- Spatial grid cell visualization in client
- Network time synchronization across servers
- Delta compression for entity state updates (disabled pending per-entity interpolation)
- QUIC transport (TLS/stream overhead not needed locally)

---

## Known Risks & Limitations

| Risk | Current mitigation |
|------|--------------------|
| Authority transfer atomicity | Direct send+accept; rebalancer self-corrects within 10s |
| UDP packet ordering | Tick-tagged; receiver discards stale updates |
| Gateway aggregation latency | Forwards every tick (50ms); no batching |
| Crash recovery duplication | Multiple survivors may adopt same shadows; rebalancer evens out |
| No clock sync | Servers assume similar system clocks; fine for local/single-node |

---

## Metrics & Observability

Each sim-server exposes `http://localhost:810N/metrics` with per-tick phase timing:
- **Phases tracked**: input drain, simulation, transfers, replication, shadow cleanup
- **Percentiles**: p50, p95, p99 for each phase
- **Tick budget**: max phase total / tick duration (50ms)

Gateway broadcasts `_metrics` to clients: snapshots/sec, bandwidth (KB/s), rebalance events, per-server entity counts.
