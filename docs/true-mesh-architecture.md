# True Peer-to-Peer Server Meshing

## Problem

The current architecture funnels all client traffic through a single Node.js gateway. Every `WorldSnapshot` from every sim-server and every player input passes through this one process. This:

- Creates a scalability ceiling (gateway CPU/bandwidth is the bottleneck, not the mesh)
- Introduces a single point of failure
- Isn't true server meshing — it's one proxy running distributed NPCs

In actual server meshing (the Star Citizen model), players connect **directly** to a sim-server. The servers themselves are peers, each authoritative over its own entities, sharing state with each other. One zone is served by N peer-to-peer servers that balance everything and share the needed game state.

## Target Architecture

```
Browser A ←─WS─→ Sim-Server 1 ←─UDP─→ Sim-Server 2 ←─WS─→ Browser B
                       │                     │
                       └──UDP──→ Sim-Server 3 ←──UDP──┘
                                     │
                              (all register with)
                              Coordinator (7870)
```

- Each player connects directly to one sim-server via WebSocket
- Each sim-server composes the full world view for its clients (own authoritative entities + shadow copies from peers)
- Player inputs for locally-owned entities are processed directly; inputs for peer-owned entities are forwarded
- Authority transfers trigger client connection migration (or transparent proxying)

## What Already Exists

Most of the machinery is in place:

| Capability | Status | Where |
|---|---|---|
| UDP peer-to-peer replication | Done | `replication.rs` + `receiver.rs` |
| Shadow entity storage | Done | `world.rs` — `shadow_entities: HashMap` |
| Peer address discovery | Done | `coordinator_client.rs` — `world.peer_addrs` |
| Authority tracking | Done | `EntityState.authority_server` on every entity |
| Client WebSocket server | Done | `network.rs` — already broadcasts snapshots |
| Player spawn handling | Done | `simulation.rs` — `SpawnPlayer` message |
| Crash recovery / entity adoption | Done | `coordinator_client.rs` — `RecoverEntities` |

## What Needs to Change

### Phase 1: Sim-Server Serves Full World View

**Goal:** Each sim-server broadcasts authoritative + shadow entities to its connected clients.

Currently `world.rs:get_snapshot()` returns only authoritative entities. Change it (or add a new method) to merge authoritative entities with shadow entities into one `WorldSnapshot`. This is the client-facing snapshot — the peer-facing replication continues sending authoritative-only.

Changes:
- `world.rs` — add `get_client_snapshot()` that merges `self.entities` + `self.shadow_entities`
- `network.rs` — use `get_client_snapshot()` for client broadcasts
- `replication.rs` — no change (keeps sending authoritative-only to peers)

### Phase 2: Direct Client Connection

**Goal:** Clients connect to a sim-server directly instead of the gateway.

The client currently connects to `ws://localhost:4322` (gateway). Instead, it needs to:
1. Contact a lightweight **connection broker** to get assigned a sim-server address
2. Connect directly to that sim-server's WebSocket port (`7878+N`)

The connection broker can be the coordinator (it already knows all servers and could pick the least-loaded one) or a minimal HTTP endpoint. It just returns a WebSocket URL — it's not in the data path.

Changes:
- `coordinator` — add an HTTP endpoint (or WS message) for client assignment: returns `ws://host:port` of the least-loaded server
- `client/network.ts` — connect to coordinator first for assignment, then connect to the assigned sim-server
- `sim-server/network.rs` — handle `PlayerAssigned` flow directly (generate UUID, send back to client)
- `sim-server/simulation.rs` — `SpawnPlayer` already works, just needs to be self-initiated on client connect

### Phase 3: Input Routing Between Peers

**Goal:** If a client sends a `MoveInput` for an entity owned by a different server, forward it.

This happens when authority transfers but the client hasn't migrated yet, or for cross-server interactions.

Currently the gateway's `Router` does this lookup. Move it into the sim-server:
1. Receive `MoveInput { entity_id, direction }` from client
2. Check if `entity_id` is in `self.entities` (authoritative) — process locally
3. If not, check `self.shadow_entities` to find `authority_server` — forward to that peer

Forwarding can use the existing UDP channel (add a `PeerMessage::ForwardedInput` variant) or a new lightweight WebSocket connection between peers.

Changes:
- `shared/protocol.rs` — add `PeerMessage::ForwardedInput { entity_id, direction }` (or similar)
- `sim-server/simulation.rs` — on non-local `MoveInput`, forward to authority peer
- `sim-server/receiver.rs` — handle incoming `ForwardedInput`, process as local input

### Phase 4: Authority Transfer with Client Migration

**Goal:** When a player entity transfers authority to another server, migrate the client connection.

This is the hardest part. Options:

**Option A: Server-side proxy (simpler)**
- The original server keeps the client WebSocket open
- It forwards the client's inputs to the new authority server
- It receives the entity's state from the new authority via shadows
- Client never knows the difference

**Option B: Client reconnection (cleaner)**
- Server sends the client a `Reconnect { url: "ws://host:port" }` message
- Client disconnects and reconnects to the new server
- New server already has the entity (it just accepted authority)
- Brief interruption but architecturally clean

**Option C: Hybrid**
- Start proxying immediately (no interruption)
- Send `Reconnect` to client in parallel
- Client migrates at its own pace
- Proxy torn down after migration completes

Recommendation: **Start with Option A** (proxy) for simplicity, move to Option C later. Player authority transfers are relatively rare (rebalancing), so the proxy overhead is minimal.

Changes:
- `sim-server/network.rs` — track which client WebSocket owns which player entity
- `sim-server/simulation.rs` — on outgoing authority transfer of a player entity, set up proxy or send `Reconnect`
- `client/network.ts` — handle `Reconnect` message (Option B/C)
- `shared/protocol.rs` — add `ServerMessage::Reconnect { url }` variant

### Phase 5: Gateway Removal

**Goal:** Remove the gateway from the architecture entirely.

Once clients connect directly and sim-servers compose full snapshots:
- Delete `gateway/` directory
- Remove gateway from `start.sh` and `stop.sh`
- Update coordinator to not filter server_id 0 from peer lists (or remove the concept)
- Update `docker-compose.test.yml`
- Update test harness if it depends on gateway port 4322

The coordinator gains the small responsibility of client assignment (one HTTP request per client connect), but stays out of the data path.

## Migration Strategy

Phases 1-3 can be done while keeping the gateway functional (backward compatible). The gateway would still work — it's just that clients could also connect directly.

Suggested order:
1. **Phase 1** first — low risk, no client changes, validates that shadow-inclusive snapshots work
2. **Phase 3** next — input forwarding between peers, testable with the existing gateway
3. **Phase 2** then — direct client connection, can test alongside gateway
4. **Phase 4** — authority migration, only matters once clients connect directly
5. **Phase 5** last — cleanup, only after everything works without the gateway

## Risks and Considerations

**Snapshot size**: Each sim-server now sends the entire world (own + shadows) to each client, not just its slice. With 3 servers and 500 entities, every server sends 500 entities instead of ~167. Bandwidth per server increases 3x for client-facing traffic. Mitigation: delta compression (already stubbed in `binary-protocol.ts`), or only send shadows within a relevance radius.

**Shadow staleness**: Shadows are updated at UDP tick rate but may lag behind authoritative state. Clients connected to server A will see server B's entities with slight delay. This is inherent to distributed systems and acceptable at 20Hz — the current gateway has the same latency profile.

**Coordinator as SPOF**: The coordinator is still a single point. If it goes down, no new peers can register and no rebalancing happens. But existing connections and replication continue — it's control plane only, not data plane. This is acceptable for the prototype.

**Client complexity**: The client needs to handle `Reconnect` messages and potentially brief disconnection during authority transfer. This is new complexity but bounded.

## Entity Count Estimates

| Scenario | Current (gateway) | Target (direct) |
|---|---|---|
| Client bandwidth | Gateway sends 1 merged snapshot | Each sim-server sends full snapshot (same size) |
| Gateway bandwidth | O(servers * entities) in + O(clients * entities) out | Eliminated |
| Sim-server client bandwidth | O(entities/servers) per client | O(entities) per client |
| Sim-server peer bandwidth | O(entities/servers) per peer | Same (unchanged) |
| Input latency | Client → gateway → authority server | Client → server (direct or 1 forward hop) |
