# Work Items: Tiered AoI-Aware Shadow Replication

Design doc: `docs/mesh-architect/designs/010-tiered-aoi-shadow-replication.md`

All file paths are relative to project root `/Users/dz/sources/ZZServerMeshing/`.

---

### WI-100: Add PEER_AOI_RADIUS_SQ constant
**Size:** small
**Depends on:** none
**Files:** `crates/shared/src/spatial.rs`
**Description:**
Add a pre-computed squared peer AoI radius constant for distance checks, avoiding sqrt at runtime.

Add after the existing `AOI_RADIUS_SQ` constant (line 109):

```rust
/// Pre-computed squared peer AoI radius for hot-tier distance checks in peer replication.
pub const PEER_AOI_RADIUS_SQ: f32 = PEER_AOI_RADIUS * PEER_AOI_RADIUS;
```

**Tests:** Add a test verifying `PEER_AOI_RADIUS_SQ == 562500.0` (750^2). Add alongside the existing `peer_aoi_radius_is_750` test.

**Done when:** `PEER_AOI_RADIUS_SQ` exists in `shared/spatial.rs`, test passes.

---

### WI-101: Update ReplicationConfig with full_snapshot_enabled flag
**Size:** small
**Depends on:** none
**Files:** `crates/sim-server/src/resources.rs`
**Description:**
1. Change `FULL_REPL_INTERVAL` default from 20 to 600 (30 seconds at 20Hz).
2. Add `full_snapshot_enabled: bool` field to `ReplicationConfig`.
3. Update `Default` impl: set `full_interval: 600` and `full_snapshot_enabled: false`.
4. Update the doc comment on `FULL_REPL_INTERVAL` to say "600 ticks = 30 seconds at 20Hz".
5. Update the doc comment on `ReplicationConfig` to explain the new field.

The existing `resources_default_initialization` test (line 311) does not assert specific ReplicationConfig field values, so it should not break. But verify.

**Tests:**
- Add test: `ReplicationConfig::default().full_interval == 600`
- Add test: `ReplicationConfig::default().full_snapshot_enabled == false`
- Add test: construct ReplicationConfig with `full_snapshot_enabled: true` and verify

**Done when:** `ReplicationConfig` has both fields, defaults are correct, tests pass. `cargo build` succeeds.

---

### WI-102: Update CLI flags for full snapshot control
**Size:** small
**Depends on:** WI-101
**Files:** `crates/sim-server/src/main.rs`
**Description:**
1. Change `--full-repl-interval` default from 20 to 600.
2. Update the help text for `--full-repl-interval` to say "Default: 600 (30 seconds at 20Hz). Only used when --full-snapshot is enabled."
3. Add `--full-snapshot` boolean flag (clap: `#[arg(long, default_value_t = false)]`). Help text: "Enable periodic full snapshots to all peers. Disabled by default (cold tier at 5Hz provides sufficient coverage)."
4. Pass both values when constructing `ReplicationConfig` in `run_game_loop_ecs` call (line 189).
5. Update the `run_game_loop_ecs` function signature to accept `full_snapshot_enabled: bool`.
6. In `run_game_loop_ecs`, update the `ReplicationConfig` insertion (line 152-154) to set `full_snapshot_enabled`.

**Tests:** Manual: `cargo run -p sim-server -- --help` shows both flags with correct defaults.

**Done when:** Both CLI flags work. `cargo build` succeeds. Running without flags uses new defaults (full_snapshot_enabled=false, full_interval=600).

---

### WI-103: Add EntityKind to shadow query in prepare_replication
**Size:** medium
**Depends on:** WI-100
**Files:** `crates/sim-server/src/systems.rs`
**Description:**
Modify the `prepare_replication` system (line 1112) to add `EntityKind` to the shadow query.

Current shadow query (line 1129):
```rust
shadow_query: Query<(&AuthorityServer, &Position), With<Shadow>>,
```

Change to:
```rust
shadow_query: Query<(&AuthorityServer, &Position, &EntityKind), With<Shadow>>,
```

Then modify the shadow position collection loop (lines 1154-1162) to build TWO maps:
1. `peer_player_positions: HashMap<u16, Vec<Vec3>>` -- only Player shadows
2. `peer_all_positions: HashMap<u16, Vec<Vec3>>` -- all shadows (for PeerBounds fallback)

```rust
let mut peer_player_positions: std::collections::HashMap<u16, Vec<Vec3>> =
    std::collections::HashMap::new();
let mut peer_all_positions: std::collections::HashMap<u16, Vec<Vec3>> =
    std::collections::HashMap::new();

for (auth, pos, kind) in shadow_query.iter() {
    if auth.0 == config.server_id {
        continue;
    }
    let sid = auth.0.0;
    peer_all_positions.entry(sid).or_default().push(pos.0);
    if kind.0 == shared::entity::EntityType::Player {
        peer_player_positions.entry(sid).or_default().push(pos.0);
    }
}
```

At this stage, keep the rest of the function unchanged -- still use `peer_all_positions` for `PeerBounds` computation. The behavioral change (using player positions for hot/cold) happens in WI-104.

**Tests:** All existing `peer_repl_*` tests must continue to pass unchanged. The shadow query now returns an extra field but the filtering logic is the same. Run `cargo test -p sim-server -- peer_repl` to verify.

**Done when:** Shadow query includes EntityKind. Both `peer_player_positions` and `peer_all_positions` maps are built. All existing tests pass. No behavioral change yet.

---

### WI-104: Implement tiered hot/cold replication logic
**Size:** large
**Depends on:** WI-101, WI-103
**Files:** `crates/sim-server/src/systems.rs`
**Description:**
This is the core change. Modify `prepare_replication` to implement tiered replication.

Replace the current filtering loop (lines 1196-1214) with tiered logic:

```rust
let is_cold_tick = tick.0 % 4 == 0;

outbound.full_snapshot = None;
outbound.per_peer.clear();

for (&peer_id, all_positions) in &peer_all_positions_map {
    let player_positions = peer_player_positions.get(&peer_id);

    let relevant: Vec<EntityState> = match player_positions {
        Some(players) if !players.is_empty() => {
            // Tiered: use player positions for hot/cold classification
            entities.iter().filter(|e| {
                let is_hot = players.iter().any(|pp| {
                    let dx = e.position.x - pp.x;
                    let dz = e.position.z - pp.z;
                    (dx * dx + dz * dz) <= shared::spatial::PEER_AOI_RADIUS_SQ
                });
                is_hot || is_cold_tick
            }).cloned().collect()
        }
        _ => {
            // Fallback: no player shadows for this peer.
            // Use PeerBounds from all shadows (current behavior).
            if let Some(bounds) = peer_bounds.get(&peer_id) {
                entities.iter()
                    .filter(|e| bounds.contains(e.position))
                    .cloned()
                    .collect()
            } else {
                continue; // no bounds at all, skip
            }
        }
    };

    outbound.per_peer.insert(peer_id, relevant);
}
```

Important details:
1. The `peer_bounds` HashMap must still be computed from `peer_all_positions` for the fallback path. Keep the existing `PeerBounds::from_positions` call but build it from `peer_all_positions_map` (renamed from `peer_positions`).
2. The full snapshot condition (line 1139) must check `repl_config.full_snapshot_enabled`:
   ```rust
   let is_full_tick = repl_config.full_snapshot_enabled && tick.0 % repl_config.full_interval == 0;
   ```
3. The "no peers" and "empty entities" checks (lines 1140-1151) remain unchanged.
4. The "peer_bounds.is_empty()" early return (line 1191) stays. If ALL peers have no shadows, we skip. But we also need to handle the case where peer_player_positions has entries even when peer_bounds might not (unlikely but defensive).
5. The debug logging at tick % 100 (line 1173) can be updated to log player count vs total shadow count per peer.

**Tests:**

1. `tiered_hot_only_tick_sends_nearby`:
   - Server 1 auth entities at (0,0) and (4000,0,0)
   - Peer 2 with a Player shadow at (100,0,0)
   - Tick 1 (not cold: 1 % 4 != 0)
   - Verify per_peer[2] contains only the entity at (0,0)

2. `tiered_cold_tick_sends_all`:
   - Same setup
   - Tick 4 (cold: 4 % 4 == 0)
   - Verify per_peer[2] contains BOTH entities

3. `tiered_player_vs_npc_shadow`:
   - Peer 2 has Player shadow at (0,0) and NPC shadow at (3000,0,0)
   - Auth entity at (2800,0,0)
   - Tick 1 (not cold)
   - Verify: entity at (2800,0,0) is NOT in per_peer[2] (it's near the NPC shadow but far from the player)
   - This confirms player-based filtering is more precise than bounding-box

4. `tiered_no_player_shadows_falls_back`:
   - Peer 2 has ONLY NPC shadows (no Player type)
   - Verify: falls back to PeerBounds approach (same as current behavior)

5. `tiered_full_snapshot_disabled_by_default`:
   - Default ReplicationConfig (full_snapshot_enabled=false)
   - Tick 600 (would be full_interval hit)
   - Verify full_snapshot is None, per_peer is populated

6. `tiered_full_snapshot_enabled`:
   - ReplicationConfig { full_snapshot_enabled: true, full_interval: 600 }
   - Tick 600
   - Verify full_snapshot is Some, per_peer is empty

7. `tiered_cold_tick_modulo`:
   - Verify tick 4 is cold, tick 5 is not, tick 8 is cold, tick 3 is not

**Done when:** Hot-only ticks send only hot entities. Cold ticks send hot+cold. Player-based distance checks work. PeerBounds fallback works. All new and existing tests pass. `cargo test -p sim-server` green.

---

### WI-105: Update metrics for hot/cold replication counts
**Size:** small
**Depends on:** WI-104
**Files:** `crates/sim-server/src/metrics.rs`, `crates/sim-server/src/game_loop.rs`
**Description:**

1. In `metrics.rs`, add fields to `MetricsCollector`:
   ```rust
   pub replication_hot_count: usize,
   pub replication_cold_count: usize,
   ```
   Initialize both to 0 in `MetricsCollector::new()`.

2. Add the same fields to `MetricsSnapshot`:
   ```rust
   pub replication_hot_count: usize,
   pub replication_cold_count: usize,
   ```
   Populate them in `snapshot()`.

3. In `game_loop.rs`, Phase 6 (metrics recording, line 463), update the replication metrics:
   - When `repl_full.is_some()`: set both hot and cold to the full entity count (it is a full snapshot, all are sent)
   - When using `repl_per_peer`:
     - On cold ticks (tick % 4 == 0): `replication_cold_count = total sent count`, `replication_hot_count = 0` (we cannot distinguish hot from cold in the merged list without extra tracking, so report the full sent count as "cold" since cold ticks include everything)
     - On non-cold ticks: `replication_hot_count = total sent count`, `replication_cold_count = 0`

   Alternative (more precise): have `prepare_replication` also write hot_count and cold_count into `OutboundReplication`. This requires adding two fields to the resource. Choose the simpler approach first.

   Simpler approach: add `hot_count: usize` and `cold_count: usize` to `OutboundReplication`. Set them in `prepare_replication`:
   ```rust
   outbound.hot_count = 0;
   outbound.cold_count = 0;
   // ... in the tiered loop:
   //   count hot entities
   //   count cold entities (on cold ticks)
   ```
   Then in the game loop, read them from the resource.

**Tests:**
- Add test: `MetricsCollector::new(1)` has `replication_hot_count == 0` and `replication_cold_count == 0`
- Add test: verify `MetricsSnapshot` serializes the new fields to JSON

**Done when:** Metrics endpoint shows `replication_hot_count` and `replication_cold_count`. `cargo test -p sim-server` passes.

---

### WI-106: Update OutboundReplication with hot/cold tracking fields
**Size:** small
**Depends on:** WI-104
**Files:** `crates/sim-server/src/resources.rs`, `crates/sim-server/src/systems.rs`
**Description:**

Add tracking fields to `OutboundReplication` for metrics (used by WI-105):

```rust
pub struct OutboundReplication {
    pub per_peer: HashMap<u16, Vec<EntityState>>,
    pub full_snapshot: Option<Vec<EntityState>>,
    pub tick: u64,
    /// Count of hot-tier entities sent this tick (across all peers).
    pub hot_count: usize,
    /// Count of cold-tier entities sent this tick (across all peers, 0 on non-cold ticks).
    pub cold_count: usize,
}
```

Update `Default` impl to initialize both to 0.

In `prepare_replication` (WI-104), set these counters as entities are classified:
- In the tiered path: increment hot_count for each hot entity, cold_count for each cold entity (on cold ticks)
- In the PeerBounds fallback path: add the count to hot_count (since we cannot distinguish)
- On full snapshot: set both to 0 (the full snapshot path sets full_snapshot, not per_peer)

Update the `resources_default_initialization` test to verify the new fields default to 0.

**Tests:**
- Existing `resources_default_initialization` test updated to check `hot_count == 0` and `cold_count == 0`
- Add test: verify hot_count and cold_count are set correctly after prepare_replication runs with tiered logic

**Done when:** OutboundReplication has hot_count and cold_count. prepare_replication sets them. Tests pass.

---

### WI-107: Update existing tests for new ReplicationConfig defaults
**Size:** small
**Depends on:** WI-101, WI-104
**Files:** `crates/sim-server/src/systems.rs`, `crates/sim-server/src/game_loop.rs`
**Description:**

Several existing tests rely on the old full_interval default of 20. These need updating:

1. Tests that use `tick % 20 == 0` as the full tick trigger need to account for the new behavior where `full_snapshot_enabled` defaults to false. On tick 20 with the new defaults, `is_full_tick` is false (because `full_snapshot_enabled` is false), so `full_snapshot` is None and `per_peer` is populated instead.

2. Specifically review and fix:
   - `peer_repl_full_tick_sends_all` (line 6657): currently sets tick to 20 expecting full_snapshot. With new defaults (full_snapshot_enabled=false), tick 20 is not a full tick. Fix: either explicitly set `full_snapshot_enabled: true` in the test's ReplicationConfig, or adjust the assertion.
   - `peer_repl_no_shadows_sends_full` (line 6718): tests tick 20 as full tick. Same fix needed.
   - `peer_repl_full_interval_1_disables_filtering` (line 6841): sets full_interval=1. This test also needs `full_snapshot_enabled: true` for the full snapshot to trigger.
   - Any test in `game_loop.rs` that relies on the default full_interval.

3. The `setup_world` helper (line 557) uses `world.init_resource::<ReplicationConfig>()` which gets the Default. The new Default has `full_snapshot_enabled: false`. Most tests should work because they test non-full ticks. But verify ALL `prepare_replication` tests.

Strategy: for tests that specifically test full-snapshot behavior, insert an explicit `ReplicationConfig { full_interval: 20, full_snapshot_enabled: true }` resource. For all other tests, the new default is correct.

**Tests:** Run `cargo test -p sim-server` and fix all failures. No new test logic needed; this is purely updating existing tests for the new defaults.

**Done when:** All existing tests pass with the new ReplicationConfig defaults. `cargo test -p sim-server` is green.

---

### WI-108: Add spawn_shadow_player test helper
**Size:** small
**Depends on:** none
**Files:** `crates/sim-server/src/systems.rs`
**Description:**

The existing `spawn_shadow_npc` helper creates NPC shadow entities for tests. The tiered replication tests need Player shadow entities. Add a `spawn_shadow_player` helper near the existing `spawn_shadow_npc` (find it in the test module).

```rust
fn spawn_shadow_player(world: &mut World, pos: Vec3, authority_server: u16) -> EntityId {
    let state = EntityState::new_player(pos, ServerId(authority_server));
    let id = state.id;
    spawn_from_entity_state_direct(world, &state, false, Some(world.resource::<TickCounter>().0));
    id
}
```

This mirrors `spawn_shadow_npc` but creates a Player entity type.

**Tests:** Add a trivial test verifying the helper creates a shadow entity with EntityKind == Player.

**Done when:** `spawn_shadow_player` helper exists and is usable in tiered replication tests. Test passes.

---

### WI-109: Integration test -- tiered replication across full tick cycle
**Size:** medium
**Depends on:** WI-104, WI-106, WI-108
**Files:** `crates/sim-server/src/systems.rs`
**Description:**

Add a comprehensive integration test that runs `prepare_replication` across a full 4-tick cycle and verifies the hot/cold behavior.

Setup:
- Server 1 (our server) with 5 auth entities:
  - Entity A at (0, 0, 0) -- near peer 2's player
  - Entity B at (100, 0, 0) -- near peer 2's player
  - Entity C at (3000, 0, 3000) -- near peer 3's player
  - Entity D at (4500, 0, 0) -- far from all players (cold)
  - Entity E at (-4500, 0, -4500) -- far from all players (cold)
- Peer 2: Player shadow at (50, 0, 0)
- Peer 3: Player shadow at (3100, 0, 3100)

Run schedule for ticks 1, 2, 3, 4:

**Tick 1 (hot only):**
- per_peer[2] should contain A, B (within 750 of peer 2's player at 50,0,0)
- per_peer[3] should contain C (within 750 of peer 3's player at 3100,0,3100)
- D and E should NOT be in any per_peer list
- hot_count should be 3 (A+B for peer 2, C for peer 3)
- cold_count should be 0

**Tick 2, 3 (hot only):** Same as tick 1

**Tick 4 (cold tick):**
- per_peer[2] should contain A, B, D, E (hot + cold within PeerBounds-like range, but actually ALL entities since cold means "not hot" and they are sent regardless of bounding box)
  Wait -- clarify: cold entities are ALL authoritative entities that are NOT hot. They are sent to ALL peers on cold ticks? Or only to peers whose PeerBounds covers them?

  Per the design: cold entities are sent to each peer regardless of distance. The cold tier replaces the full snapshot for crash recovery. Every peer needs to know about every entity eventually.

  Revised: on cold ticks, for each peer:
  - Hot entities: always included
  - Cold entities: ALL remaining auth entities included (not filtered by distance)

  This is simpler and matches the full-snapshot-replacement purpose.

  Actually, re-reading the design more carefully: cold entities are "all other authoritative entities not near any peer player." On cold ticks, the per_peer list for each peer should include ALL authoritative entities (hot + cold), effectively becoming a full replication to that specific peer.

  Implementation note: on cold ticks, the filter becomes `is_hot || is_cold_tick`, and since `is_cold_tick` is true, every entity passes. This means on cold ticks, per_peer[X] = all auth entities. This is correct and simple.

**Tick 4 assertions:**
- per_peer[2] should contain ALL 5 entities
- per_peer[3] should contain ALL 5 entities
- hot_count should be 3 (same 3 hot entities)
- cold_count should be 2 (D and E are cold, sent to both peers = 4 total, but count is per-entity not per-peer)

Actually, define the counting more precisely:
- hot_count: sum across all peers of hot entities sent
- cold_count: sum across all peers of cold entities sent

For tick 4: hot_count = 2 (to peer 2) + 1 (to peer 3) = 3. cold_count = 3 (to peer 2) + 4 (to peer 3) = 7. Wait, that double-counts.

Simpler: hot_count = total unique hot entities across all peers. cold_count = total unique cold entities. Or just total entities across all peers. The exact counting definition should be determined during implementation. For the test, assert that cold ticks produce larger per_peer lists than hot-only ticks.

**Tests:** This IS the test. Verify all assertions above.

**Done when:** Integration test passes, confirming hot-only and cold-tick behavior across a 4-tick cycle.

---

### WI-110: Scene test -- tiered replication on live cluster
**Size:** medium
**Depends on:** WI-104, WI-105
**Files:** `tests/scenarios/` or equivalent test script location
**Description:**

Write a scene test that runs against a live 3-server cluster and verifies tiered replication behavior via the metrics endpoint.

Test plan:
1. Start 3-server cluster
2. Spawn entities at spread-out positions (some near origin, some far)
3. Wait for replication to stabilize (~2-3 seconds)
4. Poll `/metrics` on each server
5. Verify:
   - `replication_mode` is "filtered" (not "full")
   - `replication_hot_count` < `replication_full_count` on most polls
   - `replication_cold_count` > 0 on approximately 1/4 of polls (cold ticks)
   - `replication_sent_count` is reasonable (not zero, not full count every tick)

This is a smoke test, not a precise assertion. The goal is to verify the feature is active and producing visible metrics differences.

**Tests:** This IS the test.

**Done when:** Scene test passes against a local cluster. Metrics show hot/cold split.

---

## Implementation Order

```
WI-100  (PEER_AOI_RADIUS_SQ)          ─┐
WI-101  (ReplicationConfig changes)    ─┤─→  WI-102 (CLI flags)
WI-108  (spawn_shadow_player helper)   ─┤
                                        ├─→  WI-103 (EntityKind in shadow query)
                                        │         │
                                        │         v
                                        └──→ WI-104 (Tiered hot/cold logic)  ←── core change
                                                  │
                                        ┌─────────┼──────────┐
                                        v         v          v
                                   WI-106    WI-107     WI-105
                                 (tracking) (fix tests) (metrics)
                                        │                   │
                                        v                   v
                                   WI-109              WI-110
                               (integration)       (scene test)
```

Parallelizable: WI-100, WI-101, WI-108 can all be done simultaneously.
Critical path: WI-100 -> WI-103 -> WI-104 -> WI-107 (must be done in order).
WI-105, WI-106, WI-109, WI-110 can be done after WI-104 in any order.

## Total estimated effort

| WI | Size | Estimate |
|----|------|----------|
| 100 | small | 15 min |
| 101 | small | 20 min |
| 102 | small | 20 min |
| 103 | medium | 45 min |
| 104 | large | 2-3 hours |
| 105 | small | 30 min |
| 106 | small | 20 min |
| 107 | small | 30 min |
| 108 | small | 15 min |
| 109 | medium | 1 hour |
| 110 | medium | 1 hour |
| **Total** | | **~7-8 hours** |
