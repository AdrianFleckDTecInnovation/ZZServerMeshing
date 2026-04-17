# Design 010: Tiered AoI-Aware Shadow Replication

## Problem Statement

The current peer replication system (design 007) sends all spatially-relevant entities to each peer at 20Hz on normal ticks, with a full snapshot to all peers every 20 ticks (1/sec) for crash recovery. The spatial filtering uses `PeerBounds` -- the axis-aligned bounding box of ALL shadow entities from a peer, expanded by `PEER_AOI_RADIUS` (750 units).

This works, but it treats every entity equally: an NPC 700 units from the nearest peer player is replicated at the same rate as an NPC 10 units from a player. Since clients only see entities within `AOI_RADIUS` (500 units), the 20Hz rate for distant-but-within-bounds entities is wasted bandwidth.

### Quantified problem

At 1500 entities/server with 2 peers:
- Full snapshot: 1500 entities * ~50 bytes/entity = ~75 KB per peer, sent as ~54 UDP chunks
- Filtered tick: fewer entities but still at 20Hz for all of them, even those no player will see for seconds
- Full snapshot burst (every 20 ticks): 54 chunks * 2 peers = 108 datagrams in one tick, competing with the 50ms tick budget on UNO Q boards

### What tiered replication buys

Split replication into two tiers based on proximity to **player entities** on the peer (not all entities):

| Tier | Entities | Rate | Purpose |
|------|----------|------|---------|
| Hot | Within 750 units of any peer PLAYER | 20Hz (every tick) | Players see smooth movement |
| Cold | All other authoritative entities | 5Hz (every 4th tick) | Crash recovery, rebalance awareness, far-off entity tracking |

With well-separated players across 3 servers, if each server has 500 entities but only 40-80 are near a peer's players, the hot tier sends 40-80 entities at 20Hz and the remaining 420-460 at 5Hz. Total bandwidth per peer drops from ~50 KB/s (500 entities * 50 bytes * 20Hz) to ~14 KB/s (80 * 50 * 20 + 420 * 50 * 5).

### Full snapshot change

The 1-per-second full snapshot (FULL_REPL_INTERVAL=20) was a crash recovery mechanism. With the cold tier sending all entities at 5Hz, every entity is refreshed at least every 200ms. The full snapshot is redundant for its original purpose. Change:
- Default interval: 20 ticks (1/sec) -> 600 ticks (30/sec)
- Default enabled: true -> **false** (behind a config flag)
- Keep all code paths so it can be re-enabled

## Design

### Tier Classification

For each peer, classify our authoritative entities into hot or cold:

```
For each peer P:
    1. Find all PLAYER shadow entities owned by P (EntityKind == Player, AuthorityServer == P)
    2. For each of our authoritative entities E:
        - If E is within PEER_AOI_RADIUS (750) of ANY player from step 1: HOT
        - Otherwise: COLD
```

This replaces the current `PeerBounds` approach which builds a bounding box from ALL shadows. Using only player positions is more precise: a cluster of 200 NPC shadows on a peer does not inflate the hot tier for our entities. Only players matter because only players have clients viewing the world.

### Distance Check

Use squared 2D distance on the XZ plane (same as `in_aoi()`):

```rust
/// Pre-computed squared PEER_AOI_RADIUS for hot tier distance checks.
pub const PEER_AOI_RADIUS_SQ: f32 = PEER_AOI_RADIUS * PEER_AOI_RADIUS; // 562500.0

fn is_hot(entity_pos: Vec3, player_positions: &[Vec3]) -> bool {
    for &pp in player_positions {
        let dx = entity_pos.x - pp.x;
        let dz = entity_pos.z - pp.z;
        if (dx * dx + dz * dz) <= PEER_AOI_RADIUS_SQ {
            return true;
        }
    }
    false
}
```

This is O(E_auth * P_players) where P_players is typically 1-3 per peer. With 500 auth entities and 2 player shadows per peer, that is 1000 distance checks = 4000 float ops. Well under 1 microsecond on the UNO Q boards.

### Cold Tier Timing

Cold entities are sent every 4th tick (5Hz). Use a simple global check:

```rust
let is_cold_tick = tick % 4 == 0;
```

On non-cold ticks, only hot entities are sent to each peer. On cold ticks, both hot AND cold entities are sent (cold piggybacks on the normal replication).

### Fallback: Peer Has No Player Shadows

If we have no Player shadows from a peer (e.g., the peer just joined, or shadow cleanup removed them), we cannot determine where the peer's players are. Fallback behavior:

1. If the peer has ANY shadows (but no players): use the current `PeerBounds` approach as-is (all shadows expand the bounding box). This handles the case where the peer's player shadows were lost but NPC shadows remain.
2. If the peer has NO shadows at all: skip replication on this tick (same as current behavior). The periodic full snapshot (if enabled) or the cold tier on the next cycle handles recovery.

This fallback means the system never loses data, just falls back to less-precise filtering.

### OutboundReplication Changes

The current resource has:
```rust
pub struct OutboundReplication {
    pub per_peer: HashMap<u16, Vec<EntityState>>,
    pub full_snapshot: Option<Vec<EntityState>>,
    pub tick: u64,
}
```

Change to:
```rust
pub struct OutboundReplication {
    /// Per-peer entity lists for this tick. On hot-only ticks, contains only
    /// hot-tier entities. On cold ticks (tick % 4 == 0), contains hot + cold.
    pub per_peer: HashMap<u16, Vec<EntityState>>,
    /// Full entity list for broadcast (only on full snapshot ticks, if enabled).
    pub full_snapshot: Option<Vec<EntityState>>,
    pub tick: u64,
}
```

The structure is identical. The per_peer map simply contains fewer entities on hot-only ticks and more on cold ticks. The game loop consumption code does not change.

### ReplicationConfig Changes

Current:
```rust
pub struct ReplicationConfig {
    pub full_interval: u64,  // default: 20
}
```

New:
```rust
pub struct ReplicationConfig {
    /// How often to send a full (unfiltered) snapshot to all peers.
    /// Default: 600 (30 seconds at 20Hz). Only used when full_snapshot_enabled is true.
    pub full_interval: u64,
    /// Whether the periodic full snapshot is enabled.
    /// Default: false. The cold tier at 5Hz makes it redundant.
    pub full_snapshot_enabled: bool,
}
```

The `--full-repl-interval` CLI flag continues to work but now only takes effect when full snapshots are enabled. Add a new `--full-snapshot` boolean CLI flag (default false).

### prepare_replication System Changes

The system signature adds `EntityKind` to the shadow query:

```rust
pub fn prepare_replication(
    tick: Res<TickCounter>,
    config: Res<ServerConfig>,
    repl_config: Res<ReplicationConfig>,
    peer_addrs: Res<PeerAddresses>,
    mut outbound: ResMut<OutboundReplication>,
    auth_query: Query</* unchanged */>,
    shadow_query: Query<(&AuthorityServer, &Position, &EntityKind), With<Shadow>>,
) {
```

Logic:

```
1. Collect all authoritative entities into Vec<EntityState>
2. Set outbound.tick = tick

3. Check full snapshot: if full_snapshot_enabled AND tick % full_interval == 0:
   - Set full_snapshot = Some(entities)
   - Clear per_peer
   - Return

4. If no peers or no entities: clear and return

5. Determine if this is a cold tick: tick % 4 == 0

6. For each peer:
   a. Collect PLAYER shadow positions for this peer
   b. If no player positions found:
      - Fall back to PeerBounds from ALL shadows (current behavior)
      - On cold ticks: include all entities within bounds
      - On non-cold ticks: include all entities within bounds
        (no tier distinction without player positions)
   c. If player positions found:
      - For each auth entity: check distance to each player position
      - If hot (within PEER_AOI_RADIUS): always include
      - If cold (outside): include only if is_cold_tick
   d. Insert filtered list into per_peer

7. Skip peers with no shadows (existing behavior)
```

### Metrics Changes

Update `MetricsCollector` and `MetricsSnapshot` to include:
- `replication_hot_count`: entities sent to peers as hot tier this tick
- `replication_cold_count`: entities sent to peers as cold tier this tick (0 on non-cold ticks)

These replace the single `replication_sent_count` with more granular data.

### Data Flow

```
Tick 1 (hot only):
  prepare_replication:
    per_peer[2] = [entity_A, entity_C]        (hot: near peer 2's players)
    per_peer[3] = [entity_B]                    (hot: near peer 3's players)
  game_loop Phase 4:
    UDP to peer 2: serialize [A, C]
    UDP to peer 3: serialize [B]

Tick 2 (hot only): same pattern

Tick 3 (hot only): same pattern

Tick 4 (cold tick):
  prepare_replication:
    per_peer[2] = [entity_A, entity_C, entity_D, entity_E]  (hot + cold)
    per_peer[3] = [entity_B, entity_F, entity_G]              (hot + cold)
  game_loop Phase 4:
    UDP to peer 2: serialize [A, C, D, E]
    UDP to peer 3: serialize [B, F, G]

Tick 5 (hot only): back to hot-only
...
```

### Per-Tick Cost Analysis

**Before (current, design 007):**
- Shadow query: O(E_shadow) to build PeerBounds from ALL shadows
- Entity filter: O(P * E_auth) bounding box checks (4 float comparisons each)
- Bandwidth: O(P * E_within_bounds * 20Hz) bytes/sec

**After (tiered):**
- Shadow query: O(E_shadow) with EntityKind filter to collect player positions per peer
- Entity filter: O(P * E_auth * P_players_per_peer) distance checks (3-4 float ops each)
- Bandwidth:
  - Hot ticks (3 out of 4): O(P * E_hot * 20Hz)
  - Cold ticks (1 out of 4): O(P * E_all_within_bounds * 5Hz)

Added CPU cost per tick: minimal. We replace 4 float comparisons (bounding box) with ~6 float ops (squared distance), but only check against 1-3 player positions instead of building a bounding box from hundreds of shadows. The shadow query loop is actually cheaper because we only collect player positions, not all positions.

Net CPU change: approximately zero. The hot-cold classification adds negligible overhead.

Bandwidth savings: 50-75% reduction depending on entity distribution. In the worst case (all entities near all players), there is zero reduction but also zero added cost.

## Entity/Protocol Changes

### Modified constant in `shared/spatial.rs`

```rust
/// Pre-computed squared peer AoI radius for distance checks.
pub const PEER_AOI_RADIUS_SQ: f32 = PEER_AOI_RADIUS * PEER_AOI_RADIUS;
```

### Modified resource in `sim-server/src/resources.rs`

```rust
#[derive(Resource)]
pub struct ReplicationConfig {
    pub full_interval: u64,           // default: 600 (30s)
    pub full_snapshot_enabled: bool,  // default: false
}
```

### No new PeerMessage variants

Wire format is unchanged. The receiver cannot distinguish hot from cold entities -- they are the same `EntityStateUpdate` or `ChunkedEntityUpdate` messages.

### No changes to crash recovery

The cold tier at 5Hz ensures every entity reaches every peer at least every 200ms. This is far better than the previous 1-per-second full snapshot for crash recovery purposes. SHADOW_STALE_TICKS (300 = 15 seconds) provides ample margin.

## Server-Side Implementation

### File: `crates/shared/src/spatial.rs`
- Add `PEER_AOI_RADIUS_SQ: f32 = PEER_AOI_RADIUS * PEER_AOI_RADIUS` constant

### File: `crates/sim-server/src/resources.rs`
- Add `full_snapshot_enabled: bool` to `ReplicationConfig` (default: false)
- Change `FULL_REPL_INTERVAL` default from 20 to 600
- Update `ReplicationConfig::default()` to set `full_snapshot_enabled: false`
- Update existing test if it asserts on `full_interval` default

### File: `crates/sim-server/src/systems.rs` (`prepare_replication`)
- Add `EntityKind` to shadow query tuple
- Build `per_peer_player_positions: HashMap<u16, Vec<Vec3>>` from Player shadows
- Add `is_cold_tick = tick % 4 == 0` check
- For peers with player positions: use distance-based hot/cold classification
- For peers without player positions: fall back to PeerBounds (current behavior)
- On hot-only ticks: include only hot entities in per_peer
- On cold ticks: include hot + cold entities in per_peer
- Update the full snapshot condition to check `full_snapshot_enabled`

### File: `crates/sim-server/src/game_loop.rs`
- No structural changes to Phase 4. The per_peer map is consumed identically.
- Update metric recording to split hot/cold counts

### File: `crates/sim-server/src/metrics.rs`
- Add `replication_hot_count: usize` and `replication_cold_count: usize` to `MetricsCollector` and `MetricsSnapshot`

### File: `crates/sim-server/src/main.rs`
- Add `--full-snapshot` CLI flag (bool, default false)
- Change `--full-repl-interval` default from 20 to 600
- Pass both to `ReplicationConfig`

### File: `crates/sim-server/src/metrics_server.rs`
- Update dashboard HTML to show hot/cold replication counts (optional, not blocking)

## Client-Side Implementation

None. The client protocol is unaffected.

## Failure Modes

### Peer has only NPC shadows, no player shadows
Falls back to PeerBounds from all shadows. This is the current behavior and maintains full correctness. The system produces a warning log every 5 seconds when this happens so operators can investigate why player shadows are missing.

### Player shadow is stale (not yet updated)
Player shadow positions can be up to 1 tick (50ms) stale. At player speed of 5 units/tick, this is a 5-unit error. The 250-unit margin (750 vs 500) absorbs this entirely.

### Player moves rapidly across the world
A teleporting player could leave the hot zone between ticks. The cold tier (every 200ms) catches up within 4 ticks. No entity is permanently lost.

### Cold phase coincides with high tick budget
Cold entities are phase-spread (~25% per tick), so no single tick has a burst. If the tick is still near budget, the try_send drop mechanism handles overflow naturally.

### Full snapshot disabled and cold phase packet lost
Without the full snapshot, a lost cold-phase packet means a cold entity is not refreshed for 200ms (until its next phase tick). SHADOW_STALE_TICKS = 300 (15s) gives 75 cold-phase chances before staleness cleanup. Losing all 75 consecutive cold packets for a specific entity is effectively impossible.

### All entities clustered together
If all auth entities are within 750 units of a peer's player, everything is hot tier. No bandwidth reduction, no added cost. The system degrades to current behavior.

## Testing Plan

### Unit Tests (in `systems.rs`)

1. **Hot-only tick sends only nearby entities:** Peer has a player shadow at (0,0). Auth entities at (0,0) and (4000,0). Tick 1 (non-cold). Verify per_peer contains only the nearby entity.

2. **Cold tick sends hot + cold entities:** Same setup. Tick 4 (cold tick). Verify per_peer contains both entities.

3. **Player-based filtering vs all-shadow filtering:** Peer has player shadow at (0,0) and NPC shadows at (3000,0). Auth entity at (2500,0). Verify: with player filtering, entity is cold (far from player). The entity would be included under the old PeerBounds approach (NPC shadow at 3000 expands the box).

4. **Peer with no player shadows falls back to PeerBounds:** Peer has only NPC shadows. Verify it uses bounding box approach (current behavior).

5. **Full snapshot disabled by default:** Tick 20 (old default full interval). Verify full_snapshot is None.

6. **Full snapshot enabled at new interval:** Set full_snapshot_enabled=true, full_interval=600. Tick 600. Verify full_snapshot is Some.

7. **Multiple peers get different hot/cold splits:** Peer 2 player at (0,0), peer 3 player at (3000,0,3000). Auth entities at both locations. Verify peer 2 gets origin entities as hot, peer 3 gets distant entities as hot.

8. **Cold tick modulo is 4:** Verify tick 4, 8, 12 are cold. Verify tick 1, 2, 3, 5 are hot-only.

### Scenario Tests

1. **tiered-replication:** Three servers with entities at different positions. Verify via metrics that hot_count < total auth count on non-cold ticks, and hot_count + cold_count = total on cold ticks.

2. **crash-recovery-with-tiers:** Kill a server. Verify all entities are recovered (cold tier ensures shadows exist for all entities).

### Board Verification

1. Deploy to UNO Q boards
2. Spawn entities spread across the world
3. Monitor metrics: verify hot/cold split in replication counts
4. Verify UDP drop counter stays low
5. Verify entity visibility is unchanged for clients
6. Kill a server and verify crash recovery works

## Migration

### Backward Compatibility

- Wire format unchanged. No protocol version bump needed.
- Existing `--full-repl-interval` flag still works (but only takes effect when `--full-snapshot` is passed).
- Without the `--full-snapshot` flag, full snapshots are disabled (new default).
- The cold tier at 5Hz provides better crash recovery coverage than the old 1/sec full snapshot.

### Deployment Order

1. Add `PEER_AOI_RADIUS_SQ` constant (no behavioral change)
2. Add `full_snapshot_enabled` to `ReplicationConfig`, update defaults
3. Update `prepare_replication` with tiered logic
4. Update CLI flags
5. Update metrics
6. Deploy and verify on boards

### Rollback

If tiered replication causes issues:
- Pass `--full-repl-interval 1` to disable all filtering (every tick is full)
- Or pass `--full-snapshot` to re-enable periodic full snapshots at 30s intervals
- Or revert the code -- the old PeerBounds logic is still present as the fallback path

## Constants Summary

| Constant | Value | File | Purpose |
|----------|-------|------|---------|
| PEER_AOI_RADIUS | 750.0 | shared/spatial.rs | Hot tier distance threshold (unchanged) |
| PEER_AOI_RADIUS_SQ | 562500.0 | shared/spatial.rs | Squared threshold for distance checks (new) |
| FULL_REPL_INTERVAL | 600 | sim-server resources | Full snapshot every 30s (changed from 20) |
| full_snapshot_enabled | false | ReplicationConfig | Full snapshot off by default (new) |
| Cold phase modulo | 4 | prepare_replication | Per-entity phase spread, 5Hz per entity |
| AOI_RADIUS | 500.0 | shared/spatial.rs | Client snapshot AoI (unchanged) |
| SHADOW_STALE_TICKS | 300 (15s) | sim-server systems | Shadow cleanup (unchanged) |
| ENTITIES_PER_CHUNK | 28 | sim-server game_loop | UDP chunk size (unchanged) |
