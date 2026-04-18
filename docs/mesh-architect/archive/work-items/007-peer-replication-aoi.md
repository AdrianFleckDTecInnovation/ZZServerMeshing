# Work Items: 007 - Peer Replication AoI

Design doc: `docs/mesh-architect/designs/007-peer-replication-aoi.md`

## Phase 1: Infrastructure (no behavioral change)

### WI-070: Add PEER_AOI_RADIUS constant
**Size:** small
**Depends on:** none
**Files:** `crates/shared/src/spatial.rs`
**Description:**
Add the peer replication AoI radius constant alongside the existing client AoI constants:

```rust
/// Radius for peer-to-peer replication filtering, in world units.
/// Larger than client AOI_RADIUS (500) to include a movement margin so
/// entities approaching a peer's client AoI are pre-replicated.
pub const PEER_AOI_RADIUS: f32 = 750.0;
```

**Tests:**
- Add a test asserting `PEER_AOI_RADIUS > AOI_RADIUS` (documents the invariant that peer AoI is always larger than client AoI).
- Add a test asserting `PEER_AOI_RADIUS == 750.0` (guards against accidental changes).

**Done when:** Constant exists, tests pass, `cargo build` succeeds.

---

### WI-071: Add PeerBounds helper struct and methods
**Size:** small
**Depends on:** WI-070
**Files:** `crates/shared/src/spatial.rs`
**Description:**
Add a bounding-box struct for computing the spatial footprint of a peer's entities:

```rust
/// Axis-aligned bounding box on the XZ plane, expanded by a radius.
/// Used to determine which of our entities are spatially relevant to a peer.
#[derive(Debug, Clone)]
pub struct PeerBounds {
    pub min_x: f32,
    pub max_x: f32,
    pub min_z: f32,
    pub max_z: f32,
}

impl PeerBounds {
    /// Build from a set of positions, expanding by the given radius.
    /// Returns None if positions is empty.
    pub fn from_positions(positions: impl Iterator<Item = Vec3>, radius: f32) -> Option<Self> {
        let mut min_x = f32::MAX;
        let mut max_x = f32::MIN;
        let mut min_z = f32::MAX;
        let mut max_z = f32::MIN;
        let mut any = false;

        for pos in positions {
            any = true;
            min_x = min_x.min(pos.x);
            max_x = max_x.max(pos.x);
            min_z = min_z.min(pos.z);
            max_z = max_z.max(pos.z);
        }

        if !any {
            return None;
        }

        Some(Self {
            min_x: min_x - radius,
            max_x: max_x + radius,
            min_z: min_z - radius,
            max_z: max_z + radius,
        })
    }

    /// Check if a position falls within this bounding box (inclusive).
    #[inline]
    pub fn contains(&self, pos: Vec3) -> bool {
        pos.x >= self.min_x && pos.x <= self.max_x
            && pos.z >= self.min_z && pos.z <= self.max_z
    }
}
```

**Tests:**
- `from_positions` with a single position at (100, 0, 200) and radius 750: verify bounds are (-650, 850, -550, 950).
- `from_positions` with two positions at opposite corners: verify bounds encompass both plus radius.
- `from_positions` with empty iterator: verify returns None.
- `contains` with a position inside, on boundary, and outside.
- `contains` with negative coordinates.
- Verify Y coordinate is ignored (two positions with different Y but same XZ produce same bounds).

**Done when:** `PeerBounds` struct and methods exist with tests. `cargo test -p shared` passes.

---

### WI-072: Restructure OutboundReplication resource
**Size:** medium
**Depends on:** none
**Files:** `crates/sim-server/src/resources.rs`, `crates/sim-server/src/game_loop.rs`, `crates/sim-server/src/systems.rs`, `crates/sim-server/src/schedule.rs`
**Description:**
Modify the `OutboundReplication` resource to support both per-peer filtered replication and full-broadcast replication:

```rust
/// Full replication interval in ticks. Every FULL_REPL_INTERVAL ticks,
/// send ALL authoritative entities to ALL peers (for crash recovery).
/// At 20Hz, 20 ticks = once per second.
pub const FULL_REPL_INTERVAL: u64 = 20;

#[derive(Resource, Default)]
pub struct OutboundReplication {
    /// Per-peer filtered entity lists. Key is peer server_id (u16).
    /// Populated on normal ticks. Empty on full-replication ticks.
    pub per_peer: HashMap<u16, Vec<EntityState>>,
    /// Full entity list for broadcast. Set on full-replication ticks
    /// (tick % FULL_REPL_INTERVAL == 0) or when filtering provides no benefit
    /// (e.g., only one peer, no shadows to compute bounds from).
    pub full_snapshot: Option<Vec<EntityState>>,
    /// Current tick number.
    pub tick: u64,
}
```

This is a refactor of the existing `OutboundReplication` which currently has `snapshot: Option<Vec<EntityState>>` and `tick: u64`. The rename from `snapshot` to `full_snapshot` plus the addition of `per_peer` requires updating all consumers:

1. **`prepare_replication` in systems.rs:** Currently sets `outbound.snapshot`. Change to set `outbound.full_snapshot` (same behavior for now -- filtering comes in WI-074).
2. **Game loop Phase 4 in game_loop.rs:** Currently reads `outbound.snapshot.take()`. Change to check `full_snapshot` first, then `per_peer`. For this WI, only `full_snapshot` will be populated, so behavior is unchanged.
3. **Schedule tests in schedule.rs:** Update `outbound.snapshot` references to `outbound.full_snapshot`.
4. **Game loop tests in game_loop.rs:** Same rename.

Important: `prepare_replication` should still set `full_snapshot` for every tick in this WI. The per-peer filtering logic comes in WI-074. This WI is purely structural.

**Tests:**
- All existing tests that reference `OutboundReplication.snapshot` should be updated to use `full_snapshot` and still pass.
- Add a test that `per_peer` defaults to empty HashMap and `full_snapshot` defaults to None.
- Verify `cargo test -p sim-server` passes with no behavioral change.

**Done when:** Resource restructured, all references updated, all existing tests pass.

---

## Phase 2: Spatial Filtering Logic

### WI-073: Compute per-peer bounding boxes from shadows
**Size:** medium
**Depends on:** WI-071, WI-072
**Files:** `crates/sim-server/src/systems.rs`
**Description:**
Modify the `prepare_replication` system to compute per-peer spatial bounding boxes from shadow entities. The system currently queries only `Authoritative` entities. Add a second query for `Shadow` entities to build per-peer bounding boxes:

```rust
pub fn prepare_replication(
    tick: Res<TickCounter>,
    config: Res<ServerConfig>,
    peer_addrs: Res<PeerAddresses>,
    mut outbound: ResMut<OutboundReplication>,
    auth_query: Query<
        (&NetworkId, &Position, &Velocity, &EntityKind, &AuthorityServer, Option<&LastInputSeq>),
        With<Authoritative>,
    >,
    shadow_query: Query<(&AuthorityServer, &Position), With<Shadow>>,
) {
    // ... existing entity collection ...

    // Build per-peer bounding boxes from shadow positions
    let mut peer_positions: HashMap<u16, Vec<Vec3>> = HashMap::new();
    for (auth, pos) in shadow_query.iter() {
        // Skip our own server_id (shouldn't have shadows of self, but be safe)
        if auth.0 == config.server_id {
            continue;
        }
        peer_positions.entry(auth.0.0).or_default().push(pos.0);
    }

    let peer_bounds: HashMap<u16, PeerBounds> = peer_positions.into_iter()
        .filter_map(|(sid, positions)| {
            PeerBounds::from_positions(positions.into_iter(), PEER_AOI_RADIUS)
                .map(|bounds| (sid, bounds))
        })
        .collect();

    // Store peer_bounds somewhere accessible (either in OutboundReplication or a new resource)
}
```

For this WI, compute the bounds and log them at debug level. Do not apply filtering yet -- that is WI-074. This allows verifying the bounding box computation is correct before using it to filter.

Add a debug log every 5 seconds (tick % 100 == 0) showing per-peer bounding box dimensions:
```
Peer S2 bounds: x=[-150, 950] z=[-200, 800] (1100x1000 units)
```

**Tests:**
- In a test world with shadows from server 2 at positions (100,0,100) and (200,0,300): verify the computed PeerBounds for server 2 are (-650, 950, -650, 1050) (positions expanded by 750).
- With shadows from two different peers: verify separate bounding boxes.
- With no shadows: verify no bounding box computed (empty map).
- Verify existing `prepare_replication` tests still pass (system signature changed but behavior unchanged).

**Done when:** `prepare_replication` computes per-peer bounding boxes from shadows. Logged at debug level. No filtering applied yet. All existing tests pass.

---

### WI-074: Apply per-peer spatial filtering in prepare_replication
**Size:** large
**Depends on:** WI-073
**Files:** `crates/sim-server/src/systems.rs`, `crates/sim-server/src/resources.rs`
**Description:**
Apply the bounding box computation from WI-073 to actually filter entities per peer. On normal ticks, `prepare_replication` populates `outbound.per_peer` instead of `outbound.full_snapshot`. On full-replication ticks (tick % FULL_REPL_INTERVAL == 0), it still sets `full_snapshot`.

Logic:
```rust
let is_full_tick = tick.0 % FULL_REPL_INTERVAL == 0;

if is_full_tick || peer_bounds.is_empty() {
    // Full replication: send everything to everyone
    outbound.full_snapshot = if entities.is_empty() { None } else { Some(entities) };
    outbound.per_peer.clear();
} else {
    // Filtered replication: per-peer entity lists
    outbound.full_snapshot = None;

    for (&peer_id, bounds) in &peer_bounds {
        let relevant: Vec<EntityState> = entities.iter()
            .filter(|e| bounds.contains(e.position))
            .cloned()
            .collect();
        if !relevant.is_empty() {
            outbound.per_peer.insert(peer_id, relevant);
        }
    }

    // For peers in PeerAddresses but NOT in peer_bounds (no shadows from them),
    // fall back to full replication for that specific peer.
    for &peer_id in peer_addrs.0.keys() {
        if !peer_bounds.contains_key(peer_id) && !outbound.per_peer.contains_key(peer_id) {
            // No shadow data for this peer -- send everything
            outbound.per_peer.insert(*peer_id, entities.clone());
        }
    }
}
```

Important edge cases:
- Peer is in `PeerAddresses` but has no shadows: send all (new peer just joined).
- Peer has shadows but no entities are within its bounds: send empty (peer is far away). The peer's existing shadows age normally and are refreshed on the next full tick.
- All peers are in range of all entities: per_peer maps contain the same entities as full_snapshot would. No savings, small overhead. This is correct.

Update the test `replication_includes_all_entities_regardless_of_distance`: this test verifies that replication does NOT filter by client AoI (which is correct -- client AoI is separate from peer AoI). However, this test uses `prepare_replication` alone without shadows, so it will hit the "no shadows = full replication" fallback. Add a comment explaining this.

**Tests:**
- **Filtered replication:** Set up world with auth entities at (0,0) and shadows from peer 2 at (3000,0,3000). Verify `per_peer[2]` does NOT contain the entity at (0,0) because it is outside peer 2's expanded bounds.
- **Full replication tick:** Set tick to a multiple of FULL_REPL_INTERVAL. Verify `full_snapshot` is Some and `per_peer` is empty.
- **No shadows fallback:** No shadows in world, peer address exists. Verify that peer gets all entities via `per_peer` (or `full_snapshot`).
- **Mixed proximity:** Auth entities at (0,0), (100,0,0), and (4000,0,0). Shadow from peer 2 at (0,0). Verify `per_peer[2]` includes (0,0) and (100,0,0) but excludes (4000,0,0).
- **Player entity always included:** This is handled implicitly by the bounding box -- the peer's player entity creates a shadow on our server, which defines the bounding box. The player's position anchors the bounds. No special handling needed.
- **Existing tests pass:** `replication_includes_all_entities_regardless_of_distance` should still pass (no shadows = full fallback).

**Done when:** `prepare_replication` produces per-peer filtered entity lists on non-full ticks. Full replication fires every 20 ticks. All tests pass.

---

## Phase 3: Game Loop Integration

### WI-075: Update game loop Phase 4 to send per-peer replication
**Size:** medium
**Depends on:** WI-074
**Files:** `crates/sim-server/src/game_loop.rs`
**Description:**
Modify the UDP replication enqueue logic (Phase 4 in the game loop) to handle both `full_snapshot` and `per_peer`.

Current logic (simplified):
```rust
if let Some(entities) = &repl_data {
    for addr in &peer_addrs {
        // serialize entities, send to addr (with chunking)
    }
}
```

New logic:
```rust
// Check full_snapshot first (full replication ticks)
let full_data = world.resource_mut::<OutboundReplication>().full_snapshot.take();
let per_peer_data = std::mem::take(&mut world.resource_mut::<OutboundReplication>().per_peer);
let repl_tick = world.resource::<OutboundReplication>().tick;

if let Some(entities) = &full_data {
    // Full replication: broadcast to all peers (unchanged from current behavior)
    let peer_addrs: Vec<_> = world.resource::<PeerAddresses>().0.values().copied().collect();
    // ... existing chunking + send logic ...
} else if !per_peer_data.is_empty() {
    // Filtered replication: send to specific peers
    let peer_addrs_map = &world.resource::<PeerAddresses>().0;
    let server_id_u16 = world.resource::<ServerConfig>().server_id;

    for (peer_id, entities) in &per_peer_data {
        if let Some(&addr) = peer_addrs_map.get(peer_id) {
            // Same chunking logic as full replication, but only to this peer
            if entities.len() <= ENTITIES_PER_CHUNK {
                let msg = PeerMessage::EntityStateUpdate { ... };
                // serialize and send to addr
            } else {
                // chunk and send to addr
            }
        }
    }
}
```

The chunking logic (ENTITIES_PER_CHUNK = 28) is the same. The only difference is the destination: all peers vs specific peer.

Refactor the chunking logic into a helper function to avoid duplication:
```rust
fn serialize_and_enqueue(
    entities: &[EntityState],
    tick: u64,
    server_id: ServerId,
    addr: SocketAddr,
    tx: &tokio::sync::mpsc::Sender<UdpOutbound>,
    drops: &mut u64,
    closed: &mut bool,
) { ... }
```

**Tests:**
- Verify full replication ticks still send to all peers.
- Verify filtered ticks send only to the specific peer.
- Verify chunking works correctly for per-peer sends.
- Verify the game loop runs without panics with both code paths.

**Done when:** Game loop sends per-peer filtered replication on normal ticks and full replication on interval ticks. No behavioral change for full ticks. All tests pass.

---

### WI-076: Add metrics for filtered vs full replication
**Size:** small
**Depends on:** WI-075
**Files:** `crates/sim-server/src/metrics.rs`, `crates/sim-server/src/game_loop.rs`
**Description:**
Add metrics tracking so we can measure the filtering effectiveness:

1. In the debug log (every 5 seconds), include:
   - `repl_mode`: "full" or "filtered"
   - `repl_entities_sent`: total entities sent across all peers this tick
   - `repl_entities_saved`: total entities NOT sent due to filtering (difference from full count)

2. In the metrics endpoint, add:
   - `replication_mode`: string "full" or "filtered"
   - `replication_entities_per_peer`: map of peer_id -> entity count sent
   - `full_replication_count`: total auth entities (what would be sent unfiltered)

These metrics allow verifying on the boards that filtering is actually reducing traffic.

**Tests:**
- Unit test: verify metrics struct accepts the new fields.
- Integration: deploy to boards, spawn entities at different positions, verify metrics show filtered counts less than full counts.

**Done when:** Metrics report filtering effectiveness. Visible in debug logs and `/metrics` endpoint.

---

## Phase 4: Safety and Edge Cases

### WI-077: Handle peer join/leave during filtered replication
**Size:** small
**Depends on:** WI-075
**Files:** `crates/sim-server/src/systems.rs`
**Description:**
When a new peer appears in `PeerAddresses` but has no shadows yet, `prepare_replication` should fall back to full replication for that specific peer. WI-074 handles this case already, but this WI adds explicit tests and logging:

1. When a peer has no shadows: log at info level "No shadows for peer S{id}, sending full replication".
2. When a peer disappears from `PeerAddresses`: its entry is simply not in `per_peer`. No cleanup needed -- stale shadows are cleaned by `shadow_cleanup`.
3. When a peer re-joins after crash: same as new peer (no shadows initially, full fallback).

**Tests:**
- Add peer to `PeerAddresses` with no shadows. Run `prepare_replication`. Verify all entities in that peer's `per_peer` entry.
- Remove peer from `PeerAddresses`. Run `prepare_replication`. Verify that peer is not in `per_peer`.

**Done when:** Edge cases for peer join/leave are tested and logged. No behavioral change beyond logging.

---

### WI-078: Crash recovery integration test with filtered replication
**Size:** medium
**Depends on:** WI-075
**Files:** `tests/` (scene tests or test scenarios)
**Description:**
Verify that crash recovery works correctly when spatial filtering is active. The concern: if Server B crashes, Server A needs shadow copies of B's entities to adopt them. With filtering, A might not have shadows for B's distant entities. The design relies on the 1-per-second full replication to maintain these shadows.

Test scenario:
1. Start 3 servers.
2. Spawn entities on each server at well-separated positions (e.g., S1 at origin, S2 at (3000,0,0), S3 at (-3000,0,0)).
3. Wait at least 2 seconds (2 full replication cycles) for shadows to be established.
4. Kill server S2.
5. Verify that S1 and S3 adopt all of S2's entities from their shadow copies.
6. Verify entity count is preserved (no entities lost).

This test exercises the most dangerous failure mode of filtered replication.

**Tests:**
- Scene test: `peer-replication-aoi-crash-recovery` that runs the above scenario.
- Verify via the coordinator's `/servers` endpoint that total entity count is preserved after crash.

**Done when:** Crash recovery works correctly with filtered replication active. Test passes on local cluster and boards.

---

### WI-079: FULL_REPL_INTERVAL tuning knob and safe rollback
**Size:** small
**Depends on:** WI-075
**Files:** `crates/sim-server/src/resources.rs`, `crates/sim-server/src/main.rs`
**Description:**
Add a command-line argument `--full-repl-interval <N>` (default 20) that controls how often full replication fires. This serves as both a tuning knob and a rollback mechanism:

- `--full-repl-interval 1` = every tick is full replication = filtering disabled = identical to current behavior.
- `--full-repl-interval 20` = default = once per second.
- `--full-repl-interval 40` = every 2 seconds (more aggressive filtering, slightly higher crash recovery risk).

The value is passed into the ECS world as a resource and read by `prepare_replication`.

```rust
#[derive(Resource)]
pub struct ReplicationConfig {
    pub full_interval: u64,
}
```

**Tests:**
- Test that `--full-repl-interval 1` results in every tick being a full replication tick.
- Test that `--full-repl-interval 20` results in tick 20, 40, 60, etc. being full ticks.
- Parse test for the CLI argument.

**Done when:** `--full-repl-interval` flag is accepted and functional. Default behavior unchanged.

---

## Summary

| WI | Title | Size | Depends | Phase |
|----|-------|------|---------|-------|
| 070 | Add PEER_AOI_RADIUS constant | small | - | 1 |
| 071 | Add PeerBounds helper struct | small | 070 | 1 |
| 072 | Restructure OutboundReplication resource | medium | - | 1 |
| 073 | Compute per-peer bounding boxes from shadows | medium | 071, 072 | 2 |
| 074 | Apply per-peer spatial filtering | large | 073 | 2 |
| 075 | Update game loop for per-peer sends | medium | 074 | 3 |
| 076 | Add replication filtering metrics | small | 075 | 3 |
| 077 | Handle peer join/leave edge cases | small | 075 | 4 |
| 078 | Crash recovery integration test | medium | 075 | 4 |
| 079 | FULL_REPL_INTERVAL CLI tuning knob | small | 075 | 4 |

**Estimated total: 12-18 hours**

### Execution Order

```
Phase 1 (parallel):
  WI-070 -> WI-071 (sequential)
  WI-072 (independent)

Phase 2 (sequential, depends on Phase 1):
  WI-073 -> WI-074

Phase 3 (sequential, depends on Phase 2):
  WI-075 -> WI-076

Phase 4 (parallel, depends on Phase 3):
  WI-077
  WI-078
  WI-079
```

### Rollback Plan

If filtering causes any issues in production:
1. **Immediate:** Set `--full-repl-interval 1` on all servers. This disables filtering without code changes.
2. **Quick fix:** Revert WI-074 (the filtering logic). The restructured resource (WI-072) and bounding box computation (WI-073) are harmless without filtering.
