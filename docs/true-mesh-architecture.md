# True Peer-to-Peer Server Meshing

> **Status: COMPLETE** — All phases below have been implemented. This document is preserved as historical design notes showing the migration path from the gateway architecture to true peer-to-peer mesh.

## Problem (Solved)

The original architecture funneled all client traffic through a single Node.js gateway. Every `WorldSnapshot` from every sim-server and every player input passed through this one process. This created a scalability ceiling, a single point of failure, and wasn't true server meshing.

## Implemented Architecture

```
Browser A ←─WSS─→ Sim-Server 1 ←─UDP─→ Sim-Server 2 ←─WSS─→ Browser B
                       │                     │
                       └──UDP──→ Sim-Server 3 ←──UDP──┘
                                     │
                              (all register with)
                         Coordinator (ws:7870, https:7871)
```

- Each player connects directly to one sim-server via WebSocket (WS or WSS)
- Each sim-server composes the full world view for its clients (own authoritative entities + shadow copies from peers)
- Player inputs for locally-owned entities are processed directly; inputs for peer-owned entities are forwarded via UDP
- The coordinator provides client assignment (`/assign`), peer discovery, rebalancing, and crash recovery
- The gateway has been completely removed from the codebase

## What Was Implemented

### Phase 1: Full World View ✅
Each sim-server broadcasts authoritative + shadow entities to its connected clients. Binary WebSocket frames at 20Hz, 47 bytes per entity.

### Phase 2: Direct Client Connection ✅
Clients fetch `/assign` from the coordinator to get a sim-server address, then connect directly. The coordinator HTTP endpoint returns the least-loaded server.

### Phase 3: Input Routing Between Peers ✅
`ForwardedInput` UDP message added. When a client sends `MoveInput` for an entity owned by another server, the connected server forwards it via UDP to the authority.

### Phase 4: Authority Transfer with Client Migration ✅
`Reconnect { url }` message implemented. When a player entity transfers authority, the client receives a reconnect instruction and migrates to the new server.

### Phase 5: Gateway Removal ✅
Gateway directory deleted. All references removed from start/stop scripts, tests, and documentation. Server ID 0 is no longer reserved.

## Current Deployment

The mesh runs entirely on embedded hardware:
- **Pi Zero W2** (zzttpi) — coordinator + client serving over HTTPS
- **3x Arduino UNO Q** (unoq, unoq2, unoq3) — sim-servers with WSS

See [UNO Q Deployment](uno-q-deployment.md) for details.

## Bandwidth Considerations

Peer replication is efficient — each server only sends its own authoritative entities to peers via UDP (e.g., ~167 entities each in a 3-server, 500-entity mesh). However, each sim-server sends the *full world* (own auth + shadows from all peers) to its connected *browser clients* so they can render everything. With 500 entities at 47 bytes each, that's ~23KB per tick per client at 20Hz (~460 KB/s). This is acceptable for the prototype but client-facing bandwidth would need interest management (distance-based filtering) to scale beyond ~1000 entities.
