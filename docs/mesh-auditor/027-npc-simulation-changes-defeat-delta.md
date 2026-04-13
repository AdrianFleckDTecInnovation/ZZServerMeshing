# NPC-DEFEATS-DELTA: NPC behavior system changes velocity every tick, defeating delta compression

## Severity: medium

## Category: bottleneck

## Summary
The `npc_behavior` system modifies NPC velocity every `NPC_DIRECTION_CHANGE_INTERVAL` (60 ticks = 3 seconds). However, the physics integration system updates position every tick based on velocity. Since every moving NPC changes position every tick by `velocity * dt`, delta compression never filters them out -- every NPC with nonzero velocity produces a delta update every tick. This means delta compression provides zero bandwidth savings for NPC-heavy worlds where all NPCs are moving, which is the common case.

## Evidence
In `crates/sim-server/src/systems.rs`, the `physics_integration` system applies velocity to position every tick:
```rust
pub fn physics_integration(
    tick: Res<TickCounter>,
    mut query: Query<(&mut Position, &Velocity), With<Authoritative>>,
) {
    for (mut pos, vel) in query.iter_mut() {
        pos.0 += vel.0 * TICK_DURATION.as_secs_f32();
    }
}
```

Every NPC with nonzero velocity moves `velocity * 0.05` per tick. At minimum velocity of 1.0 units/sec, that is 0.05 units per tick, which is 5x the `CHANGE_THRESHOLD` of 0.01. So every moving NPC triggers a delta update every tick.

**Delta compression bandwidth for a world with 500 moving NPCs:**
- Full snapshot: 27 + 500 * 47 = 23,527 bytes
- Delta with 500 updates (all NPCs moved): 31 + 500 * 47 + 0 * 16 = 23,531 bytes

The delta frame is actually 4 bytes LARGER than the full snapshot (due to the extra `removal_count: u32` field in the delta header) when all entities change.

The only entities that benefit from delta compression are:
1. Stationary entities (velocity = 0) -- these are excluded from deltas
2. Entities whose position/velocity has not changed since last tick (e.g., a player not pressing any keys)

In a typical world with 500 NPCs all moving, delta compression saves nothing. It adds CPU cost (HashMap lookups, HashSet construction) for zero bandwidth benefit.

## Impact
**Bandwidth:** Delta compression provides no savings when most entities are moving NPCs. The periodic full snapshot at 100-tick intervals is actually worse than no delta compression, because the server sends a mix of full and delta frames that are the same size, plus the CPU overhead of computing deltas.

**CPU:** The delta computation (O(E) per client per tick) adds work for no benefit. This directly eats into the 50ms tick budget.

**When delta compression IS effective:** When interest management is implemented (Phase B in mesh-improvements.md), clients will receive subsets of entities. Between ticks, most entities in a client's AOI will not change. Delta compression becomes very effective in that scenario.

**Current state:** Delta compression is premature without interest management. It adds complexity and CPU cost for negligible bandwidth savings.

## Recommended Fix
Three options:

**Option A -- Acknowledge and accept (recommended for now):**
Document that delta compression is pre-deployed for Phase B (interest management). The infrastructure is correct and tested. It just does not produce savings until entity filtering creates stable subsets. No code changes needed.

**Option B -- Skip delta for all-changed frames:**
Add a heuristic: if the delta contains more than 80% of the entity count as updates, send a full snapshot instead (fewer header bytes, same entity data):

```rust
let delta = compute_delta(&entities, baseline);
let use_delta = delta.updates.len() < entities.len() * 80 / 100;

if use_delta {
    let frame = encode_binary_delta(tick.0, &delta.updates, &delta.removals);
    update_baseline(baseline, &delta);
    frame
} else {
    *baseline = build_baseline(&entities);
    handle.ticks_since_full = 0;
    full_frame.clone()
}
```

This avoids the overhead of sending a delta frame that is the same size as a full frame.

**Option C -- Only enable delta for player entities:**
Player entities change infrequently (only when the player presses a key). NPCs change every tick. Send NPCs as full-set always; use delta only for player entity positions. This is more complex but targets where delta actually helps.

## Effort: small (option A or B)

## Related Files
- crates/sim-server/src/systems.rs (physics_integration applies velocity every tick)
- crates/sim-server/src/systems.rs (npc_behavior sets NPC velocity)
- crates/sim-server/src/delta.rs:13 (CHANGE_THRESHOLD = 0.01)
- docs/mesh-improvements.md:91-118 (Phase C: Delta Compression rationale)
