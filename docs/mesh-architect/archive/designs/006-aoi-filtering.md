# Design 006: Area of Interest (AoI) Filtering for Server-to-Client Snapshots

## Problem Statement

Today every client receives every entity in the world, every tick. With the current 500-unit world (WORLD_EXTENT=500), this is tolerable -- but as we scale the world to 10,000x10,000 units, a player standing at position (0,0) should not receive entities 4,000 units away. The bandwidth and per-client delta computation cost scales linearly with total entity count, not with what the player can see.

A previous AoI implementation failed badly on the UNO Q boards due to tiered update rates causing flicker. This design avoids those mistakes entirely.

### Why this matters

- **Bandwidth:** At 47 bytes per entity, 500 entities = 23 KB per tick per client. With a 10k world and thousands of entities, this becomes untenable for the boards' 100 Mbps ethernet.
- **CPU:** `compute_delta` does O(E) HashMap lookups per client per tick. Filtering reduces E to only nearby entities.
- **Gameplay:** In a 10k world, entities 5,000 units away are irrelevant to the player. Sending them wastes resources on both server and client.

## Design

### Core Principle: Binary In/Out at 500 Units

Every entity within 500 units of the client's player is included in the snapshot every tick. Every entity beyond 500 units is excluded every tick. No tiers, no variable rates, no partial updates.

The player's own entity is always included regardless of distance (prevents the player from disappearing).

```
  Player at (100, 0, 200)
  |
  |-- Entity at (150, 0, 250)  -- dist=70.7  -- INCLUDED (< 500)
  |-- Entity at (400, 0, 600)  -- dist=500.0  -- INCLUDED (= 500)
  |-- Entity at (3000, 0, 3000)-- dist=4101   -- EXCLUDED (> 500)
```

### What Changes

1. **World expansion (prerequisite):** WORLD_EXTENT changes from 500 to 5000. This is the reason AoI matters -- in a 500-unit world everything is within range.

2. **Client visuals (prerequisite):** Ground plane, grid, camera, bounds box all scale 10x.

3. **Test infrastructure (prerequisite):** Test helper gets `spawnCluster` and `spawnAtPositions` so tests can place entities at known distances from the client.

4. **AoI filter in `prepare_client_snapshot`:** After building the full entity list, filter it per-client before encoding. The full list is still built once (needed for peer replication and the shared full-snapshot fallback), but each client gets a filtered view.

### What Does NOT Change

- **Peer replication:** Server-to-server UDP replication remains unfiltered. Every server must know about every entity for crash recovery and authority transfer.
- **Wire format:** No changes to the binary frame format. Same 0x01 full snapshot and 0x02 delta frames.
- **System signature:** `prepare_client_snapshot` stays as a regular Bevy system with Query parameters. No exclusive system access.
- **Delta compression:** The existing delta system works per-client already. With AoI, each client's baseline only tracks entities within their AoI radius, so deltas are naturally smaller.

## Detailed Design

### Distance Calculation

The AoI check is a 2D squared-distance comparison on the XZ plane (Y is always 0 for gameplay):

```rust
const AOI_RADIUS: f32 = 500.0;
const AOI_RADIUS_SQ: f32 = AOI_RADIUS * AOI_RADIUS;

fn in_aoi(player_pos: &Vec3, entity_pos: &Vec3) -> bool {
    let dx = player_pos.x - entity_pos.x;
    let dz = player_pos.z - entity_pos.z;
    (dx * dx + dz * dz) <= AOI_RADIUS_SQ
}
```

No square root, no branching beyond the comparison. Cost: 5 floating-point ops per entity per client.

### Player Position Lookup

Each `ClientHandle` has `player_id: Option<EntityId>` set during `drain_client_events`. To find the player's position within `prepare_client_snapshot` (a regular system), we need to look it up from the entity queries that are already available.

**Approach: Build a small EntityId -> position map from the already-iterated entities.**

The `prepare_client_snapshot` system already iterates all entities to build the `entities: Vec<EntityState>` list. After building this list, we can index positions by EntityId with a simple scan:

```rust
// After building `entities` vec (already O(E) work done):
// Build player position lookup from client registry (only for connected clients)
let mut player_positions: HashMap<EntityId, Vec3> = HashMap::new();
for handle in registry.clients.values() {
    if let Some(pid) = handle.player_id {
        // Find this player in the entities vec
        for e in &entities {
            if e.id == pid {
                player_positions.insert(pid, e.position);
                break;
            }
        }
    }
}
```

With C clients and E entities, worst case is O(C*E) but C is small (typically 1-3 clients per server). We can optimize later if needed, but the typical case is < 5 linear scans over a vec of a few hundred entities -- well under 1ms.

**Why not use EntityLookup + Query?**
EntityLookup maps EntityId -> bevy_ecs::Entity, and we'd need a Query<&Position> to look up the position. The system already has Position in its query tuples but keyed by Bevy Entity, not EntityId. Using EntityLookup would require adding `Res<EntityLookup>` to the system signature and doing an extra query lookup per client. The linear scan over the already-built Vec is simpler and avoids coupling to EntityLookup.

### Integration with Delta Compression

The critical lesson from the previous attempt: **the AoI filter must be applied before delta computation, and the baseline must only track AoI-visible entities.**

When an entity leaves the AoI:
1. It is absent from the client's filtered entity list
2. `compute_delta` sees it in the baseline but not in current -> produces a removal
3. Client receives the removal in the delta frame and removes it cleanly
4. Baseline is updated to remove the entity

When an entity enters the AoI:
1. It appears in the client's filtered entity list
2. `compute_delta` sees it in current but not in baseline -> produces a new entity
3. Client receives the new entity in the delta frame and creates it
4. Baseline is updated to include the entity

This is exactly how entity spawn/despawn already works. AoI transitions are indistinguishable from entity creation/destruction at the protocol level. No special handling needed.

### Full Snapshot Path

The full snapshot (Arc-shared across clients) is currently shared because all clients see the same entity set. With AoI, each client potentially sees a different set. Two options:

**Option A: Per-client full snapshots (chosen)**
When a full snapshot is needed (no baseline, periodic refresh, stale ACK), encode it per-client with only AoI-filtered entities. This uses `ClientFrame::Owned` instead of `ClientFrame::Shared`.

**Option B: Send the shared full snapshot, let delta handle AoI on subsequent ticks.**
Simpler but wastes bandwidth on the full snapshot tick (sends entities outside AoI). The client also briefly sees entities it shouldn't, then they disappear on the next delta. This causes a flash of distant entities every 5 seconds.

Option A is slightly more work but produces correct behavior. The full snapshot is only sent every 100 ticks (5 seconds) or on connect, so per-client encoding cost is amortized.

### The force_full Probe

The existing pre-loop optimization checks `change_ratio` once against any client's baseline and skips `compute_delta` for ALL clients if >80% changed (audit #049 fix). With AoI, different clients might have different baselines (different AoI windows). However, the probe remains valid as a conservative heuristic: if >80% of the global entity set changed, then every client's subset certainly has >80% changes too. The probe remains unchanged.

When force_full fires, each client gets a per-client full snapshot (filtered to their AoI), not the shared global one.

### AOI_RADIUS Constant

Defined in `shared/spatial.rs` alongside `WORLD_EXTENT` and `CELL_SIZE`:

```rust
/// Area-of-Interest radius for client snapshots. Entities within this distance
/// of a client's player entity are included in their snapshot. Beyond this
/// distance, entities are excluded. The player's own entity is always included.
pub const AOI_RADIUS: f32 = 500.0;
```

This means in the current 500-unit world, ALL entities are within AoI of every player (maximum possible distance is ~1414 units diagonal, but most spawns are within 400 units). AoI only starts filtering after the world expansion to 5000 units.

### Per-Tick Cost Analysis

**Before AoI (current):**
- Build entity list: O(E) -- single pass
- Encode full snapshot: O(E) -- single Arc-shared pass
- Per client delta: O(E) HashMap lookups
- Total per tick: O(E) + O(C * E) = O(C * E)

**After AoI:**
- Build entity list: O(E) -- unchanged
- Build player position map: O(C * E) worst case, O(C) best case with small entity counts
- Per client: filter O(E) entities, then delta on O(E_aoi) filtered set
- Total per tick: O(E) + O(C * E) for filtering + O(C * E_aoi) for delta

The filtering pass adds O(C * E) distance checks (5 float ops each). But the delta computation drops from O(C * E) to O(C * E_aoi) HashMap lookups, which is much more expensive per operation. Net effect depends on E_aoi/E ratio:
- At E=500, E_aoi=500 (small world): no change, everything in range
- At E=2000, E_aoi=200 per client: delta cost drops 10x, filter adds 2000*C cheap checks
- At E=5000, E_aoi=100 per client: delta cost drops 50x, filter adds 5000*C cheap checks

The filter is always cheaper than what it saves in delta computation.

### Client-Side Behavior

The client already handles entities appearing and disappearing:
- Full snapshots: `applySnapshot` removes entities not in the incoming set
- Delta frames: `applyDelta` applies removals from the removal list
- Staleness timer (6 seconds): removes ghost entities as a safety net

When an entity leaves AoI, the client removes it. When it enters AoI, the client creates it. This is identical to how server-side entity spawn/despawn already works. No client changes needed for the AoI logic itself.

The client visuals DO need updating for the larger world: ground plane, grid, bounds box, camera far plane, and camera distance.

## Failure Modes

### Player has no position yet
On first connect, the client registers and `player_id` is set, but the player entity may not exist yet (spawn happens on the next tick). In this case, `player_id` has no position in the entities vec. **Fallback: send the full unfiltered entity set.** The player gets one tick of full world data before AoI kicks in. This happens exactly once per client connection.

### Player entity is on another server
If the player's authority was transferred to another server, this server still has a shadow copy. The shadow's position is used for AoI filtering. If the shadow is stale or missing, fallback to unfiltered.

### AoI boundary oscillation
An entity at exactly 500 units could flip in/out on alternating ticks if the player moves slightly. This is fine because:
1. Delta compression handles the add/remove correctly (no flicker -- tested per-entity)
2. The entity is at the edge of visibility anyway
3. A hysteresis band (e.g., include at 500, exclude at 520) could be added later but is not needed for correctness

### Client reconnection
When a client reconnects, they have no baseline. The first frame is a full snapshot filtered to their AoI. This is correct behavior.

## Entity/Protocol Changes

### shared/spatial.rs
- `WORLD_EXTENT`: 500.0 -> 5000.0
- Add: `AOI_RADIUS: f32 = 500.0`
- Add: `AOI_RADIUS_SQ: f32 = AOI_RADIUS * AOI_RADIUS`

### No wire format changes
The binary frame format (0x01 full, 0x02 delta, 47 bytes per entity) is unchanged. Clients decode the same way. They just receive fewer entities.

## Server-Side Implementation

### File: `crates/shared/src/spatial.rs`
- Change `WORLD_EXTENT` from 500.0 to 5000.0
- Add `AOI_RADIUS` and `AOI_RADIUS_SQ` constants
- Add `in_aoi(player_pos: Vec3, entity_pos: Vec3) -> bool` helper function
- Update `clamp_to_world` tests for new bounds

### File: `crates/sim-server/src/systems.rs` (`prepare_client_snapshot`)
- After building `entities` vec, build `player_positions: HashMap<EntityId, Vec3>` from registry + entities
- In the per-client loop, filter entities to AoI before delta computation
- Full snapshot path: encode per-client with filtered entities
- Delta path: compute delta against filtered entity set
- No change to system signature (stays as regular Bevy system)

### File: `crates/sim-server/src/game_loop.rs`
- NPC spawn positions use `WORLD_EXTENT * 0.8` -- this automatically expands with the constant change

## Client-Side Implementation

### File: `client/src/renderer.ts`
- Ground plane: 1000x1000 -> 10000x10000
- GridHelper: 1000 -> 10000 (keep 20 divisions, so grid lines every 500 units)
- Bounds box: 1000x10x1000 -> 10000x10x10000
- Camera far plane: 2000 -> 20000
- Camera initial position: (0, 200, 200) -> (0, 2000, 2000)
- Camera maxDistance: 1000 -> 10000
- Entity sphere/box radius unchanged (entities stay same visual size)

### No protocol changes
`binary-protocol.ts` and `world-view.ts` require no changes. They decode whatever entities the server sends.

## Testing Plan

### Unit Tests (Rust)
- `in_aoi` with entities at various distances (inside, exactly on boundary, outside)
- `in_aoi` with negative coordinates
- `clamp_to_world` with new 5000 extent
- `prepare_client_snapshot` with a registered client whose player is at a known position, entities at known distances

### Scenario Tests
- **aoi-filtering:** Spawn entities at known positions (some near origin, some far away). Connect a client whose player spawns at origin. Verify the client only receives nearby entities. Verify entity count in snapshot matches expected AoI count.
- Update existing scenarios that assume 500-unit world bounds to work with 5000-unit bounds (spawn positions may need adjustment)

### Board Verification
- Deploy to UNO Q boards
- Verify metrics endpoint shows reduced snapshot sizes
- Verify no flicker in client when entities cross AoI boundary

## Migration

### Backward Compatibility
- Clients need no changes to decode frames (same format, fewer entities)
- Existing test scenarios need spawn position updates for the larger world
- The `spawnCluster` helper ensures tests work regardless of world size

### Deployment Order
1. World expansion (shared constant + client visuals) -- no behavioral change
2. Test infrastructure updates -- no behavioral change
3. AoI filter -- behavioral change, deploy and verify on boards before proceeding

## Constants Summary

| Constant | Before | After | File |
|----------|--------|-------|------|
| WORLD_EXTENT | 500.0 | 5000.0 | shared/spatial.rs |
| AOI_RADIUS | (new) | 500.0 | shared/spatial.rs |
| AOI_RADIUS_SQ | (new) | 250000.0 | shared/spatial.rs |
| CELL_SIZE | 50.0 | 50.0 (unchanged) | shared/spatial.rs |
| Ground plane | 1000x1000 | 10000x10000 | client/renderer.ts |
| Camera far | 2000 | 20000 | client/renderer.ts |
| Camera position | (0,200,200) | (0,2000,2000) | client/renderer.ts |
| Camera maxDistance | 1000 | 10000 | client/renderer.ts |
