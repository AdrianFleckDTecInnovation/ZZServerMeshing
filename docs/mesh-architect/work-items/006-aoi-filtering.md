# Work Items: 006 AoI Filtering for Server-to-Client Snapshots

Design: `docs/mesh-architect/designs/006-aoi-filtering.md`

## Phase 1: World Expansion

### WI-060: Expand WORLD_EXTENT from 500 to 5000
**Size:** small
**Depends on:** none
**Files:** `crates/shared/src/spatial.rs`
**Description:**
Change `WORLD_EXTENT` from `500.0` to `5000.0`. Update all existing unit tests in the same file that reference the old value (the `clamp_exceeds_positive`, `clamp_exceeds_negative`, and `clamp_at_exact_boundary` tests). The tests should assert against `WORLD_EXTENT` the constant, not hardcoded `500.0`, so they pass regardless of value.

Add two new tests:
- `clamp_at_new_extent`: position at `(5000.0, 0.0, -5000.0)` is unchanged
- `clamp_beyond_new_extent`: position at `(6000.0, 0.0, -6000.0)` is clamped to `(5000.0, 0.0, -5000.0)`

**Tests:** Update existing spatial.rs tests, add 2 new tests
**Done when:** `cargo test -p shared` passes, `WORLD_EXTENT == 5000.0`

### WI-061: Add AOI_RADIUS constant and in_aoi helper
**Size:** small
**Depends on:** WI-060
**Files:** `crates/shared/src/spatial.rs`
**Description:**
Add these to `spatial.rs`:

```rust
/// Area-of-Interest radius for client snapshots. Entities within this distance
/// of a client's player entity are included in their snapshot.
pub const AOI_RADIUS: f32 = 500.0;

/// Pre-computed squared AoI radius (avoids sqrt in distance checks).
pub const AOI_RADIUS_SQ: f32 = AOI_RADIUS * AOI_RADIUS;

/// Check if an entity position is within the Area of Interest of a player.
/// Uses squared 2D distance on the XZ plane (Y is ignored -- flat world).
pub fn in_aoi(player_pos: Vec3, entity_pos: Vec3) -> bool {
    let dx = player_pos.x - entity_pos.x;
    let dz = player_pos.z - entity_pos.z;
    (dx * dx + dz * dz) <= AOI_RADIUS_SQ
}
```

Add unit tests:
- `in_aoi_same_position`: true
- `in_aoi_inside_radius`: entity 300 units away, true
- `in_aoi_exactly_on_boundary`: entity exactly 500 units away on X axis, true (uses `<=`)
- `in_aoi_just_outside`: entity 501 units away, false
- `in_aoi_far_away`: entity 3000 units away, false
- `in_aoi_negative_coords`: player at (-200, 0, -300), entity at (-600, 0, -300), dist=400, true
- `in_aoi_diagonal`: player at origin, entity at (354, 0, 354), dist=500.6, false
- `in_aoi_ignores_y`: player at (0, 0, 0), entity at (0, 9999, 0), true (Y is ignored)

**Tests:** 8 new unit tests in spatial.rs
**Done when:** `cargo test -p shared` passes, `in_aoi` works correctly

### WI-062: Update client visuals for 10k world
**Size:** small
**Depends on:** WI-060
**Files:** `client/src/renderer.ts`
**Description:**
Update the renderer to match the new world size:

1. Ground plane geometry: `PlaneGeometry(1000, 1000)` -> `PlaneGeometry(10000, 10000)`
2. GridHelper: `GridHelper(1000, 20, ...)` -> `GridHelper(10000, 20, ...)` -- grid lines every 500 units
3. Bounds box: `BoxGeometry(1000, 10, 1000)` -> `BoxGeometry(10000, 10, 10000)`
4. Camera far plane: `PerspectiveCamera(60, ..., 0.1, 2000)` -> `PerspectiveCamera(60, ..., 0.1, 20000)`
5. Camera initial position: `camera.position.set(0, 200, 200)` -> `camera.position.set(0, 2000, 2000)`
6. Camera maxDistance: `this.controls.maxDistance = 1000` -> `this.controls.maxDistance = 10000`

Do NOT change entity geometry sizes (sphere radius 4, box 6x10x6) -- entities should look the same size, just in a bigger world.

**Tests:** Manual verification -- open client, see larger world, zoom works
**Done when:** Client shows 10k x 10k world with correct ground plane, grid, and bounds

### WI-063: Update docs for new world extent
**Size:** small
**Depends on:** WI-060
**Files:** `CLAUDE.md`, `.claude/rules/llm-directions.md`
**Description:**
Update all references to world bounds:

In `CLAUDE.md`:
- Change "World bounds: -500 to 500 on X and Z axes" to "World bounds: -5000 to 5000 on X and Z axes"

In `.claude/rules/llm-directions.md`:
- Update the Key Constants table: `WORLD_EXTENT` row from `500` to `5000`, update description "bounds are [-5000, 5000]"
- Add `AOI_RADIUS` row: value `500`, location `shared/spatial.rs`, description "client AoI filter radius"

**Tests:** none (documentation only)
**Done when:** All doc references to world bounds are updated

---

## Phase 2: Test Infrastructure

### WI-064: Add spawnCluster and spawnAtPositions test helpers
**Size:** small
**Depends on:** WI-060
**Files:** `tools/test-suite/types.ts`, `tools/test-suite/runner.ts`
**Description:**
Add two new methods to `TestContext`:

```typescript
// In types.ts, add to TestContext interface:

/** Spawn `count` entities clustered within `radius` of `center`. */
spawnCluster(count: number, center: [number, number, number], radius: number, options?: Omit<SpawnOptions, 'position'>): Promise<void>;

/** Spawn entities at exact positions. */
spawnAtPositions(positions: [number, number, number][], options?: Omit<SpawnOptions, 'position'>): Promise<void>;
```

Implement in `runner.ts`:

`spawnCluster(count, center, radius, options)`:
- For each entity, compute a random position within `radius` of `center` (uniform random in circle)
- Call the existing spawn logic with that position

`spawnAtPositions(positions, options)`:
- For each position in the array, call the existing spawn logic with that exact position

Both use the same round-robin server targeting as the existing `spawn` method.

Also update the existing `spawn` method's default random range from `800` to `WORLD_EXTENT * 0.8`. Since this is a TypeScript file without access to the Rust constant, hardcode `8000` (5000 * 0.8 * 2) or better: change the spread to `(Math.random() - 0.5) * 8000` so entities spread across the larger world.

**Tests:** Use both helpers in a quick smoke test or verify manually that they work
**Done when:** `spawnCluster` and `spawnAtPositions` exist in TestContext and compile

### WI-065: Update test scenarios for 5000-unit world
**Size:** medium
**Depends on:** WI-064
**Files:** `tools/test-suite/scenarios/*.ts`, `tools/test-suite/runner.ts`
**Description:**
Several test scenarios hardcode positions or assumptions about the 500-unit world. Update them:

1. **spawn-burst.ts**: Uses `ctx.spawn(batch)` which uses the default random range. After WI-064 updates the default range to 8000, spawns will spread across the larger world. The scenario should still work -- it measures tick budget, not spatial proximity. No change needed unless failures occur.

2. **spatial-hotspot.ts**: `HOTSPOT_CENTER = { x: 100, z: 100 }` and `HOTSPOT_SIZE = 50`. These are still valid -- a cluster near origin. No change needed.

3. **authority-transfer.ts**: Spawns at `(Math.random() - 0.5) * 400`. This puts entities in a small area near origin. Still valid. No change needed.

4. **rebalance-storm.ts**: Spawns with `(Math.random() - 0.5) * 400`. Same as above. No change needed.

5. **entity-lifecycle.ts**: Spawns at a specific `spawnPos`. Check the value. If hardcoded, ensure it's near origin so the client can see it.

6. **input-seq-echo.ts**: Spawns at `[50, 0, 50]`. Near origin, fine.

7. **input-latency.ts**: Spawns at `[0, 0, 0]`. Fine.

8. **mesh-verify.ts**: Check what positions it uses. Ensure entities spawn within AoI radius of the client player.

9. **peer-replication.ts**: Check positions used.

10. **forwarded-input.ts**: Check positions used.

11. **multi-client.ts**: Check positions used.

12. **smoothness.ts**: Check positions used.

For any scenario that spawns entities and then checks them in a client snapshot, ensure entities are spawned within 500 units of the client's player position (origin). Use `spawnCluster(n, [0, 0, 0], 200)` where appropriate.

Review each scenario file. If a scenario uses `ctx.spawn()` with no position and relies on seeing those entities in client snapshots, switch to `ctx.spawnCluster(n, [0, 0, 0], 200)` so entities are guaranteed within AoI. Scenarios that only check server metrics (spawn-burst, steady-scale) don't need client-visible entities and can use the default wide spread.

**Tests:** Run `./test.sh` to verify all scenarios pass after changes
**Done when:** All test scenarios pass with WORLD_EXTENT=5000, even before AoI filter is enabled

---

## Phase 3: AoI Core Logic

### WI-066: Add AoI filtering to prepare_client_snapshot
**Size:** medium
**Depends on:** WI-061, WI-065
**Files:** `crates/sim-server/src/systems.rs`
**Description:**
Modify `prepare_client_snapshot` to filter entities per-client based on AoI distance from the client's player entity. The system stays as a regular Bevy system (no exclusive access).

**Implementation steps:**

1. After building the `entities: Vec<EntityState>` list (unchanged), build a player position lookup:

```rust
use shared::spatial::{in_aoi, AOI_RADIUS_SQ};

// Build player -> position map from entity list.
// Only need positions for connected clients' player entities.
let mut player_positions: HashMap<EntityId, Vec3> = HashMap::new();
for (_client_id, handle) in registry.clients.iter() {
    if let Some(pid) = handle.player_id {
        for e in &entities {
            if e.id == pid {
                player_positions.insert(pid, e.position);
                break;
            }
        }
    }
}
```

2. The shared full snapshot (Arc) is still built from ALL entities for backward compat (ClientSnapshot resource used by schedule tests). Keep the existing line:

```rust
let full_frame = Arc::new(encode_binary_snapshot(tick.0, &entities));
snapshot.frame = Some((*full_frame).clone());
```

3. Modify the force_full probe: the existing probe checks `change_ratio` against the global entity set. This remains valid as a conservative heuristic. Keep it unchanged.

4. In the per-client loop, filter entities before processing:

```rust
for (_client_id, handle) in registry.clients.iter_mut() {
    // Determine AoI-filtered entity set for this client.
    let filtered: Vec<&EntityState> = if let Some(pid) = handle.player_id {
        if let Some(&player_pos) = player_positions.get(&pid) {
            entities.iter().filter(|e| {
                e.id == pid || in_aoi(player_pos, e.position)
            }).collect()
        } else {
            // Player position not found (entity not spawned yet) -- send everything
            entities.iter().collect()
        }
    } else {
        // No player assigned yet -- send everything
        entities.iter().collect()
    };

    // Convert filtered refs to owned for encoding/delta functions
    // (reuse existing functions that take &[EntityState])
    let client_entities: Vec<EntityState> = filtered.iter().map(|e| (*e).clone()).collect();

    // ... rest of the per-client logic uses `client_entities` instead of `entities`
}
```

5. Full snapshot path: use `ClientFrame::Owned(encode_binary_snapshot(tick.0, &client_entities))` instead of `ClientFrame::Shared(Arc::clone(&full_frame))`.

6. Delta path: `compute_delta(&client_entities, baseline)` instead of `compute_delta(&entities, baseline)`.

7. Baseline build: `build_baseline(&client_entities)` instead of `build_baseline(&entities)`.

8. The per-client `change_ratio` check within the delta path uses `client_entities.len()` as the denominator (already does this since it uses `entities.len()` which should now be `client_entities.len()`).

**Important: The `client_entities` clone is necessary because `compute_delta`, `build_baseline`, and `encode_binary_snapshot` all take `&[EntityState]`. An optimization to avoid this clone is possible (take `&[&EntityState]` variants) but is a separate work item for later. The clone cost is O(E_aoi) per client per tick, where E_aoi is the filtered count -- much smaller than the full entity count.**

**Tests:** Add to the existing `prepare_client_snapshot` tests in systems.rs:
- `snapshot_filters_by_aoi`: Create a world with 3 entities at (0,0,0), (100,0,0), and (3000,0,0). Register a client with player at (0,0,0). Run prepare_client_snapshot. Verify client frame contains 2 entities (the first two), not 3.
- `snapshot_always_includes_player`: Player at (0,0,0), another entity at (3000,0,0). Both should be in the client's frame because the player's own entity is always included.
- `snapshot_no_player_sends_all`: Client with no player_id set. All entities should be sent.
- `snapshot_all_within_aoi_sends_all`: All entities within 500 units. Frame should contain all of them.

**Done when:** `cargo test -p sim-server` passes with all new AoI tests, `cargo build` succeeds

### WI-067: Verify AoI + delta compression interaction
**Size:** small
**Depends on:** WI-066
**Files:** `crates/sim-server/src/systems.rs` (tests only)
**Description:**
Add integration tests that verify AoI and delta compression work correctly together, specifically for the failure modes that broke the previous implementation.

Add these tests to `systems.rs`:

1. `aoi_entity_leaving_produces_removal`:
   - Tick 1: Client player at origin, entity at (400, 0, 0) -- within AoI. Full snapshot sent, baseline established.
   - Move entity to (3000, 0, 0) -- outside AoI.
   - Tick 2: Run prepare_client_snapshot. The delta should contain a removal for the entity.
   - Verify the delta frame has removal_count=1 for that entity.

2. `aoi_entity_entering_produces_update`:
   - Tick 1: Client player at origin, entity at (3000, 0, 0) -- outside AoI. Full snapshot sent (entity not in baseline).
   - Move entity to (100, 0, 0) -- inside AoI.
   - Tick 2: Run prepare_client_snapshot. The delta should contain the entity as an update (new).
   - Verify the delta frame has update_count=1.

3. `aoi_boundary_no_flicker`:
   - Entity at exactly 500 units from player. Run 5 ticks. Entity should be consistently included (no removal then re-add).
   - This tests that the boundary condition is stable when the entity doesn't move.

4. `aoi_full_snapshot_is_filtered`:
   - Force a full snapshot (set baseline to None or ticks_since_full to FULL_SNAPSHOT_INTERVAL).
   - Verify the full snapshot only contains AoI-visible entities, not the global set.

**Tests:** 4 new integration tests
**Done when:** All tests pass, confirming no flicker from AoI transitions

---

## Phase 4: End-to-End Verification

### WI-068: Add aoi-filtering test scenario
**Size:** medium
**Depends on:** WI-066
**Files:** `tools/test-suite/scenarios/aoi-filtering.ts`, `tools/test-suite/runner.ts`
**Description:**
Create a new test scenario that verifies AoI filtering end-to-end through the full stack (sim-server -> WebSocket -> test client).

Register the scenario in `runner.ts` alongside the other scenarios.

Scenario logic:
1. Connect a client (player spawns at origin).
2. Wait for initial snapshot.
3. Spawn 10 entities near origin using `ctx.spawnCluster(10, [0, 0, 0], 100)`.
4. Spawn 10 entities far away using `ctx.spawnCluster(10, [3000, 0, 3000], 100)`.
5. Wait 2 seconds for snapshots to stabilize.
6. Read the latest snapshot from the client.
7. Count entities in the snapshot. Should be ~11 (10 near + 1 player). The 10 far entities should be absent.
8. Verify the far entities' IDs are NOT in the snapshot.
9. Check that the player entity IS in the snapshot.

Pass criteria:
- Client sees 11 entities (within tolerance of +/-2 for timing/spawning race)
- No far entities visible
- Player always visible

**Tests:** This IS the test
**Done when:** `./test.sh aoi-filtering` passes locally

### WI-069: Verify on UNO Q boards
**Size:** small
**Depends on:** WI-068
**Files:** none (deployment + manual verification)
**Description:**
Deploy the complete AoI implementation to the UNO Q boards and verify:

1. `./start-unoq.sh` -- cross-compile and deploy
2. Run test suite against boards: verify all existing scenarios still pass
3. Run `aoi-filtering` scenario against boards
4. Open client in browser, connect to board mesh:
   - Spawn entities near the player -- they appear
   - Spawn entities far from the player (click at world edge) -- they should NOT appear
   - Move the player toward distant entities -- they should appear as the player enters their AoI
5. Check metrics on the boards: `curl http://<board-ip>:8101/metrics | jq .`
   - Verify tick budget is healthy
   - Compare snapshot sizes before and after (with entities spread across the 10k world, snapshots should be smaller per client)

**Tests:** Board-level smoke test
**Done when:** All scenarios pass on boards, visual verification confirms AoI filtering, no flicker

---

## Summary

| WI | Title | Size | Phase | Depends |
|----|-------|------|-------|---------|
| 060 | Expand WORLD_EXTENT to 5000 | small | 1 | -- |
| 061 | Add AOI_RADIUS and in_aoi helper | small | 1 | 060 |
| 062 | Update client visuals for 10k world | small | 1 | 060 |
| 063 | Update docs for new world extent | small | 1 | 060 |
| 064 | Add spawnCluster/spawnAtPositions helpers | small | 2 | 060 |
| 065 | Update test scenarios for 5000-unit world | medium | 2 | 064 |
| 066 | Add AoI filtering to prepare_client_snapshot | medium | 3 | 061, 065 |
| 067 | Verify AoI + delta compression interaction | small | 3 | 066 |
| 068 | Add aoi-filtering test scenario | medium | 4 | 066 |
| 069 | Verify on UNO Q boards | small | 4 | 068 |

**Estimated total: 8-12 hours**

Phases 1-2 (prerequisites) can be deployed and verified independently. Phase 3 (core AoI logic) is the main behavioral change. Phase 4 (verification) confirms everything works end-to-end.
