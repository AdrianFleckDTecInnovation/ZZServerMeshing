# Server Mesh Improvements Roadmap

## Current State

Working true peer-to-peer mesh: clients connect directly to sim-servers, each server broadcasts full world view (authoritative + shadow entities), inputs for non-local entities are forwarded via UDP. Coordinator handles peer discovery, rebalancing, and client assignment. Binary encoding for server→client snapshots. Deployed on two Arduino UNO Q boards with coordinator on Mac.

## What's Missing for Smooth Gameplay

### No Client-Side Prediction
When the player presses W, the input round-trips to the server before the player moves. That's 50-150ms of latency. For forwarded inputs (player's authority on another server), it's 3+ ticks (~150ms+). Every competitive game masks this with prediction.

### Full World Broadcast
Every server sends every entity to every peer every tick. Every client gets every entity regardless of distance. With 500 entities at 43 bytes each, that's ~21KB per tick per client at 20Hz. Doesn't scale past a few hundred entities.

### No Graceful Degradation
If a server lags, its shadows go stale and get deleted after 5 seconds. Entities vanish from other servers' views. No extrapolation, no "old data is better than no data" concept.

### No Peer Health Awareness
Servers replicate at the same rate regardless of peer health. A struggling peer gets the same firehose of updates, making its situation worse.

---

## Phase A: Client-Side Prediction + Server Reconciliation

**Impact:** Input feels instant. Single biggest improvement for gameplay feel.

### How It Works

```
Player presses W:
  1. Client immediately moves player locally (prediction)
  2. Client sends input with sequence number to server
  3. Server processes input, stamps response with last_processed_input_seq
  4. Client receives authoritative state
  5. If prediction matches server → no correction needed
  6. If mismatch → snap to server state, replay unacknowledged inputs on top
```

### Changes Required

**Protocol:**
- `ClientMessage::MoveInput` gains `seq: u32` (input sequence number)
- `EntityState` gains `last_input_seq: u32` (only meaningful for player entities, default 0 for NPCs)

**Server (`simulation.rs`):**
- Track per-player `last_input_seq` in entity state
- On MoveInput: update `entity.last_input_seq = msg.seq` alongside velocity

**Client (`network.ts`, new `prediction.ts`):**
- Maintain ring buffer of unacknowledged inputs: `{ seq, direction, timestamp }`
- On sending MoveInput: apply locally to predicted position, push to buffer
- On receiving snapshot: find player entity's `last_input_seq`, discard acknowledged inputs, replay remaining inputs on top of server position
- Smooth corrections with exponential blend (don't snap instantly)

### Forwarded Input Consideration
When the player's authority is on a different server, prediction still works — the client predicts locally regardless. The correction may be slightly larger due to the extra hop, but the smoothing handles it.

---

## Phase B: Interest Management (Area of Interest)

**Impact:** 10x scalability. Bandwidth drops dramatically.

### Distance-Based Update Tiers

| Distance from player | Update rate | Bandwidth share |
|---------------------|-------------|-----------------|
| Own entity | Every tick (20Hz) | Always |
| < 100 units | Every tick (20Hz) | Full fidelity |
| 100-300 units | Every 4th tick (5Hz) | 25% |
| 300-500 units | Every 10th tick (2Hz) | 10% |
| > 500 units | Don't send | 0% |

### Changes Required

**Server (`network.rs`):**
- Track each client's player entity position
- Per-tick, build per-client entity list filtered by distance tiers
- Use the existing spatial grid (`shared/spatial.rs`) for efficient range queries

**Server (`replication.rs`):**
- Optionally filter peer replication by proximity to peer's entity cluster centroid
- Full replication still needed for crash recovery — but can be done at reduced rate for distant entities

**Client (`world-view.ts`):**
- Interpolation already handles variable update rates (measures actual interval)
- May need to handle entities appearing/disappearing as they enter/exit AOI without flicker

---

## Phase C: Delta Compression

**Impact:** 5-10x bandwidth reduction on top of interest management.

### How It Works

Instead of sending all entities every tick, only send what changed:
- **New entities**: full state (43 bytes)
- **Changed entities**: position + velocity only (24 bytes) or just position (12 bytes) if velocity unchanged
- **Removed entities**: just entity ID (16 bytes)
- **Unchanged entities**: nothing

With 500 entities where ~50 change per tick, payload drops from 21KB to ~2KB.

### Changes Required

**Server:**
- Track per-client "last acknowledged state" (HashMap of entity positions/velocities at the last ACK'd tick)
- Compute diff each tick: new, changed, removed, unchanged
- Send binary delta frame (type `0x02`, already defined in client protocol)

**Client:**
- Send tick ACK back to server (lightweight: just a u64 tick number)
- Apply deltas on top of maintained entity state
- Already has delta decode support in `binary-protocol.ts`

**Consideration:** Delta compression requires reliable delivery for the baseline. If the ACK is lost, the server might compute deltas against a stale baseline. Solutions: periodic full snapshots (every 5 seconds), or client requests a full snapshot when it detects inconsistency.

---

## Phase D: Peer Health Monitoring + Adaptive Replication

**Impact:** Handles lag spikes and performance drops gracefully.

### Per-Peer Health Tracking

Each server maintains for every peer:

```
PeerHealth {
    last_received_tick: u64        // latest tick from this peer
    last_receive_time: Instant     // wall clock of last packet
    estimated_rtt_ms: f32          // smoothed RTT (from ping/pong)
    packet_loss_pct: f32           // estimated over last 10s
    shadow_staleness_ticks: u64    // how far behind their shadows are
    status: Healthy | Degraded | Unresponsive
}
```

### Adaptive Behavior

**When a peer is struggling (high staleness, packet loss):**
- Reduce our replication rate TO that peer (every 2nd or 4th tick)
- Increase stale shadow tolerance FROM that peer (don't delete shadows aggressively)
- Notify coordinator: "S2 appears degraded"
- Coordinator can preemptively rebalance entities off the struggling server

**When WE are struggling (tick budget > 80%):**
- Reduce NPC behavior complexity (less frequent direction changes)
- Reduce shadow cleanup frequency (run every 5th tick instead of every tick)
- Reduce replication frequency (every 2nd tick)
- Report to coordinator → coordinator rebalances entities away from us

**Ping/Pong Protocol:**
- New `PeerMessage::Ping { send_tick, send_time_us }` / `PeerMessage::Pong { ... }`
- Sent every second per peer
- Used to estimate RTT and detect packet loss

### Changes Required

**Protocol:** Add `Ping`/`Pong` variants to `PeerMessage`
**Server:** `peer_health.rs` module tracking per-peer stats, consulted by replication and shadow cleanup
**Coordinator:** Accept health reports from servers, factor into rebalancing decisions

---

## Phase E: Shadow Extrapolation

**Impact:** Smooth visuals during lag spikes. Entities don't vanish.

### Multi-Stage Staleness

Instead of binary fresh/stale, shadow entities degrade gracefully:

| Shadow age | Behavior | Visual |
|------------|----------|--------|
| < 100ms | Normal | Full opacity |
| 100-500ms | Extrapolate from velocity | Full opacity |
| 500ms-2s | Extrapolate, mark uncertain | Slight transparency |
| 2-5s | Freeze position | Ghost effect |
| > 5s | Remove | Fade out |

### Changes Required

**Server (`world.rs`):**
- Replace binary stale check with multi-stage staleness
- Extrapolate shadow positions using `position + velocity * dt` when source is late
- Include staleness indicator in client snapshot

**Client:**
- Render extrapolated entities with visual indicator (transparency, particle effect)
- Player sees entities continuing to move during lag spikes instead of freezing

---

## Phase F: Smarter Rebalancing

**Impact:** Better entity distribution, fewer unnecessary transfers.

### Improvements Over Current System

**Cost-weighted balancing:**
Not all entities cost the same. Player with AI companions > NPC with complex behavior > stationary NPC. Weight by actual CPU cost per tick, not just count.

**Spatial clustering:**
Transfer entities spatially close to the target server's entity centroid. Minimizes cross-server interactions and forwarded inputs.

**Hysteresis:**
Don't transfer an entity back within 30 seconds of arrival. Prevents ping-pong between servers.

**Predictive:**
If a player moves toward a cluster on another server at 5 units/sec and is 50 units away, start migrating those entities before the player arrives (~10 seconds lead time).

### Changes Required

**Server (`simulation.rs`):** Track per-entity CPU cost (simple: measure process_input time per entity type)
**Coordinator (`registry.rs`):** Cost-weighted comparison, spatial awareness in transfer selection, hysteresis cooldown

---

## Implementation Priority

| Phase | Effort | Gameplay Impact | Scalability Impact |
|-------|--------|-----------------|-------------------|
| A: Client prediction | Medium | Massive | None |
| B: Interest management | Medium | Moderate | Massive |
| C: Delta compression | Medium | Minor | Large |
| D: Peer health | Large | Large (resilience) | Moderate |
| E: Shadow extrapolation | Small | Moderate (resilience) | None |
| F: Smart rebalancing | Medium | Moderate | Moderate |

**Recommended order:** A → B → C → D → E → F

Phases A-C are the biggest wins. D-E handle degradation and resilience. F is optimization.

## References

- [Overwatch GDC Talk: Netcode](https://www.youtube.com/watch?v=W3aieHjyNvw) — Client prediction + server reconciliation
- [Star Citizen Server Meshing](https://robertsspaceindustries.com/comm-link/engineering/12790-Server-Meshing) — Authority transfer and entity streaming
- [Valve Source Multiplayer Networking](https://developer.valvesoftware.com/wiki/Source_Multiplayer_Networking) — Interpolation, prediction, lag compensation
- [Gabriel Gambetta: Fast-Paced Multiplayer](https://www.gabrielgambetta.com/client-server-game-architecture.html) — Definitive guide to prediction and reconciliation
