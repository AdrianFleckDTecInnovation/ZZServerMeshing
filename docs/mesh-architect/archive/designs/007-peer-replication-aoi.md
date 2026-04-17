# Design 007: Spatial Filtering for Peer-to-Peer UDP Replication

## Problem Statement

Server-to-server UDP replication sends ALL authoritative entities to ALL peers every tick (20Hz). With entity-level authority (not zone-based), this creates a fundamental scaling bottleneck: if Server A owns 400 entities clustered at (0,0) and Server B owns 400 entities at (3000,0,3000), neither server needs the other's full entity list for its connected clients. Yet both serialize, transmit, deserialize, and store ~400 shadow entities they may never use.

### The real constraint: UDP bandwidth on the UNO Q boards

The problem hits early on the target hardware. Each entity is approximately 50 bytes in bincode. With 28 entities per UDP chunk (to stay under the 1400-byte MTU), a server with 600 entities sends 22 chunks to each peer, every tick. At 20Hz with 2 peers, that is 880 UDP datagrams per second. At 600+ entities, the boards see thousands of drops per 5-second window (logged as "UDP replication channel full: N drops in last 5s"), well before tick budget becomes an issue (only ~27% at 3000 entities in CPU terms).

### What spatial filtering buys us

With entity-level authority distribution across a 10k x 10k world, servers' entity clusters naturally separate. A peer only needs shadows for entities that are:
1. Near its own entities (for client snapshot AoI filtering)
2. Near its clients' player entities (so clients see nearby entities from other servers)
3. Potentially receivable via authority transfer (the target needs the full state)
4. Recoverable after a crash (survivors need shadow copies)

Requirements 1-2 are spatial. Requirements 3-4 need special handling but do not require full-world replication every tick.

### Quantified impact

| Scenario | Before (entities/tick to each peer) | After (estimated) | Reduction |
|----------|--------------------------------------|---------------------|-----------|
| 3 servers, 200 each, all near origin | 200 | 200 | 0% (all in range) |
| 3 servers, 200 each, well-separated | 200 | ~30-50 (boundary entities) | 75-85% |
| 3 servers, 600 each, well-separated | 600 | ~50-80 (boundary entities) | 87-92% |
| 10 servers, 200 each, spread across world | 200 | ~20-40 (per peer) | 80-90% |

At 50 bytes per entity and 20Hz, going from 600 to 80 entities per peer reduces UDP throughput from ~60 KB/s to ~8 KB/s per peer. More critically, it reduces chunk count from 22 to 3 per peer per tick, eliminating the channel saturation that causes drops.

## Design

### Core Concept: Per-Peer Spatial Relevance

Instead of sending all authoritative entities to every peer, each server computes a per-peer "relevance set" based on spatial proximity. An entity on this server is relevant to a peer if it is within `PEER_AOI_RADIUS` of any entity the peer owns.

The key insight: we do not need to know the peer's exact entity positions in real time to compute relevance. We already have shadow copies of every peer's entities. We use the shadow positions (which are at most 1 tick stale) to compute the set of cells the peer occupies, then replicate our entities that are in or near those cells.

### Spatial Grid Approach

Rather than O(N*M) distance checks between our entities and each peer's shadows, we use a cell-based approach:

1. **Build a grid of cells occupied by our authoritative entities** (already available via `shared::spatial::world_to_cell`)
2. **For each peer, compute the set of cells their shadows occupy** (from our shadow entities tagged with that peer's server_id)
3. **Expand each peer's cell set by a radius** to get the "interest region" for that peer
4. **Filter our authoritative entities**: include only those whose cell falls within the peer's interest region

This is O(E_auth + E_shadow) per tick, not O(E_auth * E_shadow).

### Constants

```rust
/// Radius for peer-to-peer replication filtering, in world units.
/// Larger than client AOI_RADIUS (500) to ensure peers have shadows
/// for entities that might enter a client's AoI soon. The margin
/// accounts for entity movement between ticks and AoI boundary cases.
///
/// 750 units = 500 (client AoI) + 250 (movement margin at ~5 units/tick
/// for 50 ticks = 250 units of look-ahead).
pub const PEER_AOI_RADIUS: f32 = 750.0;

/// Peer AoI in grid cells. At CELL_SIZE=50, 750/50 = 15 cells.
pub const PEER_AOI_CELLS: i32 = (PEER_AOI_RADIUS / CELL_SIZE) as i32;
```

Why 750, not 500:
- Client AoI is 500 units. A peer needs shadows for entities that are within 500 units of the peer's clients.
- But the peer's clients might be moving toward our entities. At player speed ~5 units/tick and 20Hz, a player covers 100 units/second.
- A 250-unit margin gives ~2.5 seconds of look-ahead. Entities entering the client AoI will already be shadowed on the peer.
- If an entity enters client AoI without a shadow, the client simply does not see it until the next replication tick includes it. This is a brief flicker (50ms), not a correctness issue.

### Data Flow

```
prepare_replication (current):
  1. Query all Authoritative entities -> Vec<EntityState>
  2. Set OutboundReplication.snapshot = Some(entities)

prepare_replication (new):
  1. Query all Authoritative entities -> Vec<EntityState>
  2. Build HashMap<(i32,i32), Vec<usize>> mapping cell -> indices into entity vec
  3. For each peer:
     a. Collect cells occupied by shadows from this peer
     b. Expand cell set by PEER_AOI_CELLS
     c. Intersect with our cell index -> per-peer entity list
  4. Set OutboundReplication.per_peer = HashMap<u16, Vec<EntityState>>
  5. Keep OutboundReplication.snapshot = Some(all_entities) for crash recovery protocol
```

### The Crash Recovery Problem

Crash recovery depends on shadow copies. When a server dies, the coordinator tells survivors to adopt that server's entities from their shadows. If we filter replication, a survivor might not have shadows for distant entities from the dead server.

**Solution: Periodic full replication + crash recovery protocol.**

1. **Normal ticks (19 out of 20):** Send only spatially relevant entities to each peer.
2. **Full replication tick (1 out of 20 = every second):** Send ALL authoritative entities to ALL peers, exactly like today. This ensures every peer's shadow set is at most 1 second stale for crash recovery purposes. The existing `SHADOW_STALE_TICKS = 300 (15 seconds)` gives ample margin.
3. **On crash detection:** The coordinator's `RecoverEntities` fires. Survivors use their shadow copies (at most 1 second old from the last full replication) to adopt entities.

Cost of periodic full replication: 1 tick out of 20 sends the full set. This is the same cost as today but only 5% of the time instead of 100%. The other 95% of ticks benefit from filtering.

**Alternative considered: Crash recovery heartbeat with entity inventory.**
A separate low-frequency message listing all entity IDs (not full state) so peers know WHAT entities exist even if they do not have full shadows. Rejected because:
- Adds protocol complexity.
- The 1-per-second full replication is simpler and provides the same guarantee.
- Full state (not just IDs) is needed for crash recovery anyway.

### Authority Transfer Handling

Authority transfers already send the full `EntityState` via `PeerMessage::AuthorityTransfer`. This is sent on the control channel (high-priority, separate from replication). Spatial filtering does not affect transfers because:

1. The transfer message includes `full_state` -- the receiver has everything it needs.
2. If the receiver already has a shadow (from replication), it promotes the shadow to authoritative using the transfer's full_state. If not, it spawns fresh.
3. The two-phase protocol (send, wait for ACK, retry on timeout) handles lost packets.

No changes needed to the authority transfer path.

### Input Forwarding Handling

When a client sends `MoveInput` for an entity owned by another server, the local server forwards the input via `PeerMessage::ForwardedInput` on the control channel. This does not depend on replication filtering -- it uses the shadow's `authority_server` field to find the correct peer address.

However, if replication filtering causes a shadow to not exist locally (entity far from any of our entities), the local server will not know about the entity and will log a "not found" warning. This is correct behavior: if the entity is too far from the client's player for the client to see it, the client should not be sending inputs for it.

### Per-Peer Replication Resource

The `OutboundReplication` resource changes from a single `Vec<EntityState>` to a per-peer map plus a flag for full replication ticks:

```rust
#[derive(Resource, Default)]
pub struct OutboundReplication {
    /// Per-peer filtered entity lists (normal ticks).
    pub per_peer: HashMap<u16, Vec<EntityState>>,
    /// Full entity list for full-replication ticks and crash recovery.
    /// Set to Some on full-replication ticks (every FULL_REPL_INTERVAL).
    pub full_snapshot: Option<Vec<EntityState>>,
    /// Current tick.
    pub tick: u64,
}
```

### The Game Loop Changes

The game loop (Phase 4 in `game_loop.rs`) currently reads `OutboundReplication.snapshot`, serializes it, and sends to all peers. With per-peer filtering:

```
if full_repl_tick:
    serialize full_snapshot, send to all peers (unchanged from today)
else:
    for each (peer_id, entities) in per_peer:
        serialize entities, send to that specific peer
```

The chunking logic (ENTITIES_PER_CHUNK = 28) applies to each per-peer payload independently.

### Determining Peer Cell Sets

Each shadow entity has an `AuthorityServer` component identifying which peer owns it. To build the per-peer cell set:

```rust
// In prepare_replication system:
let mut peer_cells: HashMap<u16, HashSet<(i32, i32)>> = HashMap::new();

// Iterate shadows to collect per-peer cells
for (auth, pos) in shadow_query.iter() {
    let cell = world_to_cell(pos.0);
    peer_cells.entry(auth.0.0).or_default().insert(cell);
}
```

Then expand each peer's cell set by `PEER_AOI_CELLS`:

```rust
fn expand_cells(cells: &HashSet<(i32, i32)>, radius: i32) -> HashSet<(i32, i32)> {
    let mut expanded = HashSet::new();
    for &(cx, cz) in cells {
        for dx in -radius..=radius {
            for dz in -radius..=radius {
                expanded.insert((cx + dx, cz + dz));
            }
        }
    }
    expanded
}
```

At PEER_AOI_CELLS = 15, expanding one cell produces (2*15+1)^2 = 961 cells. With 200 shadow entities spread across maybe 50-100 unique cells, expansion produces a set of a few thousand cells. The HashSet lookup for each authoritative entity is O(1).

**Optimization: Convex hull or bounding box instead of per-cell expansion.**
For a first implementation, per-cell expansion with HashSet is fine. The cost is dominated by the expansion step, which for 100 unique cells at radius 15 is 100 * 961 = 96,100 HashSet insertions. On the UNO Q boards this takes <1ms. If profiling shows this matters, we can optimize to bounding-box checks (compute the bounding box of the peer's cells, expand by radius, and check each entity against the box). This is a constant-time check per entity instead of a HashSet lookup.

### Per-Tick Cost Analysis

**Before (current):**
- `prepare_replication`: O(E_auth) query -- collect all authoritative entities
- Game loop: serialize once, clone to all peers -- O(E_auth * P) bytes sent

**After:**
- `prepare_replication`:
  - O(E_auth) query to collect entities and build cell index
  - O(E_shadow) query to collect per-peer cell sets
  - O(P * C_peer * R^2) to expand cell sets (C_peer = unique cells per peer, R = PEER_AOI_CELLS)
  - O(P * E_auth) to filter entities per peer (HashSet lookup per entity per peer)
- Game loop: serialize per-peer, send to specific peer -- O(sum(E_relevant_per_peer)) bytes sent

The added CPU cost is:
- Shadow cell collection: negligible (one `world_to_cell` per shadow entity)
- Cell expansion: ~100 * 961 = 96K HashSet inserts per peer. At ~50ns each = ~5ms for 1 peer. This needs the bounding-box optimization for >2 peers.
- Entity filtering: 1 HashSet lookup per entity per peer. With 600 entities and 2 peers: 1200 lookups. Negligible.

**Revised approach using bounding box (required for boards):**
Instead of expanding individual cells into a HashSet, compute the axis-aligned bounding box of each peer's shadow positions, expand by PEER_AOI_RADIUS, and test each entity against the box. This is 4 float comparisons per entity per peer:

```rust
struct PeerBounds {
    min_x: f32, max_x: f32,
    min_z: f32, max_z: f32,
}

impl PeerBounds {
    fn contains(&self, pos: Vec3) -> bool {
        pos.x >= self.min_x && pos.x <= self.max_x
            && pos.z >= self.min_z && pos.z <= self.max_z
    }
}
```

Cost: O(E_shadow) to compute bounds + O(P * E_auth) for 4 float comparisons per entity per peer. With 2 peers and 600 entities: 2 * 600 * 4 = 4800 float ops. Well under 1 microsecond.

### Edge Cases

#### Peer with no shadows

If we have no shadows from a peer (e.g., just joined, or all shadows were stale-cleaned), we cannot compute its spatial footprint. **Fallback: send all entities to that peer.** This happens during startup and is self-correcting: once we receive the peer's first replication update, we create shadows and spatial filtering activates.

#### Peer with clients but no entities

A peer might have connected clients but zero authoritative entities (e.g., just started, or all entities were transferred away). Its entity footprint is empty. But its clients still need to see our entities. **This case is handled by the full-replication tick.** The client on the empty server sees entity arrivals delayed by at most 1 second (the full replication interval), then they are refreshed via filtered replication based on the peer's shadow positions (which include the client's player entity on that server -- oh wait, the client's player IS an entity on that server, so the peer does have at least one entity).

Actually, if a client is connected to a server, that server has the client's player entity as authoritative. So "peer with clients but no entities" cannot happen in practice. The player entity always provides a spatial anchor.

#### Entities on the boundary

An entity at exactly PEER_AOI_RADIUS distance might flicker in and out of the per-peer set. This is tolerable because:
1. The shadow stale timeout (15 seconds) is far longer than the 1-second full replication interval
2. Even if filtered out for a few ticks, the entity reappears on the next full tick
3. The 250-unit margin (750 vs 500) makes boundary flicker rare

#### Moving entities

An entity moving fast (say 5 units/tick = 100 units/second) could leave the peer's expanded bounding box between full replication ticks. The 250-unit margin handles this: at 100 units/second, it takes 2.5 seconds to traverse the margin. Full replication fires every 1 second.

#### All entities clustered in one spot

If all servers' entities are in the same area (e.g., during initial spawn), filtering provides no benefit. Every entity is within range of every peer's cells. The system gracefully degrades to full replication behavior, with the small overhead of computing the bounding boxes.

## Entity/Protocol Changes

### New constant in `shared/spatial.rs`

```rust
pub const PEER_AOI_RADIUS: f32 = 750.0;
```

### Modified resource in `crates/sim-server/src/resources.rs`

```rust
#[derive(Resource, Default)]
pub struct OutboundReplication {
    /// Per-peer filtered entity lists. Key is peer server_id.
    /// Empty on full-replication ticks (use full_snapshot instead).
    pub per_peer: HashMap<u16, Vec<EntityState>>,
    /// Full entity list. Set on full-replication ticks or when there is
    /// only one peer / no filtering benefit.
    pub full_snapshot: Option<Vec<EntityState>>,
    pub tick: u64,
}
```

### No new PeerMessage variants

The wire format is unchanged. Filtered replication uses the same `EntityStateUpdate` and `ChunkedEntityUpdate` messages. The receiver does not know or care that the sender filtered the entity list.

### No changes to crash recovery

`RecoverEntities` logic in `drain_inbound_events` queries shadows with `With<Shadow>` and promotes those from the dead server. The periodic full replication ensures shadows exist for all entities, not just nearby ones.

## Server-Side Implementation

### File: `crates/shared/src/spatial.rs`
- Add `PEER_AOI_RADIUS: f32 = 750.0` constant

### File: `crates/sim-server/src/resources.rs`
- Modify `OutboundReplication` to have `per_peer: HashMap<u16, Vec<EntityState>>` and rename `snapshot` to `full_snapshot`
- Add `FULL_REPL_INTERVAL: u64 = 20` constant (1 per second at 20Hz)

### File: `crates/sim-server/src/systems.rs` (`prepare_replication`)
- Change system signature to also query shadows: `Query<(&AuthorityServer, &Position), With<Shadow>>`
- Add `PeerAddresses` resource parameter to know which peers exist
- Compute per-peer bounding boxes from shadow positions
- Filter authoritative entities per peer
- On full-replication ticks, set `full_snapshot` instead of `per_peer`
- Use tick modulo `FULL_REPL_INTERVAL` to decide mode

### File: `crates/sim-server/src/game_loop.rs`
- Modify Phase 4 to handle both `full_snapshot` (broadcast to all peers) and `per_peer` (send to specific peers)
- Per-peer sends use the same chunking logic (ENTITIES_PER_CHUNK = 28)
- Clear both `per_peer` and `full_snapshot` after sending

## Client-Side Implementation

None. The client protocol is unaffected. Clients still receive server-to-client snapshots via WebSocket.

## Failure Modes

### Peer shadow data is stale
Shadow positions used for bounding box computation might be up to 1 tick old (50ms). At entity speeds of ~5 units/tick, this means the bounding box is off by at most 5 units. The 250-unit margin absorbs this entirely.

### Peer disappears (crash)
When a peer crashes:
1. Its shadows stop being refreshed but remain for 15 seconds (SHADOW_STALE_TICKS).
2. Within 5 seconds, the coordinator detects the crash and sends `RecoverEntities`.
3. Survivors use their shadow copies (at most 1 second old from last full replication) to adopt entities.
4. After adoption, the newly authoritative entities are replicated normally.

### Peer joins mid-session
A new peer has no shadows yet. The fallback sends all entities until the first replication from the new peer creates shadows. This is a brief window (1-2 ticks).

### Network partition
If Server A cannot reach Server B but both are alive:
- Server A's shadows of B go stale after 15 seconds and are cleaned up.
- Server A falls back to full replication for B (no shadows = no bounding box).
- The coordinator may detect B as dead and trigger recovery.
- If the partition heals, shadows are re-established within 1 tick.

### Full replication tick drops
If the 1-per-second full replication packet is lost (UDP is unreliable), distant shadows are not refreshed. But they survive for 15 seconds thanks to SHADOW_STALE_TICKS. The next full replication tick (1 second later) will refresh them. The probability of losing 15 consecutive full ticks is astronomically low.

## Testing Plan

### Unit Tests (Rust)

1. **Bounding box computation:** Given shadow positions, verify the bounding box is correct and expands by PEER_AOI_RADIUS.
2. **Entity filtering:** Given an authoritative entity set and per-peer bounding boxes, verify correct inclusion/exclusion.
3. **Full replication interval:** Verify that every Nth tick produces a full snapshot, not per-peer filtering.
4. **No shadows fallback:** Verify that when there are no shadows for a peer, all entities are sent.
5. **Multiple peers:** Verify each peer gets a different filtered entity set based on its spatial footprint.
6. **prepare_replication system test:** Integration test with ECS world containing authoritative and shadow entities at known positions.

### Scenario Tests

1. **peer-replication-aoi:** Three servers, each with entities at different positions. Verify each server's UDP replication to peers only includes relevant entities. Check via metrics or log output.
2. **crash-recovery-with-aoi:** Enable filtering, kill a server, verify recovery adopts all entities (including distant ones refreshed by full replication ticks).

### Board Verification

1. Deploy to UNO Q boards.
2. Spawn entities spread across the world.
3. Monitor UDP drop counter -- should decrease dramatically.
4. Verify entity visibility is unchanged for clients.
5. Kill a server and verify crash recovery still works.

## Migration

### Backward Compatibility

- Wire format unchanged. Old and new servers can interoperate (the receiver does not care what subset is sent).
- The periodic full replication ensures crash recovery works identically to today.
- No client changes.

### Deployment Order

1. Add `PEER_AOI_RADIUS` constant and bounding box helper (no behavioral change).
2. Modify `OutboundReplication` resource structure.
3. Update `prepare_replication` to compute per-peer sets.
4. Update game loop to send per-peer.
5. Deploy and verify on boards.

### Rollback

If filtering causes issues, changing `FULL_REPL_INTERVAL` to 1 (every tick is a full tick) effectively disables filtering with zero code changes.

## Constants Summary

| Constant | Value | File | Purpose |
|----------|-------|------|---------|
| PEER_AOI_RADIUS | 750.0 | shared/spatial.rs | Relevance radius for peer replication |
| FULL_REPL_INTERVAL | 20 | sim-server resources | Full replication every N ticks (1/sec at 20Hz) |
| AOI_RADIUS | 500.0 | shared/spatial.rs | Client snapshot AoI (unchanged) |
| SHADOW_STALE_TICKS | 300 (15s) | sim-server systems | Shadow cleanup (unchanged) |
| ENTITIES_PER_CHUNK | 28 | sim-server game_loop | UDP chunk size (unchanged) |
